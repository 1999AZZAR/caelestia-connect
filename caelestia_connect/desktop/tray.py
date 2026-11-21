"""StatusNotifierItem (SNI) implementation for KDE Plasma, GNOME (AppIndicator), and Waybar."""

from __future__ import annotations

import logging
import subprocess
from typing import TYPE_CHECKING, Any

from gi.repository import Gio, GLib

from ..utils import which

if TYPE_CHECKING:
    from ..daemon import Daemon

log = logging.getLogger(__name__)

SNI_XML = """
<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="WindowId" type="i" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>

    <method name="ContextMenu">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="Activate">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="SecondaryActivate">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="Scroll">
      <arg type="i" name="delta" direction="in"/>
      <arg type="s" name="orientation" direction="in"/>
    </method>

    <signal name="NewTitle"/>
    <signal name="NewIcon"/>
    <signal name="NewAttentionIcon"/>
    <signal name="NewOverlayIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewStatus">
      <arg type="s" name="status"/>
    </signal>
  </interface>
</node>
"""

OBJECT_PATH = "/StatusNotifierItem"
INTERFACE_NAME = "org.kde.StatusNotifierItem"


class StatusNotifierTray:
    def __init__(self, daemon: Daemon) -> None:
        self.daemon = daemon
        self.conn: Gio.DBusConnection | None = None
        self._reg_id = 0
        self.status = "Active"
        self.icon_name = "org.caelestia.Connect"
        self.tooltip_text = "Caelestia Connect"
        self._node_info = Gio.DBusNodeInfo.new_for_xml(SNI_XML)

    async def start(self) -> None:
        try:
            self.conn = await Gio.bus_get(Gio.BusType.SESSION)
            self._reg_id = self.conn.register_object(
                OBJECT_PATH,
                self._node_info.interfaces[0],
                self._handle_method_call,
                self._handle_get_property,
                None,
            )
            # Register with StatusNotifierWatcher
            await self.conn.call(
                "org.kde.StatusNotifierWatcher",
                "/StatusNotifierWatcher",
                "org.kde.StatusNotifierWatcher",
                "RegisterStatusNotifierItem",
                GLib.Variant("(s)", (OBJECT_PATH,)),
                None,
                Gio.DBusCallFlags.NONE,
                2000,
            )
            log.info("StatusNotifierItem registered on session bus (%s)", OBJECT_PATH)
        except GLib.Error as e:
            log.debug("StatusNotifierWatcher unavailable: %s", e.message)

    def stop(self) -> None:
        if self.conn and self._reg_id:
            try:
                self.conn.unregister_object(self._reg_id)
            except GLib.Error:
                pass
            self._reg_id = 0

    def update(self, state: dict[str, Any]) -> None:
        if not self.conn or not self._reg_id:
            return

        devices = state.get("devices", [])
        connected = [d for d in devices if d.get("paired") and d.get("reachable")]
        pair_requests = [d for d in devices if d.get("pairState") == "requestedByPeer"]

        prev_status = self.status
        prev_icon = self.icon_name
        prev_tooltip = self.tooltip_text

        if pair_requests:
            self.status = "NeedsAttention"
            self.icon_name = "phone"
            names = ", ".join(d.get("name", "Phone") for d in pair_requests)
            self.tooltip_text = f"Pair request from {names}"
        elif connected:
            self.status = "Active"
            self.icon_name = "phone"
            parts = []
            for d in connected:
                name = d.get("name", "Phone")
                bat = d.get("battery", {})
                chg = bat.get("charging", False)
                lvl = bat.get("level")
                if lvl is not None:
                    icon_chg = " ⚡" if chg else ""
                    parts.append(f"{name}: {lvl}%{icon_chg}")
                else:
                    parts.append(f"{name} (Connected)")
            self.tooltip_text = " • ".join(parts)
        else:
            self.status = "Active"
            self.icon_name = "org.caelestia.Connect"
            self.tooltip_text = "No devices connected"

        # Emit change signals if properties changed
        try:
            if self.tooltip_text != prev_tooltip:
                self.conn.emit_signal(None, OBJECT_PATH, INTERFACE_NAME, "NewToolTip", None)
            if self.icon_name != prev_icon:
                self.conn.emit_signal(None, OBJECT_PATH, INTERFACE_NAME, "NewIcon", None)
            if self.status != prev_status:
                self.conn.emit_signal(None, OBJECT_PATH, INTERFACE_NAME, "NewStatus", GLib.Variant("(s)", (self.status,)))
        except GLib.Error:
            pass

    def _open_gui(self) -> None:
        cmd = which("caelestia-connect") or "caelestia-connect"
        try:
            subprocess.Popen([cmd])
        except Exception as e:
            log.warning("Could not launch GUI from tray: %s", e)

    def _handle_method_call(
        self,
        conn: Gio.DBusConnection,
        sender: str,
        path: str,
        iface: str,
        method: str,
        params: GLib.Variant,
        invocation: Gio.DBusMethodInvocation,
    ) -> None:
        if method in ("Activate", "SecondaryActivate", "ContextMenu"):
            self._open_gui()
            invocation.return_value(None)
        elif method == "Scroll":
            invocation.return_value(None)
        else:
            invocation.return_value(None)

    def _handle_get_property(
        self,
        conn: Gio.DBusConnection,
        sender: str,
        path: str,
        iface: str,
        prop: str,
    ) -> GLib.Variant | None:
        if prop == "Category":
            return GLib.Variant("s", "ApplicationStatus")
        if prop == "Id":
            return GLib.Variant("s", "caelestia-connect")
        if prop == "Title":
            return GLib.Variant("s", "Caelestia Connect")
        if prop == "Status":
            return GLib.Variant("s", self.status)
        if prop == "WindowId":
            return GLib.Variant("i", 0)
        if prop == "IconName":
            return GLib.Variant("s", self.icon_name)
        if prop == "IconThemePath":
            return GLib.Variant("s", "")
        if prop == "Menu":
            return GLib.Variant("o", "/NO_DBUSMENU")
        if prop == "ItemIsMenu":
            return GLib.Variant("b", False)
        if prop == "ToolTip":
            return GLib.Variant("(sa(iiay)ss)", (self.icon_name, [], "Caelestia Connect", self.tooltip_text))
        return None
