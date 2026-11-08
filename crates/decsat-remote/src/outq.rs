// made by CasualArclamp
//! What is waiting to go out to one client, by priority. One TCP connection
//! carries everything, so the order things leave in is the only lever there
//! is when the link is short: control and TS go first, the display data
//! fills what is left and is thinned or replaced by newer data.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::wire::Kind;

/// TS waiting beyond this is dropped (a few seconds of a typical carrier).
const TS_MAX_BYTES: usize = 16 << 20;
/// Rows waiting beyond this push out the oldest.
const ROWS_MAX: usize = 16;
/// Every so many frames the display goes first, so a TS that fills the link
/// cannot freeze the waterfall altogether.
const FAIR_EVERY: u32 = 16;

/// One thing to send.
pub enum Out {
    Frame(Kind, Vec<u8>),
    Row(u64, Vec<f32>),
    Spectrum(u64, Vec<f32>),
}

#[derive(Default)]
struct State {
    control: VecDeque<Vec<u8>>,
    ts: VecDeque<Vec<u8>>,
    ts_bytes: usize,
    ts_lost: u32,
    iq: VecDeque<Vec<u8>>,
    iq_bytes: usize,
    front: Option<Vec<u8>>,
    vfos: BTreeMap<u32, Vec<u8>>,
    spectrum: Option<(u64, Vec<f32>)>,
    rows: VecDeque<(u64, Vec<f32>)>,
    pops: u32,
    closed: bool,
    iq_dropped: u64,
    ts_dropped: u64,
    rows_thinned: u64,
}

impl State {
    fn display(&mut self) -> Option<Out> {
        if let Some(f) = self.front.take() {
            return Some(Out::Frame(Kind::Front, f));
        }
        if let Some((id, _)) = self.vfos.first_key_value() {
            let id = *id;
            return self.vfos.remove(&id).map(|v| Out::Frame(Kind::Vfo, v));
        }
        if let Some((s, t)) = self.spectrum.take() {
            return Some(Out::Spectrum(s, t));
        }
        self.rows.pop_front().map(|(s, r)| Out::Row(s, r))
    }

    fn take(&mut self) -> Option<Out> {
        if let Some(c) = self.control.pop_front() {
            return Some(Out::Frame(Kind::Control, c));
        }
        self.pops = self.pops.wrapping_add(1);
        if self.pops.is_multiple_of(FAIR_EVERY)
            && let Some(d) = self.display()
        {
            return Some(d);
        }
        if let Some(mut t) = self.ts.pop_front() {
            self.ts_bytes -= t.len();
            // The frame carries how many packets went missing before it.
            let lost = std::mem::take(&mut self.ts_lost);
            t[4..8].copy_from_slice(&lost.to_le_bytes());
            return Some(Out::Frame(Kind::Ts, t));
        }
        if let Some(i) = self.iq.pop_front() {
            self.iq_bytes -= i.len();
            return Some(Out::Frame(Kind::Iq, i));
        }
        self.display()
    }
}

/// One client's outgoing queues.
#[derive(Default)]
pub struct OutQueue {
    state: Mutex<State>,
    ready: Condvar,
}

/// Counts for [`crate::msg::LinkStats`].
pub struct Counts {
    pub iq_dropped: u64,
    pub ts_dropped: u64,
    pub rows_thinned: u64,
}

impl OutQueue {
    fn push(&self, f: impl FnOnce(&mut State)) {
        let mut s = self.state.lock().unwrap();
        if !s.closed {
            f(&mut s);
            self.ready.notify_one();
        }
    }

    pub fn control(&self, m: Vec<u8>) {
        self.push(|s| s.control.push_back(m));
    }

    /// `packets` is what is lost if the frame has to be dropped.
    pub fn ts(&self, frame: Vec<u8>, packets: u32) {
        self.push(|s| {
            if s.ts_bytes + frame.len() > TS_MAX_BYTES {
                s.ts_lost = s.ts_lost.saturating_add(packets);
                s.ts_dropped += u64::from(packets);
            } else {
                s.ts_bytes += frame.len();
                s.ts.push_back(frame);
            }
        });
    }

    /// `cap_bytes` bounds the queue; past it the whole frame is dropped.
    pub fn iq(&self, frame: Vec<u8>, samples: u64, cap_bytes: usize) {
        self.push(|s| {
            if s.iq_bytes + frame.len() > cap_bytes {
                s.iq_dropped += samples;
            } else {
                s.iq_bytes += frame.len();
                s.iq.push_back(frame);
            }
        });
    }

