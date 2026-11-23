"""Screen mirroring, camera preview and webcam against fake adb/scrcpy/ffmpeg programs."""

import asyncio
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

from android_tools import setup_android_tools

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

TMP = Path(tempfile.mkdtemp(prefix="cc-android-"))
BIN = TMP / "bin"
LOG = TMP / "calls"
os.environ["PATH"] = f"{BIN}:{os.environ['PATH']}"
os.environ["XDG_RUNTIME_DIR"] = str(TMP)


def adb_state(**state) -> None:
    (TMP / "adb.json").write_text(json.dumps({"devices": {}, **state}))
    LOG.write_text("")


def calls(tool: str) -> list[str]:
    return [line for line in LOG.read_text().splitlines() if line.startswith(tool + " ")]


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return Path(f"/proc/{pid}/stat").read_text().split()[2] != "Z"


setup_android_tools(BIN)
from caelestia_connect.desktop import android, webcam  # noqa: E402
from caelestia_connect.plugins import android as android_plugin  # noqa: E402

# Never touch this machine's real v4l2loopback devices
webcam.find_loopback = lambda _label: None


class FakeConfig:
    def __init__(self) -> None:
        self.settings: dict = {}

    def plugin_settings(self, _pid: str) -> dict:
        return self.settings

    def save(self) -> None:
        pass


class FakeDevice:
    def __init__(self, name="Pixel 9", address="192.168.0.5") -> None:
        self.id = "a" * 32
        self.name = name
        self.address = address
        self.config = FakeConfig()
        self.daemon = type("D", (), {"state_changed": lambda self: None})()


def plugin(**kw) -> android_plugin.AndroidPlugin:
    return android_plugin.AndroidPlugin(FakeDevice(**kw))


def test_phone_found_by_address() -> None:
    adb_state(devices={"R5CX123": "device", "192.168.0.5:41234": "device"})
    found = asyncio.run(android.find("Pixel 9", "192.168.0.5"))
    assert found.serial == "192.168.0.5:41234", found


def test_phone_found_by_name() -> None:
    adb_state(devices={"R5CX123": "device", "ZY22": "device"}, names={"R5CX123": "Other", "ZY22": "Pixel 9"})
    assert asyncio.run(android.find("pixel 9", "")).serial == "ZY22"


def test_wireless_debugging_is_connected() -> None:
    adb_state(mdns=["adb-ZY22-abc\t_adb-tls-connect._tcp\t192.168.0.5:37001"])
    found = asyncio.run(android.find("Pixel 9", "192.168.0.5"))
    assert found.serial == "192.168.0.5:37001", found
    assert "adb connect 192.168.0.5:37001" in calls("adb")


def test_other_phone_is_not_picked() -> None:
    adb_state(devices={"R5CX123": "device"}, names={"R5CX123": "Someone else"})
    found = asyncio.run(android.find("Pixel 9", "192.168.0.5"))
    assert found.serial is None and found.authorized == ["R5CX123"]
    adb_state(devices={"R5CX123": "unauthorized"})
    assert asyncio.run(android.find("Pixel 9", "")).unauthorized


def test_mirroring() -> None:
    async def run():
        adb_state(devices={"192.168.0.5:41234": "device"})
        p = plugin()
        assert await p.mirror() == {}
        assert p.state()["mirroring"]
        (line,) = calls("scrcpy")
        assert "--serial 192.168.0.5:41234" in line and "--window-title=Pixel 9" in line
        pid = int(line.rsplit("pid=", 1)[1])
        p.destroy()
        await asyncio.sleep(0.3)
        assert not alive(pid)

        (TMP / "scrcpy.fail").touch()
        try:
            assert "error" in await plugin().mirror()
        finally:
            (TMP / "scrcpy.fail").unlink()

        # Mirroring with audio disabled
        p2 = plugin()
        p2.device.config.settings["audio"] = False
        assert await p2.mirror() == {}
        assert "--no-audio" in calls("scrcpy")[-1]
        p2.destroy()

        # Mirroring with custom resolution, bitrate, fps and virtual display
        p3 = plugin()
        p3.device.config.settings.update(
            {
                "max_size": 1080,
                "video_bit_rate": "16M",
                "max_fps": 60,
                "new_display": "1920x1080",
                "custom_args": "--hover",
            }
        )
        assert await p3.mirror() == {}
        last_call = calls("scrcpy")[-1]
        assert "--max-size=1080" in last_call
        assert "--video-bit-rate=16M" in last_call
        assert "--max-fps=60" in last_call
        assert "--new-display=1920x1080" in last_call
        assert "--hover" in last_call
        p3.destroy()

    asyncio.run(run())


