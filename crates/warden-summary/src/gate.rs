//! The anti-invention gate: a small model asked for a done/now/next line fabricates `done:`
//! milestones when the session did nothing but plan. Rationale: agentmux's `docs/ai-summary.md`.

/// Whether the digest carries evidence of finished work: a segment starting `edited `,
/// `wrote ` (file mutations) or `todo-done:` (the agent's own report), at the digest start or
/// after the ` / ` joiner. `ran:` is deliberately not evidence: read-only investigation emits
/// it, and planning sessions are where the model invents `done:`.
fn has_mutation(digest: &str) -> bool {
    const MARKERS: [&str; 3] = ["edited ", "wrote ", "todo-done:"];
    MARKERS
        .iter()
        .any(|m| digest.starts_with(m) || digest.contains(&format!(" / {m}")))
}

/// Remove the `done:` clause of a stand line. `done:` counts as a label only at the line start
/// or right after a `.` or `;`, so a `done:` inside another clause's text stays. The clause
/// runs to the next `;` or the end; trailing whitespace and `;` are trimmed, `.` is kept.
fn strip_done(line: &str) -> String {
    let b = line.as_bytes();
    let ws = |c: u8| c == b' ' || c == b'\t';
    // Leftmost `(^|[.;])[ \t]*done[ \t]*:`; returns (index of "done", index after the colon).
    let found = (0..=b.len()).find_map(|i| {
        let starts: &[usize] = match (i == 0, b.get(i).is_some_and(|c| *c == b'.' || *c == b';')) {
            (true, true) => &[0, 1],
            (true, false) => &[0],
            (false, true) => &[i + 1],
            _ => &[],
        };
        starts.iter().find_map(|&s| {
            let mut p = s;
            while b.get(p).is_some_and(|c| ws(*c)) {
                p += 1;
            }
            if !line[p..].starts_with("done") {
                return None;
            }
            let done_at = p;
            p += 4;
            while b.get(p).is_some_and(|c| ws(*c)) {
                p += 1;
            }
            (b.get(p) == Some(&b':')).then_some((done_at, p + 1))
        })
    });
    let Some((done_at, end)) = found else {
        return line.to_string();
    };
    let head = &line[..done_at];
    let tail = &line[end..];
    let mut out = match tail.find(';') {
        Some(j) => format!("{head}{}", tail[j + 1..].trim_start_matches([' ', '\t'])),
        None => head.to_string(),
    };
    out.truncate(out.trim_end_matches([' ', '\t', ';']).len());
    out
}

/// `line` unchanged when `digest` shows finished work, else `line` without its `done:` clause.
pub fn strip_unbacked_done(digest: &str, line: &str) -> String {
    if has_mutation(digest) {
        line.to_string()
    } else {
        strip_done(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_positive_cases() {
        assert!(has_mutation("edited foo.sh")); // ta1
        assert!(has_mutation("wrote bar.txt")); // ta2
        assert!(has_mutation("user prose / edited foo.sh")); // ta3
        assert!(has_mutation("x / y / wrote bar / z")); // ta4
        assert!(has_mutation("todo-done: shipped backfill")); // ta11
        assert!(has_mutation("prose / todo-done: shipped backfill")); // ta12
    }

    #[test]
    fn mutation_negative_cases() {
        assert!(!has_mutation("ran: pytest -q")); // ta5
        assert!(!has_mutation("prose / ran: ls -la")); // ta6
        assert!(!has_mutation(
            "prose / ran: git status / more / ran: git log"
        )); // ta7
        assert!(!has_mutation("")); // ta8
        assert!(!has_mutation("i edited the file earlier")); // ta9
        assert!(!has_mutation("discussing migration")); // ta10
    }

    #[test]
    fn strip_done_cases() {
        let cases = [
            (
                "subject. done: foo; now: bar; next: baz",
                "subject. now: bar; next: baz",
            ), // sd1
            ("subject. done: foo; now: bar", "subject. now: bar"), // sd2
            ("subject. now: bar; done: foo", "subject. now: bar"), // sd3
            ("subject. done: foo", "subject."),                    // sd4
            (
                "subject. now: bar; next: baz",
                "subject. now: bar; next: baz",
            ), // sd5
            ("subject. done : foo; now: bar", "subject. now: bar"), // sd6
            ("subject. done: foo;now: bar", "subject. now: bar"),  // sd7
            (
                "subject. now: rewrote done: notes; next: ship",
                "subject. now: rewrote done: notes; next: ship",
            ), // sd8
            ("done: foo; now: bar", "now: bar"),                   // sd9
            (
                "subject. predone: marker; now: bar",
                "subject. predone: marker; now: bar",
            ), // sd10
        ];
        for (input, want) in cases {
            assert_eq!(strip_done(input), want, "input {input:?}");
        }
    }

    #[test]
    fn gate_cases() {
        // g1: mutation present, done kept.
        let s = "subject. done: shipped a; now: writing tests";
        assert_eq!(strip_unbacked_done("user said hi / edited a.sh", s), s);
        // g2: zero mutations, done stripped.
        assert_eq!(
            strip_unbacked_done("user prose / more user prose", s),
            "subject. now: writing tests"
        );
        // g3: the originally observed failure: prose plus read-only Bash.
        let bad = "product migration. done: initial data transfer, setup of new environment; now: enhancement of desktop electron shell; next: integration testing of migrated components";
        let digest = "we are part way through the product migration / i have an agent working on improving the desktop electron shell / where are we at with the migration / ran: git log --oneline / ran: git status / ran: git diff main / shadow CRDT shipped step 1 backend removal in flight on step-2";
        assert!(!strip_unbacked_done(digest, bad).contains("done:"));
        // g4: a session with edits keeps its done.
        let good =
            "auth refactor. done: split session module; now: writing tests; next: integration";
        assert_eq!(
            strip_unbacked_done(
                "lets refactor auth / edited auth.py / wrote tests/test_auth.py / ran: pytest -q",
                good
            ),
            good
        );
        // g5: todo-done is evidence without file mutations.
        let t = "subj. done: shipped backfill; now: writing tests";
        assert_eq!(
            strip_unbacked_done(
                "discussing scope / todo-done: shipped backfill / todo-now: writing tests",
                t
            ),
            t
        );
        // cli1 / cli2: the digest decides.
        assert_eq!(
            strip_unbacked_done("edited a.sh", "subj. done: a; now: b"),
            "subj. done: a; now: b"
        );
        assert_eq!(
            strip_unbacked_done("ran: ls", "subj. done: a; now: b"),
            "subj. now: b"
        );
    }
}
