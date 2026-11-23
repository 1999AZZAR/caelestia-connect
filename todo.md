# Caelestia Connect Rust migration

Baseline `acae300` on `origin/main`. Rust runs the background service. The existing Python GTK app and CLI use the shared local IPC interface. The regular profile keeps its identity, pairings, and preferences when switching backends. [Build, install, and switch backends](rust-daemon/README.md)

## Status at a glance

| Python plugin registry | Count |
| --- | ---: |
| Main workflows available in Rust | 17 |
| Partial Rust implementation | 3 |
| Python-only | 11 |
| Total with some Rust support | 20 of 31 (65%) |

The counts follow the Python plugin registry. Check a plugin when its tracked workflow works in Rust and its verification is recorded. [Python plugin registry](caelestia_connect/plugins/__init__.py)

**Next small candidates** are the low-battery alert and ringing this computer from a paired phone.

## Shared groundwork

- [x] Nix-built Rust user service with the existing GTK app, CLI, and desktop tray.
- [x] IPv4 LAN discovery over UDP and mDNS, TLS pairing and verification, and reuse of saved Python profiles.
- [x] Authenticated address handoff and automatic reconnect to the last known endpoint with backoff.
- [ ] Port Bluetooth data transport over RFCOMM.

## Ported plugins

- [x] `connectivity_report` · Mobile network type and signal strength.
- [x] `contacts` · Sync contacts and reuse the existing vCard cache.
- [x] `findmyphone` · Ring the paired phone from the desktop.
- [x] `mpriscontrol` · Let the phone control desktop media players.
- [x] `mprisremote` · Show and control phone playback, including album art and desktop MPRIS export.
- [x] `notifications` · Receive phone notifications with actions, replies, and dismissal.
- [x] `pausemusic` · Pause or mute media during calls and restore it according to the saved options.
- [x] `phoneaudio` · Switch Bluetooth audio playback between the phone and desktop.
- [x] `ping` · Ping paired devices and show incoming pings.
- [x] `remotecommands` · List and trigger commands published by the paired device.
- [x] `remotesystemvolume` · Show and control audio outputs on the paired device.
- [x] `runcommand` · Manage predefined desktop commands and report their output and exit status.
- [x] `screensaver_inhibit` · Keep the desktop awake while an enabled device is connected.
- [x] `share` · Transfer files and folders and share links and text.
- [x] `sms` · Read conversations, send SMS/MMS, and handle attachments with the existing history and cache.
- [x] `systemvolume` · Let the paired device control desktop outputs and the default sink.
- [x] `telephony` · Show call status, resolve caller names, display supplied photos in desktop notifications, and mute the phone ringtone.

## Partial ports

- [ ] `battery` · Port the low-battery desktop alert with duplicate suppression. Charge and charging state already appear in GTK and the tray. The last reading survives brief reconnects.
- [ ] `clipboard` · Port automatic text sync and file clipboard handling. Manual desktop-to-phone text sharing and received phone text already work.
- [ ] `android` · Port the system-wide virtual webcam through `v4l2loopback`. Screen mirroring, in-place screen power, and camera preview already work in Rust. Front-camera mirroring is optional and starts off for each preview.

## Python-only plugins

### Desktop controls and integration

- [ ] `findthisdevice` · Ring this computer from the paired device and stop the ring.
- [ ] `lockdevice` · Report lock state and support the existing session-control workflow.
- [ ] `sendnotifications` · Send desktop notifications to the phone.
- [ ] `sftp` · Browse phone storage through SFTP/SSHFS.

### Remote input and displays

- [ ] `mousepad` · Use the phone as a desktop touchpad and keyboard.
- [ ] `remotekeyboard` · Type on the phone with the desktop keyboard.
- [ ] `remotecontrol` · Control the paired device from the desktop.
- [ ] `presenter` · Control slides and the presentation pointer.
- [ ] `digitizer` · Handle drawing-tablet input and pressure.
- [ ] `shareinputdevices` · Share mouse and keyboard input across screen edges.
- [ ] `virtualmonitor` · Extend the desktop onto a paired device.

## Verification

- [x] `acae300` passed 35 Python tests, Clippy, Ruff, and the Rust integration suite with and without GTK.
- [x] Verified the installed Rust daemon with the real paired Android device. The tray battery reading matched the phone, and all three pairings were preserved.
- [x] Verified reconnection, scrcpy launch options, screen power, camera rotation, and the mirror switch with simulated peers and command-line tools.
- [ ] Verify physical screen mirroring and camera video over USB and wireless debugging. Check both lenses, rotation, and screen power while a virtual display is open.
- [ ] Run a multi-hour comparison of Python and Rust with a paired phone. Measure memory and CPU through idle periods, reconnects, and active streams to assess the original out-of-memory issue.

The earlier 65-second idle comparison used one simulated paired phone. Python used 42.7 MiB PSS and Rust used 9.1 MiB. Median local state requests took 3.34 ms and 0.55 ms respectively. CPU time was 60 ms and 100 ms respectively. This baseline predates caller photos and covers idle daemon behavior. [Rust integration suite](tests/run_rust_milestone.py)
