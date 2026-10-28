//! Agent summaries: each Claude Code agent tab on the local server gets a done/now/next summary
//! of its conversation, refreshed as it works and shown in the strip ([`crate::strip`]).
//!
//! The pipeline is `warden_summary`'s; this is its driver. [`SummaryDriver`] lives on the main
//! thread: it decides when a record refreshes ([`SummaryDriver::consider`]) and owns each
//! record's [`SummaryState`]. One worker thread ([`spawn_worker`]) runs the refreshes one at a
//! time, finding the transcript, reading it and calling the model, so the main thread never waits
//! on the disk or the model. Each result lands back on the main thread
//! ([`SummaryDriver::accept`]) and is kept only while the record is still on the conversation it
//! was derived from. A failure keeps the last good rows; nothing error-shaped reaches the strip.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use warden_config::SummaryConfig;
use warden_summary::{apply, due, refresh, Model, RefreshResult, StandRows, SummaryState};
use warden_summary::{Transcript, THROTTLE};

use super::agents::{self, AgentRecord, RecordState};
use super::windows::AgentState;

/// How often a working agent is reconsidered between its status changes.
pub const TICK: Duration = Duration::from_secs(5);

/// Builds the model a `[summary]` config names.
pub type MakeModel = Box<dyn Fn(&SummaryConfig) -> Arc<dyn Model> + Send>;

/// One refresh for the worker: record `record_id`'s conversation `conversation` (its transcript's
/// file name), from `state`, with `model`.
pub struct Job {
    pub record_id: String,
    pub conversation: String,
    pub state: SummaryState,
    pub model: Arc<dyn Model>,
}

/// A record's summary as the driver tracks it.
#[derive(Default)]
struct Entry {
    /// The resume command `state` belongs to; a different one is a new conversation.
    resume: Option<String>,
    state: SummaryState,
    /// The status last considered with, to see `Done` arrive.
    status: Option<AgentState>,
    /// `Done` arrived and its refresh hasn't been queued yet.
    done_owed: bool,
    /// A refresh is out with the worker. Every job is answered, so this always clears.
    in_flight: bool,
}

/// What [`SummaryDriver::accept`] hands back for the record: its new rows and subject.
pub type Summary = (Option<StandRows>, Option<String>);

/// Decides when each agent record refreshes and keeps its summary state. Main thread only.
pub struct SummaryDriver {
    entries: HashMap<String, Entry>,
    /// The configured model; `None` while summaries are off.
    model: Option<Arc<dyn Model>>,
    make_model: MakeModel,
    jobs: mpsc::Sender<Job>,
}

impl SummaryDriver {
    /// A driver queueing onto `jobs` (a [`spawn_worker`]'s), off until [`Self::configure`]d.
    pub fn new(make_model: MakeModel, jobs: mpsc::Sender<Job>) -> Self {
        SummaryDriver {
            entries: HashMap::new(),
            model: None,
            make_model,
            jobs,
        }
    }

    /// Use `[summary]` as `config` says for every later refresh: a model built from it, or none
    /// (summaries off). Rows already shown stay; a refresh already queued keeps its model.
    pub fn configure(&mut self, config: Option<&SummaryConfig>) {
        self.model = config.map(|c| (self.make_model)(c));
    }

    /// Whether summaries are on.
    pub fn is_on(&self) -> bool {
        self.model.is_some()
    }

    /// Record `record`'s block reported `status` (or a tick came round), at `now`. Queues a
    /// refresh when summaries are on and the record is live, on the local server (`local`), on a
    /// Claude Code conversation, has none out already, and is due: the throttle has passed, or it
    /// has just entered `Done`, which refreshes once past the throttle. The attempt is stamped
    /// here, before the call, so a failing model is throttled too. True when one was queued.
    pub fn consider(
        &mut self,
        record: &AgentRecord,
        status: Option<AgentState>,
        local: bool,
        now: Instant,
    ) -> bool {
        let Some(model) = &self.model else {
            return false;
        };
        let entry = entry_for(&mut self.entries, record);
        if status == Some(AgentState::Done) && entry.status != Some(AgentState::Done) {
            entry.done_owed = true;
        }
        entry.status = status;
        let conversation = record
            .resume
            .as_deref()
            .filter(|r| agents::is_claude(r))
            .and_then(agents::resume_id);
        let Some(conversation) = conversation else {
            return false;
        };
        if record.state != RecordState::Live || !local || entry.in_flight {
            return false;
        }
        if !entry.done_owed && !due(&entry.state, now, THROTTLE) {
            return false;
        }
        entry.state.last_attempt = Some(now);
        let job = Job {
            record_id: record.id.clone(),
            conversation: conversation.to_owned(),
            state: entry.state.clone(),
            model: model.clone(),
        };
        if self.jobs.send(job).is_err() {
            return false; // no worker: nothing can refresh
        }
        entry.done_owed = false;
        entry.in_flight = true;
        true
    }

