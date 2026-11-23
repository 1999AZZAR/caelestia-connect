"""Fake Android command-line tools shared by the backend tests."""

from pathlib import Path

# Fake tools: every call is logged; their behaviour comes from files in TMP
FAKE_ADB = r"""#!/usr/bin/env python3
import json, sys
from pathlib import Path
tmp = Path(sys.argv[0]).parent.parent
state = json.loads((tmp / "adb.json").read_text())
with open(tmp / "calls", "a") as f:
    f.write("adb " + " ".join(sys.argv[1:]) + "\n")
args = sys.argv[1:]
if args[:1] == ["devices"]:
    print("List of devices attached")
    for serial, status in state["devices"].items():
        print(f"{serial}\t{status}")
elif args[:2] == ["mdns", "services"]:
    for line in state.get("mdns", []):
        print(line)
elif args[:1] == ["connect"]:
    state["devices"][args[1]] = "device"
    (tmp / "adb.json").write_text(json.dumps(state))
elif args[:1] == ["-s"] and "device_name" in args:
    print(state.get("names", {}).get(args[1], ""))
elif args[:1] == ["-s"] and "window" in args:
    print(f"mRotation={state.get('rotation', 0)}")
elif args[:1] == ["-s"] and "media.camera" in args:
    print("== Camera HAL device back\n  android.lens.facing (x):\n    [BACK ]")
    print("  android.sensor.orientation (x):\n    [90 ]")
    print("== Camera HAL device front\n  android.lens.facing (x):\n    [FRONT ]")
    print("  android.sensor.orientation (x):\n    [270 ]")
elif args[:1] == ["-s"] and "display" in args:
    if "power-off" in args:
        state["screen_on"] = False
    elif "power-reset" in args:
        state["screen_on"] = True
    if "power-off" in args or "power-reset" in args:
        (tmp / "adb.json").write_text(json.dumps(state))
    print(f"mState={'ON' if state.get('screen_on', True) else 'OFF'}")
"""
FAKE_LONG = r"""#!/usr/bin/env python3
import os, sys, time
from pathlib import Path
tmp = Path(sys.argv[0]).parent.parent
with open(tmp / "calls", "a") as f:
    f.write(Path(sys.argv[0]).name + " " + " ".join(sys.argv[1:]) + f" pid={os.getpid()}\n")
if (tmp / (Path(sys.argv[0]).name + ".fail")).exists():
    sys.exit(1)
once = tmp / (Path(sys.argv[0]).name + ".failonce")
if once.exists():
    once.unlink()
    sys.exit(1)
time.sleep(600)
"""


def setup_android_tools(directory: Path) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    for name, body in (("adb", FAKE_ADB), ("scrcpy", FAKE_LONG), ("ffmpeg", FAKE_LONG), ("v4l2-ctl", "#!/bin/sh\n")):
        (directory / name).write_text(body)
        (directory / name).chmod(0o755)
