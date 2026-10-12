use evdev::{AttributeSet, Device, EventType, InputEvent, RelativeAxisCode, uinput::VirtualDevice};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

const MIN_DEBOUNCE_MS: u64 = 1;
const MAX_DEBOUNCE_MS: u64 = 1000;
const DEFAULT_DEBOUNCE_MS: u64 = 40;

fn clamp_debounce_ms(ms: u64) -> u64 {
    ms.clamp(MIN_DEBOUNCE_MS, MAX_DEBOUNCE_MS)
}

fn debounce_duration(ms: u64) -> Duration {
    Duration::from_millis(clamp_debounce_ms(ms))
}

fn deadline_after(now: Instant, dur: Duration) -> Instant {
    now.checked_add(dur).unwrap_or(now)
}

fn is_mouse_button(code: u16) -> bool {
    code >= 0x100
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn acquire_singleton() -> std::io::Result<fs::File> {
    use fs2::FileExt;
    let uid = unsafe { libc::getuid() };
    let candidates = [
        PathBuf::from("/run/mouse-debounce.lock"),
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| PathBuf::from(d).join("mouse-debounce.lock"))
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/mouse-debounce-{uid}.lock"))),
        PathBuf::from(format!("/tmp/mouse-debounce-{uid}.lock")),
    ];
    let mut last_err: Option<std::io::Error> = None;
    for path in &candidates {
        match fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path) {
            Ok(f) => match f.try_lock_exclusive() {
                Ok(()) => {
                    use std::io::{Seek, SeekFrom, Write};
                    let mut f = f;
                    let _ = f.set_len(0);
                    let _ = f.seek(SeekFrom::Start(0));
                    let _ = writeln!(f, "{}", std::process::id());
                    let _ = f.flush();
                    return Ok(f);
                }
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            },
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no lock path")))
}

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
    absolute_axes: Option<bool>,
}

impl Config {
    fn find(explicit: Option<PathBuf>) -> PathBuf {
        if let Some(p) = explicit {
            return p;
        }
        let system = PathBuf::from("/etc/mouse-debounce/config.toml");
        // Root daemon must not trust invoking-user env (sudo -E keeps $HOME/$XDG).
        // Prefer the admin-owned system config when running as root.
        if is_root() {
            if system.exists() {
                return system;
            }
            if let Some(p) = explicit {
                return p;
            }
            eprintln!("Warning: running as root with no system config; falling back to defaults");
            return system;
        }
        let xdg_raw = std::env::var("XDG_CONFIG_HOME").ok();
        let xdg = match xdg_raw {
            Some(v) if !v.is_empty() => {
                let p = PathBuf::from(&v);
                if p.is_absolute() {
                    p
                } else {
                    eprintln!("Warning: ignoring non-absolute XDG_CONFIG_HOME={v:?}");
                    dirs_next::home_dir()
                        .unwrap_or_else(|| PathBuf::from("/root"))
                        .join(".config")
                }
            }
            _ => dirs_next::home_dir()
                .unwrap_or_else(|| PathBuf::from("/root"))
                .join(".config"),
        };
        let candidates = [
            xdg.join("mouse-debounce/config.toml"),
            system,
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
            Err(_) => {
                eprintln!("No config at {}; using defaults", path.display());
                Config::default()
            }
        }
    }
}