    /// Fold the worker's answer for record `record_id` (now `record`, `None` once it's gone) into
    /// its state; a `None` result (no transcript yet) changes nothing. Returns the record's new
    /// summary when the result was for its current conversation.
    pub fn accept(
        &mut self,
        record_id: &str,
        record: Option<&AgentRecord>,
        result: Option<RefreshResult>,
    ) -> Option<Summary> {
        let Some(record) = record else {
            self.entries.remove(record_id);
            return None;
        };
        let entry = entry_for(&mut self.entries, record);
        entry.in_flight = false;
        if !apply(&mut entry.state, result?) {
            return None;
        }
        Some((entry.state.rows.clone(), entry.state.subject.clone()))
    }
}

/// `record`'s entry, made from its persisted summary on first sight. A changed `resume` is a new
/// conversation: the generation moves on, so a refresh of the old one is dropped when it lands,
/// and the state starts over.
fn entry_for<'a>(entries: &'a mut HashMap<String, Entry>, record: &AgentRecord) -> &'a mut Entry {
    let entry = entries.entry(record.id.clone()).or_insert_with(|| Entry {
        resume: record.resume.clone(),
        state: SummaryState {
            subject: record.subject.clone(),
            rows: record.summary.clone(),
            ..SummaryState::default()
        },
        ..Entry::default()
    });
    if entry.resume != record.resume {
        entry.resume = record.resume.clone();
        entry.state = SummaryState {
            generation: entry.state.generation + 1,
            ..SummaryState::default()
        };
        entry.done_owed = false;
    }
    entry
}

/// Start the worker that runs `jobs` one at a time: each finds its transcript (`locate`, cached
/// per conversation once found; a miss is looked for again on the next job), refreshes it with
/// `transcript` and the job's model, and hands the result to `deliver` — `None` when there was no
/// transcript to read.
pub fn spawn_worker<T: Transcript + Send + 'static>(
    jobs: mpsc::Receiver<Job>,
    transcript: T,
    locate: impl Fn(&str) -> Option<PathBuf> + Send + 'static,
    deliver: impl Fn(String, Option<RefreshResult>) + Send + 'static,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("warden-summary".into())
        .spawn(move || {
            let mut paths: HashMap<String, PathBuf> = HashMap::new();
            for job in jobs {
                let path = match paths.get(&job.conversation) {
                    Some(p) => Some(p.clone()),
                    None => locate(&job.conversation).inspect(|p| {
                        paths.insert(job.conversation.clone(), p.clone());
                    }),
                };
                let result = path.map(|p| refresh(&job.state, &transcript, &*job.model, &p));
                deliver(job.record_id, result);
            }
        })
        .map(|_| ())
}

/// The model a `[summary]` config names: its OpenAI-compatible endpoint.
#[cfg(target_os = "macos")]
fn http_model(c: &SummaryConfig) -> Arc<dyn Model> {
    Arc::new(warden_summary::HttpModel {
        url: c.url.clone(),
        model: c.model.clone(),
        timeout: c.timeout,
    })
}