@pytest.mark.parametrize(("mirror", "portrait", "landscape"), [(False, "270", "180"), (True, "flip90", "flip180")])
def test_camera_preview_turns_with_the_phone(mirror, portrait, landscape) -> None:
    async def run():
        adb_state(devices={"ZY22": "device"}, names={"ZY22": "Pixel 9"}, rotation=0)
        p = plugin()
        assert await p.camera(facing="front", mirror=mirror) == {}
        assert p.setting("facing") == "front" and p.state()["preview"]
        first = calls("scrcpy")[-1]
        assert "--video-source=camera" in first and f"--display-orientation={portrait}" in first, first
        state = json.loads((TMP / "adb.json").read_text())
        state["rotation"] = 1  # phone turned to landscape
        (TMP / "adb.json").write_text(json.dumps(state))
        await asyncio.sleep(2.5)
        second = calls("scrcpy")[-1]
        assert f"--display-orientation={landscape}" in second, second
        assert not alive(int(first.rsplit("pid=", 1)[1]))
        await p.stop_camera()
        assert not p.state()["preview"]
        assert not alive(int(second.rsplit("pid=", 1)[1]))

    asyncio.run(run())


def test_both_cameras_are_upright() -> None:
    """The front sensor is mounted opposite to the back one: their pictures need turning the other way."""

    async def run():
        adb_state(devices={"ZY22": "device"}, rotation=0)
        assert await android.camera_rotation("ZY22", "back") == 90
        assert await android.camera_rotation("ZY22", "front") == 270
        for facing, capture in (("back", 180), ("front", 0)):
            LOG.write_text("")
            cam = webcam.PhoneWebcam(lambda: _const("ZY22"), lambda f=facing: (f, 0), lambda: None)
            cam.device = str(TMP / "video9")
            task = asyncio.ensure_future(cam._camera())
            await asyncio.sleep(0.8)
            (line,) = calls("scrcpy")
            assert f"--capture-orientation={capture}" in line, (facing, line)
            cam.stop()
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(run())


def test_camera_preview_retries_while_the_camera_is_closing() -> None:
    async def run():
        adb_state(devices={"ZY22": "device"}, names={"ZY22": "Pixel 9"})
        (TMP / "scrcpy.failonce").touch()
        p = plugin()
        assert await p.camera(facing="back", mirror=True) == {}
        assert len(calls("scrcpy")) == 2 and p.state()["preview"]
        assert "--display-orientation=90" in calls("scrcpy")[-1]
        await p.stop_camera()

    asyncio.run(run())


def test_webcam_feeds_the_camera_and_cleans_up() -> None:
    async def run():
        adb_state(devices={"ZY22": "device"}, names={"ZY22": "Pixel 9"})
        cam = webcam.PhoneWebcam(lambda: _const("ZY22"), lambda: ("back", 90), lambda: None)
        cam.device = str(TMP / "video9")
        task = asyncio.ensure_future(cam._camera())
        await asyncio.sleep(1)
        (line,) = calls("scrcpy")
        for arg in ("--camera-facing=back", f"--v4l2-sink={cam.device}", "--angle=90", "--no-window"):
            assert arg in line, line
        cam.stop()  # synchronous, as when the daemon exits
        await asyncio.sleep(0.3)
        assert not alive(int(line.rsplit("pid=", 1)[1]))
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)

    asyncio.run(run())


async def _const(value):
    return value


def test_tools_die_with_the_daemon() -> None:
    """Even a daemon killed outright must not leave scrcpy holding the phone camera."""
    code = (
        f"import asyncio, sys; sys.path.insert(0, {str(ROOT)!r})\n"
        "from caelestia_connect.desktop import android\n"
        "async def main():\n"
        "    p = await android.start_tool('scrcpy', '--video-source=camera')\n"
        "    print(p.pid, flush=True); await asyncio.sleep(600)\n"
        "asyncio.run(main())\n"
    )
    daemon = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE, text=True)
    pid = int(daemon.stdout.readline())
    time.sleep(0.3)
    assert alive(pid)
    daemon.send_signal(signal.SIGKILL)
    daemon.wait()
    for _ in range(30):
        if not alive(pid):
            break
        time.sleep(0.1)
    assert not alive(pid)


def test_ioctl_numbers() -> None:
    assert webcam.SUBSCRIBE_EVENT == 0x4020565A
    assert webcam.DEQUEUE_EVENT == 0x80885659


def test_toggle_screen() -> None:
    async def run():
        adb_state(devices={"ZY22": "device"}, names={"ZY22": "Pixel 9"}, screen_on=True)
        p = plugin()
        assert await p.toggle_screen() == {"screenOn": False}
        assert not p.state()["screenOn"]
        assert await p.toggle_screen() == {"screenOn": True}
        assert p.state()["screenOn"]

    asyncio.run(run())


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_"):
            fn()
            print("PASS", name)
