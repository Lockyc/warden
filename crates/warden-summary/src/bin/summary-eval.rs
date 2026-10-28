//! Replay eval for the done/now/next summary: replays transcripts through warden's `refresh` at
//! five evenly spaced cuts and, when amux is installed, through amux's shell pipeline on the same
//! cuts, then prints a Markdown side-by-side. Method and traps: agentmux's `docs/ai-summary.md`
//! ("How to evaluate", "LM backend"). Transcripts are read, never modified; the output quotes
//! them, so it belongs in an untracked file.
//!
//! Usage: `summary-eval [--url U --model M] <transcript.jsonl>...`

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use warden_summary::{
    apply, refresh, Claude, CtxMode, HttpModel, Model, SummaryState, Transcript, DIGEST_BUDGET,
    SUBJECT_MIN_SEGMENTS,
};

const CUTS: usize = 5;
/// Refresh ticks replayed per cut, so the drift counter (3 ticks) can act as it would live.
const TICKS_PER_CUT: usize = 3;
const CELL_MAX: usize = 120;
const TIMEOUT: Duration = Duration::from_secs(120);

fn cell(s: &str) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.replace('|', "\\|");
    if s.chars().count() > CELL_MAX {
        let t: String = s.chars().take(CELL_MAX - 1).collect();
        format!("{t}…")
    } else {
        s
    }
}

/// A mutation segment in a digest: `edited `, `wrote `, or `todo-done:` at a segment start.
fn has_mutation(digest: &str) -> bool {
    digest.split(" / ").any(|s| {
        let s = s.trim_start();
        s.starts_with("edited ") || s.starts_with("wrote ") || s.starts_with("todo-done:")
    })
}

fn has_done(stand: &str) -> bool {
    stand.starts_with("done:") || stand.contains(" done:")
}

/// Assert the endpoint answers as the named model with no reasoning tokens, and report the
/// per-request window by a large probe. Returns the report lines.
fn preflight(url: &str, model: &str) -> Result<Vec<String>, String> {
    let post = |content: &str, max: u32| -> Result<Value, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let mut r = agent
            .post(url)
            .send_json(json!({"model": model, "temperature": 0, "max_tokens": max,
                "reasoning_effort": "none",
                "messages": [{"role": "user", "content": content}]}))
            .map_err(|e| format!("endpoint unreachable: {e}"))?;
        let status = r.status();
        let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                text.chars().take(200).collect::<String>()
            ));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    };
    let v = post("Reply with the single word: ok", 50)?;
    let served = v["model"].as_str().unwrap_or("?").to_string();
    let reasoning = v
        .pointer("/usage/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .ok_or("response usage carries no reasoning_tokens; cannot assert it is 0")?;
    if reasoning != 0 {
        return Err(format!("reasoning_tokens = {reasoning}, expected 0"));
    }
    if served != model {
        return Err(format!("endpoint served {served}, expected {model}"));
    }
    let big = "alpha bravo charlie delta ".repeat(750);
    let w = post(&format!("{big}\nReply with the single word: ok"), 20)
        .map(|v| v.pointer("/usage/prompt_tokens").and_then(Value::as_u64))
        .map_or_else(|e| format!("probe failed: {e}"), |t| format!("{t:?}"));
    Ok(vec![
        format!("- endpoint: {url}"),
        format!("- model served: {served}"),
        format!("- usage.reasoning_tokens on the probe: {reasoning} (asserted == 0)"),
        format!("- large-prompt probe prompt_tokens (accepted): {w}"),
    ])
}

fn sh_out(script: &str, args: &[&str], envs: &[(&str, &str)], stdin: &str) -> String {
    let mut c = Command::new("sh");
    c.arg("-c").arg(script).arg("_");
    c.args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = c.spawn() else {
        return String::new();
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin.as_bytes());
    }
    child
        .wait_with_output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

struct Amux {
    scripts: PathBuf,
    url: String,
    model: String,
}

struct AmuxCut {
    subject: String,
    stand: String,
    digest: String,
}

impl Amux {
    fn detect() -> Option<Amux> {
        let home = std::env::var("HOME").ok()?;
        let scripts = Path::new(&home).join(".agentmux/scripts");
        (scripts.join("summarise.sh").is_file() && scripts.join("claude/ctx.sh").is_file())
            .then_some(Amux {
                scripts,
                url: String::new(),
                model: String::new(),
            })
    }

