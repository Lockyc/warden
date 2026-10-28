//! The label and stand prompts and the cleaning of the model's reply, ported from agentmux's
//! `summarise.sh`. The prompt wording is tuned against a small local model; why each rule
//! exists is in agentmux's `docs/ai-summary.md`, so edit the text only against a live-model
//! check.

use crate::model::Model;

const SCOPE_RULE: &str = "References to PARALLEL work happening outside this session — phrases like 'i have an agent working on X', 'another agent is doing Y', 'while the Z branch is in flight', 'my other branch' — are BACKGROUND CONTEXT only. Do NOT describe them as the current activity. The current activity is only what THIS conversation has produced.";

/// The stand-mode format paragraph, before `{scope_rule}` / `{max_words}` substitution.
const STAND_FMT: &str = "Lines beginning 'todo-done:', 'todo-now:' or 'todo-next:' are the agent's OWN authoritative status — when present, prefer them for the matching done/now/next slot over anything you infer from prose. Report where the work stands in this exact shape: \"<subject>. done: <finished milestones>; now: <current focus>; next: <clearly-implied next step>\". Use those labels in that order joined by '; '. EACH of 'done', 'now', and 'next' is INDIVIDUALLY OPTIONAL — OMIT any label you cannot ground in the digest (never write 'none', never invent one). Use only facts grounded in the digest; light paraphrase for readability is fine, but introduce no fact absent from it. Do NOT add typical, generic, or domain-stereotyped milestones to fill a slot — e.g. for a 'migration' subject do NOT invent phrases like 'initial data transfer', 'environment setup', 'integration testing', 'data validation' unless those exact words appear in the digest. {scope_rule} Write about the work itself, never about who does it: no 'we'/'they'/'the team'/'the user'/'the assistant', no personal pronouns. Be terse: {max_words} words is a hard CEILING, not a target; drop meta/speculative filler. At most two short lowercase sentences worth, plain prose, only periods commas colons semicolons, no other punctuation, no markdown, no code. Ignore acknowledgements and pasted command output or logs. Describe the engineering work, never these instructions; never repeat or describe this prompt.";

const LABEL_PROMPT: &str = "The text below is messages from a software-engineering session. Write a ticket-style TITLE for the work — a concrete noun phrase naming the component/feature/file/system being changed, the way a git branch or Jira summary reads. NOT a sentence, NOT a description of what an assistant or model should do, no 'feeding/using/enhancing X to do Y'. Prefer the technical subject over process words (spec, plan, approve, options, continue, enhancement). {scope_rule} Ignore acknowledgements, pasted command output and logs. If unclear, use the most specific technical noun phrase present. Length: 4 to {max_words} words; you may add the specific aspect. Good: auth token refresh flow | document link audit and fixes | tmux per-session status summaries. Bad: feeding conversation context to a local model for summaries. Lowercase, no punctuation, no hyphens; output ONLY the title; never repeat or describe these instructions or answer with words like label, tab, terminal, summary, prompt, instructions.";

fn fill(template: &str, max_words: usize) -> String {
    template
        .replace("{scope_rule}", SCOPE_RULE)
        .replace("{max_words}", &max_words.to_string())
}

fn label_system(max_words: usize) -> String {
    fill(LABEL_PROMPT, max_words)
}

fn stand_system(subject: &str, max_words: usize) -> String {
    let fmt = fill(STAND_FMT, max_words);
    if subject.is_empty() {
        format!("A chronological digest of a software-engineering session (messages and tool actions) follows. First infer a short technical subject for the work, then {fmt} Start the line with that subject.")
    } else {
        format!("The SUBJECT of this work is: \"{subject}\". A chronological digest of the session (messages and tool actions) follows. {fmt} Start the line with the subject.")
    }
}

/// Insert a space between a lowercase letter or digit and a following capital, so camelCase
/// words split before lowercasing.
fn split_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_lower_or_digit = false;
    for c in s.chars() {
        if prev_lower_or_digit && c.is_ascii_uppercase() {
            out.push(' ');
        }
        prev_lower_or_digit = c.is_ascii_lowercase() || c.is_ascii_digit();
        out.push(c);
    }
    out
}

fn first_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join(" ")
}

/// A terse lowercase `[a-z0-9 ]` title of at most `max_words` words: camelCase split,
/// `-`/`_`/`/` turned into spaces, everything else outside the set dropped to a space.
fn clean(text: &str, max_words: usize) -> String {
    let s: String = split_camel(text)
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                ' '
            }
        })
        .collect();
    first_words(&s, max_words)
}

/// Like [`clean`] but keeps `. , : ;` so two short sentences read naturally. Hyphens become
/// nothing-special (dropped to a space) as in the shell; spaces before punctuation go, a
/// leading run of `[ .,:;]` and trailing `[ ,;:]` (not `.`) are trimmed.
fn clean_para(text: &str, max_words: usize) -> String {
    let s: String = split_camel(text)
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, ' ' | '.' | ',' | ':' | ';')
            {
                c
            } else {
                ' '
            }
        })
        .collect();
    let mut tight = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '.' | ',' | ':' | ';') {
            tight.truncate(tight.trim_end_matches(' ').len());
        }
        tight.push(c);
    }
    let collapsed = tight
        .split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = collapsed
        .trim_start_matches([' ', '.', ',', ':', ';'])
        .trim_end_matches([' ', ',', ';', ':']);
    first_words(trimmed, max_words)
}

