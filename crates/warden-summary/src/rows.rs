//! Splitting a stand line into its three display rows, as agentmux's `summary_rows.sh` does.

use serde::{Deserialize, Serialize};

/// A stand line split into its parts, labels removed: the strip renders its own DONE/NOW/NEXT
/// labels. Each is `None` when empty or absent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StandRows {
    /// The leading `<subject>.` text, without its trailing period.
    pub subject: Option<String>,
    pub done: Option<String>,
    pub now: Option<String>,
    pub next: Option<String>,
}

/// Split `line` at its ` next:`, ` now:` and `done:` labels, as `summary_rows.sh` splits its
/// rows. Text with no markers at all lands whole in `done` (subject `None`). Fields are trimmed
/// of spaces and `;`. Clipping and the tmux `#` escape are not ported: the UI ellipsises, and
/// nothing here re-parses the text.
pub fn parse_stand(line: &str) -> StandRows {
    let (head2, next) = match line.find(" next:") {
        Some(i) => (&line[..i], &line[i + " next:".len()..]),
        None => (line, ""),
    };
    let (head1, now) = match head2.find(" now:") {
        Some(i) => (&head2[..i], &head2[i + " now:".len()..]),
        None => (head2, ""),
    };
    let field = |t: &str| {
        let t = t.trim_matches([' ', ';']);
        (!t.is_empty()).then(|| t.to_string())
    };
    let has_markers = head1.len() != line.len();
    let done_at = if head1.starts_with("done:") {
        Some((0, "done:".len()))
    } else {
        head1.find(" done:").map(|i| (i, " done:".len()))
    };
    let (subject, done) = match done_at {
        Some((i, len)) => (&head1[..i], &head1[i + len..]),
        None if has_markers => (head1, ""),
        None => ("", head1),
    };
    StandRows {
        subject: field(subject.trim_end().trim_end_matches('.')),
        done: field(done),
        now: field(now),
        next: field(next),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "billing soft delete migration. done: edited models.py, wrote backfill.py; now: add proration tests; next: fix and rerun";

    fn rows(sub: Option<&str>, d: Option<&str>, n: Option<&str>, x: Option<&str>) -> StandRows {
        let o = |v: Option<&str>| v.map(String::from);
        StandRows {
            subject: o(sub),
            done: o(d),
            now: o(n),
            next: o(x),
        }
    }

    #[test]
    fn parse_stand_splits_subject_and_strips_labels() {
        assert_eq!(
            parse_stand(S),
            rows(
                Some("billing soft delete migration"),
                Some("edited models.py, wrote backfill.py"),
                Some("add proration tests"),
                Some("fix and rerun")
            )
        );
    }

    #[test]
    fn full_line_splits_into_four_fields() {
        // lt1, lt2, lt3
        let r = parse_stand(S);
        assert_eq!(
            r.done.as_deref(),
            Some("edited models.py, wrote backfill.py")
        );
        assert_eq!(r.now.as_deref(), Some("add proration tests"));
        assert_eq!(r.next.as_deref(), Some("fix and rerun"));
    }

    #[test]
    fn missing_next_is_none() {
        // lt4, lt5
        assert_eq!(
            parse_stand("subj. done: x; now: y"),
            rows(Some("subj"), Some("x"), Some("y"), None)
        );
    }

    #[test]
    fn missing_done_leaves_the_subject() {
        // lt6, lt7
        assert_eq!(
            parse_stand("subj. now: exploring options."),
            rows(Some("subj"), None, Some("exploring options."), None)
        );
    }

    #[test]
    fn no_markers_lands_whole_in_done() {
        // lt8, lt9
        assert_eq!(
            parse_stand("just a bare subject phrase"),
            rows(None, Some("just a bare subject phrase"), None, None)
        );
    }

    #[test]
    fn hash_is_not_escaped() {
        // lt12 without the tmux-only '##'
        assert_eq!(
            parse_stand("subj #1. now: y").subject.as_deref(),
            Some("subj #1")
        );
    }

    #[test]
    fn done_label_without_subject() {
        assert_eq!(
            parse_stand("done: a; now: b"),
            rows(None, Some("a"), Some("b"), None)
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
