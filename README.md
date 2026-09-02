# Lenovo Ideapad Fan and Keyboard Manager

A Linux-only Lenovo Ideapad management application written in Rust. The current GUI provides keyboard lighting control for supported Lenovo devices.

## Features

- Static, breath, wave, hue, and off lighting effects
- Four keyboard zones with live preview
- HEX, RGB, and HSV color input
- Brightness, speed, and wave direction controls
- Automatic live apply while editing
- Permanent settings with the `Apply` button
- Optional root execution at system startup
- No application-level `unsafe` code

## Requirements

- Linux
- Rust and Cargo
- libusb
- pkexec with a desktop PolicyKit authentication agent
- Supported Lenovo keyboard device: `048d:c963`

If USB access is denied, the application opens the desktop password dialog once and keeps a privileged helper active for later changes.

## Install

```sh
cargo install --path . --locked
~/.cargo/bin/lenovo-keyboard-light
```

If `~/.cargo/bin` is in your `PATH`, run it with:

```sh
lenovo-keyboard-light
```

## Run

```sh
cargo run --release
```

Use `Apply` to save and apply the current configuration. Use `Save` to save without applying. Changes made in the GUI are applied immediately but are not saved until `Apply` or `Save` is pressed.

Enable the startup checkbox to apply the saved configuration once when the desktop session starts. The user autostart entry is created at:

```text
~/.config/autostart/lenovo-keyboard-light.desktop
```

If needed, add a udev rule for non-root USB access:

```text
/etc/udev/rules.d/10-kblight.rules
SUBSYSTEM=="usb", ATTR{idVendor}=="048d", ATTR{idProduct}=="c963", MODE="0666"
```

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger
```

## Project

[efeozkan.com.tr](https://efeozkan.com.tr)
