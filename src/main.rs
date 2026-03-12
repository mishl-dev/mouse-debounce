use evdev::{AttributeSet, Device, EventType, InputEvent, RelativeAxisCode, uinput::VirtualDevice};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(clap::Parser, Debug)]
#[clap(
    name = "mouse-debounce",
    about = "Software mouse button debounce daemon",
    long_about = "Grabs a mouse device and filters bounce events using Asymmetric Eager-Defer with adaptive EMA thresholds.\nPress events are emitted immediately; release events are deferred until the debounce window expires.\nThe debounce window per button adapts over time by tracking bounce vs. repress interval distributions.\nCLI flags override config file values."
)]
struct Cli {
    #[clap(short, long)]
    device: Option<String>,

    #[clap(short, long, default_value_t = 50)]
    ms: u64,

    #[clap(short, long)]
    config: Option<PathBuf>,

    #[clap(short, long, value_delimiter = ',')]
    buttons: Vec<u16>,

    #[clap(short, long)]
    verbose: bool,
}

#[derive(serde::Deserialize, Debug, Default)]
struct Config {
    device: Option<String>,
    ms: Option<u64>,
    buttons: Option<HashMap<String, u64>>,
}

impl Config {
    fn find(explicit: Option<PathBuf>) -> PathBuf {
        if let Some(p) = explicit {
            return p;
        }
        let xdg = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs_next::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/root"))
                    .join(".config")
            });
        let candidates = [
            xdg.join("mouse-debounce/config.toml"),
            PathBuf::from("/etc/mouse-debounce/config.toml"),
        ];
        for p in &candidates {
            if p.exists() {
                return p.clone();
            }
        }
        candidates[0].clone()
    }

    fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                eprintln!("Using config: {}", path.display());
                toml::from_str(&s).unwrap_or_else(|e| {
                    eprintln!("Warning: failed to parse config: {e}");
                    Config::default()
                })
            }
            Err(_) => Config::default(),
        }
    }
}

