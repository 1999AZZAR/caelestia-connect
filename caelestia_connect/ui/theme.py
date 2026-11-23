"""Caelestia look for GTK: Material 3 colours from the shell's scheme, its fonts and shapes.

The scheme file is watched so the app follows wallpaper / scheme changes live, like the shell does.
"""

from __future__ import annotations

import json
import logging
import os
import re
from pathlib import Path

from gi.repository import Adw, Gdk, Gio, Gtk

log = logging.getLogger(__name__)

STATE_DIR = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state")) / "caelestia"
SCHEME_PATH = STATE_DIR / "scheme.json"
FONT_CANDIDATES = [
    Path(__file__).parent / "assets" / "fonts",
    Path.home() / ".local/share/fonts/caelestia",
    Path.home() / ".config/quickshell/caelestia/assets/google-sans-flex",
    Path("/etc/xdg/quickshell/caelestia/assets/google-sans-flex"),
]
SANS = (
    "-apple-system, BlinkMacSystemFont, 'SF Pro Text', 'SF Pro Display', "
    "'Inter', 'GoogleSansFlex', 'Google Sans', 'Cantarell', sans-serif"
)
ICON_FONT = "Material Symbols Rounded"

# Apple macOS Sonoma / Sequoia design palette
FALLBACK_DARK = {
    "background": "1e1e21",
    "surface": "1c1c1e",
    "surfaceContainerLowest": "121214",
    "surfaceContainerLow": "252528",  # Sidebar
    "surfaceContainer": "2c2c2e",     # Inset grouped cards
    "surfaceContainerHigh": "36363a", # Card hover / elevated
    "surfaceContainerHighest": "444448", # Well / inset
    "surfaceBright": "505054",
    "onSurface": "f5f5f7",            # Apple primary label
    "onSurfaceVariant": "98989d",     # Apple secondary label (60%)
    "outline": "636366",              # Apple system gray
    "outlineVariant": "38383a",       # Hairline divider
    "primary": "0a84ff",              # Apple system blue
    "onPrimary": "ffffff",
    "primaryContainer": "0040dd",
    "onPrimaryContainer": "d0e2ff",
    "secondary": "5e5ce6",            # Apple system indigo
    "onSecondary": "ffffff",
    "secondaryContainer": "323238",   # Subtle pill fill
    "onSecondaryContainer": "e5e5ea",
    "tertiary": "30d158",             # Apple system green
    "onTertiary": "ffffff",
    "tertiaryContainer": "1b4724",
    "onTertiaryContainer": "a3e8b4",
    "error": "ff453a",                # Apple system red
    "onError": "ffffff",
    "errorContainer": "4d1412",
    "onErrorContainer": "ffd4d1",
    "success": "30d158",
    "inverseSurface": "f2f2f7",
    "inverseOnSurface": "1c1c1e",
    "scrim": "000000",
    "shadow": "000000",
}

FALLBACK_LIGHT = {
    "background": "f2f2f7",
    "surface": "ffffff",
    "surfaceContainerLowest": "ffffff",
    "surfaceContainerLow": "e9e9ed",
    "surfaceContainer": "ffffff",
    "surfaceContainerHigh": "f2f2f7",
    "surfaceContainerHighest": "e5e5ea",
    "surfaceBright": "ffffff",
    "onSurface": "000000",
    "onSurfaceVariant": "6c6c70",
    "outline": "aeaeb2",
    "outlineVariant": "e5e5ea",
    "primary": "007aff",
    "onPrimary": "ffffff",
    "primaryContainer": "007aff",
    "onPrimaryContainer": "ffffff",
    "secondary": "5856d6",
    "onSecondary": "ffffff",
    "secondaryContainer": "e5e5ea",
    "onSecondaryContainer": "1c1c1e",
    "tertiary": "34c759",
    "onTertiary": "ffffff",
    "tertiaryContainer": "34c759",
    "onTertiaryContainer": "ffffff",
    "error": "ff3b30",
    "onError": "ffffff",
    "errorContainer": "ff3b30",
    "onErrorContainer": "ffffff",
    "success": "34c759",
    "inverseSurface": "1c1c1e",
    "inverseOnSurface": "f2f2f7",
    "scrim": "000000",
    "shadow": "000000",
}

