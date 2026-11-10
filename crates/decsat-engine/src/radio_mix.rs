// made by CasualArclamp
//! Each ED-137 radio's own volume and mute: a link carries many radios
//! (one RTP stream each), and they take turns being heard. A muted radio is
//! passed over, so it never takes the turn from one that is wanted; the
//! volume scales that radio's sound, under the app's volume.
//!
//! App-wide, as the voice floor is ([`crate::floor`]): the side panel sets
//! the levels, every VFO's decoder thread reads them. A radio is known by
//! its stream's label (`decsat_ip::ed137::StreamView::label`: addresses,
//! ports and SSRC).

use std::collections::HashMap;
use std::sync::Mutex;

/// One radio's level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// 0 to 1, a gain on its samples.
    pub volume: f32,
    pub muted: bool,
}

impl Default for Level {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
        }
    }
}

/// Rust note: `None` until the first level is set, as a `HashMap` cannot be
/// built in a `static`'s `const` initialiser. Only the radios that differ
/// from the default are kept.
static LEVELS: Mutex<Option<HashMap<String, Level>>> = Mutex::new(None);

/// The radio's level: full volume, not muted, unless set.
pub fn level(stream: &str) -> Level {
    let l = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    l.as_ref()
        .and_then(|m| m.get(stream).copied())
        .unwrap_or_default()
}

pub fn set_level(stream: &str, level: Level) {
    let level = Level {
        volume: level.volume.clamp(0.0, 1.0),
        ..level
    };
    let mut l = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    let m = l.get_or_insert_with(HashMap::new);
    if level == Level::default() {
        m.remove(stream);
    } else {
        m.insert(stream.to_string(), level);
    }
}

/// The radio's gain if it is to be heard at all: `None` when muted or
/// turned right down, so the caller lets another radio have the turn.
pub fn gain(stream: &str) -> Option<f32> {
    let l = level(stream);
    (!l.muted && l.volume > 0.0).then_some(l.volume)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_radio_is_heard_in_full_until_set_otherwise() {
        // Each test its own names: the levels are shared by the process.
        assert_eq!(gain("mix-test a"), Some(1.0));
        set_level(
            "mix-test a",
            Level {
                volume: 0.25,
                muted: false,
            },
        );
        assert_eq!(gain("mix-test a"), Some(0.25));
        assert_eq!(gain("mix-test b"), Some(1.0));
    }

    #[test]
    fn muted_or_turned_down_is_not_heard_and_keeps_its_volume() {
        set_level(
            "mix-test c",
            Level {
                volume: 0.5,
                muted: true,
            },
        );
        assert_eq!(gain("mix-test c"), None);
        assert_eq!(level("mix-test c").volume, 0.5);
        set_level(
            "mix-test c",
            Level {
                volume: 0.0,
                muted: false,
            },
        );
        assert_eq!(gain("mix-test c"), None);
        set_level("mix-test c", Level::default());
        assert_eq!(gain("mix-test c"), Some(1.0));
    }
}