/// Start the summaries for the app's life (setup, after the agent store and the Rex hub are
/// managed): the driver, configured from `config`, and its worker, driven by Rex's status changes
/// and by a [`TICK`] that reconsiders every working agent. Each result lands through [`land`].
#[cfg(target_os = "macos")]
pub fn start(app: &tauri::AppHandle, config: Option<&SummaryConfig>) {
    use tauri::Manager;
    let (tx, rx) = mpsc::channel();
    let mut driver = SummaryDriver::new(Box::new(http_model), tx);
    driver.configure(config);
    app.manage(std::sync::Mutex::new(driver));

    let back = app.clone();
    let worker = spawn_worker(
        rx,
        warden_summary::Claude,
        |id| super::transcript::locate(id, &super::transcript::claude_profiles()),
        move |id, result| {
            let main = back.clone();
            let _ = back.run_on_main_thread(move || land(&main, &id, result));
        },
    );
    if let Err(e) = worker {
        eprintln!("warden: no agent summaries: {e}");
        return;
    }

    if let Some(hub) = app.try_state::<super::RexHub>() {
        let events = hub.subscribe();
        let app = app.clone();
        let followed = std::thread::Builder::new()
            .name("warden-summary-status".into())
            .spawn(move || {
                for ev in events {
                    if let super::HubEvent::StatusChanged { block_id } = ev {
                        let main = app.clone();
                        let _ = app.run_on_main_thread(move || on_status(&main, &block_id));
                    }
                }
            });
        if let Err(e) = followed {
            eprintln!("warden: agent summaries won't follow Rex: {e}");
        }
    }

    let app = app.clone();
    let ticking = std::thread::Builder::new()
        .name("warden-summary-tick".into())
        .spawn(move || loop {
            std::thread::sleep(TICK);
            let main = app.clone();
            if app.run_on_main_thread(move || tick(&main)).is_err() {
                return;
            }
        });
    if let Err(e) = ticking {
        eprintln!("warden: agent summaries won't refresh while agents work: {e}");
    }
}

/// Use `config` (`[summary]` after a reload that changed it) for every later refresh (main thread).
#[cfg(target_os = "macos")]
pub fn configure(app: &tauri::AppHandle, config: Option<&SummaryConfig>) {
    use tauri::Manager;
    if let Some(driver) = app.try_state::<std::sync::Mutex<SummaryDriver>>() {
        super::lock(&driver).configure(config);
    }
}

/// Block `block_id`'s status changed (main thread): consider the agent record showing it.
#[cfg(target_os = "macos")]
fn on_status(app: &tauri::AppHandle, block_id: &str) {
    use tauri::Manager;
    let Some(store) = app.try_state::<std::sync::Mutex<super::agents::AgentStore>>() else {
        return;
    };
    let record = super::lock(&store).by_block(block_id);
    consider_all(app, record.into_iter().collect());
}

/// A [`TICK`] came round (main thread): consider every working agent, so a long turn with no
/// status change still refreshes once the throttle passes.
#[cfg(target_os = "macos")]
fn tick(app: &tauri::AppHandle) {
    use tauri::Manager;
    let (Some(store), Some(hub), Some(driver)) = (
        app.try_state::<std::sync::Mutex<super::agents::AgentStore>>(),
        app.try_state::<super::RexHub>(),
        app.try_state::<std::sync::Mutex<SummaryDriver>>(),
    ) else {
        return;
    };
    if !super::lock(&driver).is_on() {
        return;
    }
    let working: Vec<AgentRecord> = super::lock(&store)
        .live()
        .into_iter()
        .filter(|r| {
            r.block_id
                .as_deref()
                .and_then(|b| hub.block_status(b))
                .is_some_and(|s| s.state == AgentState::Working)
        })
        .collect();
    consider_all(app, working);
}

/// Hand `records` to the driver with their block's status and whether their tab is on the local
/// server, as the current config says (main thread).
#[cfg(target_os = "macos")]
fn consider_all(app: &tauri::AppHandle, records: Vec<AgentRecord>) {
    use tauri::Manager;
    let (Some(hub), Some(driver)) = (
        app.try_state::<super::RexHub>(),
        app.try_state::<std::sync::Mutex<SummaryDriver>>(),
    ) else {
        return;
    };
    if records.is_empty() {
        return;
    }
    let local: std::collections::HashSet<String> = {
        let st = app.state::<crate::ManagerState>();
        let m = st.lock();
        m.last_good
            .windows
            .iter()
            .flat_map(|w| &w.tabs)
            .filter(|t| t.server.as_deref() == Some(super::server::LOCAL))
            .map(|t| t.key.clone())
            .collect()
    };
    let seen: Vec<(AgentRecord, Option<AgentState>)> = records
        .into_iter()
        .map(|r| {
            let status = r.block_id.as_deref().and_then(|b| hub.block_status(b));
            (r, status.map(|s| s.state))
        })
        .collect();
    let now = Instant::now();
    let mut d = super::lock(&driver);
    for (r, status) in seen {
        d.consider(&r, status, local.contains(&r.project), now);
    }
}

