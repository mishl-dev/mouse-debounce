# mouse-debounce

Got a mouse that double-clicks when you single-click? That's a worn switch
bouncing inside. This tool sits between your mouse and your computer and
filters out those fake extra clicks, so you don't have to buy a new mouse (or
open it up to replace the switch).

Works on Linux, both Wayland and X11.

## Install

```bash
cargo build --release
sudo cp target/release/mouse-debounce /usr/local/bin/
```

## Setup

1. Copy the example config and pick your mouse:
   ```bash
   mkdir -p ~/.config/mouse-debounce
   cp config/config.example.toml ~/.config/mouse-debounce/config.toml
   ```
   Find your mouse in `/dev/input/by-id/` (look for the `event-mouse` one) and
   put that path in the `device` line.
2. Run it with sudo (it needs permission to read your mouse):
   ```bash
   sudo mouse-debounce
   ```
   The service version reads `/etc/mouse-debounce/config.toml` instead. When
   running as root, always point at the system config explicitly:
   `sudo -H mouse-debounce --config /etc/mouse-debounce/config.toml`.

## Tuning

One number matters: `ms` — how long to ignore bounces after a click.
Bigger = fewer double-clicks, but releasing the button feels slightly later.
Start at 40. Worn-out left click? Give it more (like 55), leave the rest low:

```toml
ms = 35
[buttons]
"272" = 55   # left click — the worn one
"273" = 20   # right click
"274" = 10   # middle click
```

(272 = left, 273 = right, 274 = middle, 275/276 = side buttons.)

Things to know:
- Clicking still feels instant. Only the *release* is delayed by `ms`, so
  super-fast double-clicks (faster than your `ms`) can merge into one — lower
  `ms` if that happens.
- Holding the button to drag is safe — shaky switches won't drop the drag.
- Only mouse buttons are filtered. If your receiver also handles a keyboard
  (combo dongle), typing passes through untouched — but use the mouse-only
  `event-mouse` device if you can.

## Usage

```bash
# Use config file
sudo mouse-debounce

# Just try it on one mouse without a config
sudo mouse-debounce --device /dev/input/by-id/usb-...-event-mouse --ms 50

# See what it's filtering
sudo mouse-debounce --verbose
```