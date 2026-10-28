use crate::{CtxMode, Transcript};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Lines `Head` considers from the file start. Bounds a very long session while keeping the
/// opening, where the stated goal lives.
const HEAD_LINES: usize = 1500;
/// Lines `Tail` and `Todos` consider from the file end.
const TAIL_LINES: usize = 800;
/// Per-message slice taken before whitespace collapsing: a transcript line carrying a multi-MB
/// base64 or file dump must not make the later passes quadratic. Output is capped far below this.
const PRE_SLICE: usize = 5000;
/// Chars of a prose turn the digest keeps.
const DIGEST_PROSE_CAP: usize = 240;
/// Chars of a Bash command the digest keeps.
const DIGEST_CMD_CAP: usize = 60;
const SEP: &str = " / ";
const CHUNK: u64 = 64 * 1024;

/// Conversational filler for `context`: a turn made only of these words is not a subject.
const CTX_FILLER: &[&str] = &[
    "y", "n", "k", "ok", "okay", "yes", "yep", "yeah", "ya", "yup", "no", "nope", "nah", "sure",
    "ta", "thx", "thanks", "thank", "cheers", "continue", "please", "keep", "going", "carry", "on",
    "proceed", "go", "ahead", "do", "it", "next", "lgtm", "looks", "good", "sounds", "perfect",
    "great", "nice", "cool", "done", "stop", "wait", "hold", "hmm", "huh", "same", "again",
    "retry", "right", "fine", "this", "that",
];
/// Filler for `digest`. Differs from [`CTX_FILLER`] by lacking `no`: the two scripts disagree and
/// the digest keeps a bare "no" as prose.
const DIGEST_FILLER: &[&str] = &[
    "yes", "yep", "yeah", "yup", "nope", "nah", "okay", "ok", "sure", "thanks", "thank", "thx",
    "cheers", "ta", "continue", "please", "keep", "going", "carry", "on", "proceed", "go", "ahead",
    "do", "it", "next", "lgtm", "looks", "good", "sounds", "perfect", "great", "nice", "cool",
    "done", "stop", "wait", "hold", "hmm", "huh", "same", "again", "retry", "right", "fine",
    "this", "that", "ya", "y", "n", "k",
];

/// Claude Code's `~/.claude*/projects/*/<session>.jsonl` transcripts.
pub struct Claude;

impl Transcript for Claude {
    fn context(&self, path: &Path, max_msgs: usize, percap: usize, mode: CtxMode) -> String {
        let Ok(file) = File::open(path) else {
            return String::new();
        };
        match mode {
            CtxMode::Tail => tail_context(file, max_msgs, percap),
            CtxMode::Head => head_context(file, max_msgs, percap),
            CtxMode::Todos => todos_context(file, max_msgs, percap),
        }
    }

    fn digest(&self, path: &Path, start_line: usize, char_budget: usize) -> String {
        digest_file(path, start_line.max(1), char_budget).unwrap_or_default()
    }

    fn line_count(&self, path: &Path) -> usize {
        count_newlines(path).unwrap_or(0)
    }
}

// ---- shared text rules ----

/// jq `gsub("\\s+";" ")`.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// jq `.[0:n]` on a string: the first `n` chars.
fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `[[:space:][:punct:]]`: whitespace and anything printable that is not a letter or digit.
fn is_sep(c: char) -> bool {
    c.is_whitespace() || (!c.is_alphanumeric() && !c.is_control())
}

/// Whether `s` is only `words` (any number, in any order, even run together) and separators.
/// Words are prefixes of one another, so the match backtracks, memoised by position.
fn is_filler(s: &str, words: &[&str]) -> bool {
    let b: Vec<char> = s.chars().map(|c| c.to_ascii_lowercase()).collect();
    let skip = |mut i: usize| {
        while i < b.len() && is_sep(b[i]) {
            i += 1;
        }
        i
    };
    fn rec(
        i: usize,
        b: &[char],
        words: &[&str],
        memo: &mut Vec<Option<bool>>,
        skip: &dyn Fn(usize) -> usize,
    ) -> bool {
        if let Some(r) = memo[i] {
            return r;
        }
        let mut res = false;
        for w in words {
            let n = w.chars().count();
            if i + n <= b.len() && w.chars().zip(&b[i..i + n]).all(|(x, y)| x == *y) {
                let j = skip(i + n);
                if j == b.len() || rec(j, b, words, memo, skip) {
                    res = true;
                    break;
                }
            }
        }
        memo[i] = Some(res);
        res
    }
    let mut memo = vec![None; b.len() + 1];
    rec(skip(0), &b, words, &mut memo, &skip)
}

