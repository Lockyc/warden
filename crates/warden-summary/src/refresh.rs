//! The refresh state machine: when and how a session's subject and done/now/next stand line are
//! derived from its transcript. Port of agentmux's `tmux-status.sh` refresh job; the failure
//! taxonomy it must not regress is in agentmux's `docs/ai-summary.md`.

use crate::{
    label, parse_stand, stand, strip_unbacked_done, CtxMode, Model, StandRows, Transcript,
};
use std::path::Path;
use std::time::{Duration, Instant};

/// Minimum gap between prompt-less refreshes.
pub const THROTTLE: Duration = Duration::from_secs(30);
/// Consecutive off-subject probes that make a drift sustained.
pub const DRIFT_TICKS: u32 = 3;
/// Head segments needed before a first subject is derived (about the first fifth of a session).
pub const SUBJECT_MIN_SEGMENTS: usize = 6;
/// Char budget handed to `Transcript::digest`.
pub const DIGEST_BUDGET: usize = 10_000;
/// Word ceiling of a subject label.
pub const LABEL_WORDS: usize = 6;
/// Word ceiling of a stand line.
pub const STAND_WORDS: usize = 55;

const SEP: &str = " / ";

/// A session's summary state. The caller owns it and applies results with [`apply`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SummaryState {
    /// The stable session subject, once derived.
    pub subject: Option<String>,
    /// Consecutive ticks the recent work shared no content word with `subject`.
    pub drift: u32,
    /// 1-based transcript line the stand digest starts from; 0 (default) reads as 1.
    pub start_line: usize,
    /// Bumped by the caller when the transcript is replaced; results of an older generation are
    /// discarded.
    pub generation: u64,
    /// When the last refresh was attempted; stamped before the call so failures throttle too.
    pub last_attempt: Option<Instant>,
    /// The last good stand rows.
    pub rows: Option<StandRows>,
}

/// What one [`refresh`] decided; [`apply`] folds it into a [`SummaryState`].
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshResult {
    pub generation: u64,
    pub subject: Option<String>,
    pub drift: u32,
    pub start_line: usize,
    /// The new stand rows, or `None` when the stand failed and the old rows stand.
    pub rows: Option<StandRows>,
}

/// Whether a refresh may start: never attempted, or the last attempt is older than `throttle`.
pub fn due(state: &SummaryState, now: Instant, throttle: Duration) -> bool {
    state
        .last_attempt
        .is_none_or(|t| now.saturating_duration_since(t) >= throttle)
}

/// Fold `result` into `state`. Returns false, changing nothing, when the transcript was replaced
/// since the refresh began. Absent rows keep the last good ones.
pub fn apply(state: &mut SummaryState, result: RefreshResult) -> bool {
    if result.generation != state.generation {
        return false;
    }
    state.subject = result.subject;
    state.drift = result.drift;
    state.start_line = result.start_line;
    if result.rows.is_some() {
        state.rows = result.rows;
    }
    true
}

/// Segments in a `" / "`-joined context: 0 when empty.
fn segments(ctx: &str) -> usize {
    if ctx.is_empty() {
        0
    } else {
        ctx.matches(SEP).count() + 1
    }
}

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Whether `cand` shares a whole word longer than 3 chars with `subject`.
fn overlaps(subject: &str, cand: &str) -> bool {
    let subj: Vec<String> = words(subject).collect();
    words(cand).any(|w| w.len() > 3 && subj.contains(&w))
}

