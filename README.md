# Caelestia Connect

**A bloat-free, standalone KDE Connect client for modern Linux desktops (KDE Plasma, GNOME, Hyprland, Sway, Waybar, and Caelestia Shell).**

Built in Python, GTK4, and Libadwaita. Zero Qt or KDE library runtime dependencies. High-performance low-latency screen mirroring and virtual webcam powered by [scrcpy](https://github.com/Genymobile/scrcpy).

![Overview](docs/screenshots/overview.png)

| Messages | Settings |
|---|---|
| ![Messages](docs/screenshots/messages.png) | ![Settings](docs/screenshots/settings.png) |
| **Remote input** | |
| ![Remote input](docs/screenshots/remote.png) | |

---

## Why This Fork?

While originally conceived for the Caelestia quickshell, this fork transforms Caelestia Connect into a **universal, lightweight, standalone KDE Connect desktop client** that fits seamlessly into any Linux environment:

- ⚡ **Zero Bloat & Ultra-Lightweight (~60 MB RAM):** Official `kdeconnectd` pulls in hundreds of megabytes of heavy Qt6 and KDE Frameworks runtime libraries. Caelestia Connect is pure Python + GTK4 / Libadwaita, idling at a feather-light ~59 MB memory footprint.
- 🛡️ **Resolved 21 GB Memory Leak & Connection Storms:** Fixed an upstream architectural flaw where unclosed `Gio.TlsClientConnection` streams and self-interface loopbacks caused the background daemon to balloon to 21 GB. Added proper TLS lifecycle teardown, self-address link loop prevention, and exponential reconnect backoff.
- 🖥️ **Universal Desktop Integration:**
  - **KDE Plasma:** Native Dolphin file manager right-click sharing via KIO Service Menu (`caelestia-share`) with automatic device detection and `kdialog` multi-device selection picker. Full StatusNotifierItem (SNI) system tray.
  - **GNOME:** Native Libadwaita dark/light styling, Nautilus file manager Python extension, and AppIndicator/SNI tray.
  - **Wayland Compositors (Hyprland, Sway, River, Niri):** Works seamlessly with Waybar, clipboard synchronization (`wl-clipboard`), and keyboard/mouse pointer edge sharing (`uinput`).
  - **Caelestia Shell:** Preserved and refined Quickshell QML status bar integration.
- 📱 **Pro-Grade Screen Mirroring (`scrcpy`):**
  - **Dynamic Phone Display Power Toggle:** Turn the physical phone screen on or off while mirroring without freezing video or disabling touchscreen/mouse input (`SurfaceControl` reflection + `--stay-awake`).
  - **Granular Settings:** Customizable max resolution (unrestricted, 1440p, 1080p, 720p), framerate (up to 120 FPS), video bitrates (4–32 Mbps), virtual display mode, and audio forwarding toggles with automatic Bluetooth media audio coordination.
  - **Custom User Flags:** Seamlessly inject custom scrcpy arguments alongside defaults.
  - **In-App Shortcuts:** Clear UI hints for hotkeys (`Alt+O` to toggle phone screen, `Alt+Shift+O` to wake screen, `Ctrl+F` for fullscreen).
- 🌐 **Multi-Network & Tailscale Continuity:** Smart link deduplication prevents connection storms and duplicate handshakes when devices are reachable across both local Wi-Fi and Tailscale / VPN interfaces.
- 🔤 **Bundled Standalone Typography:** Automatically bundles and installs `Google Sans Flex` and `Material Symbols Rounded`, guaranteeing sharp fonts and icons on any distribution out-of-the-box.

---

## Features

### 📡 Connectivity & Discovery
- Automatic discovery over UDP broadcast and mDNS.
- Encrypted TLS connections over Wi-Fi, Ethernet, and Bluetooth RFCOMM.
- Secure pairing with verification keys.
- Multi-address deduplication (simultaneous LAN and Tailscale links handled cleanly).

### 🔔 Notifications & Calls
- Bi-directional notification forwarding with quick actions, inline replies, and dismiss sync.
- Forward desktop notifications to your phone.
- Incoming call alerts, call muting, and automatic desktop media pausing during calls.
- Full SMS and MMS conversation history with contact names and direct replies.

### 📁 File Sharing & Clipboard
- Bi-directional file, folder, link, and text transfer.
- **KDE Dolphin Integration:** Right-click any file → *"Share via Celestia…"*.
- **GNOME Nautilus Integration:** Right-click context menu via python extension.
- Bidirectional clipboard synchronization (`wl-clipboard`).

### 🎵 Media & Audio
- Bi-directional MPRIS control: control your desktop media players from the phone, or control phone media players from your desktop media widgets.
- Ring phone / Find my phone (and let the phone ring your PC).
- PipeWire Bluetooth audio routing: switch phone sound output between phone speakers and PC audio seamlessly.

### 🖱️ Remote Control & Input
- Use your phone as a high-precision touchpad and keyboard.
- **Edge Sharing (Mouse & Keyboard):** Move your desktop mouse pointer past any designated screen edge to control your phone directly (`uinput`).
- Remote command execution (trigger pre-configured scripts/commands on the PC from your phone).
- Presentation remote, volume controls, and drawing tablet mode.

### 📺 Screen Mirroring & Virtual Camera
- Ultra-low latency phone display mirroring with mouse and keyboard input.
- Real-time phone screen off/on control (`Alt+O`) while maintaining active desktop control.
- High-quality phone camera preview that rotates automatically with device orientation.
- **Virtual Webcam (`v4l2loopback`):** Turn your phone camera into a crisp Linux webcam for Google Meet, Zoom, OBS, and browsers under the device name `Caelestia Phone Camera`.

---

## Installation

### 1. Install Dependencies

#### Debian / Ubuntu / Linux Mint
```sh
sudo apt update
sudo apt install -y \
  python3-gi gir1.2-gtk-4.0 gir1.2-adw-1 python3-evdev python3-cryptography \
  wl-clipboard pulseaudio-utils sshfs scrcpy adb kdialog
```

#### Arch Linux / EndeavourOS / Manjaro
```sh
sudo pacman -S --needed \
  python-gobject libadwaita python-cryptography python-evdev \
  wl-clipboard libpulse sshfs gtk4-layer-shell \
  scrcpy android-tools android-udev kdialog
```

*(Optional for virtual display extending between computers: `wayvnc` / `freerdp`)*

### 2. Clone & Run the Installer

```sh
git clone https://github.com/1999AZZAR/caelestia-connect.git
cd caelestia-connect
./install.sh
```

The installer will:
1. Validate required and optional system dependencies.
2. Install launcher scripts into `~/.local/bin` (`caelestia-connect`, `caelestia-connectd`, `caelestia-connect-cli`, `caelestia-share`).
3. Install bundled fonts (`Google Sans Flex` and `Material Symbols Rounded`) into `~/.local/share/fonts/caelestia`.
4. Install the desktop entry, application icon, Dolphin KIO service menu, and Nautilus extension.
5. Enable and start the systemd user service `caelestia-connectd.service`.
6. (Optional) Integrate with Caelestia Quickshell if present (pass `--no-shell` to skip).

### 3. Open Firewall Ports
KDE Connect protocol requires ports `1714:1764` for UDP and TCP:
```sh
sudo ufw allow 1714:1764/udp && sudo ufw allow 1714:1764/tcp
```
*(Or the equivalent for `firewalld`: `sudo firewall-cmd --permanent --zone=home --add-service=kdeconnect && sudo firewall-cmd --reload`)*

### 4. Input Sharing Setup (`uinput`)
To allow remote controlling this PC from your phone, ensure `/dev/uinput` is accessible:
```sh
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", TAG+="uaccess"' | sudo tee /etc/udev/rules.d/60-uinput.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
```

---

## Screen Mirroring & Webcam Setup

### Pairing scrcpy with Android
Screen mirroring and the camera preview operate via Android debugging (ADB):
- **USB:** Turn on *USB debugging* in Developer Options on your phone, connect via USB, and accept the debugging prompt.
- **Wireless ADB:** Turn on *Wireless debugging* on your phone and pair once via `adb pair <ip>:<port>`. Caelestia Connect will automatically match the device by IP or hostname.

### Mirroring Controls & Shortcuts
Inside the mirroring window:
- `Alt + O` — Toggle phone physical screen off / on (display stays dark, mouse/touch controls remain fully active).
- `Alt + Shift + O` — Turn phone physical screen back on.
- `Ctrl + F` — Toggle fullscreen.
- `Ctrl + G` — Resize window to 1:1 pixel scale.
- `Ctrl + H` — Emulate Home button.
- `Ctrl + B` — Emulate Back button.
- `Ctrl + S` — Emulate App Switcher.

All mirroring settings (Resolution limit, Max FPS, Bitrate, Audio forwarding, Virtual display mode, and custom flags) can be configured directly inside the GTK4 application under **Device Settings → Screen Mirroring (scrcpy)**.

### Virtual Webcam (`v4l2loopback`)
To use your phone's camera as a system webcam:
```sh
# On Arch Linux:
sudo pacman -S --needed v4l2loopback-dkms linux-headers ffmpeg v4l-utils
./install.sh --webcam

# On Debian / Ubuntu:
sudo apt install -y v4l2loopback-dkms linux-headers-generic ffmpeg v4l-utils
./install.sh --webcam
```
This automatically configures the loopback device `Caelestia Phone Camera`. The camera activates automatically when an application (browser, Zoom, OBS) requests the video stream and turns off when closed.

---

## File Manager Sharing

### KDE Dolphin
Right-click any file or collection of files in Dolphin:
- Select **Share via Celestia…**
- If only one paired device is online, the transfer begins immediately with a desktop notification.
- If multiple devices are available, a clean `kdialog` dialog lets you choose the target device.

### GNOME Files (Nautilus)
Right-click files in Nautilus and select **Send via Caelestia Connect**.

---

## Command Line Interface (CLI)

`caelestia-connect-cli` provides instant terminal control for scripting, keybindings, and automation:

```sh
# List paired devices and connection statuses
caelestia-connect-cli list

# Send a ping notification to all reachable devices
caelestia-connect-cli ping "Hello from terminal"

# Send files or links
caelestia-connect-cli share photo.jpg document.pdf https://github.com

# Push current desktop clipboard to phone
caelestia-connect-cli send-clipboard

# Send an SMS
caelestia-connect-cli sms +15551234567 "Hello from Caelestia Connect"

# Media playback controls
caelestia-connect-cli media PlayPause
caelestia-connect-cli media Next

# Ring phone to locate it
caelestia-connect-cli ring

# Low-level plugin invocation
caelestia-connect-cli call <plugin> <action> '<json args>'

# Watch live IPC event stream
caelestia-connect-cli watch
```

---

## Project Structure

```
caelestia_connect/
  daemon.py, api.py       # Core background service & JSON-RPC over UNIX socket
  lan.py                  # Discovery, TLS links, reconnect backoff, & payload transfers
  mdns.py                 # ZeroConf / mDNS responder and browser
  bluetooth.py            # BlueZ RFCOMM multiplexed transport
  device.py               # Pairing logic and plugin packet router
  desktop/                # Desktop integrations (tray, notifications, audio, MPRIS, uinput, webcam)
    tray.py               # Standalone StatusNotifierItem (SNI) D-Bus implementation
    android.py            # scrcpy process management, camera, & display control
  plugins/                # Modular plugin architecture (share, mpris, sftp, sms, etc.)
  ui/                     # GTK4 + Libadwaita responsive desktop interface
    device_settings.py    # Per-device settings with scrcpy configuration suite
    mirroring.py          # scrcpy launcher with display power management
  cli.py                  # caelestia-connect-cli implementation
data/
  kio/                    # Dolphin KIO Service Menu and helper (caelestia-share)
  nautilus/               # GNOME Nautilus Python extension
  caelestia-connectd.service # Systemd user service unit
tests/                    # Pytest test suite with fake Android peer & reconnect simulations
```

---

## Testing & Quality Assurance

All core functionality is covered by automated unit and integration tests:

```sh
# Run full test suite with pytest
python3 -m pytest tests/

# Test specific subsystems
python3 -m pytest tests/test_lan_reconnect.py   # TLS lifecycle, loopback refusal, backoff
python3 -m pytest tests/test_android.py         # scrcpy argument formatting & controls
python3 -m pytest tests/test_bluetooth.py       # BlueZ packet multiplexing

# Linting and formatting
ruff check .
ruff format --check .

# Run demo mode (emulates phone peer without real device)
python3 tests/demo.py
```

---

## Uninstall

To cleanly remove launchers, services, and desktop integrations:
```sh
./install.sh --uninstall
```
*(Your pairing keys and configurations in `~/.config/caelestia-connect` are preserved).*

---

## Credits & Acknowledgements

- **Upstream:** [maylton/caelestia-connect](https://github.com/maylton/caelestia-connect) for the initial GTK4 client and daemon architecture.
- **Screen Mirroring & Webcam:** [ProSahil (@KingSahil)](https://github.com/KingSahil) for initial scrcpy mirroring foundation.
- **Protocol & Ecosystem:** [KDE Connect](https://invent.kde.org/network/kdeconnect-kde) team for the protocol specification.
- **Streaming Engine:** [Romain Vimont (@rom1v)](https://github.com/rom1v) and contributors for [scrcpy](https://github.com/Genymobile/scrcpy).
- **Fork Enhancements:** [1999AZZAR](https://github.com/1999AZZAR) — memory leak elimination, Dolphin KIO share integration, multi-interface deduplication, stay-awake screen power controls, granular scrcpy suite, and standalone desktop refinements.

---

## License

GPL-3.0-or-later
