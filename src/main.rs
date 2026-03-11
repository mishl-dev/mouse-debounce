use evdev::{
    uinput::VirtualDevice,
    AttributeSet, Device, EventType, InputEvent,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(clap::Parser, Debug)]
#[clap(
    name = "mouse-debounce",
    about = "Software mouse button debounce daemon",
    long_about = "Grabs a mouse device and filters bounce events.\nCLI flags override config file values."
)]
struct Cli {
    /// Path to the input device, e.g. /dev/input/by-id/usb-...-event-mouse
    #[clap(short, long)]
    device: Option<String>,

    /// Default debounce time in milliseconds
    #[clap(short, long, default_value_t = 50)]
    ms: u64,

    /// Path to TOML config file (overrides auto-discovery)
    #[clap(short, long)]
    config: Option<PathBuf>,

    /// Only debounce these buttons by evdev code (e.g. 272=left, 273=right, 274=middle).
    /// If omitted, all buttons are debounced.
    #[clap(short, long, value_delimiter = ',')]
    buttons: Vec<u16>,

    /// Print every debounced event (useful for tuning)
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
    // Search order:
    // 1. CLI --config flag
    // 2. $XDG_CONFIG_HOME/mouse-debounce/config.toml
    // 3. ~/.config/mouse-debounce/config.toml
    // 4. /etc/mouse-debounce/config.toml
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

    fn load(path: &PathBuf) -> Self {
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

// Scan /dev/input for devices that look like mice (have left click + relative axes).
// If multiple are found, picks the first. Prints what it found so the user knows.
fn detect_mouse() -> Option<PathBuf> {
    let by_id = PathBuf::from("/dev/input/by-id");
    if by_id.exists() {
        // Prefer by-id symlinks — stable across reboots
        if let Ok(entries) = std::fs::read_dir(&by_id) {
            let mut candidates: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.to_string_lossy().contains("event-mouse")
                })
                .collect();
            candidates.sort();
            if let Some(path) = candidates.first() {
                return Some(path.clone());
            }
        }
    }
    // Fallback: scan /dev/input/event* and check for BTN_LEFT support
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
                    // BTN_LEFT = 272
                    if keys.contains(evdev::KeyCode::BTN_LEFT) {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();
    let config_path = Config::find(cli.config);
    let cfg = Config::load(&config_path);

    let default_ms = cli.ms.max(cfg.ms.unwrap_or(0)).max(1);
    let device_path = cli.device
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

    let mut button_ms: HashMap<u16, Duration> = HashMap::new();
    if let Some(cfg_buttons) = &cfg.buttons {
        for (code_str, &ms) in cfg_buttons {
            if let Ok(code) = code_str.parse::<u16>() {
                button_ms.insert(code, Duration::from_millis(ms));
            }
        }
    }
    for code in &cli.buttons {
        button_ms.entry(*code).or_insert(Duration::from_millis(default_ms));
    }

    let debounce_all = cli.buttons.is_empty() && cfg.buttons.is_none();
    let default_debounce = Duration::from_millis(default_ms);

    let mut device = Device::open(&device_path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {e}", device_path.display()));

    println!(
        "Opened: {} | default debounce: {}ms | mode: {}",
        device.name().unwrap_or("unknown"),
        default_ms,
        if debounce_all { "all buttons" } else { "selected buttons only" }
    );

    device.grab().expect("Failed to grab device — are you root?");

    let mut keys = AttributeSet::<evdev::KeyCode>::new();
    if let Some(supported) = device.supported_keys() {
        for key in supported.iter() {
            keys.insert(key);
        }
    }

    let mut virt = VirtualDevice::builder()
        .expect("Failed to create virtual device builder")
        .name("mouse-debounce")
        .with_keys(&keys)
        .expect("Failed to set keys")
        .build()
        .expect("Failed to build virtual device");

    let mut state: HashMap<u16, (Instant, i32)> = HashMap::new();
    let epoch = Instant::now() - Duration::from_secs(10);

    loop {
        let events: Vec<InputEvent> = device
            .fetch_events()
            .expect("Failed to fetch events")
            .collect();

        for ev in events {
            let pass = if ev.event_type() == EventType::KEY {
                let code = ev.code();
                let value = ev.value();

                let debounce = if debounce_all {
                    default_debounce
                } else if let Some(&d) = button_ms.get(&code) {
                    d
                } else {
                    virt.emit(&[ev]).expect("emit failed");
                    continue;
                };

                let entry = state.entry(code).or_insert((epoch, -1));
                let elapsed = entry.0.elapsed();
                let last_value = entry.1;

                if elapsed >= debounce && last_value != value {
                    *entry = (Instant::now(), value);
                    true
                } else {
                    if cli.verbose && elapsed < debounce {
                        eprintln!(
                            "Debounced BTN={code} value={value} elapsed={}ms",
                            elapsed.as_millis()
                        );
                    }
                    false
                }
            } else {
                true
            };

            if pass {
                virt.emit(&[ev]).expect("emit failed");
            }
        }
    }
}