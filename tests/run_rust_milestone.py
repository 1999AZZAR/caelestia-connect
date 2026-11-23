"""Check interoperability, pairing, and KDE Connect ping delivery on loopback."""

from __future__ import annotations

import argparse
import asyncio
import base64
import ipaddress
import json
import os
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import textwrap
import time
from contextlib import contextmanager
from pathlib import Path

from android_tools import setup_android_tools
from fake_android import FakeAndroid, packet, read_line

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

BUS_CONFIG = """<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>
"""
TEST_COVER = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNQcGgAAAFkAOHHE8n2AAAAAElFTkSuQmCC"
)
TEST_CALL_PHOTO = (
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIs"
    "IxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIy"
    "MjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAADAAIDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAA"
    "AAb/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFQEBAQAAAAAAAAAAAAAAAAAABQf/xAAUEQEAAAAAAAAAAAAAAAAA"
    "AAAA/9oADAMBAAIRAxEAPwCSAHKU/9k="
)
TEST_AUDIO_ADDRESS = "02:11:22:33:44:55"
TEST_AUDIO_UUID = "0000110a-0000-1000-8000-00805f9b34fb"
SMS_NUMBER = "+1 555 000 1234"


@contextmanager
def daemon(command: list[str], socket_path: Path):
    with subprocess.Popen(command, cwd=ROOT) as process:
        try:
            deadline = time.monotonic() + 8
            while True:
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f"daemon did not start: {command}")
                try:
                    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                        probe.settimeout(0.5)
                        probe.connect(str(socket_path))
                    break
                except OSError:
                    pass
                time.sleep(0.05)
            yield process
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise


@contextmanager
def phone(certificate: str, *, device_id: str | None = None):
    peer = FakeAndroid("127.0.0.1", certificate, device_id=device_id)
    try:
        yield peer
    finally:
        disconnect(peer)
        shutil.rmtree(peer.tmp)


def disconnect(peer: FakeAndroid) -> None:
    if peer.tls is None:
        return
    try:
        peer.tls.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    peer.tls.close()
    peer.tls = None


def connect_by_mdns(peer: FakeAndroid):
    from caelestia_connect.mdns import MDNS_ADDR, MDNS_PORT, MdnsService

    address = ipaddress.IPv4Address("127.0.0.1")
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        for port in range(1740, 1765):
            try:
                listener.bind((str(address), port))
                break
            except OSError:
                continue
        else:
            raise RuntimeError("no test TCP port available for mDNS")
        listener.listen(1)
        listener.settimeout(0.5)
        mdns = MdnsService(
            peer.device_id, lambda: {"id": peer.device_id, "protocol": "8"}, lambda: port, lambda *_: None
        )
        record = mdns._records(address)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as multicast:
            multicast.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, address.packed)
            multicast.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 255)
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                multicast.sendto(record, (MDNS_ADDR, MDNS_PORT))
                try:
                    connection, _ = listener.accept()
                    break
                except TimeoutError:
                    continue
            else:
                raise AssertionError("Rust daemon did not connect to the mDNS peer")
        connection.settimeout(5)
        invite = json.loads(read_line(connection))
        assert invite["body"]["targetDeviceId"] == peer.device_id
        peer.tls = peer.client_ctx().wrap_socket(connection, server_side=False)
        peer._secure_identity()


def trust_with_python(socket_path: Path, config_dir: Path):
    from caelestia_connect.cli import Client

    command = [sys.executable, "-m", "caelestia_connect.daemon", "--no-discovery", "-q"]
    with daemon(command, socket_path):
        client = Client()
        with client.sock:
            me = client.call("state")["self"]
            certificate = (config_dir / "certificate.pem").read_text()
            peer = FakeAndroid("127.0.0.1", certificate)
            try:
                peer.connect_direct(me["tcpPort"], me["id"])
                peer.send("kdeconnect.pair", {"pair": True, "timestamp": int(time.time())})
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    devices = client.call("state")["devices"]
                    if any(d["id"] == peer.device_id and d["pairState"] == "requestedByPeer" for d in devices):
                        break
                    time.sleep(0.05)
                else:
                    raise AssertionError("Python daemon did not offer the pairing")
                assert client.call("device.accept", device=peer.device_id)
                peer.wait_for("kdeconnect.pair", lambda body: body.get("pair") is True)
                return me, certificate, peer
            except BaseException:
                shutil.rmtree(peer.tmp)
                raise
            finally:
                disconnect(peer)