/// One refresh: derive or re-anchor the subject, then the stand line. Pure over its inputs; the
/// caller applies the result only while `generation` still matches.
pub fn refresh(
    state: &SummaryState,
    tx: &dyn Transcript,
    model: &dyn Model,
    path: &Path,
) -> RefreshResult {
    let mut r = RefreshResult {
        generation: state.generation,
        subject: state.subject.clone(),
        drift: state.drift,
        start_line: state.start_line,
        rows: None,
    };
    let blob = tx.context(path, 6, 400, CtxMode::Tail);
    if blob.is_empty() {
        return r;
    }

    // The subject's context is the session goal (early intent plus the task list), never recent
    // activity: folding that in narrows the subject to the current subtask.
    let early = tx.context(path, 8, 300, CtxMode::Head);
    let todos = tx.context(path, 12, 120, CtxMode::Todos);
    let mut goal = early.clone();
    if !todos.is_empty() {
        if !goal.is_empty() {
            goal.push_str(SEP);
        }
        goal.push_str("plan: ");
        goal.push_str(&todos);
    }
    if goal.is_empty() {
        goal = blob.clone();
    }

    match state.subject.clone().filter(|s| !s.is_empty()) {
        None => {
            if segments(&early) >= SUBJECT_MIN_SEGMENTS {
                if let Some(s) = label(model, &goal, LABEL_WORDS) {
                    r.subject = Some(s);
                }
            }
        }
        Some(subj) => {
            if let Some(cand) = label(model, &blob, LABEL_WORDS) {
                if overlaps(&subj, &cand) {
                    r.drift = 0;
                } else if state.drift + 1 >= DRIFT_TICKS {
                    // Sustained drift: re-derive from the goal. Only a changed subject is a
                    // pivot that re-anchors the digest; the same subject keeps the window.
                    if let Some(new) = label(model, &goal, LABEL_WORDS) {
                        if new != subj {
                            r.subject = Some(new);
                            r.start_line = tx.line_count(path);
                        }
                        r.drift = 0;
                    }
                } else {
                    r.drift = state.drift + 1;
                }
            }
        }
    }

    let start = if r.start_line == 0 { 1 } else { r.start_line };
    let mut digest = tx.digest(path, start, DIGEST_BUDGET);
    if digest.is_empty() {
        digest = blob;
    }
    let subject = r.subject.as_deref().unwrap_or("");
    if let Some(line) = stand(model, &digest, subject, STAND_WORDS) {
        let line = strip_unbacked_done(&digest, &line);
        if !line.is_empty() {
            r.rows = Some(parse_stand(&line));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Fix {
        head: String,
        tail: String,
        todos: String,
        digest: String,
        lines: usize,
    }

    impl Transcript for Fix {
        fn context(&self, _: &Path, _: usize, _: usize, mode: CtxMode) -> String {
            match mode {
                CtxMode::Head => self.head.clone(),
                CtxMode::Tail => self.tail.clone(),
                CtxMode::Todos => self.todos.clone(),
            }
        }
        fn digest(&self, _: &Path, _: usize, _: usize) -> String {
            self.digest.clone()
        }
        fn line_count(&self, _: &Path) -> usize {
            self.lines
        }
    }

    #[derive(Default)]
    struct Script {
        replies: Mutex<VecDeque<Option<String>>>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl Script {
        fn new(replies: &[Option<&str>]) -> Self {
            Script {
                replies: Mutex::new(replies.iter().map(|r| r.map(str::to_owned)).collect()),
                calls: Mutex::default(),
            }
        }
        fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl Model for Script {
        fn complete(&self, system: &str, user: &str) -> Option<String> {
            self.calls
                .lock()
                .unwrap()
                .push((system.into(), user.into()));
            self.replies.lock().unwrap().pop_front().flatten()
        }
    }

    fn segs(n: usize) -> String {
        vec!["build the auth flow"; n].join(" / ")
    }

    fn fix(head_segs: usize) -> Fix {
        Fix {
            head: segs(head_segs),
            tail: "fixing the cache layer".into(),
            todos: String::new(),
            digest: "edited a.rs / ran: cargo test".into(),
            lines: 77,
        }
    }

    fn p() -> &'static Path {
        Path::new("/t.jsonl")
    }

    fn with_subject(s: &str) -> SummaryState {
        SummaryState {
            subject: Some(s.into()),
            start_line: 1,
            ..Default::default()
        }
    }

    #[test]
    fn no_subject_until_six_head_segments() {
        let m = Script::new(&[Some("stand text")]);
        let r = refresh(&SummaryState::default(), &fix(5), &m, p());
        assert_eq!(r.subject, None);
        assert_eq!(m.count(), 1); // stand only, no label call

        let m = Script::new(&[Some("auth flow"), Some("auth flow. now: x")]);
        let r = refresh(&SummaryState::default(), &fix(6), &m, p());
        assert_eq!(r.subject.as_deref(), Some("auth flow"));
        assert_eq!(m.count(), 2);
    }

    #[test]
    fn drift_needs_three_consecutive_ticks() {
        let s = with_subject("auth token flow");
        let m = Script::new(&[Some("cache layer"), Some("x. now: y")]);
        let r = refresh(&s, &fix(8), &m, p());
        assert_eq!((r.drift, m.count()), (1, 2));
        let s = SummaryState { drift: 1, ..s };
        let m = Script::new(&[Some("cache layer"), Some("x. now: y")]);
        let r = refresh(&s, &fix(8), &m, p());
        assert_eq!((r.drift, m.count()), (2, 2));
        let s = SummaryState { drift: 2, ..s };
        let m = Script::new(&[Some("cache layer"), Some("auth token flow"), Some("x")]);
        let r = refresh(&s, &fix(8), &m, p());
        assert_eq!((r.drift, m.count()), (0, 3)); // re-derive happened
    }

    #[test]
    fn overlap_resets_drift() {
        let s = SummaryState {
            drift: 2,
            ..with_subject("auth token flow")
        };
        let m = Script::new(&[Some("Token refresh"), Some("x. now: y")]);
        let r = refresh(&s, &fix(8), &m, p());
        assert_eq!(r.drift, 0);
        assert_eq!(m.count(), 2); // probe + stand, no re-derive
    }

    #[test]
    fn sustained_drift_same_subject_keeps_start_line() {
        let s = SummaryState {
            drift: 2,
            start_line: 5,
            ..with_subject("auth token flow")
        };
        let m = Script::new(&[Some("cache layer"), Some("auth token flow"), Some("x")]);
        let r = refresh(&s, &fix(8), &m, p());
        assert_eq!(r.subject.as_deref(), Some("auth token flow"));
        assert_eq!((r.start_line, r.drift), (5, 0));
    }

    #[test]
    fn pivot_replaces_subject_and_reanchors_start_line() {
        let s = SummaryState {
            drift: 2,
            start_line: 5,
            ..with_subject("auth token flow")
        };
        let m = Script::new(&[Some("cache layer"), Some("billing export"), Some("x")]);
        let r = refresh(&s, &fix(8), &m, p());
        assert_eq!(r.subject.as_deref(), Some("billing export"));
        assert_eq!((r.start_line, r.drift), (77, 0));
    }

    #[test]
    fn gate_strips_unbacked_done() {
        let mut f = fix(5);
        f.digest = "ran: ls / ran: cat x".into();
        let m = Script::new(&[Some("plan. done: invented; now: reading; next: edit")]);
        let r = refresh(&SummaryState::default(), &f, &m, p());
        let rows = r.rows.unwrap();
        assert_eq!(rows.done, None);
        assert_eq!(rows.now.as_deref(), Some("reading"));
    }

    #[test]
    fn failed_refresh_keeps_last_rows() {
        let old = StandRows {
            done: Some("old".into()),
            ..Default::default()
        };
        let mut s = SummaryState {
            rows: Some(old.clone()),
            ..Default::default()
        };
        let m = Script::new(&[None]);
        let r = refresh(&s, &fix(5), &m, p());
        assert_eq!(r.rows, None);
        assert!(apply(&mut s, r));
        assert_eq!(s.rows, Some(old));
    }

    #[test]
    fn throttle_covers_failures() {
        let t0 = Instant::now();
        let mut s = SummaryState::default();
        assert!(due(&s, t0, THROTTLE));
        s.last_attempt = Some(t0);
        let m = Script::new(&[None]);
        let r = refresh(&s, &fix(5), &m, p()); // fails
        assert_eq!(r.rows, None);
        assert!(!due(&s, t0 + Duration::from_secs(29), THROTTLE));
        assert!(due(&s, t0 + THROTTLE, THROTTLE));
    }

    #[test]
    fn new_transcript_discards_inflight_result() {
        let mut s = SummaryState::default();
        let m = Script::new(&[Some("plan. now: x")]);
        let r = refresh(&s, &fix(5), &m, p());
        assert!(r.rows.is_some());
        s.generation += 1;
        assert!(!apply(&mut s, r));
        assert_eq!(s.rows, None);
    }
}
