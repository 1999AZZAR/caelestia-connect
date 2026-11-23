import asyncio
import sys
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from caelestia_connect.packet import Packet  # noqa: E402
from caelestia_connect.plugins import sms  # noqa: E402


def test_replayed_sms_packets_do_not_rewrite_cache_or_rebuild_status(tmp_path, monkeypatch):
    monkeypatch.setattr(sms.config, "DATA_DIR", tmp_path)
    monkeypatch.setattr(sms.config, "CACHE_DIR", tmp_path)
    daemon = SimpleNamespace(state_changed=Mock())
    plugin = sms.SmsPlugin(SimpleNamespace(id="test-device", daemon=daemon))
    message = {"thread_id": 12, "_id": 20, "type": 1, "date": 1000, "read": False}
    packet = Packet("kdeconnect.sms.messages", {"messages": [message]})

    async def replay():
        await plugin.handle(packet)
        plugin._persist()
        first_write = plugin.cache_path.stat().st_mtime_ns
        await plugin.handle(packet)
        plugin.destroy()
        assert plugin.cache_path.stat().st_mtime_ns == first_write

    asyncio.run(replay())
    daemon.state_changed.assert_called_once()
    with_conversations = Mock(side_effect=AssertionError("status expanded conversations"))
    monkeypatch.setattr(plugin, "conversations", with_conversations)
    assert plugin.state() == {"version": 1, "threads": 1, "unread": 1}