async def check_rust(me, certificate, peer, newcomer, port, check_gtk, tray_record):
    from gi.repository import Gio, GLib

    from caelestia_connect.config import IPC_SOCKET
    from caelestia_connect.crypto import cert_from_pem, verification_key
    from caelestia_connect.ipc import IpcClient

    events = asyncio.Queue()

    def event(name, data):
        if name == "state":
            events.put_nowait(data)

    client = IpcClient(on_event=event)
    await client.connect()
    bus = Gio.bus_get_sync(Gio.BusType.SESSION)

    def tray_properties():
        service = tray_record.read_text()
        reply = bus.call_sync(
            service,
            "/StatusNotifierItem",
            "org.freedesktop.DBus.Properties",
            "GetAll",
            GLib.Variant("(s)", ("org.kde.StatusNotifierItem",)),
            None,
            Gio.DBusCallFlags.NONE,
            1000,
            None,
        )
        return reply.unpack()[0]

    async def check_tray(status, content):
        async with asyncio.timeout(6):
            while True:
                if tray_record.is_file():
                    props = await asyncio.to_thread(tray_properties)
                    if props["Status"] == status and content in props["ToolTip"][3]:
                        return
                await asyncio.sleep(0.05)

    def device(state, device_id):
        return next((d for d in state["devices"] if d["id"] == device_id), None)

    async def wait_for_device(device_id, condition):
        async with asyncio.timeout(6):
            while True:
                found = device(await client.call("state"), device_id)
                if condition(found):
                    return found
                await events.get()

    async def send_ping(device_id, text):
        return await client.call("device.call", device=device_id, plugin="ping", action="ping", args={"message": text})

    async def gtk_action(device_id, action):
        ui = await asyncio.create_subprocess_exec(
            sys.executable,
            __file__,
            "--gtk-device",
            device_id,
            "--gtk-action",
            action,
            cwd=ROOT,
            env={
                **os.environ,
                "GTK_A11Y": "none",
                "LANGUAGE": "en",
                "CAELESTIA_CONNECT_PREVIEW_SOCKET": str(IPC_SOCKET),
            },
        )
        try:
            assert await asyncio.wait_for(ui.wait(), 15) == 0, f"GTK action failed: {action}"
        finally:
            if ui.returncode is None:
                ui.terminate()
                await ui.wait()

    def key_for(phone_peer, timestamp):
        return verification_key(cert_from_pem(certificate), phone_peer.cert, timestamp)

    async def check_discovery_quiet(listener):
        with listener:
            listener.setblocking(False)
            try:
                payload = await asyncio.wait_for(asyncio.get_running_loop().sock_recv(listener, 8192), 63)
            except TimeoutError:
                return
        raise AssertionError(f"Unsolicited discovery announcement after manual refresh: {json.loads(payload)['type']}")

    discovery_check = None
    try:
        await check_tray("Active", "No devices connected")
        before = await client.call("subscribe")
        assert await client.call("ping") == "pong"
        assert before["self"]["id"] == me["id"]
        assert before["self"]["fingerprint"] == me["fingerprint"]
        trusted = device(before, peer.device_id)
        assert trusted["paired"] and not trusted["reachable"]
        tcp = before["self"]["tcpPort"]

        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.122.33.44", 1716))
            listener.settimeout(5)
            assert await client.call("devices.add", address="127.122.33.44")
            announcement, address = await asyncio.to_thread(listener.recvfrom, 8192)
            identity = json.loads(announcement)
            assert address[1] == port and address[1] != 1716
            assert identity["type"] == "kdeconnect.identity"
            assert identity["body"]["deviceId"] == me["id"] and identity["body"]["tcpPort"] == tcp
            assert await client.call("settings.setName", name="Rust LAN test") == "Rust LAN test"
            renamed, _ = await asyncio.to_thread(listener.recvfrom, 8192)
            assert json.loads(renamed)["body"]["deviceName"] == "Rust LAN test"
            discovery_check = asyncio.create_task(check_discovery_quiet(listener.dup()))
        print("PASS discovery uses the standard port and advertises a saved name change", flush=True)

        with phone(certificate) as advertised:
            await asyncio.to_thread(connect_by_mdns, advertised)
            discovered = await wait_for_device(advertised.device_id, lambda d: d and d["reachable"])
            assert not discovered["paired"]
        print("PASS mDNS discovery of a peer using the Python responder format", flush=True)

        with phone(certificate, device_id=peer.device_id) as other_key:
            try:
                await asyncio.to_thread(other_key.connect_direct, tcp, me["id"])
            except (ssl.SSLError, ConnectionError):
                pass
            else:
                raise AssertionError("device with a different certificate was accepted")
        assert not device(await client.call("state"), peer.device_id)["reachable"]
        print("PASS rejected a different certificate with the paired device ID", flush=True)

        await check_android(
            client, peer, wait_for_device, tray_record.parent / "android", gtk_action if check_gtk else None
        )
        await asyncio.to_thread(peer.connect_by_broadcast, discovery_port=port)
        online = await wait_for_device(peer.device_id, lambda d: d and d["reachable"])
        assert online["paired"] and online["fingerprint"] == trusted["fingerprint"]
        assert {plugin["id"] for plugin in online["plugins"] if plugin["loaded"]} == {
            "ping",
            "share",
            "battery",
            "connectivity_report",
            "findmyphone",
            "clipboard",
            "mprisremote",
            "mpriscontrol",
            "phoneaudio",
            "sms",
            "contacts",
            "notifications",
            "telephony",
            "pausemusic",
            "systemvolume",
            "remotesystemvolume",
            "runcommand",
            "remotecommands",
            "android",
        }
        assert await send_ping(peer.device_id, "sent by Rust")
        await asyncio.to_thread(peer.wait_for, "kdeconnect.ping", lambda body: body.get("message") == "sent by Rust")
        print("PASS UDP discovery, Python pairing reuse, subscription, and ping delivery", flush=True)
        await check_media(client, peer, gtk_action if check_gtk else None)
        await check_desktop_media(client, peer, certificate, me["id"], tcp)
        await check_volume(client, peer, certificate, me["id"], tcp, wait_for_device)
        await check_commands(
            client, peer, certificate, me["id"], tcp, wait_for_device, gtk_action if check_gtk else None
        )
        await check_phone_audio(client, peer, wait_for_device)
        await check_messaging(client, peer, gtk_action if check_gtk else None)
        await check_notifications(client, peer, tray_record.parent, gtk_action if check_gtk else None)
        inhibitor = tray_record.parent / "inhibitor"
        assert not inhibitor.exists()

        async def expect_inhibit(active):
            async with asyncio.timeout(5):
                while not inhibitor.exists() or json.loads(inhibitor.read_text())["active"] != active:
                    await asyncio.sleep(0.04)

        async def set_inhibit(enabled):
            assert await client.call(
                "device.setPluginEnabled", device=peer.device_id, plugin="screensaver_inhibit", enabled=enabled
            )

        assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="ping", enabled=False)
        try:
            await send_ping(peer.device_id, "disabled")
        except RuntimeError as error:
            assert "Ping unavailable" in str(error)
        else:
            raise AssertionError("Disabled Ping still sent a packet")
        assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="ping", enabled=True)

        assert "kdeconnect.battery.request" in peer.desktop_identity["body"]["outgoingCapabilities"]
        await asyncio.to_thread(
            peer.wait_for, "kdeconnect.battery.request", lambda body: body == {"request": True}, 5
        )
        peer.send("kdeconnect.battery", {"currentCharge": 76.0, "isCharging": True})
        peer.send(
            "kdeconnect.connectivity_report", {"signalStrengths": {"0": {"networkType": "5G", "signalStrength": 3}}}
        )
        status = await wait_for_device(peer.device_id, lambda d: d and "connectivity_report" in d["state"])
        assert status["state"]["battery"] == {"charge": 76, "charging": True}
        await check_tray("Active", "76%")
        if check_gtk:
            await gtk_action(peer.device_id, "battery")
        assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="battery", enabled=False)
        peer.send("kdeconnect.battery", {"currentCharge": 80, "isCharging": False})
        peer.send(
            "kdeconnect.connectivity_report", {"signalStrengths": {"0": {"networkType": "5G", "signalStrength": 4}}}
        )
        disabled = await wait_for_device(
            peer.device_id, lambda d: d and d["state"].get("connectivity_report", {}).get("strength") == 4
        )
        assert "battery" not in disabled["state"]
        assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="battery", enabled=True)
        await asyncio.to_thread(peer.wait_for, "kdeconnect.battery.request", timeout=5)
        peer.send("kdeconnect.battery", {"currentCharge": "76", "isCharging": "1"})
        battery = await wait_for_device(peer.device_id, lambda d: d and d["state"].get("battery"))
        assert battery["state"]["battery"] == {"charge": 76, "charging": True}
        assert status["state"]["connectivity_report"]["strength"] == 3
        assert status["state"]["connectivity_report"]["networkType"] == "5G"
        assert await client.call("device.call", device=peer.device_id, plugin="findmyphone", action="ring")
        await asyncio.to_thread(peer.wait_for, "kdeconnect.findmyphone.request")
        print("PASS battery, mobile signal, and Ring device", flush=True)

        clipboard = Path(os.environ["CC_TEST_CLIPBOARD"])
        clipboard.write_text("Explicit clipboard send")
        assert await client.call("device.call", device=peer.device_id, plugin="clipboard", action="send_clipboard")
        await asyncio.to_thread(
            peer.wait_for, "kdeconnect.clipboard", lambda body: body == {"content": "Explicit clipboard send"}
        )
        peer.send("kdeconnect.clipboard", {"content": "Received clipboard"})
        async with asyncio.timeout(5):
            while clipboard.read_text() != "Received clipboard":
                await asyncio.sleep(0.05)
        print("PASS explicit clipboard send and received clipboard text", flush=True)

        async def share(action, **args):
            return await client.call("device.call", device=peer.device_id, plugin="share", action=action, args=args)

        url = "https://example.com/hello"
        assert await share("share_url", url=url)
        await asyncio.to_thread(peer.wait_for, "kdeconnect.share.request", lambda body: body == {"url": url})
        try:
            await share("share_url", url="file:///tmp/local-file")
        except RuntimeError as error:
            assert "unsupported link" in str(error)
        else:
            raise AssertionError("Rust accepted file sharing through the link action")
        print("PASS outbound link sharing and file URL rejection", flush=True)

        if check_gtk:
            await gtk_action(peer.device_id, "audio-settings")
            await gtk_action(peer.device_id, "preferences")
            await gtk_action(peer.device_id, "ping")
            await asyncio.to_thread(peer.wait_for, "kdeconnect.ping", lambda body: not body, 10)
            print("PASS GTK overview Ping action reached the phone", flush=True)
            await gtk_action(peer.device_id, "text")
            await asyncio.to_thread(
                peer.wait_for, "kdeconnect.share.request", lambda body: body == {"text": "Hello from GTK\nOlá"}
            )
            print("PASS GTK preview sends Unicode text and exposes file sending", flush=True)

        await set_inhibit(True)
        await expect_inhibit(True)
        disconnect(peer)
        await wait_for_device(peer.device_id, lambda d: d and not d["reachable"])
        await expect_inhibit(False)
        await asyncio.to_thread(peer.connect_direct, tcp, me["id"])
        await wait_for_device(peer.device_id, lambda d: d and d["reachable"])
        await expect_inhibit(True)
        await set_inhibit(False)
        await expect_inhibit(False)
        reconnected = device(await client.call("state"), peer.device_id)
        assert reconnected["state"]["battery"] == {"charge": 76, "charging": True}
        assert json.loads(inhibitor.read_text())["acquisitions"] == 2
        print("PASS idle inhibition starts only when enabled and follows the connection lifecycle", flush=True)
        assert await send_ping(peer.device_id, "direct reconnect")
        await asyncio.to_thread(
            peer.wait_for, "kdeconnect.ping", lambda body: body.get("message") == "direct reconnect"
        )
        print("PASS direct reconnect and ping with reversed TLS roles", flush=True)
        await check_connection_recovery(peer, me["id"], tcp, port, wait_for_device, send_ping)

        await asyncio.to_thread(newcomer.connect_direct, tcp, me["id"])
        unknown = await wait_for_device(newcomer.device_id, lambda d: d and d["reachable"])
        assert not unknown["paired"] and unknown["plugins"] == []
        try:
            await send_ping(newcomer.device_id, "blocked")
        except RuntimeError as error:
            assert "not paired" in str(error)
        else:
            raise AssertionError("Rust sent a ping to an unpaired phone")
        newcomer.send("kdeconnect.pair", {"pair": True})
        assert not await client.call("device.accept", device=newcomer.device_id)

        timestamp = int(time.time())
        newcomer.send("kdeconnect.pair", {"pair": True, "timestamp": timestamp - 3600})
        await asyncio.to_thread(newcomer.wait_for, "kdeconnect.pair", lambda body: body.get("pair") is False)
        stale = await wait_for_device(newcomer.device_id, lambda d: d and d["pairError"])
        assert stale["pairState"] == "notPaired" and not stale["verificationKey"]
        newcomer.send("kdeconnect.pair", {"pair": True, "timestamp": timestamp})
        incoming = await wait_for_device(newcomer.device_id, lambda d: d and d["pairState"] == "requestedByPeer")
        assert incoming["verificationKey"] == key_for(newcomer, timestamp)
        await check_tray("NeedsAttention", "Pair request from")
        print("PASS Rust registered the desktop tray and updated battery and pairing status", flush=True)
        assert await client.call("device.reject", device=newcomer.device_id)
        await asyncio.to_thread(newcomer.wait_for, "kdeconnect.pair", lambda body: body.get("pair") is False)
        await wait_for_device(newcomer.device_id, lambda d: d and d["pairState"] == "notPaired")
        newcomer.send("kdeconnect.pair", {"pair": True, "timestamp": timestamp + 1})
        incoming = await wait_for_device(newcomer.device_id, lambda d: d and d["pairState"] == "requestedByPeer")
        assert incoming["verificationKey"] == key_for(newcomer, timestamp + 1)
        if check_gtk:
            await gtk_action(newcomer.device_id, "accept")
        else:
            assert await client.call("device.accept", device=newcomer.device_id)
        accepted = await asyncio.to_thread(newcomer.wait_for, "kdeconnect.pair", lambda body: body.get("pair") is True)
        assert "timestamp" not in accepted["body"]
        await wait_for_device(newcomer.device_id, lambda d: d and d["paired"])
        await asyncio.to_thread(
            newcomer.wait_for, "kdeconnect.mpris.request", lambda body: body.get("requestPlayerList")
        )
        print("PASS inbound pairing, verification key, stale request rejection, and explicit acceptance", flush=True)
        await check_file_sharing(client, newcomer)

        peer.send("kdeconnect.pair", {"pair": False})
        await wait_for_device(peer.device_id, lambda d: d and d["pairState"] == "notPaired")
        if check_gtk:
            await gtk_action(peer.device_id, "pair")
        else:
            assert await client.call("device.pair", device=peer.device_id)
        request = await asyncio.to_thread(peer.wait_for, "kdeconnect.pair", lambda body: body.get("pair") is True)
        outgoing = await wait_for_device(peer.device_id, lambda d: d and d["pairState"] == "requested")
        assert outgoing["verificationKey"] == key_for(peer, request["body"]["timestamp"])
        peer.send("kdeconnect.pair", {"pair": True})
        await wait_for_device(peer.device_id, lambda d: d and d["paired"])
        assert await client.call("device.unpair", device=peer.device_id)
        await asyncio.to_thread(peer.wait_for, "kdeconnect.pair", lambda body: body.get("pair") is False)
        await wait_for_device(peer.device_id, lambda d: d and d["pairState"] == "notPaired")
        print("PASS remote unpair, desktop-initiated pairing, and local unpair", flush=True)

        assert await client.call("device.pair", device=peer.device_id)
        await asyncio.to_thread(peer.wait_for, "kdeconnect.pair", lambda body: body.get("pair") is True)
        await asyncio.to_thread(peer.wait_for, "kdeconnect.pair", lambda body: body.get("pair") is False, 35)
        expired = await wait_for_device(peer.device_id, lambda d: d and d["pairError"] == "Timed out")
        assert expired["pairState"] == "notPaired" and not expired["verificationKey"]
        peer.send("kdeconnect.pair", {"pair": True})
        peer.send("kdeconnect.pair", {"pair": True, "timestamp": timestamp - 3600})
        await asyncio.to_thread(peer.wait_for, "kdeconnect.pair", lambda body: body.get("pair") is False)
        assert not device(await client.call("state"), peer.device_id)["paired"]
        print("PASS pairing expires and late acceptance cannot restore trust", flush=True)
        await discovery_check
        assert await client.call("devices.remove", address="127.122.33.44")
        assert "127.122.33.44" not in (await client.call("state"))["settings"]["customDevices"]
        print("PASS UDP discovery stays quiet after startup and manual announcements", flush=True)
        assert await client.call("device.setPluginEnabled", device=newcomer.device_id, plugin="share", enabled=False)
    finally:
        if discovery_check is not None:
            discovery_check.cancel()
            await asyncio.gather(discovery_check, return_exceptions=True)
        client.close()


async def check_connection_recovery(peer, desktop_id, desktop_port, discovery_port, wait_for_device, ping):
    from caelestia_connect.config import CONFIG_DIR

    next_address = "127.0.0.2"
    trust = CONFIG_DIR / "trusted_devices.json"
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        for next_port in range(1750, 1765):
            try:
                listener.bind((next_address, next_port))
                break
            except OSError:
                continue
        else:
            raise AssertionError("no free peer port for reconnection test")
        listener.settimeout(8)
        old = peer.tls
        try:
            await asyncio.to_thread(
                peer.connect_direct, desktop_port, desktop_id, source=next_address, advertised_port=next_port
            )
            recovered = await wait_for_device(
                peer.device_id, lambda d: d and d["reachable"] and d["address"] == next_address
            )
            assert recovered["state"]["battery"] == {"charge": 76, "charging": True}
            assert await ping(peer.device_id, "address changed")
            await asyncio.to_thread(
                peer.wait_for, "kdeconnect.ping", lambda body: body.get("message") == "address changed"
            )
        finally:
            old.close()

        record = json.loads(trust.read_text())[peer.device_id]
        assert (record["lastAddress"], record["lastPort"]) == (next_address, next_port)
        disconnect(peer)
        await wait_for_device(peer.device_id, lambda d: d and not d["reachable"])
        await asyncio.sleep(2.3)
        listener.listen(2)
        connection, source = await asyncio.to_thread(listener.accept)
        assert source[0] == "127.0.0.1"
        await asyncio.to_thread(peer.accept_connection, connection)
        recovered = await wait_for_device(
            peer.device_id, lambda d: d and d["reachable"] and d["address"] == next_address
        )
        assert recovered["state"]["battery"] == {"charge": 76, "charging": True}
        assert await ping(peer.device_id, "automatic reconnect")
        await asyncio.to_thread(
            peer.wait_for, "kdeconnect.ping", lambda body: body.get("message") == "automatic reconnect"
        )
    print("PASS authenticated address handoff, saved endpoint, and reconnect without a new phone broadcast", flush=True)

    discovered_address = "127.0.0.3"
    with socket.socket() as listener:
        listener.bind((discovered_address, next_port))
        listener.listen(1)
        listener.settimeout(0.35)
        peer.announce(next_port, discovery_port, source=discovered_address)
        try:
            unexpected = await asyncio.to_thread(listener.accept)
        except TimeoutError:
            pass
        else:
            unexpected[0].close()
            raise AssertionError("a secondary address advertisement interrupted a healthy link")
        assert await ping(peer.device_id, "existing link preserved")
        await asyncio.to_thread(
            peer.wait_for, "kdeconnect.ping", lambda body: body.get("message") == "existing link preserved"
        )
        listener.settimeout(8)
        disconnect(peer)
        connection, _ = await asyncio.to_thread(listener.accept)
        await asyncio.to_thread(peer.accept_connection, connection)
        await wait_for_device(peer.device_id, lambda d: d and d["reachable"] and d["address"] == discovered_address)
        record = json.loads(trust.read_text())[peer.device_id]
        assert (record["lastAddress"], record["lastPort"]) == (discovered_address, next_port)
    print("PASS discovery keeps an active link and can recover through a previously seen address", flush=True)


