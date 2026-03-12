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
    long_about = "Grabs a mouse device and filters bounce events using Asymmetric Eager-Defer.\nCLI flags override config file values."
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
    debounce_dur: Duration,
    is_debounced: bool,
}

#[derive(Clone, Copy)]
struct ButtonState {
    logical_state: bool,
    physical_state: bool,
    press_lockout_until: Option<Instant>,
    release_deadline: Option<Instant>,
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();
    let config_path = Config::find(cli.config);
    let cfg = Config::load(&config_path);

    let default_ms = cli.ms.max(cfg.ms.unwrap_or(0)).max(1);
    let device_path = cli
        .device
        .or(cfg.device)
        .map(PathBuf::from)
        .or_else(|| {
            let found = detect_mouse();
            if let Some(ref p) = found {
                eprintln!("Auto-detected mouse: {}", p.display());
            }
            found
        })
        .expect("No mouse device found. Use --device or set 'device' in config.");

    let debounce_all = cli.buttons.is_empty() && cfg.buttons.is_none();
    let default_debounce = Duration::from_millis(default_ms);

    let mut device = Device::open(&device_path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {e}", device_path.display()));

    println!(
        "Opened: {} | Eager-Defer Debounce: {}ms | mode: {}",
        device.name().unwrap_or("unknown"),
        default_ms,
        if debounce_all { "all buttons" } else { "selected buttons only" }
    );

    device.grab().expect("Failed to grab device — are you root?");

    let mut keys = AttributeSet::<evdev::KeyCode>::new();
    let mut max_key_code = 0;
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
        .expect("Failed to create virtual device builder")
        .name("mouse-debounce")
        .with_keys(&keys)
        .expect("Failed to set keys")
        .with_relative_axes(&axes)
        .expect("Failed to set axes")
        .build()
        .expect("Failed to build virtual device");

    let table_size = (max_key_code + 1).max(768) as usize;

    let mut btn_configs = vec![
        ButtonConfig {
            debounce_dur: default_debounce,
            is_debounced: debounce_all,
        };
        table_size
    ];
    let mut btn_states = vec![
        ButtonState {
            logical_state: false,
            physical_state: false,
            press_lockout_until: None,
            release_deadline: None,
        };
        table_size
    ];

    if let Some(cfg_buttons) = &cfg.buttons {
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

    for &code in &cli.buttons {
        if (code as usize) < table_size {
            btn_configs[code as usize] = ButtonConfig {
                debounce_dur: default_debounce,
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
                    eprintln!("Device read error (disconnected?): {}", e);
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
                            let config = &btn_configs[code];
                            let state = &mut btn_states[code];

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
                                } else if cli.verbose && !lockout_over && !state.logical_state {
                                    eprintln!("Blocked bounce on PRESS BTN={code}");
                                }
                            } else if state.logical_state {
                                state.release_deadline = Some(now + config.debounce_dur);
                                if !active_buttons.contains(&code) {
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
                eprintln!("Event reader thread exited. Shutting down.");
                break;
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
                        if cli.verbose {
                            eprintln!("Emitted deferred RELEASE BTN={code}");
                        }
                    }
                    state.release_deadline = None;
                    return false;
                }
            } else {
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
            virt.emit(&emit_buffer).expect("emit failed");
        }
    }
}