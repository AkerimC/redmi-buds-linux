#!/usr/bin/env python3
"""
redmi_buds.py - control Xiaomi/Redmi buds from a Linux desktop over RFCOMM.

Target: Redmi Buds 6 Play (VID 0x2717, PID 0x509b), but the same RCSP derivative
is used by other Xiaomi/JieLi earphones.

Protocol summary (see PROTOCOL.md for the full write-up):

  Transport   Bluetooth Classic RFCOMM (SDP UUID 0000fd2d-… and plain SPP).
  Frame       FE DC BA | flags | opcode | len(u16 BE) | payload | EF
              flags: bit7 command(1)/response(0), bit6 expects response,
                     low 3 bits target app id (1 = MIUI BT, 4 = com.mi.earphone).
  Auth        ACK AUTH_CHECK (0x50) challenge/response + AUTH_SEND_CALC_RESULT (0x51).
              The 16-byte response is computed by xiaomi_auth.py\n              (pure Python, no vendor binary or emulator needed).
  Config      SET_DEVICE_CONFIG (0xF2) / GET_DEVICE_CONFIG (0xF3) /
              NOTIFY_DEVICE_CONFIG (0xF4) with TLV parameters.

Usage:
  ./redmi_buds.py info
  ./redmi_buds.py battery
  ./redmi_buds.py eq get
  ./redmi_buds.py eq set 3
  ./redmi_buds.py get 7 11
  ./redmi_buds.py set 7 03          # raw config id + hex value
  ./redmi_buds.py find
"""
from __future__ import annotations

import argparse
import os
import shlex
import socket
import sys
import time
from dataclasses import dataclass

from xiaomi_auth import ADDR6_PLACEHOLDER as ADDR6
from xiaomi_auth import transform as xiaomi_transform

MAGIC = b"\xfe\xdc\xba"
TRAILER = 0xEF

# Target app ids (BasePacket.ID_PKG_MAP in the SDK).
APP_MIUI = 1        # com.xiaomi.bluetooth
APP_EARPHONE = 4    # com.mi.earphone

# UUIDs the app probes, in order.
UUID_MIUI = "0000fd2d-0000-1000-8000-00805f9b34fb"
UUID_SPP = "00001101-0000-1000-8000-00805f9b34fb"

def find_paired_buds() -> str | None:
    """First paired device whose name looks like Xiaomi/Redmi buds."""
    import subprocess
    try:
        out = subprocess.run(["bluetoothctl", "devices", "Paired"],
                             capture_output=True, text=True).stdout
    except OSError:
        return None
    for line in out.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[0] == "Device" and (
                "buds" in parts[2].lower() or "airdots" in parts[2].lower()):
            return parts[1]
    return None

OPCODES = {
    0x01: "DATA", 0x02: "GET_TARGET_INFO", 0x03: "REBOOT_DEVICE",
    0x04: "NOTIFY_DEVICE_APP_INFO", 0x06: "DISCONNECT", 0x09: "GET_DEVICE_RUN_INFO",
    0x42: "FIND_DEVICE", 0x50: "AUTH_CHECK", 0x51: "AUTH_SEND_CALC_RESULT",
    0xF2: "SET_DEVICE_CONFIG", 0xF3: "GET_DEVICE_CONFIG", 0xF4: "NOTIFY_DEVICE_CONFIG",
}

STATUS = {0: "ok", 1: "fail", 2: "unknown_cmd", 3: "busy", 4: "no_resource",
          5: "crc", 7: "param_error", 9: "not_supported"}

CONFIG_IDS = {
    1: "?" , 2: "gesture_click_set", 3: "auto_answer", 4: "?", 5: "fit_test",
    7: "eq_preset", 10: "longpress_noise_cycle", 11: "anc_state", 12: "?",
    13: "?", 15: "?", 29: "spatial_audio", 30: "?", 35: "?",
    36: "virtual_surround", 37: "adaptive_anc", 39: "serial_number", 40: "time",
    41: "adaptive_sense", 47: "low_latency", 54: "scene_rendering", 55: "custom_eq",
    58: "notify_sound", 59: "personalized_noise_reduction", 60: "ear_canal_fit",
    62: "detection_exists", 72: "adaptive_volume", 74: "bluetooth_info",
    76: "app_country", 83: "click_set_new", 93: "restricted",
    103: "commuting_immerse", 114: "case_sound_list", 115: "case_sound_setting",
    120: "translate_settings", 161: "notification_volume",
}

# EQ preset names are supplied by the device (the app shows its own list), so
# only the numeric ids are documented here. Observed range on the Buds 6 Play: 0..7.
EQ_PRESET_RANGE = range(0, 8)