async def check_android(client, peer, wait_for_device, root, gtk_action):
    state_file = root / "adb.json"
    log_file = root / "calls"

    def configure(**changes):
        state = json.loads(state_file.read_text())
        target = state_file.with_suffix(".tmp")
        target.write_text(json.dumps(state | changes))
        target.replace(state_file)

    def scrcpy():
        return [line for line in log_file.read_text().splitlines() if line.startswith("scrcpy ")]

    async def action(verb, **args):
        return await client.call("device.call", device=peer.device_id, plugin="android", action=verb, args=args)

    async def shown(**values):
        await wait_for_device(
            peer.device_id,
            lambda d: d and all(d["state"].get("android", {}).get(key) == value for key, value in values.items()),
        )

    configure(devices={"FAKE-USB": "unauthorized"})
    assert await action("scan") == {"serial": "", "devices": [], "unauthorized": True}
    configure(devices={"FAKE-USB": "device"}, names={"FAKE-USB": peer.name})
    assert await action("scan") == {"serial": "FAKE-USB", "devices": ["FAKE-USB"], "unauthorized": False}
    assert "error" in await action("mirror", serial="unknown")
    phone = await wait_for_device(peer.device_id, lambda d: d is not None and not d["reachable"])
    settings = next(p for p in phone["plugins"] if p["id"] == "android")
    assert settings["values"]["video_bit_rate"] == "16M"
    fail = root / "scrcpy.fail"
    fail.touch()
    assert "error" in await action("mirror", serial="FAKE-USB")
    await shown(mirroring=False)
    fail.unlink()
    if gtk_action:
        await gtk_action(peer.device_id, "mirror")
    else:
        assert await action("mirror", serial="FAKE-USB") == {}
    await shown(mirroring=True)
    screen = scrcpy()[-1]
    assert "--serial FAKE-USB" in screen and "--video-bit-rate=16M" in screen, screen
    assert "--new-display=1920x1080" in screen and "--max-size=1440" in screen, screen
    assert "--no-audio" in screen and "--window-x 80" in screen, screen
    assert await action("toggle_screen") == {"screenOn": False}
    await shown(mirroring=True, screenOn=False)
    assert await action("set_screen", on=True) == {"screenOn": True}
    assert scrcpy()[-1] == screen
    assert await action("set_screen", on=False) == {"screenOn": False}
    await action("stop_mirror")
    await shown(mirroring=False, screenOn=True)
    assert json.loads(state_file.read_text())["screen_on"]
    print("PASS Rust scrcpy screen settings, failed start, in-place power controls, and power restoration", flush=True)

    configure(devices={}, mdns=["test _adb-tls-connect._tcp 127.0.0.1:43567"])
    assert (await action("scan"))["serial"] == "127.0.0.1:43567"
    assert await action("camera", facing="front") == {}
    assert "--display-orientation=270" in scrcpy()[-1]
    await action("stop_camera")
    if gtk_action:
        await gtk_action(peer.device_id, "camera")
    else:
        assert await action("camera", facing="front", mirror=True) == {}
    await shown(preview=True)
    front = scrcpy()[-1]
    assert "--video-source=camera --camera-facing=front" in front
    assert "--display-orientation=flip90" in front
    configure(rotation=1)
    deadline = time.monotonic() + 10
    while "--display-orientation=flip180" not in scrcpy()[-1]:
        assert time.monotonic() < deadline, scrcpy()
        await asyncio.sleep(0.1)
    assert await action("camera", facing="back", mirror=True) == {}
    rear = scrcpy()[-1]
    assert "--camera-facing=back" in rear and "--display-orientation=180" in rear
    assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="android", enabled=False)
    deadline = time.monotonic() + 5
    while Path(f"/proc/{rear.rsplit('pid=', 1)[1]}").exists():
        assert time.monotonic() < deadline, "Camera kept running after its feature was disabled"
        await asyncio.sleep(0.1)
    assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="android", enabled=True)
    await shown(preview=False)
    print("PASS Rust wireless debugging, optional front camera mirroring, rotation, and cleanup", flush=True)


async def check_file_sharing(client, peer):
    root = Path(os.environ["XDG_CACHE_HOME"]) / "file-transfer-check"
    default = root / "global"
    assert await client.call("settings.setIncomingPath", path=str(default))
    streamed = peer.send_with_payload(
        "kdeconnect.share.request", {"filename": "streamed.txt"}, b"Unknown size", declared_size=-1
    )
    async with asyncio.timeout(7):
        while not (default / "streamed.txt").exists():
            await asyncio.sleep(0.05)
    await asyncio.to_thread(streamed.join, 5)
    assert streamed.result == {"ok": True}
    assert (default / "streamed.txt").read_bytes() == b"Unknown size"
    destination = root / "incoming" / peer.name
    destination.mkdir(parents=True)
    original = destination / "notes.bin"
    original.write_bytes(b"previous file")
    assert await client.call(
        "device.setPluginSetting",
        device=peer.device_id,
        plugin="share",
        key="incomingPath",
        value=str(root / "incoming" / "%1"),
    )

    async def transfer(filename, status):
        async with asyncio.timeout(10):
            while True:
                state = await client.call("state")
                matching = [t for t in state["transfers"] if t["filename"] == filename and t["state"] == status]
                if matching:
                    return matching[0]
                await asyncio.sleep(0.05)

    body = bytes(range(256)) * 600
    stamp = 1_700_000_000_000
    sending = peer.send_with_payload(
        "kdeconnect.share.request", {"filename": "../../notes.bin", "lastModified": stamp}, body
    )
    received = await transfer("notes.bin", "done")
    await asyncio.to_thread(sending.join, 5)
    assert sending.result == {"ok": True}
    saved = Path(received["path"])
    assert saved == destination / "notes (1).bin" and saved.read_bytes() == body
    assert int(saved.stat().st_mtime * 1000) == stamp
    assert original.read_bytes() == b"previous file" and received["bytes"] == len(body)

    sending = peer.send_with_payload("kdeconnect.share.request", {"filename": "short.bin"}, b"abc", declared_size=12)
    await transfer("short.bin", "failed")
    await asyncio.to_thread(sending.join, 5)
    assert not (destination / "short.bin").exists()
    assert sorted(path.name for path in destination.iterdir()) == ["notes (1).bin", "notes.bin"]

    source = root / "upload" / "nested"
    source.mkdir(parents=True)
    (source / "a.txt").write_bytes(b"alpha")
    (source / "b.bin").write_bytes(body)
    assert (
        await client.call(
            "device.call",
            device=peer.device_id,
            plugin="share",
            action="share_files",
            args={"paths": [source.parent.as_uri()]},
        )
        == 2
    )
    for filename, payload in [("a.txt", b"alpha"), ("b.bin", body)]:
        announcement = await asyncio.to_thread(
            peer.wait_for, "kdeconnect.share.request", lambda fields, name=filename: fields.get("filename") == name
        )
        assert announcement["body"]["numberOfFiles"] == 2
        assert await asyncio.to_thread(peer.fetch_payload, announcement) == payload
        await transfer(filename, "done")

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as stalled:
        stalled.bind(("127.0.0.1", 0))
        stalled.listen(1)
        peer.tls.sendall(
            packet(
                "kdeconnect.share.request",
                {"filename": "cancel.bin"},
                payloadSize=1024,
                payloadTransferInfo={"port": stalled.getsockname()[1]},
            )
        )
        waiting = await transfer("cancel.bin", "pending")
        assert await client.call("transfer.cancel", id=waiting["id"])
        await transfer("cancel.bin", "cancelled")
        assert not (destination / "cancel.bin").exists()
    assert not list(destination.glob(".caelestia-transfer-*"))
    assert await client.call("transfers.clear")
    assert not (await client.call("state"))["transfers"]
    print("PASS file upload, download, saved destination, unique naming, short payload and cancellation", flush=True)


async def check_media(client, peer, gtk_action):
    from gi.repository import Gio, GLib

    info = "kdeconnect.mpris"
    request = "kdeconnect.mpris.request"
    player = "Test Music"
    old_player = "Paused App"
    url = "content://fixture/album"
    art = TEST_COVER

    async def receive(**fields):
        return await asyncio.to_thread(
            peer.wait_for, request, lambda body: all(body.get(k) == v for k, v in fields.items())
        )

    async def state_until(predicate):
        async with asyncio.timeout(6):
            while True:
                state = await client.call("state")
                device = next(d for d in state["devices"] if d["id"] == peer.device_id)
                media = device["state"].get("mprisremote", {})
                if predicate(media):
                    return media
                await asyncio.sleep(0.05)

    async def action(verb, **args):
        return await client.call("device.call", device=peer.device_id, plugin="mprisremote", action=verb, args=args)

    async def setting(key, value):
        assert await client.call(
            "device.setPluginSetting", device=peer.device_id, plugin="mprisremote", key=key, value=value
        )

    bus = Gio.bus_get_sync(Gio.BusType.SESSION)
    mpris_path = "/org/mpris/MediaPlayer2"
    mpris_iface = "org.mpris.MediaPlayer2.Player"
    dbus_iface = "org.freedesktop.DBus"

    def dbus(dest, path, interface, method, params=None):
        reply = bus.call_sync(dest, path, interface, method, params, None, Gio.DBusCallFlags.NONE, 2000, None)
        return reply.unpack()

    async def wait_exports(predicate):
        async with asyncio.timeout(5):
            while True:
                names = await asyncio.to_thread(dbus, dbus_iface, "/org/freedesktop/DBus", dbus_iface, "ListNames")
                exports = [
                    name for name in names[0] if name.startswith("org.mpris.MediaPlayer2.caelestiaconnect.rust_")
                ]
                if predicate(exports):
                    return exports
                await asyncio.sleep(0.05)

    await receive(requestPlayerList=True)
    peer.send(info, {"playerList": [player, old_player]})
    await receive(player=player, requestNowPlaying=True, requestVolume=True)
    await receive(player=old_player, requestNowPlaying=True, requestVolume=True)
    peer.send(
        info,
        {
            "player": player,
            "title": "Sample Track",
            "artist": "Test Artist",
            "album": "Test Album",
            "isPlaying": True,
            "length": 180_000,
            "pos": 1234,
            "volume": 75,
            "canSeek": True,
            "shuffle": False,
            "loopStatus": "Track",
            "albumArtUrl": url,
        },
    )
    await receive(player=player, albumArtUrl=url)
    sender = peer.send_with_payload(info, {"transferringAlbumArt": True, "albumArtUrl": url}, art)
    media = await state_until(lambda m: m.get("players") and m["players"][0]["art"])
    await asyncio.to_thread(sender.join, 5)
    assert sender.result == {"ok": True}
    p = media["players"][0]
    art_path = Path(p["art"])
    assert art_path.read_bytes() == art
    assert media["current"] == player and media["hidden"] == [old_player]
    assert (p["title"], p["volume"], p["length"], p["canSeek"]) == ("Sample Track", 75, 180_000, True)
    assert p["position"] >= 1234 and p["hasShuffle"] and p["hasLoop"]
    (bus_name,) = await wait_exports(lambda names: len(names) == 1)
    async with asyncio.timeout(5):
        while True:
            props = await asyncio.to_thread(
                dbus,
                bus_name,
                mpris_path,
                "org.freedesktop.DBus.Properties",
                "GetAll",
                GLib.Variant("(s)", (mpris_iface,)),
            )
            if "mpris:artUrl" in props[0]["Metadata"]:
                break
            await asyncio.sleep(0.05)
    assert props[0]["PlaybackStatus"] == "Playing"
    assert props[0]["Metadata"]["xesam:title"] == "Sample Track"
    assert props[0]["Metadata"]["mpris:artUrl"] == art_path.as_uri()
    await asyncio.to_thread(dbus, bus_name, mpris_path, mpris_iface, "Pause")
    await receive(player=player, action="Pause")
    await asyncio.to_thread(
        dbus,
        bus_name,
        mpris_path,
        "org.freedesktop.DBus.Properties",
        "Set",
        GLib.Variant("(ssv)", (mpris_iface, "Volume", GLib.Variant("d", 0.42))),
    )
    await receive(player=player, setVolume=42)
    await setting("exportMpris", False)
    await wait_exports(lambda names: not names)
    if gtk_action is not None:
        await gtk_action(peer.device_id, "media")
        await receive(player=player, action="PlayPause")
    for verb, args, expected in (
        ("media_action", {"action": "Next"}, {"action": "Next"}),
        ("media_action", {"action": "shuffle"}, {"setShuffle": True}),
        ("media_action", {"action": "repeat"}, {"setLoopStatus": "Playlist"}),
        ("set_position", {"position": 2500}, {"SetPosition": 2500}),
        ("set_volume", {"volume": 33}, {"setVolume": 33}),
    ):
        assert await action(verb, **args)
        await receive(player=player, **expected)
    assert not await action("select_player", player="Unknown player")
    await setting("hideIdle", False)
    media = await state_until(lambda m: len(m.get("players", [])) == 2)
    assert not media["hidden"]
    assert await action("select_player", player=old_player)
    await receive(player=old_player, requestNowPlaying=True, requestVolume=True)
    assert (await state_until(lambda m: m.get("current") == old_player))["current"] == old_player
    peer.send(info, {"playerList": [old_player]})
    await state_until(lambda m: [p["name"] for p in m.get("players", [])] == [old_player])
    assert not art_path.exists()
    assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="mprisremote", enabled=False)
    await state_until(lambda m: not m)
    assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="mprisremote", enabled=True)
    await receive(requestPlayerList=True)
    print("PASS media playback, metadata, album art, GTK control, MPRIS export and feature settings", flush=True)


