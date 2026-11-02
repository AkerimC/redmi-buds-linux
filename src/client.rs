//! The buds client: session auth plus the RCSP commands we use.

use std::collections::VecDeque;
use std::io;
use std::io::Read;
use std::process::Command;

use crate::auth;
use crate::bt::{parse_mac, Rfcomm};
use crate::rcsp::{self, Frame, APP_EARPHONE, APP_MIUI};
use crate::settings::{self, Gesture, GestureBinding, CFG_EQ, CFG_FIND, CFG_GESTURES, CFG_SERIAL};

/// First paired device whose name looks like Xiaomi/Redmi buds, via `bluetoothctl`.
pub fn find_paired_buds() -> Option<String> {
    let out = Command::new("bluetoothctl")
        .args(["devices", "Paired"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let mut parts = l.splitn(3, ' ');
        (parts.next()? == "Device").then_some(())?;
        let mac = parts.next()?;
        let name = parts.next().unwrap_or("").to_lowercase();
        (name.contains("buds") || name.contains("airdots")).then(|| mac.to_string())
    })
}

/// RFCOMM channels to try when SDP is unavailable, most likely first.
///
/// The MIUI UUID (`0000fd2d-…`) sits on channel **6** on the Redmi Buds 6 Play
/// and on channel **29** on the Redmi Buds 6 Lite, so both have to be probed.
/// 1–5 are the plain-SPP channels some JieLi firmwares expose; they answer
/// immediately when absent.
///
/// Keep this list short: channels the firmware does not serve as RCSP can still
/// accept the RFCOMM connection (channel 12 does on the 6 Lite) and then sit
/// silent, so a wide sweep both wastes the connect timeout and can stop at a
/// channel that never answers the auth challenge.
const CHANNEL_CANDIDATES: [u8; 7] = [6, 29, 5, 4, 3, 2, 1];

const CONNECT_TIMEOUT_MS: i32 = 15_000;
const IO_TIMEOUT_MS: i32 = 8_000;

#[derive(Debug, Default)]
pub struct TargetInfo {
    pub name: Option<String>,
    pub quantity: Option<u8>,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub multi_battery: Vec<(i16, bool)>,
    pub raw: Vec<(u8, Vec<u8>)>,
}

pub struct Buds {
    conn: Rfcomm,
    buf: Vec<u8>,
    pending: VecDeque<Frame>,
    sn: u8,
    verbose: bool,
    pub mac: [u8; 6],
    pub channel: u8,
}

fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = [0u8; N];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Ask `sdptool` which channel serves the MIUI UUID.
///
/// `sdptool browse` prints one block per service record, with the RFCOMM channel
/// *after* the class-id/UUID lines, so the whole record has to be considered.
fn sdp_channel(mac: &str) -> Option<u8> {
    let out = Command::new("sdptool").arg("browse").arg(mac).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);

    fn channel_of(record: &[&str]) -> Option<u8> {
        if !record.iter().any(|l| l.contains("0000fd2d")) {
            return None;
        }
        record.iter().find_map(|l| {
            l.trim()
                .strip_prefix("Channel:")
                .and_then(|rest| rest.trim().parse::<u8>().ok())
        })
    }

    let mut record: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("Service Name:") && !record.is_empty() {
            if let Some(ch) = channel_of(&record) {
                return Some(ch);
            }
            record.clear();
        }
        record.push(line);
    }
    channel_of(&record)
}

