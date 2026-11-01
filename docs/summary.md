---
type: decision
links:
  - rel: part-of
    to: CLAUDE.md
    note: CLAUDE.md points here before any work on the agent summaries
  - rel: references
    to: docs/rex.md
    note: the summary driver runs per agent record of a Rex tab
---
# Agent summaries — design rationale

Each Claude Code agent tab on the local Rex server gets a **subject** (a stable label for what the
session is about) and a **done / now / next** stand line, shown in the strip under the agent pane.
This doc records why the pipeline is shaped this way and which approaches are already ruled out, so
a revisit measures and moves forward instead of re-trying dead ends.

> **Start every revisit by running the replay eval (see "How to evaluate") before forming any
> hypothesis.** A screenshot leads to the wrong cause. Reproduce the bad subject from a real
> transcript first, then theorize.

The implementation lives in the `warden-summary` crate (pure pipeline) and
`crates/warden-app/src/rex/summary.rs` (its driver); their docblocks carry the per-symbol why. This
doc holds only the cross-cutting understanding no single symbol owns. When an open question is
resolved, fold the answer into its section and delete the question.

## Pipeline

```
OSC 7501 status change / TICK   →  SummaryDriver (main thread: due? which conversation?)
                                     → worker thread: refresh()
                                         ├─ Transcript::ctx     (head / todos / tail → goal context, drift probe)
                                         ├─ Transcript::digest  (→ done/now/next signal, DIGEST_BUDGET chars)
                                         ├─ label / stand       (the model call, HttpModel)
                                         └─ strip_unbacked_done (anti-invention gate)
                                     → SummaryDriver::accept → apply() → strip + tab name
```

- **The transcript is found from the resume command's id**, never from a reported path (a report
  is a forgeable terminal escape). A new resume id (`/clear`, a resumed session) is a new
  conversation; a result derived from the old one is dropped.
- **Refreshes** are driven by the record's OSC 7501 changes while working, time-throttled
  (`THROTTLE`), plus one on entering `done`. The throttle stamp is taken before the call, so a
  failing endpoint is not hammered.
- **Two channels, two scopes** (invariant 3): the subject is derived from the goal context and
  held in `SummaryState`; the stand line is re-derived each refresh from the digest and parsed by
  `parse_stand` into the strip's rows.
- **The subject names the tab** while the user hasn't renamed it (`subject_name` in
  `rex/summary.rs`); a fork keeps its `⑂ n` name until its subject differs from its parent's.

## Invariants

These are load-bearing. Breaking one reintroduces a failure below.

1. **The summary never affects the agent.** All model work runs in warden-app, off the main thread;
   nothing in the agent's hook path waits on it.
2. **Degrade silently.** Every failure keeps the last good rows; nothing error-shaped reaches the
   strip, and a blank or stale summary is acceptable.
3. **Subject = goal (stable); stand line = current task (moving).** The subject names what the
   session is about; subtask churn belongs in done/now/next. The subject is replaced only on
   sustained drift (`DRIFT_TICKS`), then re-derived from the goal context.
4. **`recent` never feeds the subject.** Recent activity reliably narrows the goal to the latest
   action. Recent is for the drift probe and the stand line only.
5. **Head = session start; tail = recent.** The stated goal lives at the opening of the transcript.
   Never bound the head window by a tail cap, or long sessions lose their goal.
6. **A first subject waits for enough head** (`SUBJECT_MIN_SEGMENTS`). A subject derived from a
   session's opening exchange names its first question, not its goal — and the subject is also the
   tab's name.
7. **Anti-invention.** The stand line must not fabricate milestones, generic domain-stereotyped
   steps, or a `done:` clause unbacked by file-mutation evidence (`strip_unbacked_done` is the
   gate). Read-only investigation is not "done".
8. **Third-party scope.** "another agent is doing X", "my other branch" are background context,
   not the current activity.

## Model backend: why a healthy-looking endpoint returns nothing

**Two independent backend faults both present as a permanently blank summary and nothing else.**
Invariant 2 means a 500, a truncation and a weak model look identical from outside, so when the
summary is blank, measure the endpoint before blaming the model. Both were once live together and
each was misread as "the local model is too weak".

### Reasoning tokens eat the whole output budget

`HttpModel` sends `max_tokens: 200`, and **reasoning tokens are drawn from that same budget**. A
hybrid-reasoning model left to think spends all 200 on reasoning and returns
`finish_reason: "length"` with `content: ""`, on every call (measured on qwen3.5-4b: 199/200
reasoning tokens).

- The fix is `reasoning_effort: "none"` in the request body, sent unconditionally; non-reasoning
  models ignore it (byte-identical output verified on qwen2.5-14b-instruct and qwen3-coder-30b).
- **Do not "fix" this by raising `max_tokens`.** That pays seconds of thinking for a label, and
  the model still reasons.
- **Do not swap in `chat_template_kwargs: {enable_thinking: false}`** — Qwen's documented switch,
  which LM Studio ignores. Verify any replacement by asserting `usage.reasoning_tokens == 0`, not
  by reading a vendor doc.
- Hybrid reasoning is the default posture of current small models (Qwen3.5, Gemma 4), so it must
  be forced per request.

### The context budget

**Each request needs about 4k tokens of context; 4096 is the practical floor.** Stand mode sends
the digest (`DIGEST_BUDGET` chars, about 2700 tokens) plus a ~500-token system prompt; label mode
sends far less.