async def check_desktop_media(client, peer, certificate, desktop_id, tcp):
    info = "kdeconnect.mpris"
    request = "kdeconnect.mpris.request"
    name = "Desktop Music"
    root = Path(os.environ["XDG_CACHE_HOME"]) / "mpris-fixture"
    root.mkdir(parents=True)
    cover = root / "Local cover.png"
    cover.write_bytes(TEST_COVER)
    http_hits = []

    async def cover_server(reader, writer):
        async with asyncio.timeout(2):
            http_hits.append(await reader.readuntil(b"\r\n\r\n"))
            writer.write(f"HTTP/1.1 200 OK\r\nContent-Length: {len(TEST_COVER)}\r\nConnection: close\r\n\r\n".encode())
            writer.write(TEST_COVER)
            await writer.drain()
            writer.close()
            await writer.wait_closed()

    async def receive(**fields):
        return await asyncio.to_thread(
            peer.wait_for, info, lambda body: all(body.get(k) == v for k, v in fields.items())
        )

    async def enabled(value):
        assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="mpriscontrol", enabled=value)

    async with await asyncio.start_server(cover_server, "127.0.0.1", 0) as http:
        web_url = f"http://127.0.0.1:{http.sockets[0].getsockname()[1]}/cover.png"
        await receive(playerList=[])
        player = await asyncio.create_subprocess_exec(
            sys.executable, __file__, "--mpris-fixture", str(root), "--cover-url", web_url
        )
        try:
            await receive(playerList=[name], supportAlbumArtPayload=True)
            peer.send(request, {"player": name, "requestNowPlaying": True, "requestVolume": True})
            recorded = await receive(player=name, title="Desktop Track", volume=67)
            body = recorded["body"]
            assert (body["artist"], body["album"], body["length"], body["pos"]) == (
                "Test Artist",
                "Test Album",
                60000,
                1250,
            )
            assert body["isPlaying"] and body["canSeek"]
            assert body["albumArtUrl"] == cover.as_uri()

            with phone(certificate) as unknown:
                await asyncio.to_thread(unknown.connect_direct, tcp, desktop_id)
                unknown.send(request, {"player": name, "requestPlayerList": True, "action": "Pause"})
                try:
                    await asyncio.to_thread(unknown.wait_for, info, timeout=0.5)
                except TimeoutError:
                    pass
                else:
                    raise AssertionError("unpaired phone received desktop player information")
                assert not (root / "actions.jsonl").exists()

            peer.send(request, {"player": name, "albumArtUrl": web_url})
            peer.send(request, {"player": name, "albumArtUrl": cover.as_uri()})
            announced = await receive(player=name, transferringAlbumArt=True, albumArtUrl=cover.as_uri())
            assert await asyncio.to_thread(peer.fetch_payload, announced) == TEST_COVER
            assert not http_hits

            peer.send(
                request,
                {
                    "player": name,
                    "action": "Pause",
                    "setVolume": 42,
                    "setShuffle": True,
                    "setLoopStatus": "Track",
                    "Seek": 250_000,
                },
            )
            await receive(player=name, isPlaying=False, volume=42, shuffle=True, loopStatus="Track")
            await receive(player=name, pos=1500)
            peer.send(request, {"player": name, "SetPosition": 2250})
            await receive(player=name, pos=2250)
            peer.send(request, {"player": name, "action": "Next"})
            await receive(player=name, albumArtUrl=web_url)
            peer.send(request, {"player": name, "albumArtUrl": web_url})
            announced = await receive(player=name, transferringAlbumArt=True, albumArtUrl=web_url)
            assert await asyncio.to_thread(peer.fetch_payload, announced) == TEST_COVER
            assert len(http_hits) == 1

            actions = (root / "actions.jsonl").read_text().splitlines()
            observed = [json.loads(row) for row in actions]
            assert observed == [
                ["Pause", None],
                ["LoopStatus", "Track"],
                ["Shuffle", True],
                ["Volume", 0.42],
                ["Seek", 250000],
                ["Seek", 750000],
                ["Next", None],
            ], observed
            await enabled(False)
            peer.send(request, {"player": name, "action": "Play"})
            await asyncio.sleep(0.3)
            assert (root / "actions.jsonl").read_text().splitlines() == actions
            await enabled(True)
            await receive(playerList=[name])
            await check_call_audio(client, peer, root, name)
        finally:
            if player.returncode is None:
                player.terminate()
            await asyncio.wait_for(player.wait(), 5)
        await receive(playerList=[])
    print(
        "PASS desktop player discovery, metadata, playback commands, seeking, album art and pairing checks", flush=True
    )


def run_mpris_fixture(root: Path, web_url: str) -> None:
    from gi.events import GLibEventLoopPolicy

    from caelestia_connect.desktop.mpris import ExportedPlayer

    class Controller:
        position = 1_250_000

        def record(self, action, value=None):
            with (root / "actions.jsonl").open("a") as log:
                log.write(json.dumps([action, value]) + "\n")

        def action(self, action):
            self.record(action)
            if action == "Pause":
                player.update(PlaybackStatus="Paused")
            if action == "Play":
                player.update(PlaybackStatus="Playing")
            if action == "Next":
                player.update(Metadata={**metadata, "mpris:artUrl": web_url})

        def set_volume(self, value):
            self.record("Volume", value)
            player.update(Volume=value)

        def set_shuffle(self, value):
            self.record("Shuffle", value)
            player.update(Shuffle=value)

        def set_loop(self, value):
            self.record("LoopStatus", value)
            player.update(LoopStatus=value)

        def seek(self, offset):
            self.record("Seek", offset)
            self.position += offset
            player.seeked(self.position)

        def position_us(self):
            return self.position

    metadata = {
        "xesam:title": "Desktop Track",
        "xesam:artist": ["Test Artist"],
        "xesam:album": "Test Album",
        "mpris:length": 60_000_000,
        "mpris:artUrl": (root / "Local cover.png").as_uri(),
    }
    player = ExportedPlayer("Desktop Music", Controller())
    player.bus_name = "org.mpris.MediaPlayer2.TestFixture"

    async def run():
        forwarded = ExportedPlayer("Forwarded phone", Controller())
        await forwarded.start()
        await player.start()
        player.update(Metadata=metadata, Volume=0.67, PlaybackStatus="Playing", CanSeek=True)
        await asyncio.Event().wait()

    asyncio.set_event_loop_policy(GLibEventLoopPolicy())
    asyncio.get_event_loop().run_until_complete(run())


def files_at(path: Path) -> dict[Path, bytes]:
    return {file.relative_to(path): file.read_bytes() for file in path.rglob("*") if file.is_file()}


async def check_call_audio(client, peer, root, player):
    async def features(**settings):
        for key, value in settings.items():
            assert await client.call(
                "device.setPluginSetting", device=peer.device_id, plugin="pausemusic", key=key, value=value
            )

    def event(event, **body):
        peer.send("kdeconnect.telephony", {"event": event, **body})

    audio = Path(os.environ["CC_TEST_AUDIO"])

    async def muted(primary):
        async with asyncio.timeout(5):
            while any(
                sink["mute"] != (primary if sink["name"] == "main" else True)
                for sink in json.loads(audio.read_text())["sinks"]
            ):
                await asyncio.sleep(0.04)

    def actions():
        return [json.loads(row)[0] for row in (root / "actions.jsonl").read_text().splitlines()]

    start = len(actions())

    async def playback(*sequence):
        async with asyncio.timeout(5):
            while len(actions()) - start < len(sequence):
                await asyncio.sleep(0.04)
        assert actions()[start:] == list(sequence)

    async def call_finished():
        async with asyncio.timeout(5):
            while True:
                devices = (await client.call("state"))["devices"]
                device = next(d for d in devices if d["id"] == peer.device_id)
                if not device["state"].get("telephony", {}).get("call"):
                    return
                await asyncio.sleep(0.04)

    peer.send("kdeconnect.mpris.request", {"player": player, "action": "Play"})
    await playback("Play")
    event("ringing")
    await asyncio.sleep(0.3)
    await playback("Play")
    await muted(False)
    event("talking")
    await playback("Play", "Pause")
    await muted(True)
    event("ringing", isCancel=True)
    await asyncio.sleep(0.3)
    await playback("Play", "Pause")
    await muted(True)
    event("talking", isCancel=True)
    await playback("Play", "Pause", "Play")
    await muted(False)

    await features(actionResume=False, actionMute=False)
    event("talking")
    await playback("Play", "Pause", "Play", "Pause")
    event("talking", isCancel=True)
    await call_finished()
    await asyncio.sleep(0.3)
    await playback("Play", "Pause", "Play", "Pause")
    await features(actionResume=True, actionMute=True)
    print(
        "PASS saved call audio settings, pause and mute only while talking, resume and no-resume behavior", flush=True
    )


