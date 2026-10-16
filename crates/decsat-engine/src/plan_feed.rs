// made by CasualArclamp
//! Burst plans passed between VFOs, inside the engine.
//!
//! A hub's outbound carries the plan for its return (a Newtec hub's table
//! D1h, [`decsat_ts::newtec`]); a Return pool VFO on that return needs each
//! frame of it as it is read, a third of a second before its bursts come,
//! to cut them out of its own baseband. The GUI's few looks a second at the
//! VFOs' views are too late and drop frames between looks, so the
//! outbound's FEC thread posts here instead: every plan frame as it is
//! read, and the plan's view (learned widths, arrival line) every half
//! second. A reader takes what came from every other VFO since it last
//! looked.
//!
//! A hub that signals in GSE instead ([`decsat_gse::inroute`]: a plan per
//! inroute a 125 ms frame) posts on a channel of its own, read the same way.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use decsat_gse::inroute::{HubView, InroutePlan};
use decsat_ts::newtec::{PlanFrame, PlanView};

use crate::vfo::VfoId;

/// Frames kept per outbound for readers that have not looked: five seconds
/// of 40 ms frames.
const KEEP_FRAMES: usize = 128;

/// Inroute plans kept per outbound: five seconds of six inroutes.
const KEEP_HUB_PLANS: usize = 256;

#[derive(Default)]
struct Feed {
    view: Option<Arc<PlanView>>,
    /// Each frame with its number in the feed.
    frames: VecDeque<(u64, PlanFrame)>,
    next: u64,
}

#[derive(Default)]
struct HubFeed {
    view: Option<Arc<HubView>>,
    plans: VecDeque<(u64, InroutePlan)>,
    next: u64,
}

/// The engine's plans, by the VFO they come from.
#[derive(Default)]
pub struct PlanFeed {
    feeds: Mutex<BTreeMap<VfoId, Feed>>,
    hubs: Mutex<BTreeMap<VfoId, HubFeed>>,
}

/// What one GSE-signalling outbound posted since a reader last looked.
#[derive(Debug, Clone)]
pub struct HubNews {
    pub from: VfoId,
    pub view: Option<Arc<HubView>>,
    /// Plans new to the reader, oldest first.
    pub plans: Vec<InroutePlan>,
}

/// What one outbound posted since a reader last looked.
#[derive(Debug, Clone)]
pub struct PlanNews {
    pub from: VfoId,
    /// Its plan's latest view.
    pub view: Option<Arc<PlanView>>,
    /// Frames new to the reader, oldest first.
    pub frames: Vec<PlanFrame>,
}

impl PlanFeed {
    /// Frames read on VFO `from`'s outbound.
    pub fn post_frames(&self, from: VfoId, frames: Vec<PlanFrame>) {
        if frames.is_empty() {
            return;
        }
        let mut feeds = self.feeds.lock().unwrap();
        let f = feeds.entry(from).or_default();
        for fr in frames {
            f.frames.push_back((f.next, fr));
            f.next += 1;
        }
        while f.frames.len() > KEEP_FRAMES {
            f.frames.pop_front();
        }
    }

    /// VFO `from`'s plan as it now stands.
    pub fn post_view(&self, from: VfoId, view: PlanView) {
        self.feeds.lock().unwrap().entry(from).or_default().view = Some(Arc::new(view));
    }

    /// VFO `from` is gone.
    pub fn forget(&self, from: VfoId) {
        self.feeds.lock().unwrap().remove(&from);
        self.hubs.lock().unwrap().remove(&from);
    }

    /// Inroute plans read on VFO `from`'s outbound.
    pub fn post_hub_plans(&self, from: VfoId, plans: Vec<InroutePlan>) {
        if plans.is_empty() {
            return;
        }
        let mut hubs = self.hubs.lock().unwrap();
        let f = hubs.entry(from).or_default();
        for p in plans {
            f.plans.push_back((f.next, p));
            f.next += 1;
        }
        while f.plans.len() > KEEP_HUB_PLANS {
            f.plans.pop_front();
        }
    }

    /// VFO `from`'s hub signalling as it now stands.
    pub fn post_hub_view(&self, from: VfoId, view: HubView) {
        self.hubs.lock().unwrap().entry(from).or_default().view = Some(Arc::new(view));
    }