- **Footgun — the per-request window can be a fraction of the advertised context, silently.**
  llama.cpp (GGUF) divides `n_ctx` across parallel slots, so LM Studio's "8192" at `parallel 4`
  hands each request 2048. Every stand call overflows (HTTP 500, `Context size has been exceeded`),
  invariant 2 swallows it, and the stand rows stay blank. **The tell is the asymmetry: the subject
  keeps working while the stand line alone stays blank.**
- **Do not apply `context ÷ parallel` as a rule — it is backend-specific.** MLX does not divide:
  qwen3.5-9b (MLX) at `context 8192 / parallel 4` accepted an 8012-token prompt, while
  qwen2.5-14b-instruct (GGUF) on the same digest failed at `parallel 4` and passed at `parallel 1`.
  Measure the real ceiling: send growing prompts and read `usage.prompt_tokens` on the last that
  succeeds.

## Failure taxonomy (ruled out — do not re-try)

| Approach | Why it fails |
|---|---|
| Append the fresh candidate to the subject | Accretes stale slices into soup (`modal focus ring; siteexit…; redirecting click handlers`). |
| Replace the subject on *any* drift (zero word overlap) | A deep subgoal dive has no overlap with the goal, so the goal is replaced by the subtask. |
| Include `recent` in the subject context | Narrows the goal to the latest action (`tiptap editor` → `…init destroy race guard`). |
| Bound the head window with a tail cap | On a session longer than the cap, "head" is the earliest of the last N lines — mid-session — so the goal at line ~1 is invisible. |
| Refresh only when the user sends a prompt | During a long autonomous turn the summary freezes at the last prompt. Refresh on the agent's working status instead. |

## Signal sources and their limits

- **First user message** — the clearest goal statement when the session opens with intent.
  Captured by the head window.
- **TodoWrite task list** — the strongest goal signal for orchestration sessions, but absent in
  single-focus debugging or audit sessions. Never the only anchor.
- **Early head turns** — a good goal proxy, but early assistant investigation prose is already
  subgoal-flavoured and can dilute it.
- **Recent tail** — current activity: good for the stand line and the drift probe, toxic to the
  subject (invariant 4).

## How to evaluate (measure, don't theorize)

The reliable signal is offline replay over real transcripts: `just summary-eval <transcript.jsonl>...`.
It replays each transcript through `refresh` at five evenly spaced cuts, carrying state across cuts
so subject drift evolves, and prints a Markdown side-by-side of subject and stand per cut plus a
`done:`-vs-mutation-evidence table. Its output quotes private conversations: keep it untracked.

1. Pick recent transcripts from `~/.claude*/projects/<encoded-project>/*.jsonl`; the longest
   sessions are the most drift-prone. Include a short session, a read-only one, and one that
   changes topic, or the hysteresis and anti-invention paths go unexercised.
2. It needs a reachable model. Before measuring it asserts the endpoint serves the named model with
   `usage.reasoning_tokens == 0`; output is deterministic (temperature 0).
3. **Give every candidate the same window and confirm it by measurement** (e.g.
   `lms load <m> --context-length 8192 --parallel 1`, then a growing-prompt probe). `lms ps`
   reports the configured number, which is not always the per-request one.
4. **Load each candidate explicitly; don't rely on JIT.** JIT picks the context for you and fails
   outright under memory pressure (`Failed to load model … Operation canceled`), which degrades to
   the same silent empty output as a bad model.
5. **Load under a private identifier and prove the endpoint answers as it.** A bare model key
   routes to whichever instance holds it, and an absent identifier resolves to another loaded model
   rather than erroring. Assert the identifier is loaded before and after the run.

**Every failure mode here converges on one symptom: an empty string.** A 500, reasoning
truncation, an unloaded model, a misrouted identifier and a bad model are one observation. Read
the raw response — `finish_reason`, `usage.reasoning_tokens`, `.error` — whenever a cell is empty,
before concluding anything about the model.

## Open questions

- **Pivot-following without narrowing.** The subject is goal-anchored and won't follow a genuine
  mid-session pivot ("forget that, now do X") in a session with no task list. A re-derive from
  recent *user messages only* on sustained drift could capture a real pivot without reintroducing
  tool-action narrowing.
- **Goal anchoring without a task list.** Most non-orchestration sessions have no TodoWrite. Is the
  first user message enough, or should it be weighted over early assistant prose?
- **Local-model choice.** Re-run the eval when the model lineup moves. On five long transcripts,
  every candidate given a fair window produced usable output — qwen3.5-4b, qwen3.5-9b,
  qwen2.5-14b-instruct and qwen3-coder-30b all 5/5 — differing in latency far more than quality,
  so **model choice is a latency-and-memory decision**, and the sub-10B tier is ample. The axis
  that separates them is format adherence: the 4B drifts to `subject: done:` and pads `done:`;
  9B and 14B hold the shape. qwen3.5-9b is the current pick (stands 6.6–9.6s). gemma-4-e4b was
  faster but overran `STAND_WORDS` on 3/5 and filled slots with narration. minicpm5-2b returned
  empty on 10/10: it ignores `reasoning_effort: "none"` and reasons anyway, so **that switch is not
  universal** — a model that ignores it needs a prompt change before it can be a candidate.
