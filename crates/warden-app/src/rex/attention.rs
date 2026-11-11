//! Attention from agent tabs: which blocks need you, and when one raises a banner.
//!
//! An agent tab needs you while its block is `blocked`, or `done` (`error` alike) and not yet
//! seen — "seen" being the tab on screen after the state changed. A hidden tab moving into
//! either raises attention once: the project's sidebar row badge and a banner
//! ([`super::lifecycle::attend`] → `notify::raise`). Reports that leave the state as it was (a new
//! title, a new message) raise nothing, and neither does a block's first report, so launch counts
//! as everything seen. Held in memory only.

use std::collections::{HashMap, HashSet};

use super::windows::AgentState;

/// What each block's program last reported, and whether a `done` it reached is still unseen.
/// A `dismissed` block's agent tab is closing or going to sleep: its program ending as its window
/// closes reports `done` or `error` while the record still names the block, so it never needs you.
#[derive(Debug, Default)]
pub struct Attention {
    blocks: HashMap<String, Mark>,
    dismissed: HashSet<String>,
}

#[derive(Debug, Clone, Copy)]
struct Mark {
    state: AgentState,
    unseen: bool,
}

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

impl Attention {
    /// `block` reported `state` while its tab was `visible` or not. True when that raises
    /// attention: a hidden tab whose need changed into `blocked` or `done`.
    pub fn observe(&mut self, block: &str, state: AgentState, visible: bool) -> bool {
        let Some(mark) = self.blocks.get_mut(block) else {
            self.blocks.insert(
                block.to_owned(),
                Mark {
                    state,
                    unseen: false,
                },
            );
            return false;
        };
        let changed = need(mark.state) != need(state);
        mark.state = state;
        if visible || need(state) == Need::Nothing || self.dismissed.contains(block) {
            mark.unseen = false;
            return false;
        }
        if changed {
            mark.unseen = true;
        }
        changed
    }

    /// `block`'s tab is on screen. True if it had something unseen.
    pub fn seen(&mut self, block: &str) -> bool {
        self.blocks
            .get_mut(block)
            .is_some_and(|m| std::mem::replace(&mut m.unseen, false))
    }

    /// Whether `block`, now in `state`, needs you: blocked, or done and not yet seen.
    pub fn needs_you(&self, block: &str, state: AgentState) -> bool {
        if self.dismissed.contains(block) {
            return false;
        }
        match need(state) {
            Need::Blocked => true,
            Need::Done => self.blocks.get(block).is_some_and(|m| m.unseen),
            Need::Nothing => false,
        }
    }

    /// `block`'s agent tab is closing or going to sleep: it raises nothing from here on.
    pub fn dismiss(&mut self, block: &str) {
        self.dismissed.insert(block.to_owned());
        if let Some(m) = self.blocks.get_mut(block) {
            m.unseen = false;
        }
    }

    /// The close or sleep that dismissed `block` failed, so its agent tab stays live.
    pub fn restore(&mut self, block: &str) {
        self.dismissed.remove(block);
    }

    /// Stop tracking `block` (it belongs to no agent tab any more).
    pub fn forget(&mut self, block: &str) {
        self.blocks.remove(block);
        self.dismissed.remove(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::*;

    /// Launch (or a block's first report) is the baseline, so every block starts from one.
    fn tracking(block: &str, state: AgentState) -> Attention {
        let mut a = Attention::default();
        assert!(
            !a.observe(block, state, false),
            "a first report is the baseline"
        );
        a
    }

    #[test]
    fn blocked_hidden_tab_banners_once() {
        let mut a = tracking("b", Working);
        assert!(
            a.observe("b", Blocked, false),
            "hidden and blocked: a banner"
        );
        assert!(
            !a.observe("b", Blocked, false),
            "the same state again: none"
        );
        assert!(!a.observe("b", Blocked, false));
        assert!(a.needs_you("b", Blocked));

        assert!(!a.observe("b", Working, false), "working needs nothing");
        assert!(
            a.observe("b", Blocked, false),
            "a new transition: a new banner"
        );

        assert!(a.seen("b"));
        assert!(
            !a.observe("b", Blocked, false),
            "seen, and nothing changed: none"
        );
        assert!(a.needs_you("b", Blocked), "blocked needs you, seen or not");

        assert!(a.observe("b", Done, false), "done is a transition too");
        assert!(
            !a.observe("b", Error, false),
            "error reads as done: no second banner"
        );
    }

    #[test]
    fn selected_visible_tab_raises_nothing() {
        let mut a = tracking("b", Working);
        assert!(!a.observe("b", Blocked, true));
        assert!(!a.observe("b", Done, true));
        assert!(!a.needs_you("b", Done), "done while on screen is seen");
        assert!(
            !a.observe("b", Done, false),
            "hidden later, nothing changed: none"
        );
        assert!(!a.seen("b"), "already seen");
    }

    #[test]
    fn done_unseen_needs_you_until_seen() {
        let mut a = tracking("b", Working);
        assert!(a.observe("b", Done, false));
        assert!(a.needs_you("b", Done));
        assert!(a.seen("b"));
        assert!(!a.needs_you("b", Done));
        assert!(!a.needs_you("b", Idle));
        assert!(!a.needs_you("other", Done), "an unknown block is seen");
    }

    #[test]
    fn a_dismissed_block_never_raises() {
        let mut a = tracking("b", Working);
        assert!(a.observe("b", Done, false));
        a.dismiss("b");
        assert!(!a.needs_you("b", Done), "dismissing clears what was unseen");
        assert!(!a.observe("b", Working, false));
        assert!(
            !a.observe("b", Error, false),
            "its program ending as the window closes: none"
        );
        assert!(!a.needs_you("b", Blocked));

        a.restore("b");
        assert!(a.observe("b", Blocked, false), "restored: it raises again");
        a.dismiss("b");
        a.forget("b");
        assert!(
            !a.observe("b", Working, false),
            "forgotten: a baseline again"
        );
        assert!(a.observe("b", Blocked, false), "and no longer dismissed");
    }

    #[test]
    fn a_first_report_never_raises() {
        let mut a = Attention::default();
        assert!(
            !a.observe("b", Done, false),
            "launch counts as everything seen"
        );
        assert!(!a.needs_you("b", Done));
        a.forget("b");
        assert!(
            !a.observe("b", Blocked, false),
            "forgotten: a baseline again"
        );
    }
}
