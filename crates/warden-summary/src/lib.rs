//! The done/now/next summary pipeline's pure logic: transcript readers here, the model call and
//! the refresh state machine beside them. No GUI, no async.

mod claude;
mod gate;
mod model;
mod prompt;
mod refresh;
mod rows;

pub use claude::Claude;
pub use gate::strip_unbacked_done;
pub use model::{HttpModel, Model};
pub use prompt::{label, stand};
pub use refresh::{
    apply, due, refresh, RefreshResult, SummaryState, DIGEST_BUDGET, DRIFT_TICKS, LABEL_WORDS,
    STAND_WORDS, SUBJECT_MIN_SEGMENTS, THROTTLE,
};
pub use rows::{parse_stand, StandRows};

use std::path::Path;

/// Which end of a transcript [`Transcript::context`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxMode {
    /// The earliest turns, read from the file start: the session's stated goal. Bounding this by
    /// the file's tail would return mid-session turns on a long transcript.
    Head,
    /// The most recent turns: current activity.
    Tail,
    /// The agent's latest task-list snapshot: the session's umbrella goal.
    Todos,
}

/// A coding agent's session transcript, reduced to the prose a summariser should see. Every
/// failure (missing file, unreadable, malformed) yields `""`/`0`: the callers are cosmetic and
/// keep their last good value rather than show an error.
pub trait Transcript {
    /// Up to `max_msgs` prose turns (each cut to `percap` chars) joined by `" / "`.
    fn context(&self, path: &Path, max_msgs: usize, percap: usize, mode: CtxMode) -> String;
    /// A chronological digest of the transcript from 1-based `start_line` to the end, trimmed to
    /// `char_budget` by dropping the oldest prose first.
    fn digest(&self, path: &Path, start_line: usize, char_budget: usize) -> String;
    /// Newline count, the cursor a later `digest` resumes from.
    fn line_count(&self, path: &Path) -> usize;
}
