# Experimental Rust daemon

The Rust daemon runs in the background and provides the desktop tray. The existing Python GTK client opens on demand and uses the same local socket with either daemon.

Rust creates a private desktop identity on first run, or reuses `certificate.pem` and `privateKey.pem` from Python. Both daemons read `trusted_devices.json`. Rust checks each paired phone against its saved certificate and atomically saves confirmed pairings and unpairing. Existing settings and device preferences are preserved. Run only one daemon against a profile at a time.

After a LAN link drops, Rust retries the last authenticated address and TCP port without waiting for a new broadcast. Retries back off to one minute when a device stays offline. Recent discovery addresses are also tried once the active link drops. A paired device can reconnect from a new address while an old connection is still open. Rust announces itself over UDP at startup, when a local IPv4 address appears, and on manual discovery or a device name change.

## Build and verify

From the repository root:

```sh
nix develop path:./rust-daemon -c cargo build --locked --manifest-path rust-daemon/Cargo.toml
python3 tests/run_rust_milestone.py
xvfb-run -a python3 tests/run_rust_milestone.py --gtk
```

The test needs Python with the project's dependencies, `dbus-run-session`, and `curl`. It uses a simulated phone, desktop media and audio outputs, and D-Bus services in an isolated session. The GTK test also exercises the regular client's overview, media card, Messages and Notifications pages. Use `--binary` to test a different build. If the regular daemon is already listening on UDP 1716, run the test in a network namespace with loopback enabled.

## Run the preview alongside the regular app

From your desktop session, after building the daemon:

```sh
python3 tools/run_rust_preview.py
```

Open KDE Connect on your phone and pair with **Caelestia Rust Preview**, or the name you saved in Preferences. Use **Search for devices** or **Add device by IP** in the preview if needed. The preview uses its own profile at `~/.config/caelestia-connect-rust-preview` by default. It keeps saved pairings, the device name, manual IPs, download locations, and feature switches across restarts. The installed app can run alongside it.

Received files go to Downloads by default. Use **Preferences → Received files** to choose the default folder, or **Settings → Features → Share and receive** for a per-device destination. Completed downloads can be opened from the transfer list. Failed or cancelled downloads leave existing files intact. Clipboard text requires `wl-copy` and `wl-paste` on Wayland, or `xclip` on X11. Use **Send clipboard** to send the current clipboard text.

When the phone reports a media session, its track, album art, and playback controls appear on the device overview. The same player is available in desktop media widgets that support MPRIS. To change either desktop export or idle-player visibility, open **Settings → Features → Multimedia controls** for that device.

The phone can also control this computer’s MPRIS players. Switch **Settings → Features → Multimedia control receiver** off for a device to stop sharing desktop playback with it. Desktop cover art can come from local files or HTTP(S) URLs. HTTP(S) cover art requires `curl`.

**System volume** lets a paired device list audio outputs, change their volume and mute state, or select the default output. It uses `pactl` and accepts volume up to 100%. **Remote system volume** exposes output status and volume and mute commands for peers that offer the reverse direction.

When the phone is paired in BlueZ and offers Bluetooth media audio, the media card can switch playback between the phone and this computer. Rust matches the saved Bluetooth address or a unique device name.

The Messages page reuses the existing SMS history, vCard contacts, and attachment cache with the regular profile. The isolated preview stores those under its own profile. MMS attachment limits are 4 MiB per outgoing message and 8 MiB per incoming attachment.

The Notifications page shows phone notifications and supports actions, replies, and dismissal. Device settings control desktop popups and copying verification codes when you choose the corresponding notification action. Incoming calls show on the overview and offer **Mute call** in the desktop popup. The Rust call handler currently uses caller names supplied by the phone and falls back to the phone number.

**Pause media during calls** can pause MPRIS players, mute system audio through `pactl`, and restore both when the call ends. **Prevent screensaver** is off by default and stays active only while an enabled device is connected. It uses Caelestia’s idle inhibitor when available, then tries the desktop ScreenSaver service or logind.

**Run commands** keeps the predefined commands under **Settings → Features**. Paired devices trigger these commands by ID and can receive output and exit status. The daemon runs up to four commands at a time and stops each after two minutes. **Remote commands** shows commands offered by the peer on the device overview.

Rust starts scrcpy for **Mirror screen** and **Phone camera** in the existing GTK app. Pair the phone in Caelestia Connect and authorize USB or wireless debugging. Screen power controls keep the mirroring session open, including when scrcpy creates a virtual display. The camera preview follows phone rotation and uses the saved front or rear camera setting. These controls also work over USB when the paired phone is offline in Caelestia Connect.

The Python daemon still provides Bluetooth data transport, automatic and file clipboard sync, sending desktop notifications to the phone, call photos and local caller lookup, SFTP browsing, remote input and presentation controls, device locking and desktop ringing, the system-wide virtual webcam, and virtual monitors.

## Install the Rust user service

```sh
./install.sh --rust --no-shell
```

The installer builds the Rust package with Nix and starts the existing Caelestia Connect service with your regular profile. The service uses the saved certificate, pairings, and preferences. To switch back, run `./install.sh --python --no-shell`. Later installs keep the selected backend unless you pass `--rust` or `--python`.

## Run the daemon with the standard profile

For a foreground test with the standard profile, stop the Python daemon first. Both daemons use the same D-Bus name, Unix socket, and local discovery port by default. Open the regular GTK app to approve pairing requests. Quit the Rust process before restarting the regular daemon.

```sh
./rust-daemon/target/debug/caelestia-connectd-rust
```

The daemon reads configuration from `$XDG_CONFIG_HOME/caelestia-connect` or `~/.config/caelestia-connect` by default. Use `--config-dir` for a different directory. Pairing writes require a private configuration directory, and the IPC socket requires a private `XDG_RUNTIME_DIR`. Use `--bind` for a specific IPv4 interface, or `--no-discovery` for direct connections.
