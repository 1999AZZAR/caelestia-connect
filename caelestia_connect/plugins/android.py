"""Mirror and control the phone, preview its camera and use it as this computer's webcam (scrcpy over ADB)."""

import asyncio

from ..desktop import android, webcam
from ..i18n import _
from ..utils import spawn
from .base import Plugin, action

# A process that exits this fast failed to start (no device, debugging not allowed...)
STARTUP_CHECK = 2.5
# Android takes a moment to free a camera after the previous user closed it
CAMERA_RELEASE = 1.0
# Only one phone can feed the webcam device at a time
_webcam_owner: list[str] = []


class AndroidPlugin(Plugin):
    id = "android"
    name = _("Screen mirroring and camera")
    description = _("Mirror the screen and use the camera as a webcam, over USB or wireless debugging")
    icon = "screen_share"
    incoming = ()
    # Not tied to any packet: offered to every Android phone or tablet
    outgoing = ("kdeconnect.ping",)
    device_types = ("phone", "tablet")
    settings_schema = [
        {"key": "webcam", "type": "bool", "label": _("Use the camera as a webcam"), "default": True},
        {"key": "audio", "type": "bool", "label": _("Forward audio while mirroring"), "default": True},
        {"key": "turn_screen_off", "type": "bool", "label": _("Turn phone screen off while mirroring"), "default": False},
        {
            "key": "max_size",
            "type": "choice",
            "label": _("Mirror resolution"),
            "default": 0,
            "choices": [
                [0, _("Native resolution")],
                [1080, "1080p (max 1080)"],
                [1440, "1440p (max 1440)"],
                [1920, "1080p landscape (max 1920)"],
                [2560, "2K (max 2560)"],
            ],
        },
        {
            "key": "video_bit_rate",
            "type": "choice",
            "label": _("Video bit rate"),
            "default": "8M",
            "choices": [
                ["4M", "4 Mbps (Low latency)"],
                ["8M", "8 Mbps (Default)"],
                ["12M", "12 Mbps (High quality)"],
                ["16M", "16 Mbps (Very high quality)"],
                ["24M", "24 Mbps (Ultra quality)"],
            ],
        },
        {
            "key": "max_fps",
            "type": "choice",
            "label": _("Max frame rate"),
            "default": 0,
            "choices": [
                [0, _("Device default")],
                [30, "30 fps"],
                [55, "55 fps (Custom)"],
                [60, "60 fps"],
                [90, "90 fps"],
                [120, "120 fps"],
            ],
        },
        {
            "key": "new_display",
            "type": "str",
            "label": _("Virtual display (e.g. 1920x1080)"),
            "default": "",
        },
        {
            "key": "custom_args",
            "type": "str",
            "label": _("Custom scrcpy arguments"),
            "default": "",
        },
        {
            "key": "facing",
            "type": "choice",
            "label": _("Camera"),
            "default": "front",
            "choices": [["front", _("Front camera")], ["back", _("Rear camera")]],
        },
        {
            "key": "rotation",
            "type": "choice",
            "label": _("Extra rotation"),
            "default": 0,
            "choices": [[0, _("None")], [90, "90°"], [180, "180°"], [270, "270°"]],
        },
    ]

    def __init__(self, device):
        super().__init__(device)
        self.webcam = webcam.PhoneWebcam(self._serial, self._camera_options, self.changed)
        self.mirroring: asyncio.subprocess.Process | None = None
        self.preview: asyncio.Task | None = None
        self._screen_on: bool = True

    async def connected(self):
        self._update_webcam()

    def destroy(self):
        self.webcam.stop()
        if self.device.id in _webcam_owner:
            _webcam_owner.remove(self.device.id)
        if self.preview:
            self.preview.cancel()
        if self.mirroring and self.mirroring.returncode is None:
            self.mirroring.terminate()

    def settings_changed(self, key):
        if key == "webcam":
            self._update_webcam()

    def _update_webcam(self) -> None:
        if self.setting("webcam") and (not _webcam_owner or _webcam_owner[0] == self.device.id):
            self.webcam.device = webcam.find_loopback(webcam.OUTPUT_LABEL)  # may have been set up meanwhile
            if self.webcam.available and self.device.id not in _webcam_owner:
                _webcam_owner.append(self.device.id)
            self.webcam.start()
        else:
            self.webcam.stop()
            if self.device.id in _webcam_owner:
                _webcam_owner.remove(self.device.id)

    async def _serial(self) -> str | None:
        return (await android.find(self.device.name, self.device.address)).serial

    def _camera_options(self) -> tuple[str, int]:
        facing = self.setting("facing")
        rotation = self.setting("rotation")
        return ("back" if facing == "back" else "front", rotation if rotation in (0, 90, 180, 270) else 0)

    def state(self):
        tools = android.tools()
        if self.webcam.available:
            setup = ""
        elif not webcam.SUPPORTED:
            setup = "unsupported"
        elif webcam.missing_tools():
            setup = "tools"
        else:
            setup = "device"
        return {
            "tools": tools["adb"] and tools["scrcpy"],
            "mirroring": bool(self.mirroring and self.mirroring.returncode is None),
            "screenOn": self._screen_on,
            "preview": bool(self.preview and not self.preview.done()),
            "webcam": {
                "enabled": bool(self.setting("webcam")),
                "setup": setup,
                "owner": self.device.id in _webcam_owner,
                "streaming": self.webcam.streaming,
                "error": self.webcam.error,
            },
        }

    @action
    async def scan(self) -> dict:
        found = await android.find(self.device.name, self.device.address)
        return {"serial": found.serial or "", "devices": found.authorized, "unauthorized": found.unauthorized}

    async def _resolve(self, serial: str) -> str | None:
        return serial or await self._serial()

    @staticmethod
    async def _started(process: asyncio.subprocess.Process) -> bool:
        try:
            await asyncio.wait_for(asyncio.shield(process.wait()), STARTUP_CHECK)
        except TimeoutError:
            return True
        return process.returncode == 0

    def _audio_forwarding(self) -> bool:
        if hasattr(self.device, "plugin_enabled") and not self.device.plugin_enabled("phoneaudio"):
            return False
        return bool(self.setting("audio"))

    @action
    async def mirror(self, serial: str = "", audio: bool | None = None) -> dict:
        if self.mirroring and self.mirroring.returncode is None:
            return {}
        serial = await self._resolve(serial)
        if not serial:
            return {"error": _("The phone is not connected over USB or wireless debugging.")}
        cmd = ["scrcpy", "--serial", serial, f"--window-title={self.device.name}"]
        should_audio = self._audio_forwarding() if audio is None else audio
        if not should_audio:
            cmd.append("--no-audio")
        if self.setting("turn_screen_off"):
            cmd.append("--turn-screen-off")
            self._screen_on = False
        else:
            self._screen_on = True

        max_size = self.setting("max_size")
        if max_size:
            cmd.append(f"--max-size={max_size}")

        bit_rate = self.setting("video_bit_rate")
        if bit_rate and bit_rate != "8M":
            cmd.append(f"-b={bit_rate}")

        max_fps = self.setting("max_fps")
        if max_fps:
            cmd.append(f"--max-fps={max_fps}")

        new_display = self.setting("new_display")
        if new_display:
            cmd.append(f"--new-display={new_display.strip()}")

        custom_args = self.setting("custom_args")
        if custom_args:
            import shlex
            cmd.extend(shlex.split(custom_args))

        self.mirroring = await android.start_tool(*cmd)
        spawn(self._watch_mirroring(self.mirroring), name="android:mirror")
        self.changed()
        if not await self._started(self.mirroring):
            return {"error": _("Screen mirroring failed. Check the Android debugging connection and try again.")}
        return {}

    async def _watch_mirroring(self, process: asyncio.subprocess.Process) -> None:
        await process.wait()
        if self.mirroring is process:
            self.mirroring = None
            if not self._screen_on:
                serial = await self._serial()
                if serial:
                    await android.set_display_power(serial, True)
                self._screen_on = True
        self.changed()

    @action
    async def stop_mirror(self) -> None:
        if self.mirroring and self.mirroring.returncode is None:
            self.mirroring.terminate()
            try:
                await asyncio.wait_for(self.mirroring.wait(), 3)
            except (TimeoutError, ProcessLookupError):
                pass
            self.mirroring = None
            if not self._screen_on:
                serial = await self._serial()
                if serial:
                    await android.set_display_power(serial, True)
                self._screen_on = True
            self.changed()

    @action
    async def toggle_screen(self) -> dict:
        serial = await self._resolve("")
        if not serial:
            return {"error": _("The phone is not connected over USB or wireless debugging.")}
        new_state = not self._screen_on
        ok = await android.set_display_power(serial, new_state)
        if ok:
            self._screen_on = new_state
            self.changed()
        return {"screenOn": self._screen_on}

    @action
    async def set_screen(self, on: bool) -> dict:
        serial = await self._resolve("")
        if not serial:
            return {"error": _("The phone is not connected over USB or wireless debugging.")}
        ok = await android.set_display_power(serial, on)
        if ok:
            self._screen_on = on
            self.changed()
        return {"screenOn": self._screen_on}

    @action
    async def camera(self, serial: str = "", facing: str = "") -> dict:
        if facing in ("front", "back"):
            self.set_setting("facing", facing)
        serial = await self._resolve(serial)
        if not serial:
            return {"error": _("The phone is not connected over USB or wireless debugging.")}
        if self.preview and not self.preview.done():
            self.preview.cancel()
            await asyncio.gather(self.preview, return_exceptions=True)
        started = asyncio.get_running_loop().create_future()
        self.preview = spawn(self._preview(serial, started), name="android:camera")
        self.changed()
        if not await started:
            return {"error": _("Phone camera failed. Android 12 or newer is needed, and the camera must be free.")}
        return {}

    @action
    async def stop_camera(self) -> None:
        if self.preview:
            self.preview.cancel()
            await asyncio.gather(self.preview, return_exceptions=True)

    async def _preview(self, serial: str, started: asyncio.Future) -> None:
        """A camera window that turns with the phone. The webcam lets go of the camera meanwhile."""
        self.webcam.preview = True
        process = None
        try:
            if self.webcam.camera_open:
                for _attempt in range(50):
                    await asyncio.sleep(0.1)
                    if not self.webcam.camera_open:
                        break
                await asyncio.sleep(CAMERA_RELEASE)
            angle = None
            retried = False
            while True:
                facing, extra = self._camera_options()
                wanted = (await android.camera_rotation(serial, facing) + extra) % 360
                if process is not None and process.returncode is not None:
                    return  # the window was closed
                if (wanted, facing) != angle:
                    if process is not None:
                        await android.stop_tool(process)
                    angle = (wanted, facing)
                    process = await android.start_tool(
                        "scrcpy", "--serial", serial, "--video-source=camera", f"--camera-facing={facing}",
                        "--no-audio", "--max-size=1280", f"--display-orientation={wanted}",
                        f"--window-title={self.device.name}",
                    )  # fmt: skip
                    if not started.done():
                        ok = await self._started(process)
                        if not ok and not retried:
                            # The camera may still have been closing: try once more
                            retried, angle, process = True, None, None
                            await asyncio.sleep(CAMERA_RELEASE)
                            continue
                        started.set_result(ok)
                        if not ok:
                            return
                await asyncio.sleep(1)
        finally:
            if not started.done():
                started.set_result(False)
            if process is not None:
                await android.stop_tool(process)
            self.webcam.preview = False
            self.changed()
