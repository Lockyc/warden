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

    fn digest(
        &self,
        path: &Path,
        anchor: u64,
        cursor: &mut DigestCursor,
        char_budget: usize,
    ) -> String {
        File::open(path)
            .ok()
            .and_then(|f| digest_from(f, anchor, cursor, char_budget))
            .unwrap_or_default()
    }

    fn anchor(&self, path: &Path) -> u64 {
        File::open(path).ok().and_then(last_line_start).unwrap_or(0)
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

#[derive(Debug, PartialEq, Clone, Copy)]
enum Tag {
    Prose,
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    tag: Tag,
    len: usize,
    text: String,
    removed: bool,
    /// A tool line's `tool_use` id, for marking it failed when its result arrives.
    tool_id: Option<String>,
}

/// Appended to a tool line whose `tool_result` errored.
const FAILED: &str = " (failed)";
/// Dropped entries kept before [`Budgeted::compact`] sweeps them out.
const COMPACT_AFTER: usize = 1024;

/// The digest body under a char budget. The oldest prose is dropped first, tool lines never;
/// trimming as entries arrive reaches the same result as trimming at the end, because totals only
/// grow (a tool line marked failed later grows too), and keeps memory to the budget plus the tool
/// lines.
#[derive(Debug, Clone, PartialEq, Default)]
struct Budgeted {
    budget: i64,
    entries: Vec<Entry>,
    total: i64,
    cursor: usize,
    removed: usize,
}

impl Budgeted {
    fn push(&mut self, tag: Tag, text: String, tool_id: Option<String>) {
        let len = text.chars().count();
        self.total += len as i64 + if self.entries.is_empty() { 0 } else { 3 };
        self.entries.push(Entry {
            tag,
            len,
            text,
            removed: false,
            tool_id,
        });
        self.trim();
    }

    /// Mark every tool line of `id` failed.
    fn fail(&mut self, id: &str) {
        let n = FAILED.chars().count();
        for e in self.entries.iter_mut().rev() {
            if e.tag == Tag::Tool && e.tool_id.as_deref() == Some(id) {
                e.tool_id = None;
                e.text.push_str(FAILED);
                e.len += n;
                self.total += n as i64;
            }
        }
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
                break;
            };
            e.removed = true;
            e.text = String::new();
            self.total -= e.len as i64 + 3;
            self.removed += 1;
        }
        self.compact();
    }

    /// Sweep out dropped entries once they outnumber the live ones, so a cursor kept across a
    /// long session holds the budget plus its tool lines, not every turn it ever saw.
    fn compact(&mut self) {
        if self.removed < COMPACT_AFTER || self.removed * 2 < self.entries.len() {
            return;
        }
        self.entries.retain(|e| !e.removed);
        self.cursor = 0;
        self.removed = 0;
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

/// How far a digest has read a transcript, so the next one reads only what was appended since.
/// Kept by the caller across refreshes ([`crate::SummaryState::digest`]); a cursor for another
/// anchor or budget, or a file now shorter than it has read, starts over from the anchor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DigestCursor {
    /// Byte offset the digest starts from.
    anchor: u64,
    /// Bytes read so far (an offset at a line start).
    read: u64,
    budget: usize,
    /// `tool_use` ids whose `tool_result` errored.
    failed: HashSet<String>,
    body: Budgeted,
    /// The latest `TodoWrite` snapshot.
    todos: Option<Value>,
}

impl DigestCursor {
    fn new(anchor: u64, budget: usize) -> Self {
        DigestCursor {
            anchor,
            read: anchor,
            budget,
            body: Budgeted {
                budget: i64::try_from(budget).unwrap_or(i64::MAX),
                ..Budgeted::default()
            },
            ..DigestCursor::default()
        }
    }

    /// One transcript line: errored results mark their tool lines, then the prose, the mutating
    /// tool one-liners and the task list it carries.
    fn feed(&mut self, v: &Value) {
        if v.get("type").and_then(Value::as_str) == Some("user") {
            for b in content_blocks(v) {
                if b.get("type").and_then(Value::as_str) == Some("tool_result")
                    && b.get("is_error") == Some(&Value::Bool(true))
                {
                    if let Some(id) = str_of(b, "tool_use_id") {
                        if self.failed.insert(id.clone()) {
                            self.body.fail(&id);
                        }
                    }
                }
            }
        }
        if let Some(t) = todo_snapshot(v) {
            self.todos = Some(t);
        }
        digest_entries(v, &self.failed, &mut self.body);
    }

