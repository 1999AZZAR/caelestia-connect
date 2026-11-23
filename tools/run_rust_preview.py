"""Launch the GTK client with an isolated Rust daemon and pairing profile."""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEVICE_NAME = "Caelestia Rust Preview"
BUS_NAME = "org.caelestia.Connect.Daemon.RustPreview"


def stop(process: subprocess.Popen | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def wait_for_socket(daemon: subprocess.Popen, socket: Path) -> None:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if daemon.poll() is not None:
            raise RuntimeError(f"Rust daemon exited with status {daemon.returncode}")
        if socket.is_socket():
            return
        time.sleep(0.05)
    raise RuntimeError("Rust daemon did not create its preview socket")


def launch(binary: Path, bind: str) -> int:
    env = os.environ.copy()
    runtime = Path(env.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}")
    if not runtime.is_absolute() or not runtime.is_dir() or runtime.stat().st_uid != os.getuid():
        raise RuntimeError("A user desktop session with a private runtime directory is required")
    if runtime.stat().st_mode & 0o077:
        raise RuntimeError(f"Runtime directory must be private: {runtime}")
    env["XDG_RUNTIME_DIR"] = str(runtime)
    if not env.get("DBUS_SESSION_BUS_ADDRESS") and (runtime / "bus").is_socket():
        env["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={runtime / 'bus'}"
    config = Path(env.get("XDG_CONFIG_HOME") or Path.home() / ".config")
    profile = config / "caelestia-connect-rust-preview"
    with tempfile.TemporaryDirectory(dir=runtime, prefix="cc-rust-") as directory:
        ipc_socket = Path(directory) / "caelestia-connect" / "ipc.sock"
        daemon_env = {
            **env,
            "CAELESTIA_CONNECT_IPC_RUNTIME_DIR": directory,
            "CAELESTIA_CONNECT_BUS_NAME": BUS_NAME,
            "CAELESTIA_CONNECT_PREVIEW_DATA": str(profile),
            "CAELESTIA_CONNECT_PREVIEW_SOCKET": str(ipc_socket),
        }
        daemon = subprocess.Popen(
            [
                str(binary),
                "--bind",
                bind,
                "--discovery-port",
                "0",
                "--no-tray",
                "--default-name",
                DEVICE_NAME,
                "--config-dir",
                str(profile),
            ],
            cwd=ROOT,
            env=daemon_env,
        )
        ui = None
        try:
            wait_for_socket(daemon, ipc_socket)
            print("Rust preview ready. Pair using the computer name shown in the app.", flush=True)
            ui = subprocess.Popen(
                [sys.executable, "-m", "caelestia_connect.ui"],
                cwd=ROOT,
                env={**env, "CAELESTIA_CONNECT_PREVIEW_SOCKET": str(ipc_socket)},
            )
            while ui.poll() is None and daemon.poll() is None:
                time.sleep(0.15)
            if ui.returncode is not None:
                return ui.returncode
            raise RuntimeError(f"Rust daemon exited with status {daemon.returncode}")
        finally:
            stop(ui)
            stop(daemon)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "rust-daemon/target/debug/caelestia-connectd-rust")
    parser.add_argument("--bind", default="0.0.0.0", help="IPv4 address for the Rust daemon listeners")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error(f"Rust preview binary missing or not executable: {binary}. Build the Rust daemon first.")

    def terminate(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    try:
        return launch(binary, args.bind)
    except KeyboardInterrupt:
        return 130
    except (OSError, RuntimeError) as error:
        print(error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
