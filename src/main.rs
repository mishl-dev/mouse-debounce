use evdev::{AttributeSet, Device, EventType, InputEvent, RelativeAxisCode, uinput::VirtualDevice};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MIN_EVENT_GAP: Duration = Duration::from_millis(1);

#[derive(clap::Parser, Debug)]
#[clap(
    name = "mouse-debounce",
    about = "Software mouse button debounce daemon",
    long_about = "Grabs a mouse device and filters bounce events using Asymmetric Eager-Defer.\nCLI flags override config file values."
)]
struct Cli {
    #[clap(short, long)]
    device: Option<String>,

    /// Debounce window in milliseconds. Defaults to 40 if neither CLI nor config sets it.
    #[clap(short, long)]
    ms: Option<u64>,

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
    debounce_dur: Duration,
    is_debounced: bool,
}

#[derive(Clone, Copy)]
struct ButtonState {
    logical_state: bool,
    physical_state: bool,
    press_lockout_until: Option<Instant>,
    release_deadline: Option<Instant>,
    last_event_instant: Option<Instant>,
    pending_release: bool,
}

struct RunParams<'a> {
    device_path: &'a Path,
    debounce_all: bool,
    default_debounce: Duration,
    cli_buttons: &'a [u16],
    cfg_buttons: &'a Option<HashMap<String, u64>>,
    verbose: bool,
}

// TODO: swap InputEvent::new for new_with_time once on evdev ≥ 0.12
fn now_event(type_: u16, code: u16, value: i32) -> InputEvent {
    let _d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    InputEvent::new(type_, code, value)
}

fn run(p: &RunParams) -> Result<(), Box<dyn std::error::Error>> {
    let mut device = Device::open(p.device_path)
        .map_err(|e| format!("Failed to open {}: {e}", p.device_path.display()))?;

    println!(
        "Opened: {} | Eager-Defer Debounce: {}ms | mode: {}",
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
        .with_absolute_axis(&evdev::UinputAbsSetup::new(
            evdev::AbsoluteAxisCode::ABS_X,
            evdev::AbsInfo::new(0, 0, 65535, 0, 0, 1),
        ))
        .map_err(|e| format!("Failed to set abs x: {e}"))?
        .with_absolute_axis(&evdev::UinputAbsSetup::new(
            evdev::AbsoluteAxisCode::ABS_Y,
            evdev::AbsInfo::new(0, 0, 65535, 0, 0, 1),
        ))
        .map_err(|e| format!("Failed to set abs y: {e}"))?
        .build()
        .map_err(|e| format!("Failed to build virtual device: {e}"))?;

    let table_size = (max_key_code + 1).max(768) as usize;

    let mut btn_configs = vec![
        ButtonConfig {
            debounce_dur: p.default_debounce,
            is_debounced: p.debounce_all,
        };
        table_size
    ];
    let mut btn_states = vec![
        ButtonState {
            logical_state: false,
            physical_state: false,
            press_lockout_until: None,
            release_deadline: None,
            last_event_instant: None,
            pending_release: false,
        };
        table_size
    ];

    if let Some(cfg_buttons) = p.cfg_buttons {
        for (code_str, &ms) in cfg_buttons {
            if let Ok(code) = code_str.parse::<u16>() {
                if (code as usize) < table_size {
                    btn_configs[code as usize] = ButtonConfig {
                        debounce_dur: Duration::from_millis(ms),
                        is_debounced: true,
                    };
                }
            }
        }
    }

    for &code in p.cli_buttons {
        if (code as usize) < table_size {
            btn_configs[code as usize] = ButtonConfig {
                debounce_dur: p.default_debounce,
                is_debounced: true,
            };
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
    let mut pending_codes: Vec<usize> = Vec::with_capacity(16);

    loop {
        let now = Instant::now();

        let next_deadline = pending_codes
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
                            let config = &btn_configs[code];
                            let state = &mut btn_states[code];

                            if let Some(last) = state.last_event_instant {
                                if now.duration_since(last) < MIN_EVENT_GAP {
                                    if p.verbose {
                                        eprintln!(
                                            "Dropped sub-1ms noise BTN={code} value={value}"
                                        );
                                    }
                                    continue;
                                }
                            }
                            state.last_event_instant = Some(now);

                            state.physical_state = value != 0;

                            if state.physical_state {
                                state.release_deadline = None;

                                let lockout_over = state
                                    .press_lockout_until
                                    .map(|t| now >= t)
                                    .unwrap_or(true);

                                if !state.logical_state && lockout_over {
                                    state.logical_state = true;
                                    state.press_lockout_until = Some(now + config.debounce_dur);
                                    emit_buffer.push(ev);
                                } else if p.verbose && !lockout_over && !state.logical_state {
                                    eprintln!("Blocked bounce on PRESS BTN={code}");
                                }
                            } else if state.logical_state {
                                state.release_deadline = Some(now + config.debounce_dur);
                                if !state.pending_release {
                                    state.pending_release = true;
                                    pending_codes.push(code);
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
        pending_codes.retain(|&code| {
            let state = &mut btn_states[code];
            if let Some(dl) = state.release_deadline {
                if now >= dl {
                    if !state.physical_state && state.logical_state {
                        state.logical_state = false;
                        emit_buffer.push(now_event(EventType::KEY.0, code as u16, 0));
                        emitted_deferred = true;
                        if p.verbose {
                            eprintln!("Emitted deferred RELEASE BTN={code}");
                        }
                    }
                    state.release_deadline = None;
                    state.pending_release = false;
                    return false;
                }
            } else {
                state.pending_release = false;
                return false;
            }
            true
        });

        if emitted_deferred {
            emit_buffer.push(now_event(
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

    let default_ms = cli.ms.or(cfg.ms).unwrap_or(40).max(1);

    let debounce_all = cli.buttons.is_empty()
        && cfg.buttons.as_ref().map_or(true, |m| m.is_empty());

    if !debounce_all
        && cli.buttons.is_empty()
        && cfg.buttons.as_ref().map_or(false, |m| m.is_empty())
    {
        eprintln!("Warning: config specifies an empty [buttons] table — no buttons will be debounced.");
    }

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