fn detect_mouse() -> Option<PathBuf> {
    fn is_self_named(path: &Path) -> bool {
        Device::open(path)
            .ok()
            .and_then(|d| d.name().map(|n| n == "mouse-debounce"))
            .unwrap_or(false)
    }
    let by_id = PathBuf::from("/dev/input/by-id");
    if by_id.exists() {
        if let Ok(entries) = std::fs::read_dir(&by_id) {
            let mut candidates: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().contains("event-mouse"))
                .collect();
            candidates.sort();
            for path in candidates {
                if is_self_named(&path) {
                    eprintln!("Skipping own virtual device in by-id scan: {}", path.display());
                    continue;
                }
                return Some(path);
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
                if dev.name().map(|n| n == "mouse-debounce").unwrap_or(false) {
                    eprintln!("Skipping own virtual device: {}", path.display());
                    continue;
                }
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
    pending_release: bool,
}

struct RunParams<'a> {
    device_path: &'a Path,
    debounce_all: bool,
    default_debounce: Duration,
    cli_buttons: &'a [u16],
    cfg_buttons: &'a Option<HashMap<String, u64>>,
    absolute_axes: bool,
    verbose: bool,
}

// Synthesized events carry zero time; the kernel restamps on uinput emit.
fn now_event(type_: u16, code: u16, value: i32) -> InputEvent {
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

    let mut builder = VirtualDevice::builder()
        .map_err(|e| format!("Failed to create virtual device builder: {e}"))?
        .name("mouse-debounce")
        .with_keys(&keys)
        .map_err(|e| format!("Failed to set keys: {e}"))?
        .with_relative_axes(&axes)
        .map_err(|e| format!("Failed to set axes: {e}"))?;

    // Absolute axes make libinput classify the proxy as a touch-class device,
    // which lets the compositor's touchpad config (e.g. natural_scroll) leak
    // onto an ordinary mouse. Off unless a tablet-like source device needs it.
    if p.absolute_axes {
        builder = builder
            .with_absolute_axis(&evdev::UinputAbsSetup::new(
                evdev::AbsoluteAxisCode::ABS_X,
                evdev::AbsInfo::new(0, 0, 65535, 0, 0, 1),
            ))
            .map_err(|e| format!("Failed to set abs x: {e}"))?
            .with_absolute_axis(&evdev::UinputAbsSetup::new(
                evdev::AbsoluteAxisCode::ABS_Y,
                evdev::AbsInfo::new(0, 0, 65535, 0, 0, 1),
            ))
            .map_err(|e| format!("Failed to set abs y: {e}"))?;
    }

    let mut virt = builder
        .build()
        .map_err(|e| format!("Failed to build virtual device: {e}"))?;

    let table_size = (max_key_code as usize + 1).max(768);

    // Combo dongles expose keyboard keys on the same node. Debouncing generic
    // keys swallows repeats and drops fast typing, so debounce_all stays
    // inside the mouse-button range (>= 0x100). Everything else passes through.
    let mut btn_configs = vec![
        ButtonConfig {
            debounce_dur: p.default_debounce,
            is_debounced: false,
        };
        table_size
    ];
    if p.debounce_all {
        btn_configs
            .iter_mut()
            .take(table_size.min(0x300))
            .skip(0x100)
            .for_each(|c| c.is_debounced = true);
    }
    let mut btn_states = vec![
        ButtonState {
            logical_state: false,
            physical_state: false,
            press_lockout_until: None,
            release_deadline: None,
            pending_release: false,
        };
        table_size
    ];

    if let Some(cfg_buttons) = p.cfg_buttons {
        for (code_str, &ms) in cfg_buttons {
            if let Ok(code) = code_str.parse::<u16>() {
                if (code as usize) < table_size {
                    if !is_mouse_button(code) {
                        eprintln!("Warning: ignoring non-mouse button override {code}");
                        continue;
                    }
                    let clamped = clamp_debounce_ms(ms);
                    if clamped != ms {
                        eprintln!("Warning: clamping button {code} {ms}ms to {clamped}ms");
                    }
                    btn_configs[code as usize] = ButtonConfig {
                        debounce_dur: Duration::from_millis(clamped),
                        is_debounced: true,
                    };
                } else {
                    eprintln!("Warning: ignoring out-of-range button override {code}");
                }
            }
        }
    }

    for &code in p.cli_buttons {
        if (code as usize) < table_size {
            if !is_mouse_button(code) {
                eprintln!("Warning: ignoring non-mouse button override {code}");
                continue;
            }
            btn_configs[code as usize] = ButtonConfig {
                debounce_dur: p.default_debounce,
                is_debounced: true,
            };
        } else {
            eprintln!("Warning: ignoring out-of-range button override {code}");
        }
    }

    if let Some(supported) = device.supported_keys() {
        let has_keyboard = supported.iter().any(|k| {
            let c = k.code();
            (0x1E..=0x35).contains(&c) || c == 0x1C || c == 0x39
        });
        if has_keyboard {
            eprintln!(
                "Warning: grabbed device exposes keyboard keys; combo dongle suspected. Only mouse buttons (>=0x100) are debounced; point --device at the mouse-only node if typing feels wrong."
            );
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

        // Drain-before-fresh: an already-expired release must hit the wire
        // before events that arrived in the same tick. Sorted by deadline so
        // heterogeneous per-button windows emit in deadline order.
        let mut due: Vec<(Instant, usize)> = pending_codes
            .iter()
            .filter_map(|&code| {
                btn_states[code]
                    .release_deadline
                    .map(|dl| (dl, code))
            })
            .filter(|(dl, _)| now >= *dl)
            .collect();
        due.sort();
        let mut emitted_deferred = false;
        for (_, code) in due {
            let dur = btn_configs[code].debounce_dur;
            let state = &mut btn_states[code];
            if !state.physical_state && state.logical_state {
                state.logical_state = false;
                emit_buffer.push(now_event(EventType::KEY.0, code as u16, 0));
                emitted_deferred = true;
                // Trailing-edge guard: the press lockout anchored at press time
                // always expired before this release fired, leaving a phantom
                // window. Re-anchor it here.
                state.press_lockout_until = Some(deadline_after(now, dur));
                if p.verbose {
                    eprintln!("Emitted deferred RELEASE BTN={code}");
                }
            }
            state.release_deadline = None;
            state.pending_release = false;
        }
        pending_codes.retain(|&code| btn_states[code].release_deadline.is_some());
        if emitted_deferred {
            emit_buffer.push(now_event(
                EventType::SYNCHRONIZATION.0,
                evdev::SynchronizationCode::SYN_REPORT.0,
                0,
            ));
        }

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
                                // Bounded release: chatter while a release is
                                // pending must not cancel/extend it. Swallow
                                // the edge and let the expiry guard decide on
                                // sampled physical state. A genuine re-hold
                                // (physical==1 at expiry) suppresses the
                                // release and clears the deadline there.
                                if state.logical_state && state.release_deadline.is_some() {
                                    continue;
                                }
                                state.release_deadline = None;

                                let lockout_over = state
                                    .press_lockout_until
                                    .map(|t| now >= t)
                                    .unwrap_or(true);

                                if !state.logical_state && lockout_over {
                                    state.logical_state = true;
                                    state.press_lockout_until =
                                        Some(deadline_after(now, config.debounce_dur));
                                    if value == 2 {
                                        emit_buffer.push(now_event(
                                            EventType::KEY.0,
                                            code as u16,
                                            1,
                                        ));
                                    } else {
                                        emit_buffer.push(ev);
                                    }
                                } else if p.verbose && !lockout_over && !state.logical_state {
                                    eprintln!("Blocked bounce on PRESS BTN={code}");
                                }
                            } else if state.logical_state {
                                // First-release-wins: re-arming on every edge
                                // lets sub-window chatter slide the deadline
                                // forever. Keep the original deadline.
                                if state.release_deadline.is_none() {
                                    state.release_deadline =
                                        Some(deadline_after(now, config.debounce_dur));
                                    if !state.pending_release {
                                        state.pending_release = true;
                                        pending_codes.push(code);
                                    }
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
                // Best-effort release flush: the compositor saw our presses on
                // this virt node; never leave it stuck down across teardown.
                let mut flush: Vec<InputEvent> = Vec::new();
                for (code, state) in btn_states.iter().enumerate() {
                    if state.logical_state {
                        flush.push(now_event(EventType::KEY.0, code as u16, 0));
                    }
                }
                if !flush.is_empty() {
                    flush.push(now_event(
                        EventType::SYNCHRONIZATION.0,
                        evdev::SynchronizationCode::SYN_REPORT.0,
                        0,
                    ));
                    let _ = virt.emit(&flush);
                }
                return Err("Device disconnected".into());
            }
        }

        // Reap stale entries left by press-cancel races (deadline None) so the
        // next min() computation never sees them.
        pending_codes.retain(|&code| {
            if btn_states[code].release_deadline.is_none() {
                btn_states[code].pending_release = false;
                false
            } else {
                true
            }
        });

        // Swallowed bounces still forward their frame SYN; drop SYN-only noise
        // so fully-suppressed clicks emit nothing.
        if !emitted_deferred
            && !emit_buffer.is_empty()
            && emit_buffer
                .iter()
                .all(|e| e.event_type() == EventType::SYNCHRONIZATION)
        {
            emit_buffer.clear();
        }

        if !emit_buffer.is_empty() {
            virt.emit(&emit_buffer).map_err(|e| format!("emit failed: {e}"))?;
        }
    }
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();
    let _singleton = acquire_singleton().unwrap_or_else(|e| {
        eprintln!("mouse-debounce is already running ({e}); exiting");
        std::process::exit(2);
    });
    let config_path = Config::find(cli.config);
    let cfg = Config::load(&config_path);

    let raw_ms = cli.ms.or(cfg.ms).unwrap_or(DEFAULT_DEBOUNCE_MS);
    let default_ms = clamp_debounce_ms(raw_ms);
    if default_ms != raw_ms {
        eprintln!("Warning: clamping debounce {raw_ms}ms to {default_ms}ms (max {MAX_DEBOUNCE_MS}ms)");
    }

    let debounce_all = cli.buttons.is_empty()
        && cfg.buttons.as_ref().map_or(true, |m| m.is_empty());

    if !debounce_all
        && cli.buttons.is_empty()
        && cfg.buttons.as_ref().map_or(false, |m| m.is_empty())
    {
        eprintln!("Warning: config specifies an empty [buttons] table — no buttons will be debounced.");
    }

    let default_debounce = debounce_duration(default_ms);
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
            absolute_axes: cfg.absolute_axes.unwrap_or(false),
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