/// The awk prose gate shared by both scripts: drops REPL/shell prompts, code fences, turns under
/// 55% letters-and-spaces (code, dumps), and turns carrying a >30-char token (ULID, path, hash,
/// base64).
fn is_prose(s: &str) -> bool {
    if s.contains('❯') || s.contains("```") {
        return false;
    }
    let rest = s.trim_start_matches([' ', '\t']);
    let mut it = rest.chars();
    if matches!(it.next(), Some('$' | '>' | '#')) && matches!(it.next(), Some(' ' | '\t')) {
        return false;
    }
    let len = s.chars().count();
    let letters = s
        .chars()
        .filter(|c| c.is_ascii_alphabetic() || *c == ' ')
        .count();
    if len > 0 && letters * 100 < len * 55 {
        return false;
    }
    !s.split([' ', '\t']).any(|w| w.chars().count() > 30)
}

fn parse(line: &[u8]) -> Option<Value> {
    let v: Value = serde_json::from_str(&String::from_utf8_lossy(line)).ok()?;
    v.is_object().then_some(v)
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn content_blocks(v: &Value) -> &[Value] {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The `text` blocks of a content array joined by a space.
fn joined_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- context (ctx.sh) ----

/// One kept prose turn of a line, or `None` when the line is not a surviving user/assistant turn.
fn ctx_turn(line: &[u8]) -> Option<String> {
    let v = parse(line)?;
    let ty = v.get("type")?.as_str()?;
    if ty != "user" && ty != "assistant" {
        return None;
    }
    let c = v.get("message")?.get("content")?;
    let text = match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => joined_text(a),
        _ => String::new(),
    };
    let text = collapse_ws(&take_chars(&text, PRE_SLICE));
    let text = text.trim_matches(' ');
    if text.is_empty() || is_filler(text, CTX_FILLER) || !is_prose(text) {
        return None;
    }
    Some(text.to_owned())
}

fn join_capped(turns: &[String], percap: usize) -> String {
    turns
        .iter()
        .map(|t| take_chars(t, percap))
        .collect::<Vec<_>>()
        .join(SEP)
}

fn head_context(file: File, max_msgs: usize, percap: usize) -> String {
    if max_msgs == 0 {
        return String::new();
    }
    let mut turns = Vec::new();
    let mut r = BufReader::new(file);
    let mut buf = Vec::new();
    for _ in 0..HEAD_LINES {
        buf.clear();
        if !matches!(r.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
            break;
        }
        if let Some(t) = ctx_turn(&buf) {
            turns.push(t);
            if turns.len() == max_msgs {
                break;
            }
        }
    }
    join_capped(&turns, percap)
}

/// Tail mode over any seekable source: reads backward in chunks and stops once `max_msgs` turns
/// are found, never past [`TAIL_LINES`] lines.
pub(crate) fn tail_context<R: Read + Seek>(src: R, max_msgs: usize, percap: usize) -> String {
    if max_msgs == 0 {
        return String::new();
    }
    let Ok(mut lines) = RevLines::new(src) else {
        return String::new();
    };
    let mut turns = Vec::new();
    for _ in 0..TAIL_LINES {
        let Some(line) = lines.next_line() else { break };
        if let Some(t) = ctx_turn(&line) {
            turns.push(t);
            if turns.len() == max_msgs {
                break;
            }
        }
    }
    turns.reverse();
    join_capped(&turns, percap)
}

fn todos_context(file: File, max_msgs: usize, percap: usize) -> String {
    let Ok(mut lines) = RevLines::new(file) else {
        return String::new();
    };
    for _ in 0..TAIL_LINES {
        let Some(line) = lines.next_line() else { break };
        if let Some(todos) = parse(&line).and_then(|v| todo_snapshot(&v)) {
            return todos_list(&todos, max_msgs, percap).unwrap_or_default();
        }
    }
    String::new()
}

/// A task-list snapshot's `content`s in list order, each capped, empties dropped.
fn todos_list(todos: &Value, max_msgs: usize, percap: usize) -> Option<String> {
    let mut out = Vec::new();
    for item in todos.as_array()? {
        let c = str_or(item.as_object()?.get("content"), "")?;
        let c = take_chars(&collapse_ws(&take_chars(&c, 2000)), percap);
        if !c.is_empty() {
            out.push(c);
        }
    }
    out.truncate(max_msgs);
    Some(out.join(SEP))
}

/// jq `x // default` over a string field: null/false take the default, a non-string is an error.
fn str_or(v: Option<&Value>, default: &str) -> Option<String> {
    match v {
        None | Some(Value::Null | Value::Bool(false)) => Some(default.to_owned()),
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => None,
    }
}

/// The `todos` input of the last `TodoWrite` call on an assistant line, if any.
fn todo_snapshot(v: &Value) -> Option<Value> {
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    content_blocks(v)
        .iter()
        .rev()
        .filter(|b| {
            b.get("type").and_then(Value::as_str) == Some("tool_use")
                && b.get("name").and_then(Value::as_str) == Some("TodoWrite")
        })
        .find_map(|b| {
            b.get("input")
                .and_then(|i| i.get("todos"))
                .filter(|t| !matches!(t, Value::Null | Value::Bool(false)))
                .cloned()
        })
}

// ---- digest (digest.sh) ----

#[derive(PartialEq, Clone, Copy)]
enum Tag {
    Prose,
    Tool,
}

struct Entry {
    tag: Tag,
    len: usize,
    text: String,
    removed: bool,
}

/// The digest body under a char budget. The oldest prose is dropped first, tool lines never;
/// trimming as entries arrive reaches the same result as trimming at the end, because totals only
/// grow, and keeps memory to the budget plus the tool lines.
struct Budgeted {
    budget: i64,
    entries: Vec<Entry>,
    total: i64,
    cursor: usize,
}

impl Budgeted {
    fn push(&mut self, tag: Tag, text: String) {
        let len = text.chars().count();
        self.total += len as i64 + if self.entries.is_empty() { 0 } else { 3 };
        self.entries.push(Entry {
            tag,
            len,
            text,
            removed: false,
        });
        self.trim();
    }

    fn trim(&mut self) {
        while self.total > self.budget {
            while self
                .entries
                .get(self.cursor)
                .is_some_and(|e| e.removed || e.tag == Tag::Tool)
            {
                self.cursor += 1;
            }
            let Some(e) = self.entries.get_mut(self.cursor) else {
                return;
            };
            e.removed = true;
            e.text = String::new();
            self.total -= e.len as i64 + 3;
        }
    }

    fn render(&self) -> String {
        self.entries
            .iter()
            .filter(|e| !e.removed)
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join(SEP)
    }
}

/// Feeds each line from 1-based `start` to the end to `f`; the file is not held in memory.
fn for_each_line(path: &Path, start: usize, mut f: impl FnMut(&[u8])) -> Option<()> {
    let mut r = BufReader::new(File::open(path).ok()?);
    let mut skipped = 1;
    while skipped < start {
        let buf = r.fill_buf().ok()?;
        if buf.is_empty() {
            return Some(());
        }
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => {
                r.consume(i + 1);
                skipped += 1;
            }
            None => {
                let n = buf.len();
                r.consume(n);
            }
        }
    }
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf).ok()? == 0 {
            return Some(());
        }
        f(&buf);
    }
}

