#!/usr/bin/env python3
"""
xiaomi_auth.py - native Python implementation of the Xiaomi/JieLi auth transform.

This replaces the black-box qemu/vendor-library oracle that the client originally
used: no proprietary binary, no cross-compiled harness, no emulator involved at
runtime. Pure standard library.

Recovered from a disassembly of the app's libxm_bluetooth.so (aarch64). The vendor library
exports three relevant entry points:

    function_E1test(addr6, challenge16, key16, out16)   -> throws its result away
    function_E21(addr6, challenge16, out16)             -> the real transform
    function_xiaomi(addr6, challenge16, key16, out16)   -> calls E1test then E21

Because E21 overwrites `out` with an address-derived pattern before mixing, the
E1test result never reaches the output: the whole transform is exactly E21.
(Verified experimentally: E21 returns the identical 16 bytes for any incoming
buffer contents, and equals the full function_xiaomi output.)

Structure:

    transform(addr6, challenge16):
        out   = addr6 || addr6 || addr6[:4]
        tmp   = challenge16 with tmp[15] ^= 0x06
        sched = _expand_key(tmp)                 # 272 bytes
        return _mix(out, sched, flag=1)

The caller (the earphone app) always passes the same fixed values:

    addr6 = 11 22 33 33 22 11          (placeholder, NOT the device MAC)
    key   = 06 77 5f 87 91 8d d4 23 00 5d f1 d8 cf 0c 14 2b
"""

from __future__ import annotations

# 256-byte tables lifted from .rodata (0x970, 0xa70, 0xb70).
_TAB_EXPAND = bytes.fromhex(
    "64ac285ac9b337c50a10b7a3bab197463d05dc666ef69af80d589567c6aaabec"
    "a0689b96d4ebbf434936e96a89d8c38a946399bc7bbec122bb5c71d51f92575d"
    "8f44411d51e64017fbfd193234b8612aca236fda39f7a2017fd631e7de8004dd"
    "2c5982afa8e00fcda1123e30d11cd03a33722e4f9002130675ce87c2efb2ad7d"
    "3815e1529f7a6c2f27c4e281a9cf8dc0d7dfff6076148c5e5509e408c74220fc"
    "d25091d94c629ee8b9a6f91a00210bfa359c4e4b6948cb0ec8a45bea8407b418"
    "f4ae6bdba7cc3f8b4a0c3c25e5544d4583ed11f0b05393f27426b59d6d7cf32d"
    "f156247e471b86bd708e1e3b731603b6ac285ac9b337c50a10b7a3bab1974688")

_TAB_A = bytes.fromhex(
    "012de293be4515ae780387a4b838cf3f08670994eb26a86bbd18341bbbbf72f7"
    "4035489c512f3b55e3c09fd8d3f38db1ffa73edc8677d7a611fbf4ba92916483"
    "f133efda2cb5b22b88d199cb8c841d14819771ca5fa38b573c82c4525c1ce8a0"
    "04b4854af61354b6df0c1a8edee039fc209b244ea9989eabf260d06ceafac7d9"
    "00d41f6e43bcec5389fe7a5d49c932c2f99af86d16db599644e9cde646428f0a"
    "c1ccb965b0d2c6ac1e4162292e0e7450025ac3257b8a2a5bf0060d476f709d7e"
    "10ce1227d54c4fd679306836757de4ed806a9037a25e76aac57f3dafa5e51961"
    "fd4d7cb70beead4b22f5e7732321c805e166ddb3586963560fa1319517073a28")