/// The worker's answer for record `record_id` (main thread): fold it in ([`SummaryDriver::accept`])
/// and, when it changed the record's summary, store it ([`AgentStore::upsert`]) and re-send the
/// strip if the record is its project's selected one. Then consider the record again, for a
/// `Done` that arrived while this refresh was out.
///
/// [`AgentStore::upsert`]: super::agents::AgentStore::upsert
#[cfg(target_os = "macos")]
fn land(app: &tauri::AppHandle, record_id: &str, result: Option<RefreshResult>) {
    use tauri::Manager;
    let (Some(store), Some(driver)) = (
        app.try_state::<std::sync::Mutex<super::agents::AgentStore>>(),
        app.try_state::<std::sync::Mutex<SummaryDriver>>(),
    ) else {
        return;
    };
    let record = super::lock(&store).record(record_id);
    let summary = super::lock(&driver).accept(record_id, record.as_ref(), result);
    let Some(record) = record else {
        return;
    };
    if let Some((rows, subject)) = summary {
        let shown = {
            let mut s = super::lock(&store);
            match s.record(record_id) {
                // A report may have moved it to a new conversation since it was read.
                Some(mut r)
                    if r.resume == record.resume && (r.summary != rows || r.subject != subject) =>
                {
                    r.summary = rows;
                    r.subject = subject;
                    let shown = r.selected.then(|| r.project.clone());
                    s.upsert(r);
                    shown
                }
                _ => None,
            }
        };
        if let Some(project) = shown {
            let st = app.state::<crate::ManagerState>();
            let m = st.lock();
            crate::strip::emit(app, &m.last_good, &project);
        }
    }
    consider_all(app, vec![record]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::agent_open::tests::record;
    use warden_summary::CtxMode;

    /// Answers every call with its endpoint's url, so a test can see which config built it.
    struct Echo(String);

    impl Model for Echo {
        fn complete(&self, _: &str, _: &str) -> Option<String> {
            Some(self.0.clone())
        }
    }

    fn cfg(url: &str) -> SummaryConfig {
        SummaryConfig {
            url: url.into(),
            model: "m".into(),
            timeout: Duration::from_secs(45),
        }
    }

    fn driver() -> (SummaryDriver, mpsc::Receiver<Job>) {
        let (tx, rx) = mpsc::channel();
        let mut d = SummaryDriver::new(
            Box::new(|c: &SummaryConfig| Arc::new(Echo(c.url.clone())) as Arc<dyn Model>),
            tx,
        );
        d.configure(Some(&cfg("http://a")));
        (d, rx)
    }

    fn agent(resume: &str) -> AgentRecord {
        record("a", RecordState::Live, Some("window:1"), Some(resume))
    }

    fn result_for(job: &Job, now: &str) -> RefreshResult {
        RefreshResult {
            generation: job.state.generation,
            subject: Some("auth flow".into()),
            drift: 0,
            start_line: 0,
            rows: Some(StandRows {
                now: Some(now.into()),
                ..Default::default()
            }),
        }
    }

    /// Consider `r` at `t`; if a job went out, answer it at once, as the worker would.
    fn step(
        d: &mut SummaryDriver,
        rx: &mpsc::Receiver<Job>,
        r: &AgentRecord,
        s: AgentState,
        t: Instant,
    ) -> bool {
        let queued = d.consider(r, Some(s), true, t);
        if let Ok(job) = rx.try_recv() {
            assert!(queued);
            let res = result_for(&job, "x");
            d.accept(&job.record_id, Some(r), Some(res));
        }
        queued
    }

    #[test]
    fn remote_tab_never_queues() {
        let (mut d, rx) = driver();
        let r = agent("claude --resume abc");
        let t0 = Instant::now();
        assert!(!d.consider(&r, Some(AgentState::Working), false, t0));
        assert!(!d.consider(&r, Some(AgentState::Done), false, t0));
        assert!(rx.try_recv().is_err(), "nothing for a remote tab");

        assert!(
            d.consider(&r, Some(AgentState::Working), true, t0),
            "the same tab, local, does"
        );
        let job = rx.try_recv().unwrap();
        assert_eq!(
            (job.record_id.as_str(), job.conversation.as_str()),
            ("a", "abc")
        );
    }

    #[test]
    fn only_live_claude_agents_queue() {
        let (mut d, rx) = driver();
        let t0 = Instant::now();
        let codex = agent("codex resume abc");
        assert!(!d.consider(&codex, Some(AgentState::Working), true, t0));
        let mut asleep = agent("claude --resume abc");
        asleep.state = RecordState::Asleep;
        assert!(!d.consider(&asleep, Some(AgentState::Working), true, t0));
        let mut unreported = agent("claude --resume abc");
        unreported.resume = None;
        assert!(!d.consider(&unreported, Some(AgentState::Working), true, t0));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn working_ticks_respect_throttle() {
        let (mut d, rx) = driver();
        let r = agent("claude --resume abc");
        let t0 = Instant::now();
        assert!(step(&mut d, &rx, &r, AgentState::Working, t0));
        for s in [5, 10, 15, 20, 25, 29] {
            let t = t0 + Duration::from_secs(s);
            assert!(
                !step(&mut d, &rx, &r, AgentState::Working, t),
                "tick at {s}s"
            );
        }
        assert!(step(&mut d, &rx, &r, AgentState::Working, t0 + THROTTLE));
    }

    #[test]
    fn a_failed_refresh_is_throttled_too() {
        let (mut d, rx) = driver();
        let r = agent("claude --resume abc");
        let t0 = Instant::now();
        assert!(d.consider(&r, Some(AgentState::Working), true, t0));
        let job = rx.try_recv().unwrap();
        assert_eq!(
            d.accept(&job.record_id, Some(&r), None),
            None,
            "no transcript yet"
        );
        let t = t0 + Duration::from_secs(5);
        assert!(!d.consider(&r, Some(AgentState::Working), true, t));
        assert!(
            d.consider(&r, Some(AgentState::Working), true, t0 + THROTTLE),
            "retried"
        );
    }

    #[test]
    fn done_queues_once_past_throttle() {
        let (mut d, rx) = driver();
        let r = agent("claude --resume abc");
        let t0 = Instant::now();
        assert!(step(&mut d, &rx, &r, AgentState::Working, t0));
        let t = |s| t0 + Duration::from_secs(s);
        assert!(
            step(&mut d, &rx, &r, AgentState::Done, t(5)),
            "entering done bypasses the throttle"
        );
        assert!(!step(&mut d, &rx, &r, AgentState::Done, t(6)), "once");
        assert!(!step(&mut d, &rx, &r, AgentState::Working, t(7)));

        // Done arriving while a refresh is out is owed, and queued once that one lands.
        assert!(d.consider(&r, Some(AgentState::Working), true, t(40)));
        let job = rx.try_recv().unwrap();
        assert!(
            !d.consider(&r, Some(AgentState::Done), true, t(41)),
            "one at a time"
        );
        assert!(d
            .accept(&job.record_id, Some(&r), Some(result_for(&job, "x")))
            .is_some());
        assert!(
            d.consider(&r, Some(AgentState::Done), true, t(42)),
            "the owed done refresh"
        );
    }

    #[test]
    fn stale_generation_result_ignored() {
        let (mut d, rx) = driver();
        let mut r = agent("claude --resume abc");
        let t0 = Instant::now();
        assert!(d.consider(&r, Some(AgentState::Working), true, t0));
        let job = rx.try_recv().unwrap();
        let stale = result_for(&job, "from the old conversation");

        r.resume = Some("claude --resume def".into()); // /clear: a new conversation
        assert_eq!(d.accept(&job.record_id, Some(&r), Some(stale)), None);

        assert!(
            d.consider(
                &r,
                Some(AgentState::Working),
                true,
                t0 + Duration::from_secs(1)
            ),
            "the new conversation starts unthrottled"
        );
        let job = rx.try_recv().unwrap();
        assert_eq!(job.conversation, "def");
        assert_eq!((&job.state.rows, &job.state.subject), (&None, &None));
        let fresh = result_for(&job, "new");
        let (rows, subject) = d.accept(&job.record_id, Some(&r), Some(fresh)).unwrap();
        assert_eq!(rows.unwrap().now.as_deref(), Some("new"));
        assert_eq!(subject.as_deref(), Some("auth flow"));
    }

    #[test]
    fn a_persisted_summary_seeds_the_state() {
        let (mut d, rx) = driver();
        let mut r = agent("claude --resume abc");
        r.subject = Some("kept subject".into());
        r.summary = Some(StandRows::default());
        assert!(d.consider(&r, Some(AgentState::Working), true, Instant::now()));
        let job = rx.try_recv().unwrap();
        assert_eq!(job.state.subject.as_deref(), Some("kept subject"));
        assert_eq!(job.state.rows, Some(StandRows::default()));
    }

    #[test]
    fn summary_config_change_hot_reloads() {
        // A `[summary]` edit is global: it reconciles to no window op, so no terminal respawns...
        let parse = |s: &str| {
            warden_config::resolve::resolve(warden_config::raw::parse(s).unwrap())
                .unwrap()
                .0
        };
        let win =
            "[[window]]\ntitle = \"w\"\n  [[window.tab]]\n  dir = \"/a\"\n  server = \"local\"\n";
        let old = parse(&format!("[summary]\nurl = \"http://a\"\n{win}"));
        let new = parse(&format!(
            "[summary]\nurl = \"http://b\"\ntimeout = 5\n{win}"
        ));
        assert_ne!(old.summary, new.summary);
        let recon = warden_config::reconcile(&old, &new);
        assert!(recon.open.is_empty() && recon.close.is_empty() && recon.update.is_empty());

        // ...and the reload hook's compare hands it to the driver, whose next refresh uses it.
        let (mut d, rx) = driver();
        let r = agent("claude --resume abc");
        let t0 = Instant::now();
        assert!(d.consider(&r, Some(AgentState::Working), true, t0));
        let job = rx.try_recv().unwrap();
        assert_eq!(job.model.complete("", "").as_deref(), Some("http://a"));
        d.accept(&job.record_id, Some(&r), None);

        d.configure(new.summary.as_ref());
        assert!(d.consider(&r, Some(AgentState::Working), true, t0 + THROTTLE));
        let job = rx.try_recv().unwrap();
        assert_eq!(job.model.complete("", "").as_deref(), Some("http://b"));
        d.accept(&job.record_id, Some(&r), None);

        d.configure(None);
        assert!(
            !d.consider(&r, Some(AgentState::Done), true, t0 + THROTTLE * 2),
            "off"
        );
    }

    /// A transcript whose every read is the same few turns.
    struct Fixed;

    impl Transcript for Fixed {
        fn context(&self, _: &std::path::Path, _: usize, _: usize, _: CtxMode) -> String {
            "edit the strip".into()
        }
        fn digest(&self, _: &std::path::Path, _: usize, _: usize) -> String {
            "edited strip.rs".into()
        }
        fn line_count(&self, _: &std::path::Path) -> usize {
            1
        }
    }

    #[test]
    fn worker_refreshes_and_reports_a_missing_transcript() {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let found = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = found.clone();
        spawn_worker(
            jobs_rx,
            Fixed,
            move |id| {
                f.load(std::sync::atomic::Ordering::SeqCst)
                    .then(|| PathBuf::from(format!("/t/{id}.jsonl")))
            },
            move |id, r| {
                let _ = out_tx.send((id, r));
            },
        )
        .unwrap();
        let job = |id: &str| Job {
            record_id: id.into(),
            conversation: "abc".into(),
            state: SummaryState::default(),
            model: Arc::new(Echo("subject. now: editing".into())),
        };
        jobs_tx.send(job("a")).unwrap();
        let (id, r) = out_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            (id.as_str(), r),
            ("a", None),
            "not found yet: nothing to read"
        );

        found.store(true, std::sync::atomic::Ordering::SeqCst);
        jobs_tx.send(job("a")).unwrap();
        let (_, r) = out_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let rows = r
            .expect("found on the next job")
            .rows
            .expect("the model answered");
        assert_eq!(rows.now.as_deref(), Some("editing"));
    }
}