    fn render(&self) -> String {
        let main = self.body.render();
        let seg = self
            .todos
            .as_ref()
            .and_then(todo_segments)
            .unwrap_or_default();
        match (main.is_empty(), seg.is_empty()) {
            (false, false) => format!("{main}{SEP}{seg}"),
            (false, true) => main,
            _ => seg,
        }
    }
}

/// The digest of `src` from byte `anchor` to its end, reading only past what `cursor` has seen.
/// Each line is parsed once. A last line with no newline that doesn't parse is still being
/// written: it is left for the next read. `None` on an I/O error, leaving `cursor` as it was.
pub(crate) fn digest_from<R: Read + Seek>(
    mut src: R,
    anchor: u64,
    cursor: &mut DigestCursor,
    budget: usize,
) -> Option<String> {
    let len = src.seek(SeekFrom::End(0)).ok()?;
    let mut c = if cursor.anchor == anchor && cursor.budget == budget && cursor.read <= len {
        std::mem::take(cursor)
    } else {
        DigestCursor::new(anchor, budget)
    };
    if c.read < len {
        if src.seek(SeekFrom::Start(c.read)).is_err() {
            *cursor = c;
            return None;
        }
        let mut r = BufReader::new(&mut src);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let v = parse(&buf);
                    if v.is_none() && buf.last() != Some(&b'\n') {
                        break;
                    }
                    c.read += n as u64;
                    if let Some(v) = v {
                        c.feed(&v);
                    }
                }
                Err(_) => break,
            }
        }
    }
    let out = c.render();
    *cursor = c;
    Some(out)
}

/// The byte offset where the last complete line starts (0 with fewer than two newlines), read
/// backward from the end.
pub(crate) fn last_line_start<R: Read + Seek>(mut src: R) -> Option<u64> {
    let mut pos = src.seek(SeekFrom::End(0)).ok()?;
    let mut newlines = 0;
    while pos > 0 {
        let start = pos.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; usize::try_from(pos - start).ok()?];
        src.seek(SeekFrom::Start(start)).ok()?;
        src.read_exact(&mut chunk).ok()?;
        for (i, &b) in chunk.iter().enumerate().rev() {
            if b == b'\n' {
                newlines += 1;
                if newlines == 2 {
                    return Some(start + i as u64 + 1);
                }
            }
        }
        pos = start;
    }
    Some(0)
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
                            let mut id = str_of(b, "id");
                            if id.as_ref().is_some_and(|id| failed.contains(id)) {
                                line.push_str(FAILED);
                                id = None;
                            }
                            out.push(Tag::Tool, line, id);
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
    out.push(Tag::Prose, take_chars(&text, DIGEST_PROSE_CAP), None);
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

/// Bytes [`RevLines`] keeps of one line. Past it the line comes back empty: a transcript line
/// carrying a large image or file dump is not a turn worth holding in memory.
const MAX_LINE: usize = 8 * 1024 * 1024;

/// Lines of a seekable source, newest first, read backward in [`CHUNK`]s so only the tail is
/// touched. A trailing newline ends the last line rather than starting an empty one.
struct RevLines<R> {
    src: R,
    /// File offset where `pending` begins.
    pos: u64,
    /// Bytes from `pos` to the end of the unreturned region; may begin mid-line.
    pending: Vec<u8>,
    done: bool,
    /// [`MAX_LINE`]; a field so a test can lower it.
    max_line: usize,
    /// The line being read outgrew `max_line`: its bytes are discarded as they arrive.
    skipping: bool,
}

impl<R: Read + Seek> RevLines<R> {
    fn new(mut src: R) -> std::io::Result<Self> {
        let len = src.seek(SeekFrom::End(0))?;
        let mut me = RevLines {
            src,
            pos: len,
            pending: Vec::new(),
            done: len == 0,
            max_line: MAX_LINE,
            skipping: false,
        };
        if len > 0 {
            me.fill()?;
            if me.pending.last() == Some(&b'\n') {
                me.pending.pop();
            }
        }
        Ok(me)
    }