async def check_volume(client, peer, certificate, desktop_id, tcp, wait_for_device):
    info = "kdeconnect.systemvolume"
    request = "kdeconnect.systemvolume.request"

    async def incoming(kind, match):
        return (await asyncio.to_thread(peer.wait_for, kind, match, 5))["body"]

    assert await incoming(request, lambda body: body.get("requestSinks"))
    local = await incoming(
        info, lambda body: any(sink["name"] == "main" and sink["enabled"] for sink in body.get("sinkList", []))
    )
    assert local["sinkList"][0]["volume"] == 32768
    assert [sink["muted"] for sink in local["sinkList"]] == [False, True]
    peer.send(request, {"name": "main", "volume": 50000, "muted": True})
    assert await incoming(
        info, lambda body: body.get("name") == "main" and body.get("volume") == 50000 and body.get("muted")
    )
    peer.send(request, {"name": "secondary", "enabled": True})
    assert await incoming(info, lambda body: body.get("name") == "secondary" and body.get("enabled"))
    assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="systemvolume", enabled=False)
    audio = Path(os.environ["CC_TEST_AUDIO"])
    before = audio.read_bytes()
    peer.send(request, {"name": "main", "volume": 1})
    with phone(certificate) as unknown:
        await asyncio.to_thread(unknown.connect_direct, tcp, desktop_id)
        unknown.send(request, {"name": "main", "volume": 2})
        try:
            await asyncio.to_thread(unknown.wait_for, info, timeout=0.5)
        except TimeoutError:
            pass
        else:
            raise AssertionError("unpaired peer received audio output state")
    assert audio.read_bytes() == before
    assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="systemvolume", enabled=True)

    remote = {"name": "phone-output", "volume": 25, "maxVolume": 100, "description": "Device speaker", "enabled": True}
    peer.send(info, {"sinkList": [remote]})
    await wait_for_device(peer.device_id, lambda d: d and d["state"].get("remotesystemvolume", {}).get("sinks"))
    peer.send(info, {"name": "phone-output", "volume": 39, "muted": True})
    await wait_for_device(
        peer.device_id,
        lambda d: d
        and any(
            sink["volume"] == 39 and sink["muted"] for sink in d["state"].get("remotesystemvolume", {}).get("sinks", [])
        ),
    )
    for action, args in (("set_volume", {"volume": 50}), ("set_muted", {"muted": False})):
        fields = {"name": remote["name"], **args}
        assert await client.call(
            "device.call", device=peer.device_id, plugin="remotesystemvolume", action=action, args=fields
        )
        assert await incoming(request, lambda body, expected=fields: body == expected)
    print("PASS system volume in both directions, sink updates, default output and unauthorized requests", flush=True)


async def check_commands(client, peer, certificate, desktop_id, tcp, wait_for_device, gtk_action):
    import shlex

    info, request, output = (f"kdeconnect.runcommand{suffix}" for suffix in ("", ".request", ".output"))

    async def wait_packet(kind, predicate):
        return (await asyncio.to_thread(peer.wait_for, kind, predicate, 5))["body"]

    async def action(plugin, verb, **args):
        return await client.call("device.call", device=peer.device_id, plugin=plugin, action=verb, args=args)

    mark = Path(os.environ["XDG_RUNTIME_DIR"]) / "started-command"
    quoted = shlex.quote(str(mark))
    assert await wait_packet(request, lambda body: body.get("requestCommandList"))
    assert (await wait_packet(info, lambda body: "fixture" in json.loads(body["commandList"])))["canAddCommand"]
    with phone(certificate) as unknown:
        await asyncio.to_thread(unknown.connect_direct, tcp, desktop_id)
        unknown.send(request, {"requestCommandList": True})
        unknown.send(request, {"key": "fixture", "command": f"touch {quoted}"})
        try:
            await asyncio.to_thread(unknown.wait_for, info, timeout=0.5)
        except TimeoutError:
            pass
        else:
            raise AssertionError("unpaired peer received command definitions")
    assert not mark.exists()

    peer.send(request, {"key": "unknown", "command": f"touch {quoted}"})
    peer.send(request, {"key": "fixture", "command": f"touch {quoted}"})
    started = await wait_packet(output, lambda body: body.get("commandStarted"))
    assert started["command"] == "printf 'saved\\n'; printf 'fixture error\\n' >&2"
    assert await wait_packet(output, lambda body: body.get("id") == started["id"] and body.get("stdout") == ["saved"])
    assert await wait_packet(
        output, lambda body: body.get("id") == started["id"] and body.get("stderr") == ["fixture error"]
    )
    assert (await wait_packet(output, lambda body: body.get("id") == started["id"] and body.get("commandFinished")))[
        "exitCode"
    ] == 0
    assert not mark.exists()

    key = await action("runcommand", "add_command", name="Temporary job", command="true")
    assert await action(
        "runcommand", "edit_command", key=key, name="Cancelable job", command=f"(sleep 1; touch {quoted}) & wait"
    )
    peer.send(request, {"key": key})
    started = await wait_packet(
        output, lambda body: body.get("commandStarted") and str(mark) in body.get("command", "")
    )
    peer.send(request, {"stop": True})
    finished = await wait_packet(output, lambda body: body.get("id") == started["id"] and body.get("commandFinished"))
    assert finished["exitCode"] == 130
    assert await action("runcommand", "remove_command", key=key)
    assert not await action("runcommand", "remove_command", key=key)
    assert await wait_packet(info, lambda body: list(json.loads(body["commandList"])) == ["fixture"])

    peer.send(
        info,
        {
            "commandList": json.dumps({"peer-fixture": {"name": "Fixture remote command", "command": "true"}}),
            "canAddCommand": True,
        },
    )
    await wait_for_device(peer.device_id, lambda d: d and d["state"].get("remotecommands", {}).get("commands"))
    if gtk_action:
        await gtk_action(peer.device_id, "commands")
    else:
        assert await action("remotecommands", "trigger", key="peer-fixture")
    assert await wait_packet(request, lambda body: body.get("key") == "peer-fixture")
    assert await action("remotecommands", "edit_commands")
    assert await wait_packet(request, lambda body: body.get("setup") is True)
    peer.send(output, {"commandStarted": True, "command": "true", "id": 2})
    peer.send(output, {"stdout": ["Fixture output"], "id": 2})
    peer.send(output, {"commandFinished": True, "exitCode": 0, "id": 2})
    await wait_for_device(
        peer.device_id,
        lambda d: d
        and d["state"].get("remotecommands", {}).get("output") == ["$ true", "Fixture output", "[exit code 0]"],
    )
    assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="runcommand", enabled=False)
    peer.send(request, {"key": "fixture"})
    try:
        await asyncio.to_thread(peer.wait_for, output, lambda body: body.get("commandStarted"), 0.5)
    except TimeoutError:
        pass
    else:
        raise AssertionError("a disabled command plugin started a job")
    assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin="runcommand", enabled=True)
    assert not mark.exists()
    print(
        "PASS saved commands, local output, cancellation, remote command UI, and paired-device authorization",
        flush=True,
    )


def seed_messages(peer):
    from caelestia_connect import config

    path = config.DATA_DIR / "sms" / f"{peer.device_id}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "threads": {
                    "42": {
                        "1-1000-1": {
                            "_id": 1,
                            "thread_id": 42,
                            "date": 1000,
                            "type": 1,
                            "body": "History from Python",
                            "read": True,
                            "addresses": [{"address": SMS_NUMBER}],
                        }
                    }
                }
            }
        )
    )
    contacts = config.DATA_DIR / "contacts" / peer.device_id
    contacts.mkdir(parents=True, exist_ok=True)
    (contacts / "saved.vcf").write_text(
        f"BEGIN:VCARD\nFN:Fixture Contact\nTEL;TYPE=CELL:{SMS_NUMBER}\nX-KDECONNECT-TIMESTAMP:10\nEND:VCARD\n"
    )


async def check_messaging(client, peer, gtk_action):
    async def plugin(plugin_id, action, **args):
        return await client.call("device.call", device=peer.device_id, plugin=plugin_id, action=action, args=args)

    async def receive(kind, **fields):
        return await asyncio.to_thread(
            peer.wait_for, f"kdeconnect.{kind}", lambda body: all(body.get(k) == v for k, v in fields.items())
        )

    async def expect(action, predicate, **args):
        async with asyncio.timeout(7):
            while True:
                found = await plugin("sms", action, **args)
                if predicate(found):
                    return found
                await asyncio.sleep(0.04)

    await receive("sms.request_conversations")
    await receive("contacts.request_all_uids_timestamps")
    assert await plugin("contacts", "list_contacts") == [
        {"uid": "saved", "name": "Fixture Contact", "phones": [SMS_NUMBER], "emails": []}
    ]
    cached = await plugin("sms", "messages", thread_id="42")
    assert cached[0]["body"] == "History from Python" and cached[0]["senderName"] == "Fixture Contact"
    peer.send("kdeconnect.contacts.response_uids_timestamps", {"uids": ["saved", "new"], "saved": 10, "new": 20})
    await receive("contacts.request_vcards_by_uid", uids=["new"])
    peer.send(
        "kdeconnect.contacts.response_vcards",
        {"uids": ["new"], "new": "BEGIN:VCARD\nFN:Synced Contact\nEMAIL:test@example.test\nEND:VCARD\n"},
    )
    attachment_id = "mms-image.png"
    message = {
        "_id": 2,
        "thread_id": 42,
        "date": 2000,
        "type": 1,
        "body": "Hello from synthetic phone",
        "read": False,
        "addresses": [{"address": SMS_NUMBER}],
        "sub_id": 3,
        "attachments": [{"part_id": 8, "unique_identifier": attachment_id, "mime_type": "image/png"}],
    }
    peer.send("kdeconnect.sms.messages", {"messages": [message]})
    conv = (await expect("conversations", lambda convs: convs and convs[0]["count"] == 2))[0]
    assert conv["title"] == "Fixture Contact" and not conv["read"] and conv["subId"] == 3
    assert [entry["name"] for entry in await plugin("contacts", "list_contacts")] == [
        "Fixture Contact",
        "Synced Contact",
    ]
    if gtk_action:
        await gtk_action(peer.device_id, "messages")
    assert await plugin("sms", "request_conversation", thread_id="42", start=1234, count=50)
    await receive("sms.request_conversation", threadID=42, rangeStartTimestamp=1234, numberToRequest=50)

    assert await plugin("sms", "get_attachment", part_id=8, unique_id=attachment_id) == ""
    await receive("sms.request_attachment", part_id=8, unique_identifier=attachment_id)
    sender = peer.send_with_payload("kdeconnect.sms.attachment_file", {"filename": attachment_id}, TEST_COVER)
    messages = await expect("messages", lambda items: items[-1]["attachments"][0]["path"], thread_id="42")
    await asyncio.to_thread(sender.join, 5)
    assert sender.result == {"ok": True}
    local = Path(messages[-1]["attachments"][0]["path"])
    assert local.read_bytes() == TEST_COVER
    assert await plugin("sms", "get_attachment", part_id=8, unique_id=attachment_id) == str(local)

    assert await plugin(
        "sms", "send_sms", addresses=[SMS_NUMBER], message="Rust MMS", sub_id=3, attachments=[str(local)]
    )
    sent = await receive("sms.request", version=2, messageBody="Rust MMS", subID=3, addresses=[{"address": SMS_NUMBER}])
    assert sent["body"]["attachments"][0]["mimeType"] == "image/png"
    assert base64.b64decode(sent["body"]["attachments"][0]["base64EncodedFile"]) == TEST_COVER
    await receive("sms.request_conversation", threadID=42, numberToRequest=10)
    await expect("messages", lambda items: items[-1]["body"] == "Rust MMS", thread_id="42")
    confirmed = {**message, "_id": 3, "date": int(time.time() * 1000), "type": 2, "body": "Rust MMS", "attachments": []}
    peer.send("kdeconnect.sms.messages", {"messages": [confirmed]})
    await expect("conversations", lambda convs: convs[0]["count"] == 3 and convs[0]["read"])
    from caelestia_connect import config

    cache = config.DATA_DIR / "sms" / f"{peer.device_id}.json"
    async with asyncio.timeout(7):
        while True:
            saved = json.loads(cache.read_text())["threads"]["42"]
            if len(saved) == 3 and all(not key.startswith("pending-") for key in saved):
                break
            await asyncio.sleep(0.05)
    print("PASS cached messages, contact sync, GTK Messages, sending MMS and downloading attachments", flush=True)