    /// What every VFO but `reader` posted of its hub's signalling since
    /// `cursor` (updated here).
    pub fn read_hubs(&self, reader: VfoId, cursor: &mut BTreeMap<VfoId, u64>) -> Vec<HubNews> {
        let hubs = self.hubs.lock().unwrap();
        cursor.retain(|id, _| hubs.contains_key(id));
        hubs.iter()
            .filter(|(id, _)| **id != reader)
            .map(|(&id, f)| {
                let from = cursor.entry(id).or_insert(0);
                let plans = f
                    .plans
                    .iter()
                    .filter(|(n, _)| *n >= *from)
                    .map(|(_, p)| p.clone())
                    .collect();
                *from = f.next;
                HubNews {
                    from: id,
                    view: f.view.clone(),
                    plans,
                }
            })
            .collect()
    }

    /// What every VFO but `reader` posted since `cursor` (the next frame
    /// number wanted from each, updated here).
    pub fn read(&self, reader: VfoId, cursor: &mut BTreeMap<VfoId, u64>) -> Vec<PlanNews> {
        let feeds = self.feeds.lock().unwrap();
        cursor.retain(|id, _| feeds.contains_key(id));
        feeds
            .iter()
            .filter(|(id, _)| **id != reader)
            .map(|(&id, f)| {
                let from = cursor.entry(id).or_insert(0);
                let frames = f
                    .frames
                    .iter()
                    .filter(|(n, _)| *n >= *from)
                    .map(|(_, fr)| fr.clone())
                    .collect();
                *from = f.next;
                PlanNews {
                    from: id,
                    view: f.view.clone(),
                    frames,
                }
            })
            .collect()
    }
}

/// A VFO's end of the engine's [`PlanFeed`]: the feed and the VFO's id.
#[derive(Clone)]
pub struct PlanLink {
    pub id: VfoId,
    pub feed: Arc<PlanFeed>,
}

// Rust note: `FecOutput`, which carries a link to its FEC thread, is
// compared to see whether it changed, so the link must compare too: the
// same VFO on the same feed.
impl PartialEq for PlanLink {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.feed, &other.feed)
    }
}

impl std::fmt::Debug for PlanLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlanLink(VFO {})", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(counter: u32) -> PlanFrame {
        PlanFrame {
            counter,
            time: 0,
            bursts: Vec::new(),
            arrival_s: 0.0,
        }
    }

    #[test]
    fn readers_get_each_frame_once_from_other_vfos() {
        let feed = PlanFeed::default();
        let (mut a, mut b) = (BTreeMap::new(), BTreeMap::new());
        feed.post_frames(1, vec![frame(10), frame(11)]);
        feed.post_view(1, PlanView::default());
        // VFO 1 reads nothing of its own; VFO 2 gets both, then nothing.
        assert!(feed.read(1, &mut a).is_empty());
        let news = feed.read(2, &mut b);
        assert_eq!(news.len(), 1);
        assert!(news[0].view.is_some());
        let counters: Vec<u32> = news[0].frames.iter().map(|f| f.counter).collect();
        assert_eq!(counters, [10, 11]);
        assert!(feed.read(2, &mut b)[0].frames.is_empty());
        // More than are kept: the reader gets the latest.
        feed.post_frames(1, (12..12 + 200).map(frame).collect());
        let news = feed.read(2, &mut b);
        assert_eq!(news[0].frames.len(), KEEP_FRAMES);
        assert_eq!(news[0].frames.last().unwrap().counter, 211);
        feed.forget(1);
        assert!(feed.read(2, &mut b).is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn hub_plans_reach_other_vfos_once() {
        let feed = PlanFeed::default();
        let mut c = BTreeMap::new();
        let plan = |frame| InroutePlan {
            frame,
            inroute: 0x2e,
            owners: Vec::new(),
            extra: Vec::new(),
            arrival_s: None,
        };
        feed.post_hub_plans(3, vec![plan(1), plan(2)]);
        feed.post_hub_view(3, HubView::default());
        assert!(feed.read_hubs(3, &mut BTreeMap::new()).is_empty());
        let news = feed.read_hubs(4, &mut c);
        assert_eq!(news.len(), 1);
        assert_eq!(news[0].plans.len(), 2);
        assert!(news[0].view.is_some());
        assert!(feed.read_hubs(4, &mut c)[0].plans.is_empty());
        feed.forget(3);
        assert!(feed.read_hubs(4, &mut c).is_empty());
    }
}