    pub fn front(&self, m: Vec<u8>) {
        self.push(|s| s.front = Some(m));
    }

    pub fn vfo(&self, id: u32, m: Vec<u8>) {
        self.push(|s| {
            s.vfos.insert(id, m);
        });
    }

    pub fn forget_vfo(&self, id: u32) {
        self.push(|s| {
            s.vfos.remove(&id);
        });
    }

    pub fn spectrum(&self, seq: u64, t: Vec<f32>) {
        self.push(|s| s.spectrum = Some((seq, t)));
    }

    pub fn row(&self, seq: u64, r: Vec<f32>) {
        self.push(|s| {
            if s.rows.len() >= ROWS_MAX {
                s.rows.pop_front();
                s.rows_thinned += 1;
            }
            s.rows.push_back((seq, r));
        });
    }

    pub fn rows_waiting(&self) -> usize {
        self.state.lock().unwrap().rows.len()
    }

    pub fn thinned(&self, n: u64) {
        self.state.lock().unwrap().rows_thinned += n;
    }

    /// Drop waiting rows down to `keep`, oldest first.
    pub fn trim_rows(&self, keep: usize) {
        let mut s = self.state.lock().unwrap();
        while s.rows.len() > keep {
            s.rows.pop_front();
            s.rows_thinned += 1;
        }
    }

    pub fn counts(&self) -> Counts {
        let s = self.state.lock().unwrap();
        Counts {
            iq_dropped: s.iq_dropped,
            ts_dropped: s.ts_dropped,
            rows_thinned: s.rows_thinned,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Stop accepting; [`pop`](Self::pop) returns `None` once drained of
    /// control messages (a parting message still goes out).
    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        self.ready.notify_all();
    }

    /// The next thing to send, waiting for one. `None` when closed.
    pub fn pop(&self) -> Option<Out> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.closed {
                return s.control.pop_front().map(|c| Out::Frame(Kind::Control, c));
            }
            if let Some(o) = s.take() {
                return Some(o);
            }
            // Rust note: `wait_timeout` releases the lock while it sleeps
            // and hands it back; the timeout only guards against a missed
            // wake-up.
            s = self
                .ready
                .wait_timeout(s, Duration::from_millis(500))
                .unwrap()
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(o: Out) -> Kind {
        match o {
            Out::Frame(k, _) => k,
            Out::Row(..) => Kind::Row,
            Out::Spectrum(..) => Kind::Spectrum,
        }
    }

    #[test]
    fn control_then_ts_then_display() {
        let q = OutQueue::default();
        q.row(1, vec![0.0]);
        q.ts(vec![0; 16], 1);
        q.control(vec![1]);
        assert_eq!(kind(q.pop().unwrap()), Kind::Control);
        assert_eq!(kind(q.pop().unwrap()), Kind::Ts);
        assert_eq!(kind(q.pop().unwrap()), Kind::Row);
    }

    #[test]
    fn newer_display_replaces_older() {
        let q = OutQueue::default();
        q.vfo(1, vec![1]);
        q.vfo(1, vec![2]);
        match q.pop().unwrap() {
            Out::Frame(Kind::Vfo, v) => assert_eq!(v, vec![2]),
            _ => panic!(),
        }
    }

    #[test]
    fn dropped_ts_is_reported_on_the_next_frame() {
        let q = OutQueue::default();
        q.ts(vec![0; TS_MAX_BYTES], 7);
        q.ts(vec![0; 16], 3); // no room
        match q.pop().unwrap() {
            Out::Frame(Kind::Ts, t) => assert_eq!(&t[4..8], &3u32.to_le_bytes()),
            _ => panic!(),
        }
        assert_eq!(q.counts().ts_dropped, 3);
    }

    #[test]
    fn the_display_is_not_starved_by_ts() {
        let q = OutQueue::default();
        for _ in 0..100 {
            q.ts(vec![0; 16], 1);
        }
        q.row(1, vec![0.0]);
        let first = (0..FAIR_EVERY as usize + 1)
            .position(|_| kind(q.pop().unwrap()) == Kind::Row)
            .expect("a row within the fairness window");
        assert!(first < FAIR_EVERY as usize);
    }

    #[test]
    fn rows_are_bounded() {
        let q = OutQueue::default();
        for i in 0..40 {
            q.row(i, vec![]);
        }
        assert_eq!(q.rows_waiting(), ROWS_MAX);
        assert_eq!(q.counts().rows_thinned, 24);
    }
}
