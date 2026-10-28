//! A Rex terminal has one size, its owner's. warden-app claims a pane's block when the user turns
//! to that pane — focus landing on it in the key window, its window becoming key, a key or mouse
//! press in it (`SurfaceSignal::UserInput`) — and resends the size when its frame changes while
//! warden owns the block. Both go to [`Sizer`]'s one worker thread, so AppKit never waits on the
//! network; the hub's own checks make an unneeded claim cost no request.

use std::collections::HashMap;
use std::sync::mpsc;

use tauri::{AppHandle, Manager};

use super::RexHub;
use crate::surface::ghostty::GhosttySurface;

/// A Rex pane's terminal size: its grid, and a cell's device-pixel size when the surface knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSize {
    pub pane_key: String,
    pub cols: u16,
    pub rows: u16,
    pub cell_px: Option<(u16, u16)>,
}

impl PaneSize {
    /// `surface`'s size as pane `pane_key`'s; `None` while it lays out no grid (never framed).
    pub fn of(pane_key: String, surface: &GhosttySurface) -> Option<PaneSize> {
        let (cols, rows) = surface.grid();
        (cols > 0 && rows > 0).then(|| PaneSize {
            pane_key,
            cols,
            rows,
            cell_px: surface.cell_px(),
        })
    }
}

/// What to do with a [`PaneSize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeAsk {
    /// [`RexHub::claim`]: become the owner, at this size.
    Claim,
    /// [`RexHub::resize_if_owner`]: this size, only while warden owns the block.
    IfOwner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeJob {
    pub ask: SizeAsk,
    pub size: PaneSize,
}

/// The size worker's queue; managed app state.
pub struct Sizer {
    tx: mpsc::Sender<SizeJob>,
}

impl Sizer {
    /// Start the worker thread over `hub`. If the thread can't start, sizing is off (logged):
    /// panes keep working at whatever size their block has.
    pub fn start(hub: RexHub) -> Sizer {
        let (tx, rx) = mpsc::channel::<SizeJob>();
        let spawned = std::thread::Builder::new()
            .name("warden-rex-size".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let jobs = coalesce(std::iter::once(first).chain(rx.try_iter()).collect());
                    for job in jobs {
                        run(&hub, &job);
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("warden: Rex panes won't follow warden's size: {e}");
        }
        Sizer { tx }
    }

    pub fn send(&self, job: SizeJob) {
        let _ = self.tx.send(job);
    }
}

/// Queue `ask` for `size` on the app's [`Sizer`]. Any thread; never blocks.
pub fn request(app: &AppHandle, ask: SizeAsk, size: PaneSize) {
    if let Some(sizer) = app.try_state::<Sizer>() {
        sizer.send(SizeJob { ask, size });
    }
}

fn run(hub: &RexHub, job: &SizeJob) {
    let s = &job.size;
    let r = match job.ask {
        SizeAsk::Claim => hub.claim(&s.pane_key, s.cols, s.rows, s.cell_px),
        SizeAsk::IfOwner => hub.resize_if_owner(&s.pane_key, s.cols, s.rows, s.cell_px),
    };
    if let Err(e) = r {
        eprintln!("warden: {}: couldn't size Rex terminal: {e}", s.pane_key);
    }
}

/// One job per pane, in the order panes first appear: the latest size, as a claim if any job
/// for that pane was one. A burst of keystrokes or a live window resize queued behind a slow
/// request becomes one request.
fn coalesce(jobs: Vec<SizeJob>) -> Vec<SizeJob> {
    let mut out: Vec<SizeJob> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for job in jobs {
        match at.get(&job.size.pane_key) {
            Some(&i) => {
                let claim = out[i].ask == SizeAsk::Claim || job.ask == SizeAsk::Claim;
                out[i] = SizeJob {
                    ask: if claim {
                        SizeAsk::Claim
                    } else {
                        SizeAsk::IfOwner
                    },
                    size: job.size,
                };
            }
            None => {
                at.insert(job.size.pane_key.clone(), out.len());
                out.push(job);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(ask: SizeAsk, pane: &str, cols: u16) -> SizeJob {
        SizeJob {
            ask,
            size: PaneSize {
                pane_key: pane.into(),
                cols,
                rows: 24,
                cell_px: None,
            },
        }
    }

    #[test]
    fn coalesce_keeps_one_job_per_pane_with_the_latest_size() {
        use SizeAsk::*;
        let jobs = vec![
            job(IfOwner, "a", 80),
            job(Claim, "b", 100),
            job(IfOwner, "a", 90),
            job(Claim, "a", 95),
            job(IfOwner, "a", 99),
            job(IfOwner, "b", 101),
            job(IfOwner, "c", 70),
        ];
        assert_eq!(
            coalesce(jobs),
            vec![
                job(Claim, "a", 99),
                job(Claim, "b", 101),
                job(IfOwner, "c", 70)
            ]
        );
    }
}