async def check_phone_audio(client, peer, wait_for_device):
    def available(device):
        return device and device["state"].get("phoneaudio", {}).get("available")

    async def audio(action, **args):
        return await client.call("device.call", device=peer.device_id, plugin="phoneaudio", action=action, args=args)

    device = await wait_for_device(peer.device_id, available)
    assert device["state"]["phoneaudio"] == {"available": True, "onComputer": False, "playing": False}
    assert await audio("toggle") == {}
    device = await wait_for_device(peer.device_id, lambda d: available(d) and d["state"]["phoneaudio"]["onComputer"])
    assert device["state"]["phoneaudio"]["playing"]
    peer.send("kdeconnect.mpris", {"player": "Audio fixture", "title": "Route test", "isPlaying": True})
    await wait_for_device(
        peer.device_id,
        lambda d: d and d["state"].get("mprisremote", {}).get("current") == "Audio fixture",
    )
    restoring = asyncio.create_task(audio("set_on_computer", on_computer=False))
    await asyncio.sleep(0.3)
    peer.send("kdeconnect.mpris", {"player": "Audio fixture", "isPlaying": False})
    assert await restoring == {}
    await asyncio.to_thread(
        peer.wait_for,
        "kdeconnect.mpris.request",
        lambda body: body == {"player": "Audio fixture", "action": "Play"},
    )
    await wait_for_device(peer.device_id, lambda d: available(d) and not d["state"]["phoneaudio"]["onComputer"])
    peer.send("kdeconnect.mpris", {"playerList": []})
    print("PASS Bluetooth audio profile routing, remembered address and playback resume on the phone", flush=True)


async def check_notifications(client, peer, root, gtk_action):
    from gi.repository import Gio, GLib

    async def action(verb, **args):
        return await client.call("device.call", device=peer.device_id, plugin="notifications", action=verb, args=args)

    async def receive(kind, **fields):
        return await asyncio.to_thread(
            peer.wait_for,
            f"kdeconnect.notification.{kind}",
            lambda body: all(body.get(k) == v for k, v in fields.items()),
        )

    async def items(predicate):
        async with asyncio.timeout(6):
            while True:
                state = await client.call("state")
                device = next(device for device in state["devices"] if device["id"] == peer.device_id)
                shown = device["state"].get("notifications", {}).get("items", [])
                if predicate(shown):
                    return shown
                await asyncio.sleep(0.04)

    async def setting(key, value):
        return await client.call(
            "device.setPluginSetting", device=peer.device_id, plugin="notifications", key=key, value=value
        )

    notice = {
        "id": "notice-one",
        "appName": "Fixture Chat",
        "title": "Fixture Notification",
        "text": "<sample>",
        "isClearable": True,
        "requestReplyId": "reply-fixture",
        "actions": ["Copy 123456"],
        "payloadHash": "fixture-notify",
    }
    await receive("request", request=True)
    art = peer.send_with_payload("kdeconnect.notification", notice, TEST_COVER)
    current = (await items(lambda shown: shown and shown[0]["icon"]))[0]
    await asyncio.to_thread(art.join, 5)
    assert art.result == {"ok": True}
    assert current["title"] == "Fixture Notification" and Path(current["icon"]).read_bytes() == TEST_COVER
    if gtk_action:
        await gtk_action(peer.device_id, "notifications")
    data_file = root / "notifications"
    async with asyncio.timeout(5):
        while True:
            data = json.loads(data_file.read_text()) if data_file.exists() else {}
            found = next(((token, value) for token, value in data.items() if value["title"] == current["title"]), None)
            if found and found[1]["icon"] == current["icon"]:
                break
            await asyncio.sleep(0.05)
    token, popup = found
    assert popup["body"] == "&lt;sample&gt;" and popup["actions"] == [
        "default",
        "Open",
        "action:Copy 123456",
        "Copy 123456",
        "reply",
        "Reply",
        "dismiss",
        "Dismiss",
    ]
    bus = Gio.bus_get_sync(Gio.BusType.SESSION)

    def trigger(signal, text, notification=token):
        return bus.call_sync(
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "TriggerFixture",
            GLib.Variant("(sus)", (signal, int(notification), text)),
            None,
            Gio.DBusCallFlags.NONE,
            1000,
            None,
        )

    await asyncio.to_thread(trigger, "ActionInvoked", "action:Copy 123456")
    await receive("action", key="notice-one", action="Copy 123456")
    clipboard = Path(os.environ["CC_TEST_CLIPBOARD"])
    async with asyncio.timeout(4):
        while not clipboard.exists() or clipboard.read_text() != "123456":
            await asyncio.sleep(0.04)
    await asyncio.to_thread(trigger, "NotificationReplied", "Reply from desktop")
    await receive("reply", requestReplyId="reply-fixture", message="Reply from desktop")
    assert await action("reply", id="notice-one", message="Reply from GTK")
    await receive("reply", requestReplyId="reply-fixture", message="Reply from GTK")
    assert await action("dismiss", id="notice-one")
    await receive("request", cancel="notice-one")
    await items(lambda shown: not shown)
    async with asyncio.timeout(5):
        while token in json.loads(data_file.read_text()):
            await asyncio.sleep(0.05)
    assert await setting("showPopups", False)
    peer.send("kdeconnect.notification", {**notice, "id": "notice-two", "title": "Without Popup", "payloadHash": ""})
    await items(lambda shown: shown and shown[0]["id"] == "notice-two")
    assert all(value["title"] != "Without Popup" for value in json.loads(data_file.read_text()).values())
    peer.send("kdeconnect.notification", {"isCancel": True, "id": "org.kde.kdeconnect_tp::notice-two"})
    await items(lambda shown: not shown)
    print("PASS GTK phone notifications, desktop popups, icon payload, actions, replies and dismissal", flush=True)

    async def call_status(event):
        async with asyncio.timeout(5):
            while True:
                devices = (await client.call("state"))["devices"]
                device = next(device for device in devices if device["id"] == peer.device_id)
                call = device["state"].get("telephony", {}).get("call") or {}
                if call.get("event") == event:
                    return call
                await asyncio.sleep(0.05)

    async def call_popup(body, *, photo):
        async with asyncio.timeout(5):
            while True:
                for token, popup in json.loads(data_file.read_text()).items():
                    if popup["body"] == body and ("image" in popup) == photo:
                        return token, popup
                await asyncio.sleep(0.05)

    thumbnail = "\n".join(textwrap.wrap(TEST_CALL_PHOTO, 76)) + "\n"
    ring = {"event": "ringing", "phoneNumber": "(555) 000-1234"}
    peer.send("kdeconnect.telephony", ring)
    assert (await call_status("ringing"))["contact"] == "Fixture Contact"
    if gtk_action:
        await gtk_action(peer.device_id, "call")
    call_token, ringing_popup = await call_popup("Incoming call from Fixture Contact", photo=False)
    assert ringing_popup["actions"] == ["mute", "Mute call"]
    peer.send("kdeconnect.telephony", {**ring, "phoneThumbnail": thumbnail})
    photo_token, ringing_popup = await call_popup("Incoming call from Fixture Contact", photo=True)
    assert call_token == photo_token
    assert ringing_popup["image"] == [2, 3, 8, True, 8, 4, 24]
    await asyncio.to_thread(trigger, "ActionInvoked", "mute", call_token)
    await asyncio.to_thread(
        peer.wait_for, "kdeconnect.telephony.request_mute", lambda body: body.get("action") == "mute"
    )
    peer.send("kdeconnect.telephony", {"event": "talking"})
    assert (await call_status("talking"))["contact"] == "Fixture Contact"
    async with asyncio.timeout(5):
        while call_token in json.loads(data_file.read_text()):
            await asyncio.sleep(0.05)
    peer.send("kdeconnect.telephony", {"event": "talking", "isCancel": "true"})
    await call_status(None)
    peer.send(
        "kdeconnect.telephony",
        {
            "event": "missedCall",
            "phoneNumber": SMS_NUMBER,
            "contactName": "Fixture Caller",
            "phoneThumbnail": base64.b64encode(TEST_COVER).decode(),
        },
    )
    assert (await call_status("missedCall"))["contact"] == "Fixture Caller"
    _, missed_popup = await call_popup("Missed call from Fixture Caller", photo=True)
    assert missed_popup["image"] == [1, 1, 4, True, 8, 4, 4]
    peer.send("kdeconnect.telephony", {"event": "missedCall", "isCancel": True})
    await call_status(None)
    unknown = "+1 555 000 9876"
    peer.send("kdeconnect.telephony", {"event": "missedCall", "phoneNumber": unknown, "phoneThumbnail": "/9j/AA=="})
    assert (await call_status("missedCall"))["contact"] == unknown
    _, fallback = await call_popup(f"Missed call from {unknown}", photo=False)
    assert fallback["icon"] == "call-start"

    for plugin in ("sms", "contacts"):
        assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin=plugin, enabled=False)
    missed = {"event": "missedCall", "phoneNumber": unknown}
    peer.send(
        "kdeconnect.telephony",
        {**missed, "contactName": "Large call photo", "phoneThumbnail": thumbnail + " " * (64 * 1024)},
    )
    _, large_popup = await call_popup("Missed call from Large call photo", photo=True)
    assert large_popup["image"] == [2, 3, 8, True, 8, 4, 24]
    peer.send(
        "kdeconnect.telephony",
        {**missed, "contactName": "Oversized call photo", "phoneThumbnail": thumbnail + " " * (256 * 1024)},
    )
    _, fallback = await call_popup("Missed call from Oversized call photo", photo=False)
    assert fallback["icon"] == "call-start"
    for plugin in ("sms", "contacts"):
        assert await client.call("device.setPluginEnabled", device=peer.device_id, plugin=plugin, enabled=True)
    print("PASS caller names and photos, invalid photo fallback, GTK call status, and ringtone mute", flush=True)


def run_tray_watcher(record: Path):
    from gi.repository import Gio, GLib

    conn = Gio.bus_get_sync(Gio.BusType.SESSION)
    namespace = "org.kde.StatusNotifierWatcher"
    interface = Gio.DBusNodeInfo.new_for_xml(
        f'<node><interface name="{namespace}">'
        '<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>'
        "</interface></node>"
    )

    def registered(_connection, sender, _path, _iface, _method, params, invocation):
        assert params.unpack() == ("/StatusNotifierItem",)
        temporary = record.with_suffix(".tmp")
        temporary.write_text(sender)
        temporary.replace(record)
        invocation.return_value(None)

    conn.register_object("/StatusNotifierWatcher", interface.interfaces[0], registered, None, None)
    Gio.bus_own_name_on_connection(conn, namespace, Gio.BusNameOwnerFlags.NONE, None, None)
    mock_bluez_audio(conn, Gio, GLib)
    mock_notification_server(conn, Gio, GLib, record.parent / "notifications")
    mock_screensaver(conn, Gio, GLib, record.parent / "inhibitor")
    GLib.MainLoop().run()


def mock_bluez_audio(conn, Gio, GLib):
    device_path = "/org/bluez/hci0/dev_02_11_22_33_44_55"
    device_interface = "org.bluez.Device1"
    routed = False
    manager = Gio.DBusNodeInfo.new_for_xml(
        '<node><interface name="org.freedesktop.DBus.ObjectManager">'
        '<method name="GetManagedObjects"><arg type="a{oa{sa{sv}}}" direction="out"/></method>'
        "</interface></node>"
    )
    device = Gio.DBusNodeInfo.new_for_xml(
        f'<node><interface name="{device_interface}">'
        '<method name="ConnectProfile"><arg type="s" direction="in"/></method>'
        '<method name="DisconnectProfile"><arg type="s" direction="in"/></method>'
        "</interface></node>"
    )

    def objects(_connection, _sender, _path, _iface, _method, _params, invocation):
        paths = {
            device_path: {
                device_interface: {
                    "Paired": GLib.Variant("b", True),
                    "Address": GLib.Variant("s", TEST_AUDIO_ADDRESS),
                    "Alias": GLib.Variant("s", "Renamed Bluetooth Phone"),
                    "UUIDs": GLib.Variant("as", [TEST_AUDIO_UUID]),
                }
            },
        }
        if routed:
            paths[f"{device_path}/fd0"] = {
                "org.bluez.MediaTransport1": {
                    "Device": GLib.Variant("o", device_path),
                    "State": GLib.Variant("s", "active"),
                }
            }
        invocation.return_value(GLib.Variant("(a{oa{sa{sv}}})", (paths,)))

    def profile(_connection, _sender, _path, _iface, method, params, invocation):
        nonlocal routed
        assert params.unpack() == (TEST_AUDIO_UUID,)
        routed = method == "ConnectProfile"
        invocation.return_value(None)

    conn.register_object("/", manager.interfaces[0], objects, None, None)
    conn.register_object(device_path, device.interfaces[0], profile, None, None)
    Gio.bus_own_name_on_connection(conn, "org.bluez", Gio.BusNameOwnerFlags.NONE, None, None)


