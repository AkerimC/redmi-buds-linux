//! RCSP framing and payload decoding.
//!
//! Frame layout:
//!
//! ```text
//! FE DC BA | flags | opcode | len (u16 BE) | payload[len] | EF
//! ```
//!
//! `flags`: bit7 command(1)/response(0), bit6 expects response, bits 0-2 target app.
//! Command payload: `sn | [xm_opcode if opcode == 0x01] | params…`
//! Response payload: `status | sn | data…`

pub const MAGIC: [u8; 3] = [0xFE, 0xDC, 0xBA];
pub const TRAILER: u8 = 0xEF;

pub const APP_MIUI: u8 = 1; // com.xiaomi.bluetooth
pub const APP_EARPHONE: u8 = 4; // com.mi.earphone

#[derive(Debug, Clone)]
pub struct Frame {
    pub flags: u8,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn is_command(&self) -> bool {
        self.flags & 0x80 != 0
    }

    pub fn target_app(&self) -> u8 {
        self.flags & 0x07
    }

    pub fn status(&self) -> u8 {
        self.payload.first().copied().unwrap_or(0xFF)
    }

    pub fn sn(&self) -> u8 {
        if self.is_command() {
            self.payload.first().copied().unwrap_or(0xFF)
        } else {
            self.payload.get(1).copied().unwrap_or(0xFF)
        }
    }

    /// Params for a command, response data for a response.
    pub fn data(&self) -> &[u8] {
        let skip = if self.is_command() {
            if self.opcode == 0x01 { 2 } else { 1 }
        } else if self.opcode == 0x01 {
            1
        } else {
            2
        };
        if self.payload.len() > skip {
            &self.payload[skip..]
        } else {
            &[]
        }
    }

    pub fn opcode_name(&self) -> &'static str {
        opcode_name(self.opcode)
    }
}

pub fn opcode_name(op: u8) -> &'static str {
    match op {
        0x01 => "DATA",
        0x02 => "GET_TARGET_INFO",
        0x03 => "REBOOT_DEVICE",
        0x04 => "NOTIFY_DEVICE_APP_INFO",
        0x06 => "DISCONNECT",
        0x09 => "GET_DEVICE_RUN_INFO",
        0x42 => "FIND_DEVICE",
        0x50 => "AUTH_CHECK",
        0x51 => "AUTH_SEND_CALC_RESULT",
        0xF2 => "SET_DEVICE_CONFIG",
        0xF3 => "GET_DEVICE_CONFIG",
        0xF4 => "NOTIFY_DEVICE_CONFIG",
        _ => "?",
    }
}

pub fn status_name(st: u8) -> &'static str {
    match st {
        0 => "ok",
        1 => "fail",
        2 => "unknown_cmd",
        3 => "busy",
        4 => "no_resource",
        5 => "crc",
        7 => "param_error",
        9 => "not_supported",
        _ => "?",
    }
}

pub fn config_id_name(id: u16) -> &'static str {
    match id {
        2 => "gesture_click_set",
        3 => "auto_answer",
        5 => "fit_test",
        7 => "eq_preset",
        9 => "find_device",
        10 => "longpress_noise_cycle",
        11 => "anc_state",
        29 => "spatial_audio",
        36 => "virtual_surround",
        37 => "adaptive_anc",
        39 => "serial_number",
        40 => "time",
        41 => "adaptive_sense",
        47 => "low_latency",
        54 => "scene_rendering",
        55 => "custom_eq",
        58 => "notify_sound",
        59 => "personalized_noise_reduction",
        60 => "ear_canal_fit",
        62 => "detection_exists",
        72 => "adaptive_volume",
        74 => "bluetooth_info",
        76 => "app_country",
        83 => "click_set_new",
        93 => "restricted",
        103 => "commuting_immerse",
        114 => "case_sound_list",
        115 => "case_sound_setting",
        120 => "translate_settings",
        161 => "notification_volume",
        _ => "?",
    }
}

pub fn target_attr_name(t: u8) -> &'static str {
    match t {
        0 => "name",
        1 => "version",
        2 => "quantity",
        3 => "vid_pid",
        4 => "-",
        5 => "mandatory_upgrade",
        6 => "uboot_version",
        7 => "multi_battery",
        8 => "codec_type",
        9 => "power_mode",
        10 => "function_key",
        11 => "hot_word",
        12 => "charset",
        13 => "color",
        14 => "spatial_audio_type",
        15 => "virtual_surround_type",
        _ => "attr",
    }
}

/// Encode one frame.
pub fn encode(flags: u8, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&MAGIC);
    out.push(flags);
    out.push(opcode);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out.push(TRAILER);
    out
}