_TAB_B = bytes.fromhex(
    "8000b00960efb9fd10129fe469baadf8c038c2654f0694fc19de6a1b5d4ea882"
    "70ede8ec72b315c3ffabb6474401ac25c9fa8e411a21cbd30d6efe2658da320f"
    "20a99d8498059cbb228c63e7c5e173c6af245b876627f757f496b1b75c8bd554"
    "79dfaaf63ea3f111caf5d1177b9383bcbd521eebaeccd63508c88ab4e2cdbfd9"
    "d050593f4d62340a4888b5564c2e6b9ed23d3c0313fb9751754a917123be762a"
    "5ff9d4550bdc37311674d777a7e607dba42f46f3614567e30ca23b1c8518041d"
    "29a08fb25ad8a67eee8d534ba19ac10e7a49a52c81c4c7362b7f439533f26c68"
    "6df00228cedd9bea5e997c1486cfe542b840782d3ae9641f92907d396fe08930")

# The app always passes this fixed placeholder address (never the device MAC).
ADDR6_PLACEHOLDER = bytes.fromhex("112233332211")

# Bit mask selecting which bytes are XORed (mask bit set) vs added (bit clear).
# 0x9999 also selects which substitution table a byte goes through.
_XOR_MASK = 0x9999


def _rol8(b: int, n: int) -> int:
    return ((b << n) | (b >> (8 - n))) & 0xFF


def _expand_key(key: bytes) -> bytes:
    """Expand a 16-byte key into the 272-byte round schedule (sub_1630)."""
    if len(key) != 16:
        raise ValueError("key must be 16 bytes")
    buf = bytearray(272)
    buf[0:16] = key

    # Seed checksum: XOR of the eight high/low byte differences.
    v = [key[i] ^ key[i + 8] if i < 8 else key[i] for i in range(16)]
    checksum = 0
    for i in range(8):
        checksum ^= v[i]

    state = list(key)
    for r in range(16):
        state = [_rol8(b, 3) for b in state]
        # bfi w0, w14, #3, #29 keeps only the low byte meaningful -> 8-bit rotate.
        checksum = _rol8(checksum & 0xFF, 3)
        # The 17-byte rolling window: rotated state plus the checksum byte.
        window = state + [checksum & 0xFF]
        table_base = r * 16
        out_base = 16 + r * 16          # key stays at buf[0:16]
        for j in range(16):
            # Table block r is consumed backwards, window advances by one.
            buf[out_base + j] = (
                _TAB_EXPAND[table_base + 15 - j] + window[(r + 1 + j) % 17]) & 0xFF
    return bytes(buf)


def _round16(b: list[int]) -> list[int]:
    """One unrolled linear mixing block over the 16 state bytes (mod 256)."""
    b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15 = b

    w7 = b1 + 2 * b0;   w16 = b1 + b0
    w20 = b3 + 2 * b2;  w3 = b3 + b2
    w22 = b5 + 2 * b4;  w5 = b5 + b4
    w24 = b7 + 2 * b6;  w17 = b7 + b6
    w26 = b9 + 2 * b8;  w4 = b9 + b8
    w28 = b11 + 2 * b10; w6 = b11 + b10
    w23 = b13 + 2 * b12; w19 = b13 + b12
    w25 = b15 + 2 * b14; w21 = b15 + b14

    w27 = w6 + 2 * w26;  w6 = w6 + w26
    w26 = w21 + 2 * w23; w21 = w21 + w23
    w23 = w16 + 2 * w20; w16 = w20 + w16
    w20 = w5 + 2 * w24;  w5 = w24 + w5
    w24 = w4 + 2 * w28;  w4 = w28 + w4
    w28 = w19 + 2 * w25; w19 = w25 + w19
    w25 = w17 + 2 * w7;  w17 = w17 + w7
    w7 = w3 + 2 * w22;   w3 = w22 + w3
    w22 = w19 + 2 * w24; w19 = w19 + w24
    w24 = w3 + 2 * w25;  w3 = w25 + w3
    w25 = w16 + 2 * w20; w16 = w20 + w16
    w20 = w4 + 2 * w28;  w4 = w28 + w4
    w28 = w17 + 2 * w7;  w17 = w17 + w7
    w30 = w20 + w17;     w17 = w17 + 2 * w20
    w7 = w5 + 2 * w27;   w5 = w27 + w5
    w27 = w21 + 2 * w23; w21 = w21 + w23

    o = [0] * 16
    o[0] = w17
    w17 = w19 + w24
    w23 = w26 + w6
    w19 = w19 + 2 * w24
    w20 = w21 + w7
    w7 = w21 + 2 * w7
    o[5] = w17
    w17 = w23 + 2 * w25
    o[4] = w19
    w19 = w4 + w28
    w4 = w4 + 2 * w28
    o[2] = w7
    o[6] = w17
    w17 = w27 + w5
    w7 = w23 + w25
    w5 = w5 + 2 * w27
    o[8] = w4
    w4 = w22 + w16
    w16 = w16 + 2 * w22
    o[11] = w17
    w17 = w6 + 2 * w26
    o[1] = w30
    o[13] = w4
    w4 = w17 + w3
    o[12] = w16
    w16 = w3 + 2 * w17
    o[3] = w20
    o[7] = w7
    o[9] = w19
    o[10] = w5
    o[15] = w4
    o[14] = w16
    return [x & 0xFF for x in o]


