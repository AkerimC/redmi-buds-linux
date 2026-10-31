//! Human-level meaning of the device-config values: EQ preset names and the
//! gesture table (config id 2).
//!
//! Names come from the earphone app: `SoundEffectExtKt.getSoundEffectName` for
//! presets, `GestureControlViewModel.getStringFromConfig` for gesture actions,
//! and `FunctionConfigImpl.gestureClickSet` / `DeviceConfigClickSet` for the
//! write encoding.

/// Config id of the EQ preset (`DeviceConfigSoundEffect`).
pub const CFG_EQ: u16 = 7;
/// Config id of the gesture table (`DeviceConfigClickSet`).
pub const CFG_GESTURES: u16 = 2;
/// Config id of the find-device ring (`DeviceConfigFindDevice`).
pub const CFG_FIND: u16 = 9;
/// Config id of the serial number.
pub const CFG_SERIAL: u16 = 39;

/// The presets the app offers for this model, in app order. The ids are the
/// values the capture shows the app writing to config id 7.
pub const EQ_PRESETS: [(u8, &str); 5] = [
    (0, "Balanced"),
    (1, "Vocal"),
    (5, "Bass boost"),
    (6, "Treble boost"),
    (7, "Volume boost"),
];

pub fn eq_name(id: u8) -> Option<&'static str> {
    EQ_PRESETS.iter().find(|(i, _)| *i == id).map(|(_, n)| *n)
}

/// Untouched-side marker in a gesture write.
pub const GESTURE_KEEP: u8 = 0xFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    Double = 1,
    Triple = 2,
    LongPress = 3,
    Single = 4,
    Slide = 5,
}

impl Gesture {
    pub const ALL: [Gesture; 5] = [
        Gesture::Single,
        Gesture::Double,
        Gesture::Triple,
        Gesture::LongPress,
        Gesture::Slide,
    ];

    pub fn from_id(id: u8) -> Option<Gesture> {
        Gesture::ALL.into_iter().find(|g| *g as u8 == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Gesture::Single => "Single tap",
            Gesture::Double => "Double tap",
            Gesture::Triple => "Triple tap",
            Gesture::LongPress => "Press and hold",
            Gesture::Slide => "Slide",
        }
    }

    pub fn parse(s: &str) -> Option<Gesture> {
        match s {
            "single" | "1x" => Some(Gesture::Single),
            "double" | "2x" => Some(Gesture::Double),
            "triple" | "3x" => Some(Gesture::Triple),
            "long" | "hold" => Some(Gesture::LongPress),
            "slide" => Some(Gesture::Slide),
            _ => None,
        }
    }
}

/// Gesture actions the app offers on music-only buds (no ANC, no camera).
pub const GESTURE_ACTIONS: [(u8, &str); 7] = [
    (8, "Nothing"),
    (1, "Play / pause"),
    (2, "Previous track"),
    (3, "Next track"),
    (4, "Volume up"),
    (5, "Volume down"),
    (0, "Voice assistant"),
];

pub fn action_name(id: u8) -> String {
    let known = match id {
        6 => Some("Noise control"),
        9 => Some("Take photo"),
        10 => Some("Record"),
        11 => Some("Volume"),
        12 => Some("Translate"),
        13 => Some("Previous / next"),
        _ => GESTURE_ACTIONS.iter().find(|(i, _)| *i == id).map(|(_, n)| *n),
    };
    known.map_or_else(|| format!("Action {id}"), str::to_string)
}

pub fn parse_action(s: &str) -> Option<u8> {
    let id = match s {
        "none" | "nothing" => 8,
        "play" | "pause" | "playpause" => 1,
        "prev" | "previous" => 2,
        "next" => 3,
        "vol+" | "volup" => 4,
        "vol-" | "voldown" => 5,
        "assistant" | "voice" => 0,
        n => n.parse().ok()?,
    };
    Some(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GestureBinding {
    pub gesture: Gesture,
    pub left: u8,
    pub right: u8,
}

/// Decode config id 2: repeated `[gesture, left, right]` triples.
pub fn decode_gestures(value: &[u8]) -> Vec<GestureBinding> {
    value
        .chunks_exact(3)
        .filter_map(|c| {
            Some(GestureBinding {
                gesture: Gesture::from_id(c[0])?,
                left: c[1],
                right: c[2],
            })
        })
        .collect()
}

/// Encode a single gesture write; `None` leaves that side unchanged.
pub fn encode_gesture(gesture: Gesture, left: Option<u8>, right: Option<u8>) -> Vec<u8> {
    vec![
        gesture as u8,
        left.unwrap_or(GESTURE_KEEP),
        right.unwrap_or(GESTURE_KEEP),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_observed_gesture_table() {
        // value read from the real buds
        let v = [4, 8, 4, 1, 1, 1, 2, 3, 3, 3, 8, 8];
        let g = decode_gestures(&v);
        assert_eq!(g.len(), 4);
        assert_eq!(
            g[0],
            GestureBinding { gesture: Gesture::Single, left: 8, right: 4 }
        );
        assert_eq!(g[1].gesture, Gesture::Double);
        assert_eq!(action_name(g[1].left), "Play / pause");
        assert_eq!(g[3].gesture, Gesture::LongPress);
    }

    #[test]
    fn gesture_write_marks_untouched_side() {
        assert_eq!(encode_gesture(Gesture::Double, Some(3), None), [1, 3, 0xFF]);
        assert_eq!(encode_gesture(Gesture::LongPress, None, Some(0)), [3, 0xFF, 0]);
    }

    #[test]
    fn names() {
        assert_eq!(eq_name(5), Some("Bass boost"));
        assert_eq!(eq_name(3), None);
        assert_eq!(action_name(42), "Action 42");
        assert_eq!(parse_action("next"), Some(3));
        assert_eq!(Gesture::parse("long"), Some(Gesture::LongPress));
    }
}
