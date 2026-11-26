// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Analyze scenarios: the app's real session and the real inspect engine
//! (`splot-studio-engine`, built in the same profile beside `examples/`)
//! on the hand-built fixtures of the engine tests, written to a temporary
//! folder.
//!
//! `analyze-empty`, `analyze-opening`, `analyze-ivf` (the extended tile
//! group selected), `analyze-filtered` (temporal delimiters only: the
//! selection is hidden), `analyze-damaged` (a truncated Annex B stream, its
//! unparsed region selected), `analyze-failed` (not an AV2 stream),
//! `analyze-jobs` (two files read, one reading and one queued behind it,
//! the first shown, for the Jobs list), and two documents restored from a last session:
//! `analyze-changed` (the file was replaced since) and `analyze-missing`
//! (nothing is at its path any more).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use studio_app::analysis::{Input, Origin, Phase, Restore, Saved};
use studio_app::engine::{Delivery, ENGINE_NAME};
use studio_app::recovery::{Recovered, RestoredView};
use studio_app::session::{Route, Session};
use studio_app::state::{AppState, INITIAL_SETTINGS, Intent, Startup};
use studio_app::workspace::Workspace;
use studio_domain::analysis::{KindFilter, RowFilter};
use studio_domain::job::SourceFingerprint;

/// The engine tests' `ivf()`: three records (TD and sequence header, an
/// empty payload, TD and a tile group with the extension), 82 bytes.
fn ivf() -> Vec<u8> {
    let mut bytes = b"DKIF".to_vec();
    bytes.extend(0_u16.to_le_bytes());
    bytes.extend(32_u16.to_le_bytes());
    bytes.extend(b"AV02");
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(30_u32.to_le_bytes());
    bytes.extend(1_u32.to_le_bytes());
    bytes.extend(3_u32.to_le_bytes());
    bytes.extend(0_u32.to_le_bytes());
    for (pts, payload) in [
        (0_u64, &[0x01, 0x08, 0x04, 0x04, 0xAA, 0xBB, 0xCC][..]),
        (1, &[]),
        (2, &[0x01, 0x08, 0x04, 0x9D, 0x45, 0x11, 0x22]),
    ] {
        bytes.extend(u32::try_from(payload.len()).unwrap_or(0).to_le_bytes());
        bytes.extend(pts.to_le_bytes());
        bytes.extend(payload);
    }
    bytes
}

/// TD, sequence header, then a padding OBU that declares 131 bytes and has
/// two. 11 bytes.
const TRUNCATED: [u8; 11] = [
    0x01, 0x08, 0x04, 0x04, 0xAA, 0xBB, 0xCC, 0x83, 0x01, 0x64, 0x5A,
];

/// A session over the real inspect engine, and the folder of its files.
pub struct Scenario {
    pub session: Session,
    rx: Receiver<(Route, Delivery)>,
    folder: PathBuf,
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

impl Scenario {
    fn file(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = self.folder.join(name);
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(path)
    }

    fn input(&mut self, input: Input) {
        self.session.analysis_input(input);
    }

