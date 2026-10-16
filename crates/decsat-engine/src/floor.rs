// made by CasualArclamp
//! The voice floor: with many carriers and channels playing their voice by
//! themselves (the CDM-600 voice preset), one talker is heard at a time, as
//! on a scanner with a hang time. Whoever starts speaking first holds the
//! floor until they have been silent for the hold time; only then can
//! another talker take it. A channel played with ▶ is not held back by the
//! others.
//!
//! A talker is one G.728 channel: a timeslot can carry several (the ML–PH
//! trunk's channel 18h, three, two of them once talking at once), each its
//! own talker ([`lane_channel`]). A timeslot played or recorded by itself
//! takes turns among its channels on a [`LocalFloor`], by the same rule.
//!
//! The floor is app-wide: every VFO's decoder thread asks it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The hold after the holder's last speech, milliseconds; 0 lets everyone
/// be heard at once.
static HOLD_MS: AtomicU32 = AtomicU32::new(3000);
static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);
/// Rust note: a `static Mutex` needs a `const fn` initialiser, which
/// `Mutex::new` is; the `Option` starts empty (nobody holds the floor).
static FLOOR: Mutex<Option<Held>> = Mutex::new(None);

/// A talker: a voice source (one per decoder stage) and its channel.
pub type Talker = (u64, u8);

#[derive(Debug, Clone, Copy)]
struct Held {
    who: Talker,
    /// When they last spoke.
    last: Instant,
}

/// One talker at a time, holding the floor `secs` after they stop; `None`
/// lets everyone be heard at once.
pub fn set_hold(secs: Option<f32>) {
    let ms = secs.map_or(0, |s| (s.clamp(0.1, 60.0) * 1000.0) as u32);
    HOLD_MS.store(ms, Ordering::Relaxed);
}

pub fn hold() -> Option<f32> {
    match HOLD_MS.load(Ordering::Relaxed) {
        0 => None,
        ms => Some(ms as f32 / 1000.0),
    }
}

/// A new voice source's number, for its talkers.
pub(crate) fn new_source() -> u64 {
    NEXT_SOURCE.fetch_add(1, Ordering::Relaxed)
}

/// May `who` be heard now? `talking`: they spoke in the last 40 ms.
pub(crate) fn may_play(who: Talker, talking: bool) -> bool {
    let Some(hold) = hold() else {
        return true;
    };
    let mut f = FLOOR.lock().unwrap_or_else(|e| e.into_inner());
    decide(
        &mut f,
        who,
        talking,
        Instant::now(),
        Duration::from_secs_f32(hold),
    )
}

/// A floor of one's own, for the channels of a timeslot played or recorded
/// by itself: the app's rule and hold, but nobody else's talkers.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LocalFloor(Option<Held>);

impl LocalFloor {
    /// May `who` be heard now? As [`may_play`], on this floor.
    pub(crate) fn may_play(&mut self, who: Talker, talking: bool) -> bool {
        let Some(hold) = hold() else {
            return true;
        };
        decide(
            &mut self.0,
            who,
            talking,
            Instant::now(),
            Duration::from_secs_f32(hold),
        )
    }

    /// The talker holding it, if any.
    pub(crate) fn holder(&self) -> Option<Talker> {
        let hold = Duration::from_secs_f32(hold()?);
        self.0.filter(|h| h.last.elapsed() <= hold).map(|h| h.who)
    }
}

/// A G.728 channel's number on the floor: its timeslot (1–31) and the
/// first of its two bits (1–7, G.704 numbering) in one octet.
pub(crate) fn lane_channel(ts: u8, first_bit: u8) -> u8 {
    ts << 3 | (first_bit & 7)
}

/// A [`lane_channel`] taken apart: (timeslot, first bit).
pub(crate) fn lane_of(channel: u8) -> (u8, u8) {
    (channel >> 3, channel & 7)
}

/// The talker holding the floor, if any (for display).
pub fn holder() -> Option<Talker> {
    let f = FLOOR.lock().unwrap_or_else(|e| e.into_inner());
    let hold = Duration::from_secs_f32(hold()?);
    f.filter(|h| h.last.elapsed() <= hold).map(|h| h.who)
}

/// The floor's rule, on its state `f` at time `now`.
fn decide(f: &mut Option<Held>, who: Talker, talking: bool, now: Instant, hold: Duration) -> bool {
    if let Some(h) = f {
        if h.who == who {
            if talking {
                h.last = now;
            }
            return true;
        }
        // Someone else's: theirs until they have been quiet for the hold.
        if now.duration_since(h.last) <= hold {
            return false;
        }
        *f = None;
    }
    if talking {
        *f = Some(Held { who, last: now });
    }
    talking
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_talker_holds_until_quiet_for_the_hold() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let hold = Duration::from_secs(3);
        let (a, b) = ((1, 1), (2, 1));
        let mut f = None;
        // A speaks first and is heard; B, speaking too, is not.
        assert!(decide(&mut f, a, true, at(0), hold));
        assert!(!decide(&mut f, b, true, at(10), hold));
        // A pauses: still A's for the hold, B still waits.
        assert!(decide(&mut f, a, false, at(1000), hold));
        assert!(!decide(&mut f, b, true, at(2900), hold));
        // A speaks again before the hold runs out: the hold starts over.
        assert!(decide(&mut f, a, true, at(2950), hold));
        assert!(!decide(&mut f, b, true, at(5900), hold));
        // A quiet for over 3 s: B takes the floor.
        assert!(decide(&mut f, b, true, at(6000), hold));
        assert!(!decide(&mut f, a, true, at(6010), hold));
        // Nobody talking and the hold run out: nobody holds it.
        assert!(!decide(&mut f, a, false, at(9100), hold));
        assert!(f.is_none());
    }

    #[test]
    fn lane_channels_go_both_ways() {
        // The ML–PH trunk's channel 18h is timeslot 3; its lanes start at
        // bits 3, 5 and 7.
        for (ts, bit) in [(3, 3), (3, 7), (31, 7), (1, 1)] {
            assert_eq!(lane_of(lane_channel(ts, bit)), (ts, bit));
        }
        assert_ne!(lane_channel(3, 3), lane_channel(3, 7));
    }
}
