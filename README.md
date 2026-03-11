# mouse-debounce

Software mouse button debounce daemon for Linux (Wayland/X11).

Tries to fix double-click issues caused by worn switches without replacing hardware.

## Install
```bash
cargo build --release
sudo cp target/release/mouse-debounce /usr/local/bin/
```

## Config

Copy `config/config.example.toml` to `~/.config/mouse-debounce/config.toml` and edit the device path.

## Usage
```bash
# Use config file
sudo mouse-debounce

# CLI only
sudo mouse-debounce --device /dev/input/by-id/usb-...-event-mouse --ms 50

# Tune with verbose output
sudo mouse-debounce --verbose
```