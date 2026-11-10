// made by CasualArclamp
//! Each ED-137 radio's own volume and mute, and their mix: a link carries
//! many radios (one RTP stream each), all heard together ([`Mixer`]). A
//! muted radio is left out; the volume scales that radio's sound, under the
//! app's volume.
//!
//! App-wide, as the voice floor is ([`crate::floor`]): the side panel sets
//! the levels, every VFO's decoder thread reads them. A radio is known by
//! its stream's label (`decsat_ip::ed137::StreamView::label`: addresses,
//! ports and SSRC).

use std::collections::{HashMap, VecDeque};
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
/// turned right down.
pub fn gain(stream: &str) -> Option<f32> {
    let l = level(stream);
    (!l.muted && l.volume > 0.0).then_some(l.volume)
}

/// How far (samples at 8 kHz: 60 ms) one radio's sound may wait for
/// another's that is behind.
const WAIT: usize = 480;

/// The radios' sound added together. Each radio's samples queue in its own
/// lane as its packets come; what is taken is as much as every lane that
/// has sound can give, so radios whose packets come a little apart stay
/// whole. A lane that falls behind (packets lost) is not waited for past
/// [`WAIT`]: it is silent there, as its lost packets were.
#[derive(Debug, Default)]
pub struct Mixer {
    lanes: Vec<(String, VecDeque<f32>)>,
}

impl Mixer {
    /// A radio's packet of sound, at its gain.
    pub fn push(&mut self, stream: &str, pcm: &[f32], gain: f32) {
        // Rust note: `position` finds the lane's index, so the borrow of
        // `self.lanes` ends before a new lane is pushed.
        let i = match self.lanes.iter().position(|(s, _)| s == stream) {
            Some(i) => i,
            None => {
                self.lanes.push((stream.to_string(), VecDeque::new()));
                self.lanes.len() - 1
            }
        };
        self.lanes[i].1.extend(pcm.iter().map(|x| x * gain));
    }

    /// The mix so far, added to `out`; lanes left empty are forgotten.
    pub fn take(&mut self, out: &mut Vec<f32>) {
        let lens = || self.lanes.iter().map(|(_, q)| q.len());
        let longest = lens().max().unwrap_or(0);
        let shortest = lens().filter(|&n| n > 0).min().unwrap_or(0);
        let n = shortest.max(longest.saturating_sub(WAIT));
        let start = out.len();
        out.resize(start + n, 0.0);
        for (_, q) in &mut self.lanes {
            let k = n.min(q.len());
            for (o, x) in out[start..].iter_mut().zip(q.drain(..k)) {
                *o += x;
            }
        }
        // Two loud radios together can pass full scale.
        for o in &mut out[start..] {
            *o = o.clamp(-1.0, 1.0);
        }
        self.lanes.retain(|(_, q)| !q.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radios_are_added_sample_by_sample() {
        let mut m = Mixer::default();
        let mut out = Vec::new();
        m.push("a", &[0.25; 160], 1.0);
        m.push("b", &[0.5; 160], 0.5);
        m.take(&mut out);
        assert_eq!(out.len(), 160);
        assert!(out.iter().all(|&x| (x - 0.5).abs() < 1e-6));
        // Loud ones together stop at full scale.
        out.clear();
        m.push("a", &[0.75; 160], 1.0);
        m.push("b", &[0.75; 160], 1.0);
        m.take(&mut out);
        assert!(out.iter().all(|&x| x == 1.0));
    }

    /// Packets a little apart: the radio ahead waits, nothing is cut up.
    #[test]
    fn a_radio_ahead_waits_for_the_other() {
        let mut m = Mixer::default();
        let mut out = Vec::new();
        m.push("a", &[0.25; 320], 1.0);
        m.push("b", &[0.25; 160], 1.0);
        m.take(&mut out);
        assert_eq!(out.len(), 160);
        m.push("b", &[0.25; 160], 1.0);
        m.take(&mut out);
        assert_eq!(out.len(), 320);
        assert!(out.iter().all(|&x| (x - 0.5).abs() < 1e-6));
    }

    /// One radio alone, or one whose packets are lost, holds nobody up.
    #[test]
    fn a_radio_behind_is_not_waited_for_long() {
        let mut m = Mixer::default();
        let mut out = Vec::new();
        m.push("a", &[0.25; 1600], 1.0);
        m.take(&mut out);
        assert_eq!(out.len(), 1600);
        out.clear();
        m.push("a", &[0.25; 1600], 1.0);
        m.push("b", &[0.25; 160], 1.0);
        m.take(&mut out);
        assert_eq!(out.len(), 1600 - WAIT);
        assert!((out[0] - 0.5).abs() < 1e-6 && (out[160] - 0.25).abs() < 1e-6);
    }

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
