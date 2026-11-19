// made by CasualArclamp
//! Even scrolling for the waterfall.
//!
//! Rows reach the window in bursts: a radio hands over its samples a buffer
//! at a time (a Pluto at 6 MS/s: about thirteen rows every 200 ms, then
//! nothing for three or four frames), and a file's rows come a block at a
//! time. Drawn as they arrive, the waterfall jerks. The pacer keeps them in
//! a queue and lets them out at the rate they arrive at on average, so the
//! picture moves the same distance every frame; the fraction of a row that
//! is due but not yet out is returned too, and the waterfall is drawn that
//! far on, so the motion is smooth between rows as well.
//!
//! The rate is the arrivals of the last few seconds, trimmed so the queue's
//! low point of the last half second sits at a few rows: deep enough to
//! bridge the gaps between bursts, and no deeper (the waterfall lags the
//! signal by what is queued).

use std::collections::VecDeque;

/// Rows the queue is steered to keep at its lowest.
const MARGIN: f64 = 3.0;
/// Arrivals are counted over this long, seconds.
const WINDOW_S: f64 = 3.0;
/// Frames the queue's low point is taken over.
const LOW_FRAMES: usize = 30;

pub struct Pacer<T> {
    queue: VecDeque<T>,
    /// When rows came and how many, over the last `WINDOW_S`.
    arrivals: VecDeque<(f64, usize)>,
    /// The queue's length at each of the last frames, before rows came.
    lows: VecDeque<usize>,
    /// Rows due and not yet let out (under one, unless the queue ran dry).
    credit: f64,
    last: Option<f64>,
}

// Rust note: `derive(Default)` would ask for `T: Default`, which a row is
// not; written by hand it needs nothing of `T`.
impl<T> Default for Pacer<T> {
    fn default() -> Self {
        Pacer {
            queue: VecDeque::new(),
            arrivals: VecDeque::new(),
            lows: VecDeque::new(),
            credit: 0.0,
            last: None,
        }
    }
}

impl<T> Pacer<T> {
    pub fn clear(&mut self) {
        *self = Pacer::default();
    }

    /// A frame at `now` (seconds): `new` rows came, `nominal` is the rate
    /// they should come at (used until enough have been seen). Returns the
    /// rows to draw now, oldest first, and how far towards the next the
    /// picture is (0 to 1).
    pub fn frame(&mut self, now: f64, nominal: f64, new: Vec<T>) -> (Vec<T>, f32) {
        self.lows.push_back(self.queue.len());
        while self.lows.len() > LOW_FRAMES {
            self.lows.pop_front();
        }
        if !new.is_empty() {
            self.arrivals.push_back((now, new.len()));
            self.queue.extend(new);
        }
        while self.arrivals.front().is_some_and(|a| now - a.0 > WINDOW_S) {
            self.arrivals.pop_front();
        }
        // The rows after the first burst, over the time since it.
        let measured = match (self.arrivals.front(), self.arrivals.back()) {
            (Some(a), Some(b)) if b.0 - a.0 >= 1.0 => {
                let rows: usize = self.arrivals.iter().skip(1).map(|x| x.1).sum();
                Some(rows as f64 / (b.0 - a.0))
            }
            _ => None,
        };
        let base = measured.unwrap_or(nominal).max(1.0);
        let low = self.lows.iter().copied().min().unwrap_or(0) as f64;
        // Two rows a second faster for each row the low point is over.
        let rate = (base + 2.0 * (low - MARGIN)).clamp(0.7 * base, 1.5 * base);
        let dt = self.last.map_or(0.0, |t| (now - t).clamp(0.0, 0.1));
        self.last = Some(now);
        self.credit += dt * rate;

        let mut out = Vec::new();
        // Far behind (a file played fast, a window that stalled): all but a
        // quarter of a second goes out at once.
        let most = (base as usize).max(60);
        if self.queue.len() > most {
            let keep = most / 4;
            out.extend(self.queue.drain(..self.queue.len() - keep));
            self.lows.clear();
        }
        while self.credit >= 1.0 {
            match self.queue.pop_front() {
                Some(r) => {
                    out.push(r);
                    self.credit -= 1.0;
                }
                // Ran dry: the picture waits at the newest row.
                None => {
                    self.credit = 1.0;
                    break;
                }
            }
        }
        (out, self.credit.min(1.0) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Pluto's bursts (13 rows every 200 ms, as filmed) come out a row a
    /// frame at 60 frames a second: after settling, the distance moved a
    /// frame (rows out plus the change in the fraction) stays within a
    /// fifth of a row of the mean, and the queue never runs dry.
    #[test]
    fn bursts_come_out_evenly() {
        let mut p = Pacer::<u32>::default();
        let (mut at, mut steps) = (0.0f64, Vec::new());
        let mut n = 0u32;
        for frame in 0..1200 {
            let now = frame as f64 / 60.0;
            let new: Vec<u32> = if frame % 12 == 0 {
                (0..13)
                    .map(|_| {
                        n += 1;
                        n
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let (out, frac) = p.frame(now, 60.0, new);
            let pos = at + out.len() as f64;
            let moved = pos + f64::from(frac);
            at = pos;
            if frame >= 600 {
                steps.push(moved);
            }
        }
        let d: Vec<f64> = steps.windows(2).map(|w| w[1] - w[0]).collect();
        let mean = d.iter().sum::<f64>() / d.len() as f64;
        assert!((mean - 65.0 / 60.0).abs() < 0.02, "mean {mean}");
        for x in &d {
            assert!(
                (x - mean).abs() < 0.2,
                "a frame moved {x} rows, mean {mean}"
            );
        }
    }

    /// A backlog (a file played fast) is let out at once, in order.
    #[test]
    fn a_backlog_goes_out_at_once() {
        let mut p = Pacer::<u32>::default();
        let (out, _) = p.frame(0.0, 60.0, (0..1000).collect());
        assert_eq!(out.len(), 985);
        assert!(out.windows(2).all(|w| w[0] < w[1]));
    }
}