    /// amux's pipeline over `tp` (a truncated copy), as `tmux-status.sh` composes it. Stateless:
    /// the subject is derived fresh from the goal context, with no drift history.
    fn run(&self, tp: &Path) -> AmuxCut {
        let s = self.scripts.to_string_lossy().to_string();
        let tp = tp.to_string_lossy().to_string();
        let envs = [
            ("AGENTMUX_LLM_URL", self.url.as_str()),
            ("AGENTMUX_LLM_MODEL", self.model.as_str()),
            ("AGENTMUX_LLM_TIMEOUT", "120"),
        ];
        let ctx = |n: &str, cap: &str, mode: &str| {
            sh_out(
                r#"sh "$1/claude/ctx.sh" "$2" "$3" "$4" "$5""#,
                &[&s, &tp, n, cap, mode],
                &envs,
                "",
            )
        };
        let early = ctx("8", "300", "head");
        let todos = ctx("12", "120", "todos");
        let mut goal = early.clone();
        if !todos.is_empty() {
            goal = if goal.is_empty() {
                format!("plan: {todos}")
            } else {
                format!("{goal} / plan: {todos}")
            };
        }
        if goal.is_empty() {
            goal = ctx("6", "400", "tail");
        }
        let segs = if early.is_empty() {
            0
        } else {
            early.matches(" / ").count() + 1
        };
        let subject = if segs >= SUBJECT_MIN_SEGMENTS {
            sh_out(r#"sh "$1/summarise.sh" 6 label"#, &[&s], &envs, &goal)
        } else {
            String::new()
        };
        let mut digest = sh_out(
            r#"sh "$1/claude/digest.sh" "$2" 1 10000"#,
            &[&s, &tp],
            &envs,
            "",
        );
        if digest.is_empty() {
            digest = ctx("6", "400", "tail");
        }
        let mut envs2 = envs.to_vec();
        envs2.push(("AGENTMUX_SUBJECT", subject.as_str()));
        let mut st = sh_out(r#"sh "$1/summarise.sh" 55 stand"#, &[&s], &envs2, &digest);
        if !st.is_empty() {
            st = sh_out(
                r#"sh "$1/strip_unbacked_done.sh" "$2""#,
                &[&s, &st],
                &envs,
                &digest,
            );
        }
        AmuxCut {
            subject,
            stand: st,
            digest,
        }
    }
}

fn truncated(src: &Path, n: usize, dst: &Path) {
    let text = std::fs::read_to_string(src).unwrap_or_default();
    let mut out = String::new();
    for l in text.lines().take(n) {
        out.push_str(l);
        out.push('\n');
    }
    let _ = std::fs::write(dst, out);
}

fn rows_line(r: &warden_summary::StandRows) -> String {
    let mut p = Vec::new();
    if let Some(s) = &r.subject {
        p.push(format!("{s}."));
    }
    for (k, v) in [("done", &r.done), ("now", &r.now), ("next", &r.next)] {
        if let Some(v) = v {
            p.push(format!("{k}: {v}"));
        }
    }
    p.join(" ")
}

fn main() {
    let mut url = "http://localhost:1234/v1/chat/completions".to_string();
    let mut model_id = "qwen/qwen3.5-9b".to_string();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--url" => url = args.next().unwrap_or(url),
            "--model" => model_id = args.next().unwrap_or(model_id),
            _ => files.push(PathBuf::from(a)),
        }
    }
    if files.is_empty() {
        eprintln!("usage: summary-eval [--url U --model M] <transcript.jsonl>...");
        std::process::exit(2);
    }
    let pre = match preflight(&url, &model_id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("preflight failed: {e}");
            std::process::exit(1);
        }
    };
    let mut amux = Amux::detect();
    if let Some(a) = amux.as_mut() {
        a.url = url.clone();
        a.model = model_id.clone();
    }
    let http = HttpModel {
        url: url.clone(),
        model: model_id.clone(),
        timeout: TIMEOUT,
    };
    let tmp = std::env::temp_dir().join(format!("summary-eval-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);

    println!("# Summary replay eval\n");
    for l in &pre {
        println!("{l}");
    }
    println!(
        "- amux pipeline: {}",
        if amux.is_some() {
            "present (stateless per cut: subject derived fresh from the goal context)"
        } else {
            "absent (amux columns blank)"
        }
    );
    println!(
        "- warden replay: {TICKS_PER_CUT} refresh+apply ticks per cut, state carried across cuts; row shows the last tick\n"
    );

    struct Cut {
        n: usize,
        goal: String,
        w_subj: String,
        a_subj: String,
        w_stand: String,
        a_stand: String,
        w_done: bool,
        w_ev: bool,
        a_done: bool,
        a_ev: bool,
    }
    let claude = Claude;
    let mut all: Vec<(String, Vec<Cut>)> = Vec::new();
    for (ti, f) in files.iter().enumerate() {
        let total = claude.line_count(f);
        let name = format!("T{}", ti + 1);
        eprintln!("{name}: {total} lines");
        let mut state = SummaryState::default();
        let mut cuts = Vec::new();
        for k in 1..=CUTS {
            let n = total * k / CUTS;
            let tp = tmp.join(format!("{name}-{k}.jsonl"));
            truncated(f, n, &tp);
            for _ in 0..TICKS_PER_CUT {
                let r = refresh(&state, &claude, &http as &dyn Model, &tp);
                apply(&mut state, r);
            }
            let goal = claude.context(&tp, 1, CELL_MAX, CtxMode::Head);
            let w_stand = state.rows.as_ref().map(rows_line).unwrap_or_default();
            let start = state.start_line.max(1);
            let w_digest = claude.digest(&tp, start, DIGEST_BUDGET);
            let a = amux.as_ref().map(|a| a.run(&tp));
            let (a_subj, a_stand, a_digest) = a
                .map(|c| (c.subject, c.stand, c.digest))
                .unwrap_or_default();
            eprintln!("  cut {k}/{CUTS} (line {n}) done");
            cuts.push(Cut {
                n,
                goal,
                w_subj: state.subject.clone().unwrap_or_default(),
                a_subj,
                w_done: has_done(&w_stand),
                w_ev: has_mutation(&w_digest),
                a_done: has_done(&a_stand),
                a_ev: has_mutation(&a_digest),
                w_stand,
                a_stand,
            });
        }
        all.push((name, cuts));
    }
    let _ = std::fs::remove_dir_all(&tmp);

    for (name, cuts) in &all {
        println!("## {name}\n");
        println!("| transcript | cut | stated goal (first user message) | warden subject | amux subject | warden stand | amux stand |");
        println!("| --- | --- | --- | --- | --- | --- | --- |");
        for (i, c) in cuts.iter().enumerate() {
            println!(
                "| {name} | {}/{CUTS} (line {}) | {} | {} | {} | {} | {} |",
                i + 1,
                c.n,
                cell(&c.goal),
                cell(&c.w_subj),
                cell(&c.a_subj),
                cell(&c.w_stand),
                cell(&c.a_stand)
            );
        }
        println!("\nAnti-invention ({name}): `done:` clause vs mutation evidence in the digest\n");
        println!("| cut | warden done | warden evidence | amux done | amux evidence |");
        println!("| --- | --- | --- | --- | --- |");
        let yn = |b: bool| if b { "yes" } else { "no" };
        for (i, c) in cuts.iter().enumerate() {
            println!(
                "| {}/{CUTS} | {} | {} | {} | {} |",
                i + 1,
                yn(c.w_done),
                yn(c.w_ev),
                yn(c.a_done),
                yn(c.a_ev)
            );
        }
        println!();
    }

    println!("## Subject as tab name\n");
    println!("Each row reads along the five cuts.\n");
    for (name, cuts) in &all {
        println!(
            "- {name} warden: {}",
            cuts.iter()
                .map(|c| cell(&c.w_subj))
                .collect::<Vec<_>>()
                .join(" → ")
        );
        if amux.is_some() {
            println!(
                "- {name} amux: {}",
                cuts.iter()
                    .map(|c| cell(&c.a_subj))
                    .collect::<Vec<_>>()
                    .join(" → ")
            );
        }
    }
}