def mock_notification_server(conn, Gio, GLib, path: Path):
    service = "org.freedesktop.Notifications"
    object_path = "/org/freedesktop/Notifications"
    notify_args = "".join(
        f'<arg type="{kind}" direction="in"/>'
        for kind in (
            "s",
            "u",
            "s",
            "s",
            "s",
            "as",
            "a{sv}",
            "i",
        )
    )
    interface = Gio.DBusNodeInfo.new_for_xml(
        f'<node><interface name="{service}"><method name="Notify">{notify_args}<arg type="u" direction="out"/></method>'
        '<method name="CloseNotification"><arg type="u" direction="in"/></method>'
        '<method name="TriggerFixture"><arg type="s" direction="in"/><arg type="u" direction="in"/>'
        '<arg type="s" direction="in"/></method></interface></node>'
    )
    sequence = 0
    active = {}

    def call(_connection, _sender, _path, _iface, method, parameters, invocation):
        nonlocal sequence
        args = parameters.unpack()
        result = None
        if method == "Notify":
            _app, replaces, icon, title, body, actions, hints, _timeout = args
            if not replaces:
                sequence += 1
            notification = replaces or sequence
            entry = {"icon": icon, "title": title, "body": body, "actions": actions}
            if "image-data" in hints:
                *layout, pixels = hints["image-data"]
                entry["image"] = [*layout, len(pixels)]
            active[notification] = entry
            result = GLib.Variant("(u)", (notification,))
        elif method == "CloseNotification":
            active.pop(args[0], None)
        elif method == "TriggerFixture":
            signal, token, value = args
            conn.emit_signal(None, object_path, service, signal, GLib.Variant("(us)", (token, value)))
        temp = path.with_suffix(".tmp")
        temp.write_text(json.dumps(active))
        temp.replace(path)
        invocation.return_value(result)

    conn.register_object(object_path, interface.interfaces[0], call, None, None)
    Gio.bus_own_name_on_connection(conn, service, Gio.BusNameOwnerFlags.NONE, None, None)


def mock_screensaver(conn, Gio, GLib, path):
    service = "org.freedesktop.ScreenSaver"
    xml = (
        f'<node><interface name="{service}"><method name="Inhibit">'
        '<arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="u" direction="out"/></method>'
        '<method name="UnInhibit"><arg type="u" direction="in"/></method></interface></node>'
    )
    interface = Gio.DBusNodeInfo.new_for_xml(xml)
    sender_id = None
    acquisitions = 0

    def call(_connection, sender, _path, _iface, method, parameters, invocation):
        nonlocal sender_id, acquisitions
        if method == "Inhibit":
            assert parameters.unpack() == ("Caelestia Connect", "Device connected")
            assert sender_id is None
            sender_id = sender
            acquisitions += 1
            value = GLib.Variant("(u)", (acquisitions,))
        else:
            assert sender_id == sender and parameters.unpack() == (acquisitions,)
            sender_id = None
            value = None
        update = path.with_suffix(".tmp")
        update.write_text(json.dumps({"active": sender_id is not None, "acquisitions": acquisitions}))
        update.replace(path)
        invocation.return_value(value)

    conn.register_object("/org/freedesktop/ScreenSaver", interface.interfaces[0], call, None, None)
    Gio.bus_own_name_on_connection(conn, service, Gio.BusNameOwnerFlags.NONE, None, None)


def setup_pactl(directory, audio):
    audio.write_text(
        json.dumps(
            {
                "default": "main",
                "sinks": [
                    {
                        "name": name,
                        "description": name.title(),
                        "volume": {"front-left": {"value": 32768}},
                        "mute": muted,
                    }
                    for name, muted in (("main", False), ("secondary", True))
                ],
            }
        )
    )
    pactl = directory / "pactl"
    pactl.write_text(
        f"#!{sys.executable}\n"
        """import json
import os
import sys
import time
from pathlib import Path

path = Path(os.environ["CC_TEST_AUDIO"])
args = sys.argv[1:]
if args == ["subscribe"]:
    previous = path.stat().st_mtime_ns
    while True:
        current = path.stat().st_mtime_ns
        if current != previous:
            print("Event 'change' on sink #0", flush=True)
            previous = current
        time.sleep(0.02)
data = json.loads(path.read_text())
if args == ["-f", "json", "list", "sinks"]:
    print(json.dumps(data["sinks"]))
elif args == ["get-default-sink"]:
    print(data["default"])
else:
    command, name, *values = args
    sink = next(sink for sink in data["sinks"] if sink["name"] == name)
    if command == "set-sink-volume":
        sink["volume"]["front-left"]["value"] = int(values[0])
    elif command == "set-sink-mute":
        sink["mute"] = values[0] == "1"
    elif command == "set-default-sink":
        data["default"] = name
    else:
        raise SystemExit(2)
    temp = path.with_suffix(f".{os.getpid()}.tmp")
    temp.write_text(json.dumps(data))
    temp.replace(path)
"""
    )
    pactl.chmod(0o700)
    os.environ["CC_TEST_AUDIO"] = str(audio)