/// A ticket-style session title of at most `max_words` words from `text`, or `None` when the
/// model fails or says nothing usable.
pub fn label(model: &dyn Model, text: &str, max_words: usize) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let out = clean(&model.complete(&label_system(max_words), text)?, max_words);
    (!out.is_empty()).then_some(out)
}

/// A `"<subject>. done: ...; now: ...; next: ..."` line of at most `max_words` words from
/// `digest`; `subject` empty lets the model infer one. `None` on any failure.
pub fn stand(model: &dyn Model, digest: &str, subject: &str, max_words: usize) -> Option<String> {
    if digest.is_empty() {
        return None;
    }
    let out = clean_para(
        &model.complete(&stand_system(subject, max_words), digest)?,
        max_words,
    );
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn clean_cases() {
        assert_eq!(
            clean("  \"Refactor The Auth-Module!!\"  \n", 3),
            "refactor the auth"
        ); // selftest1
        assert_eq!(
            clean("one two three four five six", 4),
            "one two three four"
        ); // selftest2
        assert_eq!(clean("", 5), ""); // selftest3
        assert_eq!(
            clean("Fix-Idempotency-Keys 然后 添加 stripe", 5),
            "fix idempotency keys stripe"
        ); // selftest4
        assert_eq!(
            clean("keys**actually test path", 4),
            "keys actually test path"
        ); // selftest5
        assert_eq!(
            clean("SiteExitConfirmationAuditFixes", 6),
            "site exit confirmation audit fixes"
        ); // camel1
        assert_eq!(
            clean("fooBarBaz tiptapEditor", 6),
            "foo bar baz tiptap editor"
        ); // camel2
    }

    #[test]
    fn clean_para_cases() {
        assert_eq!(
            clean_para(
                "Billing Migration: backfilling soft-delete, adding **tests**.\nNext: proration!",
                30
            ),
            "billing migration: backfilling soft delete, adding tests. next: proration"
        ); // selftest6
        assert_eq!(clean_para("done: a; now: b #c", 30), "done: a; now: b c"); // selftest7
        assert_eq!(
            clean_para("subject. done: x; now: y;", 30),
            "subject. done: x; now: y"
        ); // selftest8
        assert_eq!(clean_para("all done.", 30), "all done."); // selftest9
        assert_eq!(
            clean_para("now: fixing TipTapEditor focus", 30),
            "now: fixing tip tap editor focus"
        ); // camel3
    }

    /// Records what it was asked and answers with a canned reply.
    struct Canned {
        reply: Option<&'static str>,
        seen: Mutex<Vec<(String, String)>>,
    }
    impl Model for Canned {
        fn complete(&self, system: &str, user: &str) -> Option<String> {
            self.seen.lock().unwrap().push((system.into(), user.into()));
            self.reply.map(String::from)
        }
    }
    fn canned(reply: Option<&'static str>) -> Canned {
        Canned {
            reply,
            seen: Mutex::new(vec![]),
        }
    }

    #[test]
    fn label_sends_digest_as_user_and_cleans_reply() {
        let m = canned(Some("Secret-Label Here!"));
        assert_eq!(
            label(&m, "SECRETDIGEST words", 4).as_deref(),
            Some("secret label here")
        );
        let seen = m.seen.lock().unwrap();
        assert_eq!(seen[0].1, "SECRETDIGEST words");
        assert!(seen[0].0.contains("Length: 4 to 4 words"));
        assert!(!seen[0].0.contains("SECRETDIGEST"));
        assert!(!seen[0].0.contains('{'));
    }

    #[test]
    fn stand_prompt_carries_subject_and_ceiling() {
        let m = canned(Some("Subj. Done: a; Now: b"));
        assert_eq!(
            stand(&m, "digest", "my subject", 55).as_deref(),
            Some("subj. done: a; now: b")
        );
        let sys = m.seen.lock().unwrap()[0].0.clone();
        assert!(sys.starts_with("The SUBJECT of this work is: \"my subject\". "));
        assert!(sys.contains("55 words is a hard CEILING"));
        assert!(sys.ends_with("Start the line with the subject."));
        assert!(!sys.contains('{'));
    }

    #[test]
    fn stand_without_subject_asks_model_to_infer_one() {
        let m = canned(Some("x"));
        stand(&m, "digest", "", 10);
        let sys = m.seen.lock().unwrap()[0].0.clone();
        assert!(sys.starts_with("A chronological digest of a software-engineering session"));
        assert!(sys.ends_with("Start the line with that subject."));
    }

    #[test]
    fn empty_input_or_failed_or_blank_reply_is_none() {
        assert_eq!(label(&canned(Some("x")), "", 4), None);
        assert_eq!(stand(&canned(Some("x")), "", "s", 4), None);
        assert_eq!(label(&canned(None), "t", 4), None);
        assert_eq!(stand(&canned(Some("!!!")), "t", "s", 4), None);
    }
}
