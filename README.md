# mouse-debounce

Software mouse button debounce daemon for Linux. Fixes spurious double-clicks caused by worn switches without replacing hardware.

Uses Asymmetric Eager-Defer: press events are emitted immediately (zero added latency), release events are deferred until the debounce window expires. The debounce window adapts per-button at runtime by tracking bounce vs. repress interval distributions, so it stays correct as switches age.

## Install

```bash
cargo build --release
sudo cp target/release/mouse-debounce /usr/bin/
```

To run on boot, install the included systemd unit:

```bash
sudo cp config/mouse-debounce.service /etc/systemd/system/
sudo systemctl enable --now mouse-debounce
```

## Configuration

Copy the example config and edit as needed:

```bash
mkdir -p ~/.config/mouse-debounce
cp config/config.example.toml ~/.config/mouse-debounce/config.toml
```

The device is autodetected if not specified. Default debounce seed is 50ms. System-wide fallback: `/etc/mouse-debounce/config.toml`

## Usage

```bash
# run with autodetect and defaults
sudo mouse-debounce

# explicit device and threshold
sudo mouse-debounce --device /dev/input/by-id/usb-...-event-mouse --ms 50

# debounce specific buttons only (comma-separated evdev codes)
sudo mouse-debounce --buttons 272,273

# watch threshold adaptation in real time
sudo mouse-debounce --verbose
```

CLI flags override config file values.

## License

MIT