fn digest_file(path: &Path, start: usize, budget: usize) -> Option<String> {
    // Pass 1: tool_use ids whose paired tool_result errored.
    let mut failed: HashSet<String> = HashSet::new();
    for_each_line(path, start, |line| {
        if !contains(line, b"is_error") {
            return;
        }
        let Some(v) = parse(line) else { return };
        if v.get("type").and_then(Value::as_str) != Some("user") {
            return;
        }
        for b in content_blocks(&v) {
            if b.get("type").and_then(Value::as_str) == Some("tool_result")
                && b.get("is_error") == Some(&Value::Bool(true))
            {
                if let Some(id) = str_of(b, "tool_use_id") {
                    failed.insert(id);
                }
            }
        }
    })?;

    // Pass 2: prose and mutating tool one-liners, plus the latest TodoWrite snapshot.
    let mut body = Budgeted {
        budget: i64::try_from(budget).unwrap_or(i64::MAX),
        entries: Vec::new(),
        total: 0,
        cursor: 0,
    };
    let mut todos: Option<Value> = None;
    for_each_line(path, start, |line| {
        let Some(v) = parse(line) else { return };
        if let Some(t) = todo_snapshot(&v) {
            todos = Some(t);
        }
        digest_entries(&v, &failed, &mut body);
    })?;

    let main = body.render();
    let seg = todos.as_ref().and_then(todo_segments).unwrap_or_default();
    Some(match (main.is_empty(), seg.is_empty()) {
        (false, false) => format!("{main}{SEP}{seg}"),
        (false, true) => main,
        _ => seg,
    })
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn digest_entries(v: &Value, failed: &HashSet<String>, out: &mut Budgeted) {
    match v.get("type").and_then(Value::as_str) {
        Some("user") => {
            let Some(c) = v.get("message").and_then(|m| m.get("content")) else {
                return;
            };
            let text = match c {
                Value::String(s) => collapse_ws(&take_chars(s, PRE_SLICE)),
                Value::Array(a) => {
                    let t = collapse_ws(&take_chars(&joined_text(a), PRE_SLICE));
                    if t.trim_matches(' ').is_empty() {
                        return;
                    }
                    t
                }
                _ => return,
            };
            push_prose(out, text);
        }
        Some("assistant") => {
            for b in content_blocks(v) {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            push_prose(out, collapse_ws(&take_chars(t, PRE_SLICE)));
                        }
                    }
                    Some("tool_use") => {
                        if let Some(mut line) = tool_line(b) {
                            if str_of(b, "id").is_some_and(|id| failed.contains(&id)) {
                                line.push_str(" (failed)");
                            }
                            out.push(Tag::Tool, line);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn push_prose(out: &mut Budgeted, text: String) {
    if text.is_empty() || is_filler(&text, DIGEST_FILLER) || !is_prose(&text) {
        return;
    }
    out.push(Tag::Prose, take_chars(&text, DIGEST_PROSE_CAP));
}

/// `edited <file>` / `wrote <file>` / `ran: <command>` for the mutating tools; every other tool,
/// and a mutating one without a file or command, yields nothing.
fn tool_line(b: &Value) -> Option<String> {
    let name = b.get("name")?.as_str()?;
    let input = b.get("input");
    let field = |k: &str| input.and_then(|i| i.get(k));
    let basename = |v: Option<&Value>| -> Option<String> {
        let p = collapse_ws(v.and_then(Value::as_str)?);
        let base = p.rsplit('/').next().unwrap_or("").to_owned();
        (!base.is_empty()).then_some(base)
    };
    match name {
        "Edit" | "MultiEdit" | "NotebookEdit" => {
            let path = match field("file_path") {
                None | Some(Value::Null | Value::Bool(false)) => field("notebook_path"),
                p => p,
            };
            basename(path).map(|b| format!("edited {b}"))
        }
        "Write" => basename(field("file_path")).map(|b| format!("wrote {b}")),
        "Bash" => {
            let cmd = collapse_ws(&take_chars(field("command").and_then(Value::as_str)?, 2000));
            let cmd = take_chars(&cmd, DIGEST_CMD_CAP);
            (!cmd.is_empty()).then(|| format!("ran: {cmd}"))
        }
        _ => None,
    }
}

/// The snapshot as `todo-done:` / `todo-now:` / `todo-next:` segments, in that order.
fn todo_segments(todos: &Value) -> Option<String> {
    let items = todos.as_array()?;
    let mut by_status: [Vec<String>; 3] = Default::default();
    for item in items {
        let o = item.as_object()?;
        let cap = |s: String| take_chars(&collapse_ws(&take_chars(&s, 2000)), 240);
        match o.get("status").and_then(Value::as_str) {
            Some("completed") => {
                by_status[0].push(format!("todo-done: {}", cap(str_or(o.get("content"), "")?)));
            }
            Some("in_progress") => {
                let t = match o.get("activeForm") {
                    None | Some(Value::Null | Value::Bool(false)) => str_or(o.get("content"), "")?,
                    a => str_or(a, "")?,
                };
                by_status[1].push(format!("todo-now: {}", cap(t)));
            }
            Some("pending") => {
                by_status[2].push(format!("todo-next: {}", cap(str_or(o.get("content"), "")?)));
            }
            _ => {}
        }
    }
    Some(by_status.concat().join(SEP))
}

// ---- file plumbing ----

fn count_newlines(path: &Path) -> Option<usize> {
    let mut r = BufReader::new(File::open(path).ok()?);
    let mut n = 0;
    loop {
        let buf = r.fill_buf().ok()?;
        if buf.is_empty() {
            return Some(n);
        }
        n += buf.iter().filter(|&&b| b == b'\n').count();
        let len = buf.len();
        r.consume(len);
    }
}

/// Lines of a seekable source, newest first, read backward in [`CHUNK`]s so only the tail is
/// touched. A trailing newline ends the last line rather than starting an empty one.
struct RevLines<R> {
    src: R,
    /// File offset where `pending` begins.
    pos: u64,
    /// Bytes from `pos` to the end of the unreturned region; may begin mid-line.
    pending: Vec<u8>,
    done: bool,
}

impl<R: Read + Seek> RevLines<R> {
    fn new(mut src: R) -> std::io::Result<Self> {
        let len = src.seek(SeekFrom::End(0))?;
        let mut me = RevLines {
            src,
            pos: len,
            pending: Vec::new(),
            done: len == 0,
        };
        if len > 0 {
            me.fill()?;
            if me.pending.last() == Some(&b'\n') {
                me.pending.pop();
            }
        }
        Ok(me)
    }

    /// Prepends the next chunk toward the file start.
    fn fill(&mut self) -> std::io::Result<()> {
        let start = self.pos.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (self.pos - start) as usize];
        self.src.seek(SeekFrom::Start(start))?;
        self.src.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&self.pending);
        self.pending = chunk;
        self.pos = start;
        Ok(())
    }

    fn next_line(&mut self) -> Option<Vec<u8>> {
        if self.done {
            return None;
        }
        loop {
            if let Some(i) = self.pending.iter().rposition(|&b| b == b'\n') {
                let line = self.pending.split_off(i + 1);
                self.pending.pop();
                return Some(line);
            }
            if self.pos == 0 {
                self.done = true;
                return Some(std::mem::take(&mut self.pending));
            }
            if self.fill().is_err() {
                self.done = true;
                return None;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{self, Write};
    use std::path::PathBuf;

    fn fx(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn ctx(name: &str, n: usize, pc: usize, mode: CtxMode) -> String {
        Claude.context(&fx(name), n, pc, mode)
    }

    fn digest(name: &str, start: usize, budget: usize) -> String {
        Claude.digest(&fx(name), start, budget)
    }

    /// A scratch file removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("warden-summary-{}-{tag}.jsonl", std::process::id()));
            Scratch(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn user_line(text: &str) -> String {
        format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{text}\"}}}}\n")
    }

    // ---- ctx.sh selftest ports (ctx1..ctx17, ctx_todos1..8) ----

    #[test]
    fn ctx1_user_prose_present() {
        assert!(ctx("ctx_mixed.jsonl", 10, 240, CtxMode::Tail).contains("refactor the auth module"));
    }

    #[test]
    fn ctx2_recent_prose_present() {
        assert!(
            ctx("ctx_mixed.jsonl", 10, 240, CtxMode::Tail).contains("wire up the migration script")
        );
    }

    #[test]
    fn ctx3_assistant_prose_present() {
        assert!(ctx("ctx_mixed.jsonl", 10, 240, CtxMode::Tail).contains("sounds good"));
    }

    #[test]
    fn ctx4_to_ctx10_noise_filtered_and_full_string() {
        let out = ctx("ctx_mixed.jsonl", 10, 240, CtxMode::Tail);
        for leaked in [
            "def foo",                    // ctx4 code fence
            "ls -la",                     // ctx5 REPL prompt
            "git status",                 // ctx6 shell prompt
            "01HZQ4XJ5N8K3MFGRTVBPCY7AS", // ctx7 ULID
            "q=4",                        // ctx8 low prose
            "applied",                    // ctx9 tool_result
            "ok",                         // ctx10 filler
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
        assert_eq!(
            out,
            "lets refactor the auth module for soft deletes / sounds good, starting on the \
             model layer / now wire up the migration script"
        );
    }

    #[test]
    fn ctx11_head_is_earliest_surviving_prose() {
        assert_eq!(
            ctx("ctx_mixed.jsonl", 1, 240, CtxMode::Head),
            "lets refactor the auth module for soft deletes"
        );
    }

    #[test]
    fn ctx12_percap_clips() {
        assert_eq!(ctx("ctx_mixed.jsonl", 1, 8, CtxMode::Head), "lets ref");
    }

    #[test]
    fn ctx_todos1_to_5_latest_snapshot_in_order_joined() {
        let t = ctx("ctx_mixed.jsonl", 12, 120, CtxMode::Todos);
        assert_eq!(t, "Build site config / Implement layout / Add pricing page");
        assert!(!t.contains("Stale early plan"));
    }

    #[test]
    fn ctx_todos6_7_percap_clips_each_item() {
        assert_eq!(
            ctx("ctx_mixed.jsonl", 12, 5, CtxMode::Todos),
            "Build / Imple / Add p"
        );
    }

    #[test]
    fn ctx_todos8_count_cap() {
        assert_eq!(
            ctx("ctx_mixed.jsonl", 1, 120, CtxMode::Todos),
            "Build site config"
        );
    }

    #[test]
    fn ctx13_all_filler_is_empty() {
        for mode in [CtxMode::Head, CtxMode::Tail] {
            assert_eq!(ctx("all_filler.jsonl", 5, 240, mode), "");
        }
    }

    #[test]
    fn ctx14_missing_file_is_empty() {
        let p = std::env::temp_dir().join("warden-summary-does-not-exist");
        assert_eq!(Claude.context(&p, 5, 240, CtxMode::Tail), "");
    }

    #[test]
    fn ctx15_to_17_huge_line_bounded_and_next_line_survives() {
        let s = Scratch::new("ctx-huge");
        let big = "alpha beta gamma ".repeat(20000);
        let mut f = fs::File::create(&s.0).unwrap();
        f.write_all(user_line(&big).as_bytes()).unwrap();
        f.write_all(user_line("final short line here").as_bytes())
            .unwrap();
        drop(f);
        let out = Claude.context(&s.0, 5, 240, CtxMode::Tail);
        assert!(out.contains("alpha beta gamma"), "ctx15");
        assert!(out.contains("final short line here"), "ctx16");
        let seg = out.split(" / ").next().unwrap();
        assert!(
            seg.chars().count() <= 244,
            "ctx17 len {}",
            seg.chars().count()
        );
    }

    // ---- digest.sh selftest ports (selftest1..11, selftest_todo1..4) ----

    const FULL_DIGEST: &str = "lets refactor the auth module for soft deletes / sounds good, \
        starting on the model layer / edited auth.py / ran: pytest -q tests/test_auth.py \
        (failed) / todo-done: Write backfill / todo-now: Adding proration tests / \
        todo-next: Run migration";

    #[test]
    fn digest1_to_5_9_and_todo_snapshot() {
        let out = digest("digest_mixed.jsonl", 1, 10000);
        assert_eq!(out, FULL_DIGEST);
        // selftest1 user prose, 2 edited, 3 ran+failed, 4 Read leaked, 5 filler kept,
        // 9 bare line from an empty-input tool, todo1..4 are all pinned by the full string.
        assert!(!out.contains("models.py") && !out.contains("edited  "));
    }

    #[test]
    fn digest6_7_budget_trims_prose_keeps_tools() {
        assert_eq!(
            digest("digest_mixed.jsonl", 1, 30),
            "edited auth.py / ran: pytest -q tests/test_auth.py (failed) / \
             todo-done: Write backfill / todo-now: Adding proration tests / \
             todo-next: Run migration"
        );
    }

    #[test]
    fn digest8_start_line_skips_earlier_turns() {
        assert_eq!(
            digest("digest_mixed.jsonl", 5, 10000),
            "ran: pytest -q tests/test_auth.py (failed) / todo-done: Write backfill / \
             todo-now: Adding proration tests / todo-next: Run migration"
        );
    }

    #[test]
    fn digest10_11_huge_line_bounded() {
        let s = Scratch::new("digest-huge");
        fs::write(&s.0, user_line(&"alpha beta gamma ".repeat(20000))).unwrap();
        let out = Claude.digest(&s.0, 1, 10000);
        assert!(out.contains("alpha beta gamma"), "selftest10");
        assert!(
            out.chars().count() <= 244,
            "selftest11 len {}",
            out.chars().count()
        );
    }

    // ---- additional cases ----

    #[test]
    fn head_reads_from_session_start_on_long_transcript() {
        let s = Scratch::new("head-long");
        let mut f = fs::File::create(&s.0).unwrap();
        f.write_all(user_line("build the invoicing exporter for the finance team").as_bytes())
            .unwrap();
        for i in 0..3000 {
            f.write_all(user_line(&format!("working on step number {i} of the plan")).as_bytes())
                .unwrap();
        }
        drop(f);
        assert_eq!(
            Claude.context(&s.0, 1, 240, CtxMode::Head),
            "build the invoicing exporter for the finance team"
        );
        assert_eq!(
            Claude.context(&s.0, 1, 240, CtxMode::Tail),
            "working on step number 2999 of the plan"
        );
    }

    #[test]
    fn unreadable_path_returns_empty() {
        let missing = std::env::temp_dir().join("warden-summary-nope.jsonl");
        let dir = std::env::temp_dir();
        for p in [&missing, &dir] {
            for mode in [CtxMode::Head, CtxMode::Tail, CtxMode::Todos] {
                assert_eq!(Claude.context(p, 5, 240, mode), "");
            }
            assert_eq!(Claude.digest(p, 1, 10000), "");
            assert_eq!(Claude.line_count(p), 0);
        }
    }

    #[test]
    fn line_count_counts_newlines() {
        assert_eq!(Claude.line_count(&fx("ctx_mixed.jsonl")), 13);
        let s = Scratch::new("lc");
        fs::write(&s.0, "a\nb").unwrap();
        assert_eq!(Claude.line_count(&s.0), 1);
    }

    struct Counting<R> {
        inner: R,
        read: u64,
    }
    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n as u64;
            Ok(n)
        }
    }
    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn tail_reads_from_end_only() {
        let s = Scratch::new("fifty-mb");
        let mut f = io::BufWriter::new(fs::File::create(&s.0).unwrap());
        let mut written = 0u64;
        let mut i = 0u64;
        while written < 50 * 1024 * 1024 {
            let l = user_line(&format!(
                "filler turn number {i} describing some routine progress on the parser module"
            ));
            f.write_all(l.as_bytes()).unwrap();
            written += l.len() as u64;
            i += 1;
        }
        f.write_all(user_line("the very last message about shipping").as_bytes())
            .unwrap();
        f.flush().unwrap();
        drop(f);

        let mut r = Counting {
            inner: fs::File::open(&s.0).unwrap(),
            read: 0,
        };
        let out = tail_context(&mut r, 3, 240);
        assert!(
            out.ends_with("the very last message about shipping"),
            "{out}"
        );
        assert_eq!(out.matches(" / ").count(), 2);
        assert!(
            r.read < 1024 * 1024,
            "read {} bytes of a {written} byte file",
            r.read
        );
    }
}
