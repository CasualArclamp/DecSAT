// made by CasualArclamp
//! The packets between ffmpeg and the modulator: a bounded ring, and the
//! modulator's side of it as a [`TsInput`].
//!
//! The reader thread (crate::feeder) pushes; when the ring is full it
//! blocks, which fills ffmpeg's pipe, which holds ffmpeg back (a file
//! encodes no faster than the channel sends). The modulator pops without
//! ever waiting: an empty ring gives a null packet (counted), so the
//! carrier never stalls.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use decsat_mod::datv::{TS_LEN, TsInput};

/// A bounded FIFO of TS packets.
pub struct TsRing {
    q: Mutex<VecDeque<[u8; TS_LEN]>>,
    room: Condvar,
    cap: usize,
    closed: AtomicBool,
    /// While non-zero the ring holds at most this many, the oldest dropped
    /// (a live source waiting for the radio stays fresh).
    hold: AtomicUsize,
    /// Packets the modulator wanted when the ring was empty.
    pub starved: AtomicU64,
    /// Packets taken by the modulator.
    pub taken: AtomicU64,
}

impl TsRing {
    pub fn new(cap: usize) -> Arc<TsRing> {
        Arc::new(TsRing {
            q: Mutex::new(VecDeque::with_capacity(cap)),
            room: Condvar::new(),
            cap: cap.max(16),
            closed: AtomicBool::new(false),
            hold: AtomicUsize::new(0),
            starved: AtomicU64::new(0),
            taken: AtomicU64::new(0),
        })
    }

    /// Add packets, waiting while it is full. `false` once closed.
    pub fn push_blocking(&self, packets: &[[u8; TS_LEN]]) -> bool {
        let mut q = self.q.lock().unwrap();
        for p in packets {
            let hold = self.hold.load(Ordering::Relaxed);
            if hold > 0 && q.len() >= hold {
                q.pop_front();
            }
            while q.len() >= self.cap {
                if self.closed.load(Ordering::Relaxed) {
                    return false;
                }
                // Rust: `wait_timeout` gives the lock back while waiting,
                // and a timeout so a close is noticed.
                q = self
                    .room
                    .wait_timeout(q, Duration::from_millis(50))
                    .unwrap()
                    .0;
            }
            q.push_back(*p);
        }
        !self.closed.load(Ordering::Relaxed)
    }

    pub fn try_pop(&self) -> Option<[u8; TS_LEN]> {
        let p = self.q.lock().unwrap().pop_front();
        if p.is_some() {
            self.room.notify_one();
        }
        p
    }

    pub fn len(&self) -> usize {
        self.q.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Keep at most `n` packets (the oldest dropped) until `None`.
    pub fn set_hold(&self, n: Option<usize>) {
        self.hold.store(n.unwrap_or(0), Ordering::Relaxed);
    }

    /// Wake and refuse the producer (on stop).
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.room.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// The modulator's input: whatever the ring holds, nulls when it is empty.
pub struct LiveTs {
    ring: Arc<TsRing>,
    what: String,
    ended: Arc<AtomicBool>,
}

impl LiveTs {
    /// `ended` is set by the feeder when ffmpeg has finished (a file played
    /// once to its end).
    pub fn new(ring: Arc<TsRing>, what: String, ended: Arc<AtomicBool>) -> Self {
        LiveTs { ring, what, ended }
    }
}

impl TsInput for LiveTs {
    fn next_packet(&mut self, _now: f64) -> Option<[u8; TS_LEN]> {
        let p = self.ring.try_pop();
        if p.is_some() {
            self.ring.taken.fetch_add(1, Ordering::Relaxed);
        } else if !self.ended.load(Ordering::Relaxed) {
            self.ring.starved.fetch_add(1, Ordering::Relaxed);
        }
        p
    }

    fn describe(&self) -> String {
        self.what.clone()
    }

    fn ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed) && self.ring.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_ring_holds_the_producer_and_an_empty_one_starves() {
        let r = TsRing::new(16);
        let mut p = [0u8; TS_LEN];
        p[0] = 0x47;
        let r2 = r.clone();
        let t = std::thread::spawn(move || r2.push_blocking(&[p; 20]));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(r.len(), 16, "the producer waits at the cap");
        let ended = Arc::new(AtomicBool::new(false));
        let mut live = LiveTs::new(r.clone(), "test".into(), ended);
        let mut n = 0;
        while live.next_packet(0.0).is_some() {
            n += 1;
            if n == 20 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(t.join().unwrap());
        assert_eq!(n, 20);
        assert!(live.next_packet(0.0).is_none());
        assert_eq!(r.starved.load(Ordering::Relaxed), 1);
        r.close();
        assert!(!r.push_blocking(&[p]));
    }
}