TARGET_ATTRS = {0: "name", 1: "version", 2: "quantity", 3: "vid_pid", 4: "-",
                5: "mandatory_upgrade", 6: "uboot_version", 7: "multi_battery",
                8: "codec_type", 9: "power_mode", 10: "function_key",
                11: "hot_word", 12: "charset", 13: "color",
                14: "spatial_audio_type", 15: "virtual_surround_type"}


class BudsError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# Authentication
#
# The 16-byte response is computed by our own implementation (xiaomi_auth.py),
# with no vendor binary, emulator or cross-compiled helper involved.
# --------------------------------------------------------------------------
class AuthTransform:
    """Computes the auth response for a challenge.

    The buds and the phone both apply the same transform with fixed constants;
    only the 16-byte challenge varies. See xiaomi_auth.py for how it works.
    """

    def respond(self, challenge: bytes) -> bytes:
        if len(challenge) != 16:
            raise BudsError(f"challenge must be 16 bytes, got {len(challenge)}")
        return xiaomi_transform(ADDR6, challenge)


# --------------------------------------------------------------------------
# Framing
# --------------------------------------------------------------------------
@dataclass
class Frame:
    flags: int
    opcode: int
    payload: bytes

    @property
    def is_command(self) -> bool:
        return bool(self.flags & 0x80)

    @property
    def target_app(self) -> int:
        return self.flags & 0x07

    @property
    def status(self) -> int:
        return self.payload[0] if self.payload else -1

    @property
    def sn(self) -> int:
        if self.is_command:
            return self.payload[0] if self.payload else -1
        return self.payload[1] if len(self.payload) > 1 else -1

    def data(self) -> bytes:
        if self.is_command:
            n = 2 if self.opcode == 0x01 else 1
            return self.payload[n:]
        n = 1 if self.opcode == 0x01 else 2
        return self.payload[n:]

    def __str__(self) -> str:
        direction = "cmd" if self.is_command else "rsp"
        name = OPCODES.get(self.opcode, f"0x{self.opcode:02x}")
        st = "" if self.is_command else f" status={STATUS.get(self.status, self.status)}"
        return (f"[{direction} {name} app={self.target_app} sn=0x{self.sn & 0xff:02x}{st}] "
                f"{self.payload.hex()}")

    @staticmethod
    def encode(flags: int, opcode: int, payload: bytes) -> bytes:
        return (MAGIC + bytes([flags, opcode]) + len(payload).to_bytes(2, "big")
                + payload + bytes([TRAILER]))


def extract_frames(buf: bytes) -> tuple[list[Frame], bytes]:
    """Split whatever is buffered into complete frames; return (frames, remainder)."""
    frames, pos = [], 0
    while True:
        start = buf.find(MAGIC, pos)
        if start < 0:
            # keep a partial magic tail, drop the rest
            return frames, buf[max(pos, len(buf) - 2):]
        if start + 7 > len(buf):
            return frames, buf[start:]
        plen = int.from_bytes(buf[start + 5:start + 7], "big")
        end = start + 7 + plen
        if end >= len(buf):
            return frames, buf[start:]
        if buf[end] != TRAILER:
            pos = start + 1
            continue
        frames.append(Frame(buf[start + 3], buf[start + 4], buf[start + 7:end]))
        pos = end + 1


def parse_tlvs_u16(data: bytes) -> list[tuple[int, bytes]]:
    """Config TLVs: len(u8) | type(u16 BE) | value(len-2)."""
    out, i = [], 0
    while i + 3 <= len(data):
        ln = data[i]
        if ln < 2 or i + 1 + ln > len(data):
            break
        out.append((int.from_bytes(data[i + 1:i + 3], "big"), data[i + 3:i + 1 + ln]))
        i += 1 + ln
    return out


def parse_tlvs_u8(data: bytes) -> list[tuple[int, bytes]]:
    """Target/run info attributes: len(u8) | type(u8) | value(len-1)."""
    out, i = [], 0
    while i + 2 <= len(data):
        ln = data[i]
        if ln < 1 or i + 1 + ln > len(data):
            break
        out.append((data[i + 1], data[i + 2:i + 1 + ln]))
        i += 1 + ln
    return out


def tlv_u16(type_id: int, value: bytes) -> bytes:
    return bytes([2 + len(value)]) + type_id.to_bytes(2, "big") + value


def decode_battery(values: bytes) -> list[tuple[int, bool]]:
    """multi-battery: percent = v & 0x7f, charging = v & 0x80; 0xff = unknown."""
    out = []
    for v in values:
        if v == 0xFF:
            out.append((-1, False))
        else:
            out.append((v & 0x7F, bool(v & 0x80)))
    return out


