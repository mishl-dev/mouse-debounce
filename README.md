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
The service reads `/etc/mouse-debounce/config.toml`. When running as root
(service or sudo) without `--config`, the system path wins — root never trusts
the invoking user's `$HOME`/`$XDG_CONFIG_HOME`. For manual root runs prefer:
`sudo -H mouse-debounce --config /etc/mouse-debounce/config.toml`.

## How it works (Asymmetric Eager-Defer)

Press is eager (emitted immediately plus a lockout window); release is deferred
by the debounce window and cancelled by a re-press. Consequences: every release
costs `ms` of up-latency, clicks separated by less than `ms` across a release
merge into one hold, and holds are safe (re-press cancels the pending release).
Releases already due always emit before newly arrived events in the same tick.
Only mouse buttons (`>= 0x100`) are debounced; keyboard keys on combo dongles
pass through. Prefer the mouse-only `/dev/input/by-id/*event-mouse` node for
combo receivers. Middle-click paste (`L+R`) and drags work: presses are never
delayed, releases stretch by at most one window.

## Usage

```bash
# Use config file
sudo mouse-debounce

# CLI only
sudo mouse-debounce --device /dev/input/by-id/usb-...-event-mouse --ms 50

# Tune with verbose output
sudo mouse-debounce --verbose
```