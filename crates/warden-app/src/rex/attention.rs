//! Attention from agent tabs: which need you, and when one raises a banner.
//!
//! An agent tab needs you while its block is `blocked`, or `done` (`error` alike) at a stamp it
//! wasn't seen at ([`needs_you`]): Rex's record stamp against the agent record's `seen`, both of
//! which outlive warden. Everything that shows a need — the agent row, its group, a folded fork,
//! the tally, the project row's badge — reads that one answer from the agent list.
//!
//! [`Attention`] decides only banners: a block moving into `blocked` or `done` raises one, once.
//! It also holds which blocks are going away. Held in memory; a block's first report is the
//! baseline, so a launch raises no banner.

use std::collections::{HashMap, HashSet};

use super::windows::AgentState;

/// What a state asks of you: `error` reads as `done`, so moving between the two is no change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    Nothing,
    Blocked,
    Done,
}

fn need(state: AgentState) -> Need {
    match state {
        AgentState::Blocked => Need::Blocked,
        AgentState::Done | AgentState::Error => Need::Done,
        AgentState::Working | AgentState::Idle => Need::Nothing,
    }
}

/// Whether a block in `state`, reported at stamp `at`, needs you, its agent record having last
/// seen a finish at `seen`: blocked, or finished at a stamp not yet seen.
pub fn needs_you(state: AgentState, at: Option<&str>, seen: Option<&str>) -> bool {
    match need(state) {
        Need::Blocked => true,
        Need::Done => at.is_some_and(|at| seen != Some(at)),
        Need::Nothing => false,
    }
}

/// Whether a finish at `at` is one still to see: the stamp [`needs_you`] compares, `None` for a
/// block in any other state or already seen.
pub fn unseen(state: AgentState, at: Option<&str>, seen: Option<&str>) -> Option<String> {
    if need(state) == Need::Done && needs_you(state, at, seen) {
        at.map(str::to_owned)
    } else {
        None
    }
}

/// Each block's last need, for banners, and the blocks going away. A `dismissed` block's agent
/// tab is closing or going to sleep: its program ending as its window closes reports `done` or
/// `error` while the record still names the block, so it never needs you.
#[derive(Debug, Default)]
pub struct Attention {
    needs: HashMap<String, Need>,
    dismissed: HashSet<String>,
}

impl Attention {
    /// `block` reported `state`. True when that raises a banner: its need changed into `blocked`
    /// or `done` since a report before this one, and it isn't going away.
    pub fn observe(&mut self, block: &str, state: AgentState) -> bool {
        let now = need(state);
        let was = self.needs.insert(block.to_owned(), now);
        now != Need::Nothing && was.is_some_and(|w| w != now) && !self.is_dismissed(block)
    }

    /// `block`'s agent tab is closing or going to sleep.
    pub fn is_dismissed(&self, block: &str) -> bool {
        self.dismissed.contains(block)
    }

    /// `block`'s agent tab is closing or going to sleep: it needs nothing from here on.
    pub fn dismiss(&mut self, block: &str) {
        self.dismissed.insert(block.to_owned());
    }

    /// The close or sleep that dismissed `block` failed, so its agent tab stays live.
    pub fn restore(&mut self, block: &str) {
        self.dismissed.remove(block);
    }

    /// Stop tracking `block` (it belongs to no agent tab any more).
    pub fn forget(&mut self, block: &str) {
        self.needs.remove(block);
        self.dismissed.remove(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::*;

    #[test]
    fn blocked_needs_you_seen_or_not() {
        assert!(needs_you(Blocked, None, None));
        assert!(needs_you(Blocked, Some("t1"), Some("t1")));
        assert!(!needs_you(Working, Some("t1"), None));
        assert!(!needs_you(Idle, Some("t1"), None));
    }

    #[test]
    fn a_finish_needs_you_until_its_stamp_is_seen() {
        assert!(needs_you(Done, Some("t1"), None));
        assert!(needs_you(Error, Some("t1"), None), "error reads as done");
        assert!(!needs_you(Done, Some("t1"), Some("t1")), "seen");
        assert!(needs_you(Done, Some("t2"), Some("t1")), "a later finish");
        assert!(!needs_you(Done, None, None), "no stamp, nothing to see");
        assert_eq!(unseen(Done, Some("t2"), Some("t1")).as_deref(), Some("t2"));
        assert_eq!(unseen(Done, Some("t1"), Some("t1")), None);
        assert_eq!(
            unseen(Blocked, Some("t1"), None),
            None,
            "blocked has no seen"
        );
    }

    #[test]
    fn a_banner_once_per_change_into_a_need() {
        let mut a = Attention::default();
        assert!(!a.observe("b", Working), "a first report is the baseline");
        assert!(a.observe("b", Blocked), "into blocked");
        assert!(!a.observe("b", Blocked), "the same state again");
        assert!(!a.observe("b", Working), "working needs nothing");
        assert!(a.observe("b", Done), "into done");
        assert!(!a.observe("b", Error), "error reads as done");
        assert!(a.observe("b", Blocked), "done to blocked is a change");
    }

    #[test]
    fn a_first_report_never_banners() {
        let mut a = Attention::default();
        assert!(!a.observe("b", Done), "launch counts as everything seen");
        a.forget("b");
        assert!(!a.observe("b", Blocked), "forgotten: a baseline again");
    }

    #[test]
    fn a_dismissed_block_never_banners() {
        let mut a = Attention::default();
        a.observe("b", Working);
        a.dismiss("b");
        assert!(a.is_dismissed("b"));
        assert!(
            !a.observe("b", Error),
            "its program ending as the window closes"
        );
        a.restore("b");
        assert!(a.observe("b", Blocked), "restored: it raises again");
        a.dismiss("b");
        a.forget("b");
        assert!(!a.is_dismissed("b"), "forgotten: no longer dismissed");
    }
}