# --------------------------------------------------------------------------
# Connection
# --------------------------------------------------------------------------
class Buds:
    def __init__(self, mac: str, channel: int | None = None,
                 verbose: bool = False):
        self.mac = mac
        self.channel = channel
        self.verbose = verbose
        self.auth = AuthTransform()
        self.sock: socket.socket | None = None
        self.buf = b""
        self.pending: list[Frame] = []
        self.sn = 0

    # -- plumbing ---------------------------------------------------------
    def log(self, msg: str) -> None:
        if self.verbose:
            print(f"  {msg}", file=sys.stderr)

    def _next_sn(self) -> int:
        self.sn = (self.sn + 1) & 0xFF
        return self.sn

    def candidate_channels(self) -> list[int]:
        """Channels to try, most likely first.

        The app probes SDP for the MIUI UUID (0000fd2d-…) and plain SPP. The
        captured phone/buds session carried RCSP on RFCOMM channel 6, so that
        is the first blind guess when SDP is unavailable.
        """
        try:
            import bluetooth  # pybluez
        except ImportError:
            bluetooth = None
        found: list[int] = []
        if bluetooth is not None:
            for uuid in (UUID_MIUI, UUID_SPP):
                try:
                    services = bluetooth.find_service(address=self.mac, uuid=uuid)
                except OSError:
                    services = []
                for svc in services:
                    port = svc.get("port")
                    if port and int(port) not in found:
                        found.append(int(port))
                        self.log(f"SDP {uuid} -> channel {port}")
        for guess in (6, 5, 4, 3, 2, 1):
            if guess not in found:
                found.append(guess)
        return found

    def connect(self, timeout: float = 12.0) -> None:
        candidates = [self.channel] if self.channel else self.candidate_channels()
        last_err: Exception | None = None
        for channel in candidates:
            sock = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM,
                                 socket.BTPROTO_RFCOMM)
            sock.settimeout(timeout)
            self.log(f"connecting RFCOMM {self.mac} channel {channel}")
            try:
                sock.connect((self.mac, channel))
            except OSError as exc:
                last_err = exc
                sock.close()
                if self.channel:
                    break
                continue
            sock.settimeout(8.0)
            self.sock = sock
            self.channel = channel
            self.buf = b""
            self.pending = []
            self.log(f"connected on channel {channel}")
            return
        raise last_err or BudsError("could not connect on any RFCOMM channel")

    def close(self) -> None:
        if self.sock:
            try:
                self.sock.close()
            finally:
                self.sock = None

    def __enter__(self):
        self.connect()
        return self

    def __exit__(self, *exc):
        self.close()

    def send_frame(self, flags: int, opcode: int, payload: bytes) -> Frame:
        frame = Frame(flags, opcode, payload)
        self.log(f"-> {frame}")
        assert self.sock is not None
        self.sock.sendall(Frame.encode(flags, opcode, payload))
        return frame

    def send_command(self, opcode: int, params: bytes, sn: int | None = None,
                     app: int = APP_EARPHONE) -> int:
        if sn is None:
            sn = self._next_sn()
        payload = bytes([sn]) + params
        self.send_frame(0x80 | 0x40 | app, opcode, payload)
        return sn

    def recv_frame(self, timeout: float | None = None) -> Frame | None:
        """Return the next complete frame, or None on timeout."""
        if self.pending:
            frame = self.pending.pop(0)
            self.log(f"<- {frame}")
            return frame
        assert self.sock is not None
        deadline = None if timeout is None else time.time() + timeout
        while True:
            frames, self.buf = extract_frames(self.buf)
            if frames:
                self.pending.extend(frames[1:])
                frame = frames[0]
                self.log(f"<- {frame}")
                return frame
            if deadline is not None and time.time() >= deadline:
                return None
            self.sock.settimeout(0.5 if deadline is None
                                 else max(0.05, deadline - time.time()))
            try:
                chunk = self.sock.recv(4096)
            except socket.timeout:
                if deadline is None:
                    continue
                return None
            if not chunk:
                raise BudsError("connection closed by device")
            self.buf += chunk

    def _handle_unsolicited(self, frame: Frame) -> bool:
        """Answer a challenge initiated by the buds. Returns True if consumed."""
        if frame.is_command and frame.opcode == 0x50 and len(frame.payload) >= 2:
            challenge = frame.payload[2:18]
            if len(challenge) == 16:
                resp = self.auth.respond(challenge)
                self.send_frame(0x00 | APP_MIUI, 0x50,
                                bytes([0x00, frame.payload[0], 0x01]) + resp)
                self.log("answered device-initiated challenge")
                return True
        return False

    def wait_response(self, sn: int, opcode: int | None = None,
                      timeout: float = 8.0) -> Frame:
        deadline = time.time() + timeout
        while time.time() < deadline:
            frame = self.recv_frame(timeout=max(0.1, deadline - time.time()))
            if frame is None:
                break
            if self._handle_unsolicited(frame):
                continue
            if frame.is_command:
                continue
            if frame.sn == sn and (opcode is None or frame.opcode == opcode):
                return frame
        raise BudsError(f"no response to sn=0x{sn:02x} within {timeout}s")

    def request(self, opcode: int, params: bytes, app: int = APP_EARPHONE,
                timeout: float = 8.0) -> Frame:
        sn = self.send_command(opcode, params, app=app)
        rsp = self.wait_response(sn, opcode, timeout=timeout)
        if rsp.status != 0:
            raise BudsError(f"{OPCODES.get(opcode, hex(opcode))} failed: "
                            f"{STATUS.get(rsp.status, rsp.status)}")
        return rsp

    # -- auth -------------------------------------------------------------
    def authenticate(self) -> None:
        """AUTH_CHECK challenge/response, then AUTH_SEND_CALC_RESULT.

        Mirrors the captured app behaviour: the phone sends a random challenge
        under AUTH_CHECK; the buds reply with the same transform of it; the
        phone verifies and confirms with AUTH_SEND_CALC_RESULT(pairResult=0).
        """
        challenge = os.urandom(16)
        self.log(f"challenge {challenge.hex()}")
        expected = self.auth.respond(challenge)

        sn = self.send_command(0x50, bytes([0x01]) + challenge, app=APP_MIUI)
        rsp = self.wait_response(sn, 0x50, timeout=8.0)
        if rsp.status != 0:
            raise BudsError(f"AUTH_CHECK rejected: {STATUS.get(rsp.status, rsp.status)}")
        got = rsp.data()[1:]  # skip version byte
        if got != expected:
            raise BudsError(
                "auth mismatch\n  device    " + got.hex() +
                "\n  oracle    " + expected.hex())
        self.log("challenge accepted by device")

        sn = self.send_command(0x51, bytes([0x01, 0x00]), app=APP_MIUI)
        rsp = self.wait_response(sn, 0x51, timeout=8.0)
        if rsp.status != 0:
            raise BudsError(f"AUTH_SEND_CALC_RESULT rejected: "
                            f"{STATUS.get(rsp.status, rsp.status)}")

    # -- high level commands ----------------------------------------------
    def get_target_info(self) -> dict[str, object]:
        rsp = self.request(0x02, b"\xff\xff\xff\xff")
        attrs: dict[str, object] = {}
        for typ, val in parse_tlvs_u8(rsp.data()):
            name = TARGET_ATTRS.get(typ, f"attr{typ}")
            if typ == 0:
                attrs["name"] = val.decode("utf-8", "replace")
            elif typ == 2:
                attrs["quantity"] = val[0] if val else -1
            elif typ == 3 and len(val) >= 4:
                attrs["vid"] = int.from_bytes(val[:2], "big")
                attrs["pid"] = int.from_bytes(val[2:4], "big")
            elif typ == 7:
                attrs["multi_battery"] = decode_battery(val)
            else:
                attrs.setdefault("raw", {})[name] = val.hex()  # type: ignore[index]
        return attrs

    def get_run_info(self) -> dict[str, object]:
        rsp = self.request(0x09, b"\xff\xff\xff\xff")
        return {"raw": rsp.data().hex()}

    def get_config(self, ids: list[int]) -> dict[int, bytes]:
        params = b"".join(i.to_bytes(2, "big") for i in ids)
        rsp = self.request(0xF3, params)
        return {typ: val for typ, val in parse_tlvs_u16(rsp.data())}

    def set_config(self, items: list[tuple[int, bytes]]) -> None:
        params = b"".join(tlv_u16(i, v) for i, v in items)
        self.request(0xF2, params)

    def find_device(self, ring: bool = True, device_type: int = 3) -> None:
        """Ring the buds (and case). Device config id 9: [state, device_type]."""
        self.set_config([(9, bytes([1 if ring else 0, device_type]))])

    # -- convenience ------------------------------------------------------
    def battery(self) -> list[tuple[int, bool]]:
        info = self.get_target_info()
        mb = info.get("multi_battery")
        if isinstance(mb, list) and mb:
            return mb
        q = info.get("quantity")
        if isinstance(q, int):
            return [(q, False)]
        return []

    def get_eq(self) -> int | None:
        cfg = self.get_config([7])
        val = cfg.get(7)
        return val[0] if val else None

    def set_eq(self, preset: int) -> None:
        if not 0 <= preset <= 7:
            raise BudsError("EQ preset must be 0..7")
        self.set_config([(7, bytes([preset]))])


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------
def fmt_battery(batts: list[tuple[int, bool]]) -> str:
    names = ["L", "R", "case"]
    parts = []
    for i, (pct, charging) in enumerate(batts):
        label = names[i] if i < len(names) else f"#{i}"
        if pct < 0:
            parts.append(f"{label}=?")
        else:
            parts.append(f"{label}={pct}%{'⚡' if charging else ''}")
    return "  ".join(parts)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Control Redmi/Xiaomi buds over RFCOMM.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Usage:")[-1])
    ap.add_argument("--mac", default=None, help="buds MAC address (default: first paired *Buds*)")
    ap.add_argument("--channel", type=int, default=None, help="RFCOMM channel")
    ap.add_argument("--no-auth", action="store_true", help="skip the auth handshake")
    ap.add_argument("-v", "--verbose", action="store_true")
    sub = ap.add_subparsers(dest="cmd", required=True)

    sub.add_parser("info", help="device name, ids and battery")
    sub.add_parser("battery", help="battery levels")
    p_find = sub.add_parser("find", help="make the buds ring")
    p_find.add_argument("--stop", action="store_true", help="stop ringing")
    p_find.add_argument("--device", type=int, default=3,
                        help="1=left 2=right 3=both (default 3)")

    p_eq = sub.add_parser("eq", help="get/set the EQ preset (config id 7)")
    p_eq.add_argument("action", choices=["get", "set"])
    p_eq.add_argument("value", nargs="?", type=lambda s: int(s, 0))

    p_get = sub.add_parser("get", help="read one or more device config ids")
    p_get.add_argument("ids", nargs="+", type=lambda s: int(s, 0))

    p_set = sub.add_parser("set", help="write a raw config id/value hex pair")
    p_set.add_argument("id", type=lambda s: int(s, 0))
    p_set.add_argument("value", help="hex, e.g. 03 or 0100")

    args = ap.parse_args(argv)

    try:
        args.mac = args.mac or find_paired_buds()
        if not args.mac:
            print("error: no paired buds found; pass --mac", file=sys.stderr)
            return 1
        buds = Buds(args.mac, args.channel, args.verbose)
        buds.connect()
    except OSError as exc:
        print(f"error: cannot connect to {args.mac}: {exc}", file=sys.stderr)
        print("hint: make sure the buds are powered on, in range and paired; "
              "`bluetoothctl connect " + args.mac + "` first.", file=sys.stderr)
        return 2

    try:
        if not args.no_auth:
            buds.authenticate()
            print("authenticated", file=sys.stderr)

        if args.cmd == "info":
            info = buds.get_target_info()
            print(f"name      {info.get('name')}")
            if "vid" in info:
                print(f"vendor    VID 0x{info['vid']:04x}  PID 0x{info['pid']:04x}")
            if "quantity" in info:
                print(f"quantity  {info['quantity']}")
            print(f"battery   {fmt_battery(buds.battery())}")
            for k, v in sorted(info.get("raw", {}).items()):  # type: ignore[union-attr]
                print(f"  {k:22} {v}")

        elif args.cmd == "battery":
            print(fmt_battery(buds.battery()))

        elif args.cmd == "find":
            buds.find_device(ring=not args.stop, device_type=args.device)
            print("stopped" if args.stop else "ringing")

        elif args.cmd == "eq":
            if args.action == "get":
                val = buds.get_eq()
                print(f"eq preset {val}")
            else:
                if args.value is None:
                    print("error: eq set needs a preset 0..7", file=sys.stderr)
                    return 1
                buds.set_eq(args.value)
                print(f"eq preset set to {args.value}")

        elif args.cmd == "get":
            cfg = buds.get_config(args.ids)
            for cid in args.ids:
                val = cfg.get(cid)
                label = CONFIG_IDS.get(cid, "?")
                print(f"{cid:4} {label:24} {val.hex() if val is not None else '(absent)'}")

        elif args.cmd == "set":
            buds.set_config([(args.id, bytes.fromhex(args.value))])
            print(f"config {args.id} set to {args.value}")

        else:  # pragma: no cover - argparse enforces this
            raise BudsError(f"unknown command {args.cmd}")
    except BudsError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    finally:
        buds.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