    /// Delivers the engine's answers until nothing is asked for.
    fn settle(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.session.analysis().waiting() {
            let left = deadline.saturating_duration_since(Instant::now());
            let (route, delivery) = self
                .rx
                .recv_timeout(left)
                .map_err(|e| format!("the inspect engine did not answer: {e}"))?;
            self.session.deliver(route, delivery, Instant::now());
        }
        Ok(())
    }
}

/// A state restored from a last session that had `path` open in Analyze,
/// as a file of 4096 bytes with its seventh element selected.
fn restored(path: PathBuf) -> AppState {
    let saved = Saved {
        path,
        origin: Origin::File,
        restore: Some(Restore {
            fingerprint: SourceFingerprint {
                len: 4096,
                modified: Some((1_795_000_000, 0)),
            },
            selected: Some(7),
            filter: RowFilter::default(),
            bytes_shown: true,
        }),
    };
    AppState::new(Startup {
        recovered: Some(Recovered {
            items: Vec::new(),
            next_id: 1,
            settings: INITIAL_SETTINGS.into(),
            cleanups: Vec::new(),
            dropped: 0,
            view: RestoredView {
                workspace: Workspace::Analyze,
                documents: vec![saved],
                shown: 0,
                dropped: 0,
                unreadable: None,
            },
        }),
        problem: None,
    })
}

/// The Analyze state of `name`, `Ok(None)` when it is no Analyze scenario.
pub fn scenario(name: &str) -> Result<Option<Scenario>, String> {
    if !name.starts_with("analyze-") {
        return Ok(None);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let engine = exe
        .parent()
        .and_then(|examples| examples.parent())
        .map(|profile| profile.join(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX)))
        .filter(|path| path.is_file())
        .ok_or("build the engine first: cargo build -p studio-engine")?;
    let folder = std::env::temp_dir().join(format!("studio-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    let post = Arc::new(move |route, delivery| {
        let _ = tx.send((route, delivery));
    });
    // The file at each path was replaced or moved away since.
    let state = match name {
        "analyze-changed" => {
            std::fs::write(folder.join("park walk.ivf"), ivf()).map_err(|e| e.to_string())?;
            restored(folder.join("park walk.ivf"))
        }
        "analyze-missing" => restored(PathBuf::from("/home/user/Videos/drone over the jetty.ivf")),
        _ => AppState::default(),
    };
    let mut scenario = Scenario {
        session: Session::new(state, Ok(engine), post, None),
        rx,
        folder,
    };
    scenario.session.dispatch(
        Intent::WorkspaceRequested(Workspace::Analyze),
        Instant::now(),
    );
    match name {
        "analyze-empty" => {}
        "analyze-opening" => {
            // Asked for, and never answered here.
            let path = scenario.file("park walk.ivf", &ivf())?;
            scenario.input(Input::Open(vec![path]));
        }
        "analyze-ivf" | "analyze-filtered" => {
            let path = scenario.file("park walk.ivf", &ivf())?;
            scenario.input(Input::Open(vec![path]));
            scenario.settle()?;
            scenario.input(Input::RowClicked(7));
            scenario.settle()?;
            if name == "analyze-filtered" {
                let td = scenario
                    .session
                    .analysis()
                    .type_choices()
                    .iter()
                    .position(|k| *k == KindFilter::Obu { obu_type: 2 })
                    .and_then(|i| i32::try_from(i).ok())
                    .ok_or("no temporal delimiters")?;
                scenario.input(Input::TypeFilter(td));
                scenario.settle()?;
            }
        }
        "analyze-syntax" | "analyze-findings" => {
            linked_evidence(&mut scenario, name == "analyze-syntax")?;
        }
        "analyze-damaged" => {
            let path = scenario.file("cut short.obu", &TRUNCATED)?;
            scenario.input(Input::Open(vec![path]));
            scenario.settle()?;
            scenario.input(Input::RowClicked(2));
            scenario.settle()?;
        }
        "analyze-jobs" => {
            for (name, bytes) in [("park walk.ivf", &ivf()[..]), ("cut short.obu", &TRUNCATED)] {
                let path = scenario.file(name, bytes)?;
                scenario.input(Input::Open(vec![path]));
                scenario.settle()?;
            }
            // Asked for, and never answered here: reading, then queued.
            for name in ["drone over the jetty.ivf", "harbour at dusk.obu"] {
                let path = scenario.file(name, &ivf())?;
                scenario.input(Input::Open(vec![path]));
            }
            scenario.input(Input::Show(0));
        }
        "analyze-changed" | "analyze-missing" => scenario.settle()?,
        "analyze-failed" => {
            let path = scenario.file("notes.txt", b"not a stream")?;
            scenario.input(Input::Open(vec![path]));
            scenario.settle()?;
            let failed = scenario
                .session
                .analysis()
                .shown()
                .is_some_and(|d| matches!(d.phase(), Phase::Failed { .. }));
            if !failed {
                return Err("the engine opened a text file".into());
            }
        }
        other => return Err(format!("unknown scenario {other:?}")),
    }
    Ok(Some(scenario))
}

fn linked_evidence(scenario: &mut Scenario, syntax: bool) -> Result<(), String> {
    let path = scenario.file("bad trailing bit.obu", &[0x01, 0x08, 0x02, 0x08, 0x00])?;
    scenario.input(Input::Open(vec![path]));
    scenario.settle()?;
    scenario.input(Input::Validate);
    scenario.settle()?;
    scenario.input(Input::EvidenceRule(
        "trailing-bits/missing-one-bit".to_owned(),
    ));
    scenario.input(Input::EvidencePick(0));
    scenario.settle()?;
    if syntax {
        scenario.input(Input::EvidenceView(1));
        scenario.settle()?;
    }
    Ok(())
}