    /// Prepends the next chunk toward the file start: at least as many bytes as are pending, so
    /// a long line's chunks double and copying it stays linear rather than quadratic.
    fn fill(&mut self) -> std::io::Result<()> {
        let want = CHUNK.max(self.pending.len() as u64);
        let start = self.pos.saturating_sub(want);
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
                return Some(self.unless_skipped(line));
            }
            if self.pos == 0 {
                self.done = true;
                let line = std::mem::take(&mut self.pending);
                return Some(self.unless_skipped(line));
            }
            if self.pending.len() > self.max_line {
                self.skipping = true;
                self.pending.clear();
            }
            if self.fill().is_err() {
                self.done = true;
                return None;
            }
        }
    }

    /// `line`, or empty when it was the oversized one being skipped; ends the skip.
    fn unless_skipped(&mut self, line: Vec<u8>) -> Vec<u8> {
        if std::mem::take(&mut self.skipping) {
            Vec::new()
        } else {
            line
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

    fn digest(name: &str, anchor: u64, budget: usize) -> String {
        fresh_digest(&fx(name), anchor, budget)
    }

    #[test]
    fn rev_lines_drops_a_line_past_the_cap_and_keeps_its_neighbours() {
        let chunk = CHUNK as usize;
        let mut src = b"first\n".to_vec();
        src.extend(std::iter::repeat_n(b'x', 5 * chunk));
        src.extend_from_slice(b"\nlast\n");
        let mut lines = RevLines::new(io::Cursor::new(src)).unwrap();
        lines.max_line = chunk;
        assert_eq!(lines.next_line().as_deref(), Some(&b"last"[..]));
        assert_eq!(
            lines.next_line().as_deref(),
            Some(&b""[..]),
            "the oversized line"
        );
        assert!(
            lines.pending.len() <= 2 * chunk,
            "kept {} bytes",
            lines.pending.len()
        );
        assert_eq!(lines.next_line().as_deref(), Some(&b"first"[..]));
        assert_eq!(lines.next_line(), None);

        // Under the cap a line many chunks long comes back whole.
        let mut src = vec![b'y'; 5 * chunk];
        src.extend_from_slice(b"\nlast\n");
        let mut lines = RevLines::new(io::Cursor::new(src)).unwrap();
        assert_eq!(lines.next_line().as_deref(), Some(&b"last"[..]));
        assert_eq!(lines.next_line().map(|l| l.len()), Some(5 * chunk));
    }

    fn fresh_digest(p: &Path, anchor: u64, budget: usize) -> String {
        Claude.digest(p, anchor, &mut DigestCursor::default(), budget)
    }

    /// Byte offset of 1-based line `n` of `p`.
    fn line_offset(p: &Path, n: usize) -> u64 {
        let bytes = fs::read(p).unwrap();
        let mut seen = 1;
        for (i, &b) in bytes.iter().enumerate() {
            if seen == n {
                return i as u64;
            }
            if b == b'\n' {
                seen += 1;
            }
        }
        bytes.len() as u64
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
        let out = digest("digest_mixed.jsonl", 0, 10000);
        assert_eq!(out, FULL_DIGEST);
        // selftest1 user prose, 2 edited, 3 ran+failed, 4 Read leaked, 5 filler kept,
        // 9 bare line from an empty-input tool, todo1..4 are all pinned by the full string.
        assert!(!out.contains("models.py") && !out.contains("edited  "));
    }

    #[test]
    fn digest6_7_budget_trims_prose_keeps_tools() {
        assert_eq!(
            digest("digest_mixed.jsonl", 0, 30),
            "edited auth.py / ran: pytest -q tests/test_auth.py (failed) / \
             todo-done: Write backfill / todo-now: Adding proration tests / \
             todo-next: Run migration"
        );
    }

    #[test]
    fn digest8_anchor_skips_earlier_turns() {
        let from_line_5 = line_offset(&fx("digest_mixed.jsonl"), 5);
        assert_eq!(
            digest("digest_mixed.jsonl", from_line_5, 10000),
            "ran: pytest -q tests/test_auth.py (failed) / todo-done: Write backfill / \
             todo-now: Adding proration tests / todo-next: Run migration"
        );
    }

    #[test]
    fn digest10_11_huge_line_bounded() {
        let s = Scratch::new("digest-huge");
        fs::write(&s.0, user_line(&"alpha beta gamma ".repeat(20000))).unwrap();
        let out = fresh_digest(&s.0, 0, 10000);
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
            assert_eq!(fresh_digest(p, 0, 10000), "");
            assert_eq!(Claude.anchor(p), 0);
        }
    }

    #[test]
    fn anchor_is_the_last_complete_lines_start() {
        let mixed = fx("ctx_mixed.jsonl");
        assert_eq!(Claude.anchor(&mixed), line_offset(&mixed, 13));
        let s = Scratch::new("anchor");
        fs::write(&s.0, "a\nb").unwrap();
        assert_eq!(Claude.anchor(&s.0), 0, "one complete line");
        fs::write(&s.0, "a\nbb\ncc").unwrap();
        assert_eq!(Claude.anchor(&s.0), 2, "the partial last line isn't one");
        fs::write(&s.0, "").unwrap();
        assert_eq!(Claude.anchor(&s.0), 0);
    }

    fn tool_use(id: &str, cmd: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\
             \"id\":\"{id}\",\"name\":\"Bash\",\"input\":{{\"command\":\"{cmd}\"}}}}]}}}}\n"
        )
    }

    fn tool_error(id: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\
             \"tool_use_id\":\"{id}\",\"is_error\":true,\"content\":\"boom\"}}]}}}}\n"
        )
    }

    fn append(p: &Path, s: &str) {
        let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn a_kept_cursor_reads_on_to_the_same_digest() {
        let s = Scratch::new("cursor");
        fs::write(&s.0, user_line("build the invoicing exporter please")).unwrap();
        append(&s.0, &tool_use("t1", "cargo test"));
        let mut c = DigestCursor::default();
        assert_eq!(
            Claude.digest(&s.0, 0, &mut c, 10000),
            "build the invoicing exporter please / ran: cargo test"
        );

        // The result arrives in a later read, a line is mid-write, and the budget drops prose.
        append(&s.0, &tool_error("t1"));
        append(
            &s.0,
            &user_line("now wire the exporter into the nightly job"),
        );
        append(&s.0, "{\"type\":\"user\",\"mess");
        let kept = Claude.digest(&s.0, 0, &mut c, 10000);
        assert_eq!(
            kept,
            "build the invoicing exporter please / ran: cargo test (failed) / now wire the \
             exporter into the nightly job"
        );
        append(
            &s.0,
            "age\":{\"content\":\"and add a retry for the failed exports\"}}\n",
        );
        let kept = Claude.digest(&s.0, 0, &mut c, 10000);
        assert_eq!(
            kept,
            fresh_digest(&s.0, 0, 10000),
            "the finished line is read once"
        );
        assert!(kept.ends_with("and add a retry for the failed exports"));
        assert_eq!(
            Claude.digest(&s.0, 0, &mut c, 40),
            fresh_digest(&s.0, 0, 40),
            "a new budget starts over"
        );
        let from_2 = line_offset(&s.0, 2);
        assert_eq!(
            Claude.digest(&s.0, from_2, &mut c, 10000),
            fresh_digest(&s.0, from_2, 10000),
            "so does a new anchor"
        );
    }

    #[test]
    fn digest_reads_only_what_was_appended() {
        let s = Scratch::new("digest-fifty-mb");
        let mut f = io::BufWriter::new(fs::File::create(&s.0).unwrap());
        let mut written = 0u64;
        let mut i = 0u64;
        while written < 50 * 1024 * 1024 {
            let l = if i % 5000 == 0 {
                tool_use(&format!("t{i}"), &format!("cargo test step {i}"))
            } else {
                user_line(&format!(
                    "filler turn number {i} describing some routine progress on the parser module"
                ))
            };
            f.write_all(l.as_bytes()).unwrap();
            written += l.len() as u64;
            i += 1;
        }
        f.flush().unwrap();
        drop(f);

        let mut c = DigestCursor::default();
        let first = Claude.digest(&s.0, 0, &mut c, 10000);
        assert!(first.contains("ran: cargo test step 0"));

        append(&s.0, &tool_error("t0"));
        append(&s.0, &user_line("the very last message about shipping"));
        let mut r = Counting {
            inner: fs::File::open(&s.0).unwrap(),
            read: 0,
        };
        let next = digest_from(&mut r, 0, &mut c, 10000).unwrap();
        assert!(
            r.read < 1024 * 1024,
            "read {} bytes of a {written} byte file",
            r.read
        );
        assert!(next.contains("ran: cargo test step 0 (failed)"));
        assert!(next.ends_with("the very last message about shipping"));
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