/// Pull one complete frame off the front of `buf`, if there is one.
///
/// Bytes before the magic are discarded; a partial frame is left in place.
pub fn extract(buf: &mut Vec<u8>) -> Option<Frame> {
    let start = buf.windows(3).position(|w| w == MAGIC)?;
    if start > 0 {
        buf.drain(..start);
    }
    if buf.len() < 7 {
        return None;
    }
    let plen = u16::from_be_bytes([buf[5], buf[6]]) as usize;
    let end = 7 + plen;
    if buf.len() <= end {
        return None; // trailer not arrived yet
    }
    if buf[end] != TRAILER {
        // Not actually a frame start; resynchronise past this magic.
        buf.drain(..1);
        return None;
    }
    let frame = Frame {
        flags: buf[3],
        opcode: buf[4],
        payload: buf[7..end].to_vec(),
    };
    buf.drain(..end + 1);
    Some(frame)
}

/// Device-config TLVs: `len(u8) | type(u16 BE) | value(len-2)`.
pub fn parse_tlvs_u16(data: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        let len = data[i] as usize;
        if len < 2 || i + 1 + len > data.len() {
            break;
        }
        let id = u16::from_be_bytes([data[i + 1], data[i + 2]]);
        out.push((id, data[i + 3..i + 1 + len].to_vec()));
        i += 1 + len;
    }
    out
}

/// Target/run-info attributes: `len(u8) | type(u8) | value(len-1)`.
pub fn parse_tlvs_u8(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 2 <= data.len() {
        let len = data[i] as usize;
        if len < 1 || i + 1 + len > data.len() {
            break;
        }
        out.push((data[i + 1], data[i + 2..i + 1 + len].to_vec()));
        i += 1 + len;
    }
    out
}

pub fn tlv_u16(id: u16, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 3);
    out.push((2 + value.len()) as u8);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(value);
    out
}

/// multi-battery value: percent = v & 0x7f, charging = v & 0x80, 0xff = unknown.
pub fn decode_battery(values: &[u8]) -> Vec<(i16, bool)> {
    values
        .iter()
        .map(|&v| {
            if v == 0xFF {
                (-1, false)
            } else {
                ((v & 0x7F) as i16, v & 0x80 != 0)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let raw = encode(0xC4, 0x02, &[0x00, 0xff, 0xff, 0xff, 0xff]);
        let mut buf = raw.clone();
        let f = extract(&mut buf).expect("frame");
        assert_eq!(f.flags, 0xC4);
        assert_eq!(f.opcode, 0x02);
        assert_eq!(f.payload, vec![0x00, 0xff, 0xff, 0xff, 0xff]);
        assert!(buf.is_empty());
    }

    #[test]
    fn decodes_captured_frames() {
        // Verbatim from the HCI capture: GET_TARGET_INFO response.
        let raw = "fedcba04020036001e12005265646d692042756473203620506c617905012022202202020a05032717509b020401020500030602020407e4e432020d02ef";
        let bytes = crate::auth::hex_decode(raw).unwrap();
        let mut buf = bytes.clone();
        let f = extract(&mut buf).unwrap();
        assert_eq!(f.opcode, 0x02);
        assert!(!f.is_command());
        assert_eq!(f.status(), 0);
        let tlv = parse_tlvs_u8(f.data());
        assert_eq!(tlv[0].0, 0, "first attribute is the name");
        assert_eq!(String::from_utf8_lossy(&tlv[0].1), "Redmi Buds 6 Play");
        assert!(buf.is_empty());
    }

    #[test]
    fn battery_decoding() {
        // e4 e4 32 = both buds 100% charging, case 50%
        let b = decode_battery(&[0xe4, 0xe4, 0x32]);
        assert_eq!(b, vec![(100, true), (100, true), (50, false)]);
        assert_eq!(decode_battery(&[0xff]), vec![(-1, false)]);
    }

    #[test]
    fn config_tlv_round_trip() {
        let t = tlv_u16(7, &[3]);
        assert_eq!(t, vec![3, 0x00, 0x07, 0x03]);
        let parsed = parse_tlvs_u16(&t);
        assert_eq!(parsed, vec![(7u16, vec![3u8])]);
    }

    #[test]
    fn resyncs_on_garbage_and_partial() {
        let mut buf = vec![0xde, 0xad];
        assert!(extract(&mut buf).is_none());
        let raw = encode(0x84, 0xF3, &[0x21, 0x00, 0x5d]);
        for b in raw.iter() {
            buf.push(*b);
        }
        let f = extract(&mut buf).unwrap();
        assert_eq!(f.opcode, 0xF3);
        assert_eq!(f.sn(), 0x21);
    }
}