FALLBACK = FALLBACK_DARK

CSS_TEMPLATE = Path(__file__).with_name("style.css").read_text()


_BG_DECL = re.compile(r"(background-color|--[a-z-]*(?:bg|background|backdrop)-color):\s*#%\((\w+)\)s")


def _translucent_backgrounds(css: str) -> str:
    """Draw every background with alpha < 1.

    GTK 4 derives the surface's opaque region from the opaque render nodes it draws (window,
    sidebar, cards...). Hyprland never blurs behind opaque regions, so any fully opaque background
    would show the wallpaper sharp through the window opacity rule instead of blurred.
    """
    return _BG_DECL.sub(lambda m: f"{m.group(1)}: alpha(#%({m.group(2)})s, 0.99)", css)


class Theme:
    def __init__(self) -> None:
        self.provider = Gtk.CssProvider()
        self.colours = dict(FALLBACK)
        self.mode = "dark"
        self._monitor: Gio.FileMonitor | None = None
        self._fonts_loaded = False

    def install(self) -> None:
        self._load_fonts()
        display = Gdk.Display.get_default()
        # Keep app controls consistent when the desktop supplies its own gtk.css.
        Gtk.StyleContext.add_provider_for_display(display, self.provider, Gtk.STYLE_PROVIDER_PRIORITY_USER + 1)
        self.reload()
        try:
            gfile = Gio.File.new_for_path(str(SCHEME_PATH))
            self._monitor = gfile.monitor_file(Gio.FileMonitorFlags.WATCH_MOVES, None)
            self._monitor.connect("changed", self._changed)
        except Exception as e:
            log.debug("Cannot watch scheme: %s", e)

    def _load_fonts(self) -> None:
        if self._fonts_loaded:
            return
        self._fonts_loaded = True
        try:
            from gi.repository import PangoCairo

            fm = PangoCairo.FontMap.get_default()
            for d in FONT_CANDIDATES:
                if not d.is_dir():
                    continue
                for f in sorted(d.glob("*.ttf")):
                    try:
                        fm.add_font_file(str(f))
                    except Exception as fe:
                        log.debug("Could not add font file %s: %s", f, fe)
        except Exception as e:
            log.debug("Could not load Caelestia font: %s", e)

    def _changed(self, _mon, _f, _other, event) -> None:
        if event in (
            Gio.FileMonitorEvent.CHANGES_DONE_HINT,
            Gio.FileMonitorEvent.CREATED,
            Gio.FileMonitorEvent.RENAMED,
            Gio.FileMonitorEvent.MOVED_IN,
        ):
            self.reload()

    def reload(self) -> None:
        try:
            data = json.loads(SCHEME_PATH.read_text())
            self.mode = data.get("mode", "dark")
            base = FALLBACK_LIGHT if self.mode == "light" else FALLBACK_DARK
            colours = dict(base)
            colours.update({k: str(v).lstrip("#") for k, v in data.get("colours", {}).items()})
        except (OSError, ValueError):
            self.mode = "dark"
            colours = dict(FALLBACK_DARK)
        self.colours = colours
        css = _translucent_backgrounds(CSS_TEMPLATE) % {**colours, "sans": SANS, "icon": ICON_FONT}
        self.provider.load_from_string(css)
        Adw.StyleManager.get_default().set_color_scheme(
            Adw.ColorScheme.FORCE_LIGHT if self.mode == "light" else Adw.ColorScheme.FORCE_DARK
        )

    def rgba(self, name: str, alpha: float = 1.0) -> tuple[float, float, float, float]:
        """Scheme colour as a Cairo source tuple."""
        value = int(self.colours.get(name, "ffffff")[:6], 16)
        return (value >> 16 & 255) / 255, (value >> 8 & 255) / 255, (value & 255) / 255, alpha