def _mix(state: bytes, sched: bytes, flag: int) -> bytes:
    """The 8-round mixing network (sub_176c)."""
    if len(state) != 16 or len(sched) != 272:
        raise ValueError("bad mix operands")
    out = [b & 0xFF for b in state]
    orig = list(out)

    def sel(i):
        return (1 << i) & _XOR_MASK

    for rnd in range(8):
        base = rnd * 32
        if flag and rnd == 2:
            for i in range(16):
                out[i] = (out[i] ^ orig[i]) if sel(i) else (out[i] + orig[i])
        for i in range(16):
            v = sched[base + i]
            out[i] = (out[i] ^ v) if sel(i) else (out[i] + v)
        out = [(_TAB_A if sel(i) else _TAB_B)[out[i] & 0xFF] for i in range(16)]
        for i in range(16):
            v = sched[base + 16 + i]
            out[i] = (out[i] + v) if sel(i) else (out[i] ^ v)
        out = _round16([x & 0xFF for x in out])

    for i in range(16):
        v = sched[0x100 + i]
        out[i] = (out[i] ^ v) if sel(i) else (out[i] + v)
    return bytes(x & 0xFF for x in out)


def transform(addr6: bytes, challenge: bytes) -> bytes:
    """The 16-byte auth response for a 16-byte challenge."""
    if len(addr6) != 6 or len(challenge) != 16:
        raise ValueError("addr6 must be 6 bytes and challenge 16 bytes")
    out = bytes(addr6) + bytes(addr6) + bytes(addr6[:4])
    tmp = bytearray(challenge)
    tmp[15] ^= 0x06
    sched = _expand_key(bytes(tmp))
    return _mix(out, sched, 1)


# Known answers taken from a real HCI capture of the phone talking to the buds
# (an HCI snoop log of the official app). These are the bytes the hardware accepted,
# so they validate this implementation without needing the vendor library.
_KNOWN_ANSWERS = [
    ("385c2aecb03bfb32af3c54ec18db5c02", "99a16782b8e05df000099bc2cb3417bf"),
    ("1afe43fbfaaa3afb29d1e6053c7c9475", "f011ccd271c9d429d14bd4887d825611"),
    ("acbb8a57b2d58c8aa24d2fb232cbddb7", "e696a4c36f460b55ca6f5b8fb0a4c7f8"),
]


def selftest() -> bool:
    """Check the implementation against responses recorded from the hardware."""
    ok = True
    for chal_hex, want_hex in _KNOWN_ANSWERS:
        got = transform(ADDR6_PLACEHOLDER, bytes.fromhex(chal_hex)).hex()
        status = "ok" if got == want_hex else "MISMATCH"
        if got != want_hex:
            ok = False
        print(f"  {chal_hex} -> {got}  [{status}]")
    return ok


if __name__ == "__main__":
    import sys
    print(f"selftest against {len(_KNOWN_ANSWERS)} captured hardware responses:")
    sys.exit(0 if selftest() else 1)