fn detect_mouse() -> Option<PathBuf> {
    let by_id = PathBuf::from("/dev/input/by-id");
    if by_id.exists() {
        if let Ok(entries) = std::fs::read_dir(&by_id) {
            let mut candidates: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().contains("event-mouse"))
                .collect();
            candidates.sort();
            if let Some(path) = candidates.first() {
                return Some(path.clone());
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir("/dev/input") {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains("event"))
            .collect();
        candidates.sort();
        for path in candidates {
            if let Ok(dev) = Device::open(&path) {
                if let Some(keys) = dev.supported_keys() {
                    if keys.contains(evdev::KeyCode::BTN_LEFT) {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
struct ButtonConfig {
    is_debounced: bool,
}

const EMA_ALPHA: f64 = 0.15;
const ADAPTIVE_K: f64 = 4.0;
const ADAPTIVE_MIN_MS: f64 = 5.0;
const BOOTSTRAP_SAMPLES: u32 = 10;

#[derive(Clone, Copy)]
struct ButtonState {
    logical_state: bool,
    physical_state: bool,
    press_lockout_until: Option<Instant>,
    release_deadline: Option<Instant>,

    is_active: bool,
    floor_ms: f64,
    last_release_at: Option<Instant>,
    bounce_ema_ms: f64,
    repress_ema_ms: f64,
    adaptive_threshold: Duration,
    sample_count: u32,
}

impl ButtonState {
    fn new(bootstrap: Duration) -> Self {
        let bootstrap_ms = bootstrap.as_millis() as f64;
        ButtonState {
            logical_state: false,
            physical_state: false,
            press_lockout_until: None,
            release_deadline: None,
            last_release_at: None,
            is_active: false,
            floor_ms: bootstrap_ms,
            bounce_ema_ms: bootstrap_ms / ADAPTIVE_K,
            repress_ema_ms: (bootstrap_ms * 4.0).max(180.0),
            adaptive_threshold: bootstrap,
            sample_count: 0,
        }
    }

    fn update_adaptive(&mut self, now: Instant, verbose: bool, code: usize) {
        let Some(last_rel) = self.last_release_at else { return };

        let interval_ms = now.duration_since(last_rel).as_secs_f64() * 1000.0;
        let t = self.adaptive_threshold.as_secs_f64() * 1000.0;

        let learned = if interval_ms < t {
            self.bounce_ema_ms = self.bounce_ema_ms * (1.0 - EMA_ALPHA) + interval_ms * EMA_ALPHA;
            true
        } else if interval_ms > t * 2.0 {
            self.repress_ema_ms = self.repress_ema_ms * (1.0 - EMA_ALPHA) + interval_ms * EMA_ALPHA;
            true
        } else {
            false
        };

        if learned {
            self.sample_count = self.sample_count.saturating_add(1);
        }

        if self.sample_count >= BOOTSTRAP_SAMPLES {
            let new_t = (self.bounce_ema_ms * ADAPTIVE_K)
                .max(ADAPTIVE_MIN_MS)
                .min(self.repress_ema_ms / 2.0)
                .max(self.floor_ms);
            let new_dur = Duration::from_micros((new_t * 1000.0) as u64);
            let delta_us = (new_dur.as_micros() as i64 - self.adaptive_threshold.as_micros() as i64).abs();
            if verbose && delta_us > 3000 {
                eprintln!(
                    "Adaptive BTN={code}: threshold {:.1}ms → {:.1}ms \
                     (bounce_ema={:.1}ms repress_ema={:.1}ms interval={:.1}ms)",
                    t, new_t, self.bounce_ema_ms, self.repress_ema_ms, interval_ms
                );
            }
            self.adaptive_threshold = new_dur;
        }
    }
}

struct RunParams<'a> {
    device_path: &'a Path,
    debounce_all: bool,
    default_debounce: Duration,
    cli_buttons: &'a [u16],
    cfg_buttons: &'a Option<HashMap<String, u64>>,
    verbose: bool,
}

fn run(p: &RunParams) -> Result<(), Box<dyn std::error::Error>> {
    let mut device = Device::open(p.device_path)
        .map_err(|e| format!("Failed to open {}: {e}", p.device_path.display()))?;

    println!(
        "Opened: {} | Eager-Defer Debounce: {}ms (adaptive) | mode: {}",
        device.name().unwrap_or("unknown"),
        p.default_debounce.as_millis(),
        if p.debounce_all { "all buttons" } else { "selected buttons only" }
    );

    device.grab().map_err(|e| format!("Failed to grab device: {e}"))?;

    let mut keys = AttributeSet::<evdev::KeyCode>::new();
    let mut max_key_code = 0u16;
    if let Some(supported) = device.supported_keys() {
        for key in supported.iter() {
            keys.insert(key);
            max_key_code = max_key_code.max(key.code());
        }
    }

    let mut axes = AttributeSet::<RelativeAxisCode>::new();
    if let Some(supported) = device.supported_relative_axes() {
        for axis in supported.iter() {
            axes.insert(axis);
        }
    }

    let mut virt = VirtualDevice::builder()
        .map_err(|e| format!("Failed to create virtual device builder: {e}"))?
        .name("mouse-debounce")
        .with_keys(&keys)
        .map_err(|e| format!("Failed to set keys: {e}"))?
        .with_relative_axes(&axes)
        .map_err(|e| format!("Failed to set axes: {e}"))?
        .build()
        .map_err(|e| format!("Failed to build virtual device: {e}"))?;

    let table_size = (max_key_code + 1).max(768) as usize;

    let mut btn_configs = vec![
        ButtonConfig { is_debounced: p.debounce_all };
        table_size
    ];

    let mut btn_states: Vec<ButtonState> = (0..table_size)
        .map(|_| ButtonState::new(p.default_debounce))
        .collect();

    if let Some(cfg_buttons) = p.cfg_buttons {
        for (code_str, &ms) in cfg_buttons {
            if let Ok(code) = code_str.parse::<u16>() {
                if (code as usize) < table_size {
                    let dur = Duration::from_millis(ms);
                    btn_configs[code as usize] = ButtonConfig { is_debounced: true };
                    btn_states[code as usize] = ButtonState::new(dur);
                }
            }
        }
    }

    for &code in p.cli_buttons {
        if (code as usize) < table_size {
            btn_configs[code as usize] = ButtonConfig { is_debounced: true };
        }
    }

    let (tx, rx) = mpsc::channel::<Vec<InputEvent>>();

    std::thread::spawn(move || {
        loop {
            match device.fetch_events() {
                Ok(events) => {
                    let batch: Vec<InputEvent> = events.collect();
                    if tx.send(batch).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Device read error: {e}");
                    break;
                }
            }
        }
    });

    let mut emit_buffer: Vec<InputEvent> = Vec::with_capacity(64);
    let mut active_buttons: Vec<usize> = Vec::with_capacity(16);

    loop {
        let now = Instant::now();

        let next_deadline = active_buttons
            .iter()
            .filter_map(|&code| btn_states[code].release_deadline)
            .min();

        let timeout = next_deadline.map(|nd| nd.saturating_duration_since(now));

        let received = if let Some(t) = timeout {
            if t.is_zero() {
                Err(mpsc::RecvTimeoutError::Timeout)
            } else {
                rx.recv_timeout(t)
            }
        } else {
            rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };

        let now = Instant::now();
        emit_buffer.clear();

        match received {
            Ok(events) => {
                for ev in events {
                    if ev.event_type() == EventType::KEY {
                        let code = ev.code() as usize;
                        let value = ev.value();

                        if code < table_size && btn_configs[code].is_debounced {
                            let state = &mut btn_states[code];

                            state.physical_state = value != 0;

                            if state.physical_state {
                                state.release_deadline = None;
                                state.update_adaptive(now, p.verbose, code);
                                let threshold = state.adaptive_threshold;

                                let lockout_over = state
                                    .press_lockout_until
                                    .map(|t| now >= t)
                                    .unwrap_or(true);

                                if !state.logical_state && lockout_over {
                                    state.logical_state = true;
                                    state.press_lockout_until = Some(now + threshold);
                                    emit_buffer.push(ev);
                                } else if p.verbose && !lockout_over && !state.logical_state {
                                    eprintln!(
                                        "Blocked bounce on PRESS BTN={code} \
                                         (threshold={:.1}ms)",
                                        threshold.as_secs_f64() * 1000.0
                                    );
                                }
                            } else if state.logical_state {
                                let threshold = state.adaptive_threshold;
                                state.last_release_at = Some(now);
                                state.release_deadline = Some(now + threshold);
                                if !state.is_active {
                                    state.is_active = true;
                                    active_buttons.push(code);
                                }
                            }
                        } else {
                            emit_buffer.push(ev);
                        }
                    } else {
                        emit_buffer.push(ev);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Device disconnected".into());
            }
        }

        let mut emitted_deferred = false;
        active_buttons.retain(|&code| {
            let state = &mut btn_states[code];
            if let Some(dl) = state.release_deadline {
                if now >= dl {
                    if !state.physical_state && state.logical_state {
                        state.logical_state = false;
                        emit_buffer.push(InputEvent::new(EventType::KEY.0, code as u16, 0));
                        emitted_deferred = true;
                        if p.verbose {
                            eprintln!(
                                "Emitted deferred RELEASE BTN={code} \
                                 (threshold={:.1}ms samples={})",
                                state.adaptive_threshold.as_secs_f64() * 1000.0,
                                state.sample_count
                            );
                        }
                    }
                    state.release_deadline = None;
                    state.is_active = false;
                    return false;
                }
            } else {
                state.is_active = false;
                return false;
            }
            true
        });

        if emitted_deferred {
            emit_buffer.push(InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                evdev::SynchronizationCode::SYN_REPORT.0,
                0,
            ));
        }

        if !emit_buffer.is_empty() {
            virt.emit(&emit_buffer).map_err(|e| format!("emit failed: {e}"))?;
        }
    }
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();
    let config_path = Config::find(cli.config);
    let cfg = Config::load(&config_path);

    let default_ms = cli.ms.max(cfg.ms.unwrap_or(0)).max(1);
    let debounce_all = cli.buttons.is_empty() && cfg.buttons.is_none();
    let default_debounce = Duration::from_millis(default_ms);
    let explicit_device: Option<PathBuf> = cli.device.or(cfg.device).map(PathBuf::from);

    const RETRY_DELAY: Duration = Duration::from_millis(500);

    loop {
        let device_path = match &explicit_device {
            Some(p) => p.clone(),
            None => match detect_mouse() {
                Some(p) => {
                    eprintln!("Auto-detected mouse: {}", p.display());
                    p
                }
                None => {
                    eprintln!("No mouse device found, retrying...");
                    std::thread::sleep(RETRY_DELAY);
                    continue;
                }
            },
        };

        let params = RunParams {
            device_path: &device_path,
            debounce_all,
            default_debounce,
            cli_buttons: &cli.buttons,
            cfg_buttons: &cfg.buttons,
            verbose: cli.verbose,
        };

        if let Err(e) = run(&params) {
            eprintln!("Error: {e}. Retrying...");
            std::thread::sleep(RETRY_DELAY);
        } else {
            break;
        }
    }
}