impl Buds {
    pub fn connect(mac_str: &str, channel: Option<u8>, verbose: bool) -> io::Result<Self> {
        let mac = parse_mac(mac_str)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad MAC address"))?;

        let mut candidates: Vec<u8> = Vec::new();
        if let Some(ch) = channel {
            candidates.push(ch);
        } else {
            if let Some(ch) = sdp_channel(mac_str) {
                if verbose {
                    eprintln!("  SDP fd2d -> channel {ch}");
                }
                candidates.push(ch);
            }
            for ch in CHANNEL_CANDIDATES {
                if !candidates.contains(&ch) {
                    candidates.push(ch);
                }
            }
        }

        let mut last_err: Option<io::Error> = None;
        for ch in candidates {
            if verbose {
                eprintln!("  connecting RFCOMM {mac_str} channel {ch}");
            }
            match Rfcomm::connect(&mac, ch, CONNECT_TIMEOUT_MS) {
                Ok(conn) => {
                    if verbose {
                        eprintln!("  connected on channel {ch}");
                    }
                    return Ok(Buds {
                        conn,
                        buf: Vec::new(),
                        pending: VecDeque::new(),
                        sn: 0,
                        verbose,
                        mac,
                        channel: ch,
                    });
                }
                Err(e) => {
                    if verbose {
                        eprintln!("  channel {ch}: {e}");
                    }
                    // EHOSTDOWN / EHOSTUNREACH: the baseband page failed, so no
                    // other channel can succeed either. Don't wait out each one.
                    if matches!(e.raw_os_error(), Some(112) | Some(113)) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no usable RFCOMM channel")
        }))
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            eprintln!("  {msg}");
        }
    }

    fn next_sn(&mut self) -> u8 {
        self.sn = self.sn.wrapping_add(1);
        self.sn
    }

    pub fn send_frame(&mut self, flags: u8, opcode: u8, payload: &[u8]) -> io::Result<()> {
        if self.verbose {
            eprintln!(
                "  -> [{} {} app={} sn=0x{:02x}] {}",
                if flags & 0x80 != 0 { "cmd" } else { "rsp" },
                rcsp::opcode_name(opcode),
                flags & 0x07,
                payload.first().copied().unwrap_or(0xFF),
                auth::hex_encode(payload)
            );
        }
        let raw = rcsp::encode(flags, opcode, payload);
        self.conn.send(&raw)
    }

    pub fn send_command(&mut self, opcode: u8, params: &[u8], app: u8) -> io::Result<u8> {
        let sn = self.next_sn();
        let mut payload = Vec::with_capacity(params.len() + 1);
        payload.push(sn);
        payload.extend_from_slice(params);
        self.send_frame(0x80 | 0x40 | app, opcode, &payload)?;
        Ok(sn)
    }

    /// Next complete frame, or `None` on timeout.
    pub fn recv_frame(&mut self, timeout_ms: i32) -> io::Result<Option<Frame>> {
        if let Some(f) = self.pending.pop_front() {
            self.log_frame(&f);
            return Ok(Some(f));
        }
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
        let mut scratch = [0u8; 4096];
        loop {
            if let Some(f) = rcsp::extract(&mut self.buf) {
                self.log_frame(&f);
                return Ok(Some(f));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = remaining.as_millis() as i32;
            if wait <= 0 {
                return Ok(None);
            }
            match self.conn.recv(&mut scratch, wait)? {
                Some(n) if n > 0 => self.buf.extend_from_slice(&scratch[..n]),
                _ => {}
            }
        }
    }

    fn log_frame(&self, f: &Frame) {
        if self.verbose {
            let kind = if f.is_command() { "cmd" } else { "rsp" };
            let status = if f.is_command() {
                String::new()
            } else {
                format!(" status={}", rcsp::status_name(f.status()))
            };
            eprintln!(
                "  <- [{kind} {} app={} sn=0x{:02x}{status}] {}",
                f.opcode_name(),
                f.target_app(),
                f.sn(),
                auth::hex_encode(&f.payload)
            );
        }
    }

    /// Answer a challenge the buds initiates against us.
    fn answer_device_challenge(&mut self, f: &Frame) -> io::Result<bool> {
        if !(f.is_command() && f.opcode == 0x50) {
            return Ok(false);
        }
        let body = f.data();
        if body.len() < 17 || body[0] != 0x01 {
            return Ok(false);
        }
        let mut challenge = [0u8; 16];
        challenge.copy_from_slice(&body[1..17]);
        let resp = auth::transform(&auth::ADDR6_PLACEHOLDER, &challenge);
        let mut payload = vec![0x00, f.sn(), 0x01];
        payload.extend_from_slice(&resp);
        self.send_frame(APP_MIUI, 0x50, &payload)?;
        self.log("answered device-initiated challenge");
        Ok(true)
    }

    pub fn wait_response(&mut self, sn: u8, opcode: Option<u8>, timeout_ms: i32) -> io::Result<Frame> {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = remaining.as_millis() as i32;
            if wait <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no response to sn=0x{sn:02x}"),
                ));
            }
            let Some(f) = self.recv_frame(wait)? else {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no response to sn=0x{sn:02x}"),
                ));
            };
            if self.answer_device_challenge(&f)? {
                continue;
            }
            if f.is_command() {
                continue;
            }
            if f.sn() == sn && opcode.is_none_or(|op| f.opcode == op) {
                return Ok(f);
            }
        }
    }

    pub fn request(&mut self, opcode: u8, params: &[u8], app: u8) -> io::Result<Frame> {
        let sn = self.send_command(opcode, params, app)?;
        let rsp = self.wait_response(sn, Some(opcode), IO_TIMEOUT_MS)?;
        if rsp.status() != 0 {
            return Err(io::Error::other(format!(
                "{} failed: {}",
                rcsp::opcode_name(opcode),
                rcsp::status_name(rsp.status())
            )));
        }
        Ok(rsp)
    }

    /// AUTH_CHECK challenge/response, then AUTH_SEND_CALC_RESULT.
    pub fn authenticate(&mut self) -> io::Result<()> {
        let challenge: [u8; 16] = random_bytes()?;
        self.log(&format!("challenge {}", auth::hex_encode(&challenge)));
        let expected = auth::transform(&auth::ADDR6_PLACEHOLDER, &challenge);

        let mut params = vec![0x01];
        params.extend_from_slice(&challenge);
        let sn = self.send_command(0x50, &params, APP_MIUI)?;
        let rsp = self.wait_response(sn, Some(0x50), IO_TIMEOUT_MS)?;
        if rsp.status() != 0 {
            return Err(io::Error::other(format!(
                "AUTH_CHECK rejected: {}",
                rcsp::status_name(rsp.status())
            )));
        }
        let data = rsp.data();
        if data.len() < 17 {
            return Err(io::Error::other("short AUTH_CHECK response"));
        }
        if data[1..17] != expected {
            return Err(io::Error::other(format!(
                "auth mismatch\n  device {}\n  local  {}",
                auth::hex_encode(&data[1..17]),
                auth::hex_encode(&expected)
            )));
        }
        self.log("challenge accepted by device");

        let sn = self.send_command(0x51, &[0x01, 0x00], APP_MIUI)?;
        let rsp = self.wait_response(sn, Some(0x51), IO_TIMEOUT_MS)?;
        if rsp.status() != 0 {
            return Err(io::Error::other(format!(
                "AUTH_SEND_CALC_RESULT rejected: {}",
                rcsp::status_name(rsp.status())
            )));
        }
        Ok(())
    }

    pub fn get_target_info(&mut self) -> io::Result<TargetInfo> {
        let rsp = self.request(0x02, &[0xFF, 0xFF, 0xFF, 0xFF], APP_EARPHONE)?;
        let mut info = TargetInfo::default();
        for (typ, val) in rcsp::parse_tlvs_u8(rsp.data()) {
            match typ {
                0 => info.name = Some(String::from_utf8_lossy(&val).into_owned()),
                2 => info.quantity = val.first().copied(),
                3 if val.len() >= 4 => {
                    info.vid = Some(u16::from_be_bytes([val[0], val[1]]));
                    info.pid = Some(u16::from_be_bytes([val[2], val[3]]));
                }
                7 => info.multi_battery = rcsp::decode_battery(&val),
                _ => info.raw.push((typ, val)),
            }
        }
        Ok(info)
    }

    pub fn battery(&mut self) -> io::Result<Vec<(i16, bool)>> {
        let info = self.get_target_info()?;
        if !info.multi_battery.is_empty() {
            return Ok(info.multi_battery);
        }
        Ok(info.quantity.map(|q| vec![(q as i16, false)]).unwrap_or_default())
    }

    pub fn get_config(&mut self, ids: &[u16]) -> io::Result<Vec<(u16, Vec<u8>)>> {
        let mut params = Vec::with_capacity(ids.len() * 2);
        for id in ids {
            params.extend_from_slice(&id.to_be_bytes());
        }
        let rsp = self.request(0xF3, &params, APP_EARPHONE)?;
        Ok(rcsp::parse_tlvs_u16(rsp.data()))
    }

    pub fn set_config(&mut self, items: &[(u16, Vec<u8>)]) -> io::Result<()> {
        let mut params = Vec::new();
        for (id, val) in items {
            params.extend_from_slice(&rcsp::tlv_u16(*id, val));
        }
        self.request(0xF2, &params, APP_EARPHONE)?;
        Ok(())
    }

    fn get_one(&mut self, id: u16) -> io::Result<Option<Vec<u8>>> {
        Ok(self
            .get_config(&[id])?
            .into_iter()
            .find(|(cid, _)| *cid == id)
            .map(|(_, v)| v))
    }

    pub fn get_eq(&mut self) -> io::Result<Option<u8>> {
        Ok(self.get_one(CFG_EQ)?.and_then(|v| v.first().copied()))
    }

    pub fn set_eq(&mut self, preset: u8) -> io::Result<()> {
        if preset > 7 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "EQ preset must be 0..7",
            ));
        }
        self.set_config(&[(CFG_EQ, vec![preset])])
    }

    pub fn get_gestures(&mut self) -> io::Result<Vec<GestureBinding>> {
        Ok(self
            .get_one(CFG_GESTURES)?
            .map(|v| settings::decode_gestures(&v))
            .unwrap_or_default())
    }

    /// Rebind one gesture; `None` leaves that side as it is.
    pub fn set_gesture(&mut self, gesture: Gesture, left: Option<u8>, right: Option<u8>) -> io::Result<()> {
        let value = settings::encode_gesture(gesture, left, right);
        self.set_config(&[(CFG_GESTURES, value)])
    }

    pub fn serial(&mut self) -> io::Result<Option<String>> {
        Ok(self
            .get_one(CFG_SERIAL)?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    /// Ring the buds. Device config id 9 carries `[state, device_type]`.
    pub fn find_device(&mut self, ring: bool, device_type: u8) -> io::Result<()> {
        self.set_config(&[(CFG_FIND, vec![u8::from(ring), device_type])])
    }

    pub fn close(self) {}
}
