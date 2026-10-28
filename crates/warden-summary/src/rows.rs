//! Splitting a stand line into its three display rows, as agentmux's `summary_rows.sh` does.

use serde::{Deserialize, Serialize};

/// The three rows of a stand line. Each is the line's text for that row, verbatim from the
/// model (labels included: `done` is `"<subject>. done: ..."`, `now` is `"now: ..."`), because
/// that is what the shell's rows print. `None` when the row is empty or absent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StandRows {
    pub done: Option<String>,
    pub now: Option<String>,
    pub next: Option<String>,
}

/// Split `line` at its ` next:` then ` now:` markers. Text with no markers lands whole in
/// `done`. Each field is trimmed of spaces and `;`. Clipping and the tmux `#` escape are not
/// ported: the UI ellipsises, and nothing here re-parses the text.
pub fn parse_stand(line: &str) -> StandRows {
    let (head2, next) = match line.find(" next:") {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, ""),
    };
    let (head1, now) = match head2.find(" now:") {
        Some(i) => (&head2[..i], &head2[i + 1..]),
        None => (head2, ""),
    };
    let field = |t: &str| {
        let t = t.trim_matches([' ', ';']);
        (!t.is_empty()).then(|| t.to_string())
    };
    StandRows {
        done: field(head1),
        now: field(now),
        next: field(next),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "billing soft delete migration. done: edited models.py, wrote backfill.py; now: add proration tests; next: fix and rerun";

    #[test]
    fn full_line_splits_into_three_rows() {
        // lt1, lt2, lt3
        let r = parse_stand(S);
        assert_eq!(
            r.done.as_deref(),
            Some("billing soft delete migration. done: edited models.py, wrote backfill.py")
        );
        assert_eq!(r.now.as_deref(), Some("now: add proration tests"));
        assert_eq!(r.next.as_deref(), Some("next: fix and rerun"));
    }

    #[test]
    fn missing_next_is_none() {
        // lt4, lt5
        let r = parse_stand("subj. done: x; now: y");
        assert_eq!(r.next, None);
        assert_eq!(r.done.as_deref(), Some("subj. done: x"));
        assert_eq!(r.now.as_deref(), Some("now: y"));
    }

    #[test]
    fn missing_done_leaves_the_subject_row() {
        // lt6, lt7
        let r = parse_stand("subj. now: exploring options.");
        assert_eq!(r.done.as_deref(), Some("subj."));
        assert_eq!(r.now.as_deref(), Some("now: exploring options."));
    }

    #[test]
    fn no_markers_lands_whole_in_done() {
        // lt8, lt9
        let r = parse_stand("just a bare subject phrase");
        assert_eq!(r.done.as_deref(), Some("just a bare subject phrase"));
        assert_eq!((r.now, r.next), (None, None));
    }

    #[test]
    fn hash_is_not_escaped() {
        // lt12 without the tmux-only '##'
        assert_eq!(
            parse_stand("subj #1. now: y").done.as_deref(),
            Some("subj #1.")
        );
    }

    #[test]
    fn empty_line_is_all_none() {
        assert_eq!(parse_stand(""), StandRows::default());
    }

    #[test]
    fn round_trips_through_json() {
        let r = parse_stand(S);
        let back: StandRows = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }
}
