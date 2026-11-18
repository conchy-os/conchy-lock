# conchy-lock

Screen locker for Conchy OS. Speaks `ext-session-lock-v1` (supported by `conchy-wm`, niri, Hyprland, sway),
authenticates through PAM on a worker thread, and renders with `tiny-skia` + `cosmic-text` into SHM buffers
(no GPU context, tiny memory footprint — a locker should be boring and robust).

GPUI (used by `conchy-shell`) has no session-lock support, which is why this is a separate binary.

## Try it (safe)

```bash
cargo build --release
./target/release/conchy-lock --demo            # regular window, password: demo
./target/release/conchy-lock --png out.png --state failed   # offline preview: idle|typing|checking|failed|caps
```

## Install

```bash
sudo install -Dm755 target/release/conchy-lock /usr/local/bin/conchy-lock
sudo install -Dm644 packaging/pam.d/conchy-lock /etc/pam.d/conchy-lock
install -Dm644 config/lock.kdl ~/.config/conchy/lock.kdl
```

Then bind `conchy-lock` to a key / call it from your idle daemon (`lock_cmd`).

## Config

`~/.config/conchy/theme.kdl` supplies the palette (base16), `~/.config/conchy/lock.kdl` the wallpaper, blur and dim.
`~/.face` is used as the avatar if present.
