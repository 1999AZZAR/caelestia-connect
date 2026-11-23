"""Reconnect policy: never dial ourselves, and back off handshakes that keep failing.

A stale identity for this machine used to be retried twice a second forever; each
failed TLS handshake leaked native memory until the daemon reached 21 GB.
"""

import asyncio
import sys
from pathlib import Path
from unittest import mock

from gi.events import GLibEventLoopPolicy

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from caelestia_connect import lan  # noqa: E402

OWN_ID = "a" * 32
PHONE_ID = "b" * 32


class Identity:
    device_id = OWN_ID

    def tls_certificate(self):
        return None


class Daemon:
    def __init__(self):
        self.identity = Identity()
        self.trusted = {}
        self.addresses = []
        self.established = []
        self.lost = []

    def remember_address(self, device_id, host, port):
        self.addresses.append((device_id, host, port))

    def link_established(self, link, replaced=None):
        self.established.append(link)

    def link_lost(self, link):
        self.lost.append(link)


class Clock:
    def __init__(self):
        self.now = 1000.0

    def monotonic(self):
        return self.now


def provider():
    return lan.LanLinkProvider(Daemon(), discovery=False)


def attempt(p, clock, host="192.168.1.50", device_id=PHONE_ID, port=1716, force=False):
    spawned = []

    def fake_spawn(coro, name=None):
        spawned.append(name)
        coro.close()
        return

    p.local_addresses = {"127.0.0.1", "::1", "192.168.1.10"}
    with mock.patch.object(lan.time, "monotonic", clock.monotonic), mock.patch.object(lan, "spawn", fake_spawn):
        p.device_discovered(host, port, device_id, 8, force=force)
    return spawned


def test_own_device_id_is_never_dialled():
    p = provider()
    clock = Clock()
    assert attempt(p, clock, device_id=OWN_ID) == []


def test_our_own_address_is_never_dialled():
    p = provider()
    clock = Clock()
    assert attempt(p, clock, host="192.168.1.10") == [], "a stale self identity must not be dialled"
    assert attempt(p, clock, host="127.0.0.1") == ["lan:connect:127.0.0.1"], "loopback stays available for tests"


def test_self_address_predicate_excludes_loopback():
    p = provider()
    p.local_addresses = {"127.0.0.1", "::1", "192.168.1.10"}
    assert p._is_self_address("192.168.1.10")
    assert not p._is_self_address("127.0.0.1")
    assert not p._is_self_address("192.168.1.50")


def test_a_device_with_a_live_link_is_not_dialled_again():
    p = provider()
    clock = Clock()
    p.links[PHONE_ID] = type("Link", (), {"closed": False})()
    assert attempt(p, clock, force=True) == [], "a second address for the same device must not replace the link"

    p.links[PHONE_ID].closed = True
    assert attempt(p, clock, force=True) == ["lan:connect:192.168.1.50"], "a closed link is dialled again"


def test_a_normal_peer_is_dialled():
    p = provider()
    clock = Clock()
    assert attempt(p, clock) == ["lan:connect:192.168.1.50"]


def note(p, clock, method, times=1):
    with mock.patch.object(lan.time, "monotonic", clock.monotonic):
        for _ in range(times):
            getattr(p, method)(PHONE_ID)


def test_failed_handshakes_back_off_and_reset_on_success():
    p = provider()
    clock = Clock()

    note(p, clock, "_note_connect_failure")
    assert attempt(p, clock) == [], "the first retry is suppressed"

    clock.now += 1.0  # past the 0.5s delay but before the second backoff step
    assert attempt(p, clock) == ["lan:connect:192.168.1.50"]

    note(p, clock, "_note_connect_failure", 2)
    clock.now += 1.0
    assert attempt(p, clock) == [], "the delay doubles with each failure"

    clock.now += 60.0
    assert attempt(p, clock) == ["lan:connect:192.168.1.50"], "backoff is capped, not permanent"

    note(p, clock, "_note_connect_success")
    clock.now += 1.0
    assert attempt(p, clock) == ["lan:connect:192.168.1.50"], "a successful link clears the backoff"


def test_backoff_is_bounded():
    p = provider()
    p._note_connect_failure(PHONE_ID)
    for _ in range(20):
        p._note_connect_failure(PHONE_ID)
    delay = p._retry_at[PHONE_ID] - lan.time.monotonic()
    assert delay <= 60.0, "a device must be retried at least once a minute"


def test_gio_line_reader_distinguishes_blank_packets_from_eof():
    async def read():
        source = lan.Gio.MemoryInputStream.new_from_bytes(lan.GLib.Bytes.new(b"\npacket\n"))
        stream = lan.Gio.DataInputStream.new(source)
        assert await lan._read_line(stream, 64) == b""
        assert await lan._read_line(stream, 64) == b"packet"
        assert await lan._read_line(stream, 64) is None

    previous = asyncio.get_event_loop_policy()
    asyncio.set_event_loop_policy(GLibEventLoopPolicy())
    loop = asyncio.get_event_loop()
    try:
        loop.run_until_complete(read())
    finally:
        loop.close()
        asyncio.set_event_loop_policy(previous)
