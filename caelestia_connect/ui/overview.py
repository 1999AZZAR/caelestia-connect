"""Overview tab of a paired device: status, quick actions, media, transfers and notifications."""

from __future__ import annotations

import time
from collections.abc import Callable
from dataclasses import dataclass

from gi.repository import Adw, Gio, Gtk, Pango

from ..i18n import _
from ..utils import human_size
from . import widgets as w
from . import mirroring


@dataclass(frozen=True)
class QuickAction:
    id: str
    icon: str
    label: str
    run: Callable[[], None]
    tooltip: str = ""
    opens_view: bool = False
    active: bool = False


class Overview(Gtk.ScrolledWindow):
    def __init__(self, app, page) -> None:
        super().__init__(hscrollbar_policy=Gtk.PolicyType.NEVER, vexpand=True)
        self.app = app
        self.page = page
        self.device: dict = {}
        clamp = Adw.Clamp(maximum_size=1080, tightening_threshold=900)
        self.box = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL,
            spacing=20,
            margin_top=20,
            margin_bottom=28,
            margin_start=20,
            margin_end=20,
        )
        clamp.set_child(self.box)
        self.set_child(clamp)
        self._sigs: dict[str, str] = {}

        hero = Gtk.Box(spacing=16)
        hero.add_css_class("hero")
        self.hero_icon = w.Icon("smartphone", "large", filled=True)
        hero.append(w.circle(self.hero_icon, "hero-icon"))
        col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6, valign=Gtk.Align.CENTER, hexpand=True)
        self.hero_name = w.label(css=["device-title"], ellipsize=True)
        col.append(self.hero_name)
        self.pills = Gtk.FlowBox(
            selection_mode=Gtk.SelectionMode.NONE,
            halign=Gtk.Align.START,
            max_children_per_line=6,
            column_spacing=6,
            row_spacing=6,
            homogeneous=False,
        )
        col.append(self.pills)
        hero.append(col)
        self.box.append(hero)

        actions = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        actions.append(w.label(_("Quick actions"), css=["overview-heading"]))
        self.action_columns = Gtk.Box(spacing=10, homogeneous=True)
        self.action_columns.add_css_class("action-columns")
        self.action_groups = [self._action_group(title) for title in (_("Sharing"), _("Screen & input"), _("Device"))]
        actions.append(self.action_columns)
        self.action_hint = w.label(wrap=True, css=["action-hint"])
        self.scan_button = w.text_button(_("Search again"), "refresh", ["tonal"], lambda: self.app.call("refresh"))
        self.scan_button.set_halign(Gtk.Align.START)
        self.action_unavailable = w.card(css=["tight"])
        self.action_unavailable.append(self.action_hint)
        self.action_unavailable.append(self.scan_button)
        actions.append(self.action_unavailable)
        self.action_buttons: dict[str, w.ActionButton] = {}
        self.box.append(actions)

        self.media = Gtk.Box()
        self.box.append(self.media)
        self._build_media()

        self.transfers = w.card()
        self.box.append(self.transfers)

        self.notifs = w.card()
        self.box.append(self.notifs)

        self.commands = w.card()
        self.box.append(self.commands)

        self.files = w.card()
        self.box.append(self.files)

    def _action_group(self, title: str) -> tuple[Gtk.Box, Gtk.Box]:
        group = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        group.add_css_class("action-group")
        group.append(w.label(title, css=["action-group-title"], wrap=True))
        rows = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        group.append(rows)
        self.action_columns.append(group)
        return group, rows

    def _act(self, plugin: str, action: str, toast: str | None = None, **args):
        def report(result):
            if isinstance(result, dict) and result.get("error"):
                self.app.toast(result["error"])
            elif toast:
                self.app.toast(toast)

        def run():
            self.app.plugin(
                self.device["id"],
                plugin,
                action,
                callback=report if toast or plugin == "android" else None,
                **args,
            )

        return run

    def _quick_actions(self) -> tuple[list[QuickAction], list[QuickAction], list[QuickAction]]:
        d = self.device
        plugins = {p["id"] for p in d.get("plugins", []) if p["loaded"]}
        sharing, screen, device = [], [], []
        if w.plugin_loaded(d, "share", action="share_files"):
            sharing.append(QuickAction("send-files", "upload_file", _("Send files…"), self.page.pick_files))
        if w.plugin_loaded(d, "share", action="share_text") and w.plugin_loaded(d, "share", action="share_url"):
            sharing.append(QuickAction("send-text", "link", _("Send link or text…"), self.page.send_text_dialog))
        if "clipboard" in plugins:
            sharing.append(
                QuickAction(
                    "clipboard",
                    "content_paste_go",
                    _("Send clipboard"),
                    self._act("clipboard", "send_clipboard", _("Clipboard sent")),
                )
            )
        if "sftp" in plugins:
            sharing.append(QuickAction("browse", "folder_open", _("Browse files"), self._browse))

        android = d["state"].get("android", {})
        if "android" in plugins and android.get("tools"):
            if android.get("mirroring"):
                screen.append(
                    QuickAction(
                        "mirror",
                        "stop_screen_share",
                        _("Stop mirroring"),
                        self._act("android", "stop_mirror"),
                        active=True,
                    )
                )
                screen_on = android.get("screenOn", True)
                caption = _("Turn screen off") if screen_on else _("Turn screen on")
                tip = _("Turn screen off (Alt+O)") if screen_on else _("Turn screen on (Alt+Shift+O)")
                screen.append(
                    QuickAction(
                        "screen-power",
                        "screen_lock_portrait" if screen_on else "mobile_lock_portrait",
                        caption,
                        self._act("android", "toggle_screen"),
                        tooltip=tip,
                    )
                )
            else:
                screen.append(
                    QuickAction(
                        "mirror",
                        "screen_share",
                        _("Mirror screen…"),
                        lambda: mirroring.show(self.app, self.page.window, self.device),
                    )
                )
            if android.get("preview"):
                screen.append(
                    QuickAction(
                        "camera",
                        "no_photography",
                        _("Stop camera"),
                        self._act("android", "stop_camera"),
                        active=True,
                    )
                )
            else:
                screen.append(
                    QuickAction(
                        "camera",
                        "photo_camera",
                        _("Phone camera…"),
                        lambda: mirroring.show(self.app, self.page.window, self.device, True),
                    )
                )
        if plugins & {"remotecontrol", "remotekeyboard"}:
            screen.append(
                QuickAction(
                    "remote",
                    "mouse",
                    _("Remote input"),
                    lambda: self.page.show_page("remote"),
                    opens_view=True,
                )
            )
        if "virtualmonitor" in plugins:
            monitor = d["state"].get("virtualmonitor", {})
            if monitor.get("active"):
                screen.append(
                    QuickAction(
                        "extend",
                        "desktop_access_disabled",
                        _("Stop extending"),
                        self._act("virtualmonitor", "stop"),
                        active=True,
                    )
                )
            elif monitor.get("available"):
                screen.append(QuickAction("extend", "desktop_windows", _("Extend display"), self._extend_display))

        if "findthisdevice" in plugins and d["state"].get("findthisdevice", {}).get("ringing"):
            device.append(
                QuickAction(
                    "stop-ringing",
                    "volume_off",
                    _("Stop ringing"),
                    self._act("findthisdevice", "stop"),
                    active=True,
                )
            )
        if "findmyphone" in plugins:
            device.append(
                QuickAction(
                    "ring",
                    "ring_volume",
                    _("Ring device"),
                    self._act("findmyphone", "ring", _("Ringing {name}").format(name=d["name"])),
                )
            )
        if "ping" in plugins:
            device.append(
                QuickAction("ping", "notifications_active", _("Ping"), self._act("ping", "ping", _("Ping sent")))
            )
        if "sms" in plugins:
            device.append(
                QuickAction(
                    "messages",
                    "sms",
                    _("Messages"),
                    lambda: self.page.show_page("messages"),
                    opens_view=True,
                )
            )
        if "lockdevice" in plugins:
            locked = d["state"].get("lockdevice", {}).get("remoteLocked")
            device.append(
                QuickAction(
                    "lock",
                    "lock_open" if locked else "lock",
                    _("Unlock") if locked else _("Lock"),
                    self._act("lockdevice", "set_locked", locked=not locked),
                )
            )
        return sharing, screen, device

    def _update_actions(self) -> None:
        groups = self._quick_actions()
        expected = {action.id for actions in groups for action in actions}
        for action_id in self.action_buttons.keys() - expected:
            button = self.action_buttons.pop(action_id)
            button.get_parent().remove(button)
        for (group, rows), actions in zip(self.action_groups, groups, strict=True):
            group.set_visible(bool(actions))
            previous = None
            for action in actions:
                button = self.action_buttons.get(action.id)
                if button is None:
                    button = self.action_buttons[action.id] = w.ActionButton()
                    rows.insert_child_after(button, previous)
                button.update(action.icon, action.label, action.run, action.tooltip, action.opens_view, action.active)
                previous = button
        reachable = self.device["reachable"]
        self.action_columns.set_visible(bool(expected))
        for action_id, button in self.action_buttons.items():
            button.set_sensitive(reachable or action_id in {"mirror", "screen-power", "camera"})
        local_controls = bool(expected & {"mirror", "camera"})
        hint = _("No quick actions available.")
        if not reachable:
            hint = (
                _("Reconnect this device to use its network controls.")
                if local_controls
                else _("Reconnect this device to use its controls.")
            )
        self.action_hint.set_label(hint)
        self.action_unavailable.set_visible(not (reachable and expected))
        self.scan_button.set_visible(not reachable)

    def set_compact(self, compact: bool) -> None:
        self.box.set_margin_start(14 if compact else 20)
        self.box.set_margin_end(14 if compact else 20)
        self.box.set_spacing(16 if compact else 20)
        self.action_columns.set_orientation(Gtk.Orientation.VERTICAL if compact else Gtk.Orientation.HORIZONTAL)
        self.action_columns.set_homogeneous(not compact)
        if hasattr(self, "media_card"):
            self.media_card.set_compact(compact)

    def _extend_display(self) -> None:
        def done(result):
            if isinstance(result, dict) and result.get("error"):
                self.app.toast(result["error"])

        self.app.plugin(self.device["id"], "virtualmonitor", "start", callback=done)

    def _browse(self) -> None:
        sftp = self.device["state"].get("sftp", {})
        if not sftp.get("available", True):
            self.app.toast(_("Install sshfs to browse files on this device."))
            return
        self.app.toast(_("Mounting {name}…").format(name=self.device["name"]))
        self.app.plugin(
            self.device["id"],
            "sftp",
            "browse",
            callback=lambda ok: None if ok else self.app.toast(_("Could not mount the device")),
        )

    def _build_media(self) -> None:
        from .media import MediaCard

        def dev() -> str:
            return self.device["id"]

        self.media_card = MediaCard(
            self.app,
            on_action=lambda a: self.app.plugin(dev(), "mprisremote", "media_action", action=a),
            on_seek=lambda ms: self.app.plugin(dev(), "mprisremote", "set_position", position=ms),
            on_select=lambda name: self.app.plugin(dev(), "mprisremote", "select_player", player=name),
            on_volume=lambda v: self.app.plugin(dev(), "mprisremote", "set_volume", volume=v),
            on_output=lambda: self.app.plugin(dev(), "phoneaudio", "toggle", callback=self._output_changed),
        )
        self.media.append(self.media_card)

    def _output_changed(self, result) -> None:
        if isinstance(result, dict) and result.get("error"):
            self.app.toast(result["error"])

    def _update_media(self) -> None:
        st = self.device["state"].get("mprisremote")
        players = (st or {}).get("players") or []
        visible = bool(players) and w.plugin_loaded(self.device, "mprisremote")
        self.media.set_visible(visible)
        if visible:
            self.media_card.update(players, st.get("current") or players[0]["name"])
            self.media_card.update_output(self.device["state"].get("phoneaudio"))

    def _update_notifications(self) -> None:
        items = self.device["state"].get("notifications", {}).get("items", [])
        self.notifs.set_visible(w.plugin_loaded(self.device, "notifications"))
        sig = w.signature(items)
        if self._sigs.get("notifs") == sig:
            return
        self._sigs["notifs"] = sig
        w.clear(self.notifs)
        head = Gtk.Box(spacing=8)
        head.append(w.section_title(_("Notifications")))
        head.append(Gtk.Box(hexpand=True))
        dev = self.device["id"]
        if any(i["dismissable"] for i in items):
            head.append(
                w.text_button(
                    _("Clear all"), "clear_all", ["flat"], lambda: self.app.plugin(dev, "notifications", "dismiss_all")
                )
            )
        self.notifs.append(head)
        if not items:
            self.notifs.append(w.label(_("No notifications"), css=["body-small"]))
            return
        lst = Gtk.ListBox(selection_mode=Gtk.SelectionMode.NONE)
        lst.add_css_class("flat")
        for n in items[:30]:
            lst.append(self._notification_row(dev, n))
        self.notifs.append(lst)

    def _notification_row(self, dev: str, n: dict) -> Gtk.Widget:
        row = Gtk.Box(spacing=12, margin_top=6, margin_bottom=6)
        if n["icon"]:
            pic = Gtk.Image.new_from_file(n["icon"])
            pic.set_pixel_size(32)
            pic.set_valign(Gtk.Align.START)
            row.append(pic)
        else:
            ico = w.Icon("notifications", "large", css=["dim"])
            ico.set_valign(Gtk.Align.START)
            row.append(ico)
        col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=3, hexpand=True)
        top = Gtk.Box(spacing=6)
        app_name = w.label(n["appName"], css=["label-caps"], ellipsize=True)
        app_name.set_max_width_chars(20)
        top.append(app_name)
        top.append(w.label("· " + time.strftime("%H:%M", time.localtime(n["time"])), css=["body-small"]))
        col.append(top)
        if n["title"]:
            col.append(w.label(n["title"], css=["title-medium"], wrap=True))
        if n["text"]:
            t = w.label(n["text"], wrap=True, css=["dim"])
            t.set_lines(4)
            t.set_ellipsize(Pango.EllipsizeMode.END)
            col.append(t)
        btns = Gtk.FlowBox(
            selection_mode=Gtk.SelectionMode.NONE,
            max_children_per_line=4,
            column_spacing=8,
            row_spacing=8,
            margin_top=6,
        )

        def invoke(action: str) -> None:
            self.app.plugin(dev, "notifications", "invoke_action", id=n["id"], action=action)

        for a in n["actions"][:3]:
            btns.append(w.text_button(a, None, ["tonal"], lambda a=a: invoke(a)))
        if len(n["actions"]) > 3:
            pop = Gtk.Popover()
            pbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            for a in n["actions"][3:]:
                b = w.text_button(a, None, ["flat"], lambda a=a: (pop.popdown(), invoke(a)))
                pbox.append(b)
            pop.set_child(pbox)
            more = Gtk.MenuButton(popover=pop)
            more.set_child(w.Icon("more_horiz", "small"))
            more.add_css_class("tonal")
            more.add_css_class("round")
            btns.append(more)
        if n["replyId"]:
            btns.append(w.text_button(_("Reply"), "reply", ["tonal"], lambda: self.page.reply_dialog(n)))
        if btns.get_first_child():
            col.append(btns)
        row.append(col)
        if n["dismissable"]:
            dismiss_btn = w.icon_button(
                "close", _("Dismiss"), callback=lambda: self.app.plugin(dev, "notifications", "dismiss", id=n["id"])
            )
            dismiss_btn.set_valign(Gtk.Align.START)
            row.append(dismiss_btn)
        return row

    def _update_transfers(self, state: dict) -> None:
        items = [t for t in state.get("transfers", []) if t["device"] == self.device["id"]][:8]
        self.transfers.set_visible(bool(items))
        sig = w.signature([(t["id"], t["state"], t["bytes"] // max(1, t["total"] // 50 or 1)) for t in items])
        if self._sigs.get("transfers") == sig:
            return
        self._sigs["transfers"] = sig
        w.clear(self.transfers)
        head = Gtk.Box()
        head.append(w.section_title(_("Transfers")))
        head.append(Gtk.Box(hexpand=True))
        if any(t["state"] not in ("pending", "running") for t in items):
            head.append(w.text_button(_("Clear"), None, ["flat"], lambda: self.app.call("transfers.clear")))
        self.transfers.append(head)
        for t in items:
            r = Gtk.Box(spacing=12, margin_top=4, margin_bottom=4)
            r.append(
                w.Icon(
                    "download" if t["direction"] == "download" else "upload",
                    "medium",
                    css=["error-text"] if t["state"] == "failed" else ["primary-text"],
                )
            )
            col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4, hexpand=True)
            col.append(w.label(t["filename"], ellipsize=True))
            if t["state"] in ("pending", "running"):
                pb = Gtk.ProgressBar(fraction=(t["bytes"] / t["total"]) if t["total"] else 0)
                col.append(pb)
                col.append(
                    w.label(
                        f"{human_size(t['bytes'])} / {human_size(t['total'])} · {human_size(t['speed'])}/s",
                        css=["body-small"],
                    )
                )
            else:
                text = {
                    "done": _("Completed"),
                    "failed": _("Failed: {error}").format(error=t["error"]),
                    "cancelled": _("Cancelled"),
                }.get(t["state"], t["state"])
                col.append(w.label(f"{text} · {human_size(t['total'])}", css=["body-small"]))
            r.append(col)
            if t["state"] in ("pending", "running"):
                r.append(
                    w.icon_button(
                        "close", _("Cancel"), callback=lambda tid=t["id"]: self.app.call("transfer.cancel", id=tid)
                    )
                )
            elif t["state"] == "done" and t["path"]:
                path = t["path"]
                r.append(
                    w.icon_button(
                        "open_in_new",
                        _("Open"),
                        callback=lambda p=path: Gio.AppInfo.launch_default_for_uri(
                            Gio.File.new_for_path(p).get_uri(), None
                        ),
                    )
                )
            self.transfers.append(r)

    def _update_commands(self) -> None:
        st = self.device["state"].get("remotecommands", {})
        cmds = st.get("commands", [])
        self.commands.set_visible(bool(cmds) and w.plugin_loaded(self.device, "remotecommands"))
        sig = w.signature(cmds)
        if self._sigs.get("cmds") == sig:
            return
        self._sigs["cmds"] = sig
        w.clear(self.commands)
        self.commands.append(w.section_title(_("Commands")))
        flow = Gtk.FlowBox(
            selection_mode=Gtk.SelectionMode.NONE,
            max_children_per_line=3,
            min_children_per_line=1,
            column_spacing=10,
            row_spacing=10,
        )
        for c in cmds:
            flow.append(
                w.text_button(
                    c.get("name", c["key"]),
                    "terminal",
                    ["tonal"],
                    lambda k=c["key"]: self.app.plugin(self.device["id"], "remotecommands", "trigger", key=k),
                )
            )
        self.commands.append(flow)

    def _update_files(self) -> None:
        st = self.device["state"].get("sftp", {})
        self.files.set_visible(w.plugin_loaded(self.device, "sftp") and (st.get("mounted") or bool(st.get("error"))))
        sig = w.signature(st)
        if self._sigs.get("sftp") == sig:
            return
        self._sigs["sftp"] = sig
        w.clear(self.files)
        head = Gtk.Box()
        head.append(w.section_title(_("Files")))
        head.append(Gtk.Box(hexpand=True))
        if st.get("mounted"):
            head.append(
                w.text_button(
                    _("Unmount"), "eject", ["flat"], lambda: self.app.plugin(self.device["id"], "sftp", "unmount")
                )
            )
        self.files.append(head)
        if st.get("error"):
            self.files.append(w.label(st["error"], wrap=True, css=["error-text"]))
        for dct in st.get("directories", []):
            self.files.append(
                w.text_button(
                    dct["name"],
                    "folder",
                    ["tonal"],
                    lambda p=dct["path"]: self.app.plugin(self.device["id"], "sftp", "browse", path=p),
                )
            )

    def update(self, d: dict, state: dict) -> None:
        self.device = d
        self.hero_icon.set_icon(w.device_icon(d["type"]))
        self.hero_name.set_label(d["name"])
        pills = []
        if d["reachable"]:
            pills.append(("● " + _("Connected"), "ok"))
        else:
            pills.append((_("Disconnected"), "warn"))
        batt = d["state"].get("battery")
        if d["reachable"] and batt and batt.get("charge", -1) >= 0:
            pills.append(
                (
                    f"{batt['charge']}%" + (" · " + _("charging") if batt["charging"] else ""),
                    "warn" if batt["charge"] <= 15 and not batt["charging"] else "",
                )
            )
        conn = d["state"].get("connectivity_report")
        if d["reachable"] and conn and conn.get("strength", -1) >= 0:
            net = conn["networkType"] if conn["networkType"] not in ("", "Unknown") else _("Mobile")
            pills.append((f"{net} · {'▂▄▆█'[: max(1, conn['strength'])]}", ""))
        call = d["state"].get("telephony", {}).get("call")
        if d["reachable"] and call:
            pills.append(
                ((_("Call") + ": " + call.get("contact", "")) if call.get("contact") else _("In a call"), "ok")
            )
        sig = w.signature(pills)
        if self._sigs.get("pills") != sig:
            self._sigs["pills"] = sig
            self.pills.remove_all()
            for text, kind in pills:
                self.pills.append(w.pill(text, kind))
        self._update_actions()
        self._update_media()
        self._update_transfers(state)
        self._update_notifications()
        self._update_commands()
        self._update_files()