def run_isolated(binary: Path, root: Path, check_gtk: bool) -> None:
    os.environ.update(
        XDG_CONFIG_HOME=str(root / "config"),
        XDG_CACHE_HOME=str(root / "cache"),
        XDG_DATA_HOME=str(root / "data"),
        XDG_RUNTIME_DIR=str(root / "run"),
        CAELESTIA_CONNECT_BUS_NAME="org.caelestia.Connect.Daemon.RustTest",
        DBUS_SYSTEM_BUS_ADDRESS=os.environ["DBUS_SESSION_BUS_ADDRESS"],
        NO_PROXY="127.0.0.1",
        no_proxy="127.0.0.1",
    )
    (root / "run").mkdir(mode=0o700)
    clipboard_tools = root / "clipboard-tools"
    clipboard_tools.mkdir()
    xclip = clipboard_tools / "xclip"
    xclip.write_text(
        '#!/bin/sh\ncase "$3" in\n-o) cat "$CC_TEST_CLIPBOARD";;\n-i) cat >"$CC_TEST_CLIPBOARD";;\n*) exit 1;;\nesac\n'
    )
    xclip.chmod(0o700)
    qs = clipboard_tools / "qs"
    qs.write_text("#!/bin/sh\nexit 1\n")
    qs.chmod(0o700)
    os.environ.pop("WAYLAND_DISPLAY", None)
    os.environ.setdefault("DISPLAY", ":99")
    os.environ["PATH"] = str(clipboard_tools) + os.pathsep + os.environ["PATH"]
    os.environ["CC_TEST_CLIPBOARD"] = str(root / "clipboard.txt")
    setup_pactl(clipboard_tools, root / "audio.json")
    android_tools = root / "android" / "bin"
    setup_android_tools(android_tools)
    (android_tools.parent / "adb.json").write_text('{"devices":{}}')
    os.environ["PATH"] = str(android_tools) + os.pathsep + os.environ["PATH"]
    from caelestia_connect.config import CONFIG_DIR, IPC_SOCKET
    from caelestia_connect.cli import Client

    me, certificate, peer = trust_with_python(IPC_SOCKET, CONFIG_DIR)
    try:
        assert not IPC_SOCKET.exists()
        peer.incoming_capabilities.append("kdeconnect.systemvolume.request")
        peer.incoming_capabilities.append("kdeconnect.battery.request")
        peer.outgoing_capabilities.append("kdeconnect.systemvolume")
        peer.incoming_capabilities.append("kdeconnect.runcommand.request")
        peer.outgoing_capabilities.extend(["kdeconnect.runcommand", "kdeconnect.runcommand.output"])
        seed_messages(peer)
        device_config = CONFIG_DIR / "devices" / peer.device_id / "config.json"
        device_config.parent.mkdir(parents=True, exist_ok=True)
        device_settings = json.loads(device_config.read_text()) if device_config.exists() else {}
        device_settings.setdefault("pluginSettings", {})["phoneaudio"] = {"address": TEST_AUDIO_ADDRESS}
        device_settings["pluginSettings"]["pausemusic"] = {"conditionTalking": True, "actionMute": True}
        device_settings["pluginSettings"]["android"] = {
            "audio": False,
            "video_bit_rate": "16M",
            "max_size": 1440,
            "new_display": "1920x1080",
            "custom_args": '--window-x "80"',
            "facing": "front",
            "webcam": False,
        }
        device_settings["pluginSettings"]["runcommand"] = {
            "commands": {
                "fixture": {"name": "Existing command", "command": "printf 'saved\\n'; printf 'fixture error\\n' >&2"}
            }
        }
        device_config.write_text(json.dumps(device_settings))
        trust_file = CONFIG_DIR / "trusted_devices.json"
        trust = json.loads(trust_file.read_text())
        trust[peer.device_id].update(lastAddress="127.0.0.1")
        trust_file.write_text(json.dumps(trust))
        saved = files_at(CONFIG_DIR)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]

        command = [str(binary), "--bind", "127.0.0.1", "--discovery-port", str(port)]
        with phone(certificate) as newcomer:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stale:
                stale.bind(str(IPC_SOCKET))
            with daemon(command, IPC_SOCKET):
                assert IPC_SOCKET.stat().st_mode & 0o777 == 0o600
                duplicate = subprocess.run(command, capture_output=True, text=True, timeout=5)
                assert duplicate.returncode and "already owns" in duplicate.stderr
                competing = subprocess.run(
                    [*command, "--no-discovery"],
                    env={**os.environ, "CAELESTIA_CONNECT_BUS_NAME": "org.caelestia.Connect.Daemon.OtherTest"},
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                assert competing.returncode and "running daemon" in competing.stderr
                print("PASS Rust recovered a stale socket and preserved an active socket", flush=True)
                record = root / "tray-registration"
                with subprocess.Popen([sys.executable, __file__, "--tray-watcher", str(record)]) as watcher:
                    try:
                        asyncio.run(check_rust(me, certificate, peer, newcomer, port, check_gtk, record))
                    finally:
                        watcher.terminate()
                        watcher.wait(timeout=5)

            assert not IPC_SOCKET.exists()
            trust = CONFIG_DIR / "trusted_devices.json"
            assert trust.stat().st_mode & 0o777 == 0o600
            assert set(json.loads(trust.read_text())) == {newcomer.device_id}
            saved.pop(Path("trusted_devices.json"))
            after = files_at(CONFIG_DIR)
            after.pop(Path("trusted_devices.json"))
            global_settings = json.loads(after.pop(Path("settings.json")))
            assert global_settings["customDevices"] == []
            saved.pop(Path("settings.json"), None)
            stored_settings = json.loads(after.pop(Path("devices") / newcomer.device_id / "config.json"))
            assert stored_settings["pluginSettings"]["share"]["incomingPath"].endswith("/incoming/%1")
            assert stored_settings["plugins"]["share"] is False
            peer_config = Path("devices") / peer.device_id / "config.json"
            previous = json.loads(saved.pop(peer_config, b"{}"))
            previous.setdefault("plugins", {})["ping"] = True
            previous["plugins"]["mprisremote"] = True
            previous["plugins"]["mpriscontrol"] = True
            previous["plugins"]["systemvolume"] = True
            previous["plugins"]["runcommand"] = True
            previous["plugins"]["screensaver_inhibit"] = False
            previous["plugins"]["android"] = True
            previous["plugins"]["battery"] = True
            previous["plugins"]["sms"] = True
            previous["plugins"]["contacts"] = True
            previous.setdefault("pluginSettings", {})["mprisremote"] = {"hideIdle": False, "exportMpris": False}
            previous["pluginSettings"]["notifications"] = {"showPopups": False}
            previous["pluginSettings"]["pausemusic"]["actionResume"] = True
            previous["pluginSettings"]["android"]["facing"] = "back"
            assert json.loads(after.pop(peer_config)) == previous
            assert saved == after
            disconnect(newcomer)
            python = [sys.executable, "-m", "caelestia_connect.daemon", "--no-discovery", "-q"]
            with daemon(python, IPC_SOCKET):
                client = Client()
                with client.sock:
                    state = client.call("state")
                    assert [d["id"] for d in state["devices"] if d["paired"]] == [newcomer.device_id]
                    newcomer.connect_direct(state["self"]["tcpPort"], me["id"])
                    deadline = time.monotonic() + 5
                    while not any(d["reachable"] for d in client.call("state")["devices"]):
                        assert time.monotonic() < deadline, "Python did not accept the new connection"
                        time.sleep(0.05)
                    assert client.call("device.call", device=newcomer.device_id, plugin="ping", action="ping")
                    newcomer.wait_for("kdeconnect.ping")
            print("PASS Python reopened Rust's saved pairing, with all other configuration preserved", flush=True)
        fresh = root / "fresh-profile"
        fresh.mkdir(mode=0o700)
        (fresh / "settings.json").write_text('{"futureOption": ["unchanged"]}')
        fresh_command = [
            str(binary),
            "--bind",
            "127.0.0.1",
            "--no-discovery",
            "--config-dir",
            str(fresh),
            "--default-name",
            "Default Rust name",
        ]
        downloads = root / "saved-global-downloads"
        with daemon(fresh_command, IPC_SOCKET):
            client = Client()
            with client.sock:
                first = client.call("state")
                assert first["devices"] == [] and first["self"]["id"] != me["id"]
                assert first["self"]["name"] == "Default Rust name"
                assert client.call("settings.setName", name="Renamed Rust device") == "Renamed Rust device"
                assert client.call("settings.setIncomingPath", path=str(downloads))
                assert client.call("devices.add", address="192.0.2.10")
                assert client.call("devices.add", address="192.0.2.10")
        assert fresh.stat().st_mode & 0o777 == 0o700
        assert (fresh / "privateKey.pem").stat().st_mode & 0o777 == 0o600
        with daemon(fresh_command, IPC_SOCKET):
            client = Client()
            with client.sock:
                restarted = client.call("state")
                restored = restarted["self"]
                assert restored["id"] == first["self"]["id"]
                assert restored["fingerprint"] == first["self"]["fingerprint"]
                assert restored["name"] == "Renamed Rust device"
                assert restarted["settings"]["incomingPath"] == str(downloads)
                assert restarted["settings"]["customDevices"] == ["192.0.2.10"]
        assert json.loads((fresh / "settings.json").read_text())["futureOption"] == ["unchanged"]
        print(
            "PASS Rust reused its identity, device name, default download path and manual IP after restart", flush=True
        )
    finally:
        disconnect(peer)
        shutil.rmtree(peer.tmp)


def run_gtk(device_id: str, action: str) -> int:
    import gi

    gi.require_version("Adw", "1")
    gi.require_version("Gtk", "4.0")
    from gi.events import GLibEventLoopPolicy
    from gi.repository import Adw, GLib, Gtk

    from caelestia_connect.ui.app import ConnectApp

    asyncio.set_event_loop_policy(GLibEventLoopPolicy())
    app = ConnectApp()
    clicked = False
    android_requested = False
    camera_confirmed = False
    deadline = time.monotonic() + (10 if action in {"mirror", "camera"} else 5)

    def action_rows(widget):
        if isinstance(widget, Adw.ActionRow):
            yield widget
        child = widget.get_first_child()
        while child:
            yield from action_rows(child)
            child = child.get_next_sibling()

    def activate_action():
        nonlocal clicked, android_requested, camera_confirmed
        if app.window:
            page = app.window.device_page
            if action == "preferences" and page.device.get("id") == device_id:
                app.lookup_action("preferences").activate(None)
                assert type(app.window.get_visible_dialog()).__name__ == "PreferencesDialog"
                clicked = True
                GLib.timeout_add(100, app.quit)
                return GLib.SOURCE_REMOVE
            if action == "media":
                card = page.overview.media_card
                button = card.play if card.title.get_label() == "Sample Track" and card.visualiser.pixbuf else None
            elif action == "messages":
                if page.device.get("id") == device_id and page.view_stack.get_visible_child_name() == "messages":
                    row = page.messages.conv_list.get_row_at_index(0)
                    if row and row.conv["title"] == "Fixture Contact" and row.get_mapped():
                        page.messages.conv_list.select_row(row)
                        assert page.messages.thread_title.get_label() == "Fixture Contact"
                        if page.messages.msg_box.get_first_child():
                            clicked = True
                            GLib.timeout_add(100, app.quit)
                            return GLib.SOURCE_REMOVE
                button = None
            elif action == "notifications":
                if page.device.get("id") == device_id and page.overview.notifs.get_mapped():
                    listing = page.overview.notifs.get_last_child()
                    if isinstance(listing, Gtk.ListBox) and listing.get_first_child():
                        clicked = True
                        GLib.timeout_add(100, app.quit)
                        return GLib.SOURCE_REMOVE
                button = None
            elif action == "battery":
                row = app.window.rows.get(device_id)
                if (
                    page.device.get("id") == device_id
                    and row is not None
                    and row.status.get_label() == "Connected · 76% ⚡"
                    and any(
                        child.get_child().get_label() == "76% · charging" and child.get_mapped()
                        for child in page.overview.pills
                    )
                ):
                    clicked = True
                    GLib.timeout_add(100, app.quit)
                    return GLib.SOURCE_REMOVE
                button = None
            elif action == "call":
                call = page.device.get("state", {}).get("telephony", {}).get("call", {})
                if (
                    page.device.get("id") == device_id
                    and call
                    and any(
                        child.get_child().get_label() == "Call: Fixture Contact" and child.get_mapped()
                        for child in page.overview.pills
                    )
                ):
                    clicked = True
                    GLib.timeout_add(100, app.quit)
                    return GLib.SOURCE_REMOVE
                button = None
            elif action == "audio-settings":
                if page.device.get("id") == device_id and page.settings.get_mapped():
                    rows = {row.get_title(): row for row in action_rows(page.settings)}
                    assert {
                        "System volume",
                        "Remote system volume",
                        "Pause media during calls",
                        "Prevent screensaver",
                        "Run commands",
                        "Remote commands",
                        "Existing command",
                    } <= rows.keys()
                    editor_row = rows["Existing command"]
                    assert not editor_row.get_use_markup() and ">&2" in editor_row.get_subtitle()
                    clicked = True
                    GLib.timeout_add(100, app.quit)
                    return GLib.SOURCE_REMOVE
                button = None
            elif action == "commands":
                group = page.overview.commands.get_last_child()
                item = group.get_first_child() if isinstance(group, Gtk.FlowBox) else None
                button = item.get_child() if item else None
            elif action in {"mirror", "camera"}:
                if page.device.get("id") == device_id:
                    android = page.device.get("state", {}).get("android", {})
                    if android.get("mirroring" if action == "mirror" else "preview"):
                        clicked = True
                        app.quit()
                        return GLib.SOURCE_REMOVE
                    button = page.overview.action_buttons.get(action)
                    if not android_requested and button and button.get_mapped() and button.is_sensitive():
                        button.emit("clicked")
                        android_requested = True
                    dialog = app.window.get_visible_dialog()
                    if action == "camera" and isinstance(dialog, Adw.AlertDialog) and not camera_confirmed:
                        assert dialog.get_heading() == "Phone camera"
                        assert dialog.get_response_enabled("start")
                        rows = list(action_rows(dialog.get_extra_child()))
                        lens = next(row for row in rows if isinstance(row, Adw.ComboRow))
                        mirror = next(row for row in rows if isinstance(row, Adw.SwitchRow))
                        assert mirror.get_visible() and not mirror.get_active()
                        lens.set_selected(1)
                        assert not mirror.get_visible()
                        lens.set_selected(0)
                        assert mirror.get_visible()
                        mirror.set_active(True)
                        dialog.emit("response", "start")
                        camera_confirmed = True
                button = None
            elif action in {"ping", "text", "preferences"}:
                button = page.overview.action_buttons.get("ping" if action == "ping" else "send-text")
            else:
                button = page.pair_view.buttons.get_first_child()
                while button and button.get_child().get_last_child().get_label() != action.capitalize():
                    button = button.get_next_sibling()
            if page.device.get("id") == device_id and button and button.get_mapped() and button.is_sensitive():
                if action == "accept":
                    assert len(page.pair_view.key.get_text()) == 8
                button.emit("clicked")
                if action == "text":
                    assert app.rust_preview and "send-files" in page.overview.action_buttons
                    dialog = app.window.get_visible_dialog()
                    view = dialog.get_extra_child().get_child()
                    if isinstance(view, Gtk.Viewport):
                        view = view.get_child()
                    view.get_buffer().set_text("Hello from GTK\nOlá")
                    assert dialog.get_response_enabled("send")
                    dialog.emit("response", "send")
                clicked = True
                GLib.timeout_add(500, app.quit)
                return GLib.SOURCE_REMOVE
        if time.monotonic() >= deadline:
            if action in {"mirror", "camera"} and app.window:
                page = app.window.device_page
                buttons = page.overview.action_buttons
                print(
                    f"GTK {action} timed out (clicked={android_requested}, "
                    f"button={action in buttons}, selected={page.device.get('id') == device_id}, "
                    f"android={page.device.get('state', {}).get('android')})",
                    file=sys.stderr,
                    flush=True,
                )
            app.quit()
            return GLib.SOURCE_REMOVE
        return GLib.SOURCE_CONTINUE

    GLib.timeout_add(50, activate_action)
    args = ["caelestia-connect", "--device", device_id]
    if action == "messages":
        args.extend(["--page", "messages"])
    elif action == "notifications":
        args.extend(["--page", "notifications"])
    elif action == "audio-settings":
        args.extend(["--page", "settings"])
    result = app.run(args)
    return result or int(not clicked)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "rust-daemon/target/debug/caelestia-connectd-rust")
    parser.add_argument("--gtk", action="store_true", help="also exercise the GTK client")
    parser.add_argument("--gtk-device", help=argparse.SUPPRESS)
    parser.add_argument(
        "--gtk-action",
        choices=(
            "ping",
            "pair",
            "accept",
            "text",
            "preferences",
            "media",
            "messages",
            "notifications",
            "battery",
            "call",
            "audio-settings",
            "commands",
            "mirror",
            "camera",
        ),
        default="ping",
        help=argparse.SUPPRESS,
    )
    parser.add_argument("--isolated", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--mpris-fixture", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--cover-url", help=argparse.SUPPRESS)
    parser.add_argument("--tray-watcher", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.gtk_device:
        return run_gtk(args.gtk_device, args.gtk_action)
    if args.mpris_fixture:
        run_mpris_fixture(args.mpris_fixture, args.cover_url)
        return 0
    if args.tray_watcher:
        run_tray_watcher(args.tray_watcher)
        return 0
    binary = args.binary.resolve(strict=True)
    if args.isolated:
        run_isolated(binary, args.isolated, args.gtk)
        return 0
    with tempfile.TemporaryDirectory(prefix="cc-rust-", dir="/tmp") as directory:
        root = Path(directory)
        config = root / "dbus.conf"
        config.write_text(BUS_CONFIG)
        command = [
            "dbus-run-session",
            f"--config-file={config}",
            "--",
            sys.executable,
            __file__,
            "--binary",
            str(binary),
            "--isolated",
            str(root),
        ]
        if args.gtk:
            command.append("--gtk")
        return subprocess.run(command, check=False).returncode


if __name__ == "__main__":
    sys.exit(main())
