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
//! (nothing is at its path any more). `analyze-playback` decodes an AV2 file
//! from `STUDIO_PLAYBACK_FIXTURE` with the real preview engine. `analyze-sample`
//! shows a decoded Y plane at 4× and an exact sample value.
//! `analyze-export-setup` shows the export dialog for a real decoded frame.
//! `analyze-layers` shows the layer inspector on an AVM content-interpretation fixture.
//! `analyze-blocks-8bit` and `analyze-blocks-10bit` inspect the first coded frame.
//! `analyze-overlay`, `analyze-transforms` and `analyze-quantizer` show a selected
//! region on the decoded picture from `STUDIO_PLAYBACK_FIXTURE`.
//! `analyze-entropy` reads the selected coding block with the same fixture.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use studio_app::analysis::{BlockOverlayInput, EntropyInput, Input, Origin, Phase, Restore, Saved};
use studio_app::engine::{Delivery, ENGINE_NAME};
use studio_app::preview::{self, StageState};
use studio_app::recovery::{Recovered, RestoredView};
use studio_app::session::{Route, Session};
use studio_app::state::{AppState, INITIAL_SETTINGS, Intent, Startup};
use studio_app::workspace::Workspace;
use studio_domain::analysis::{KindFilter, RowFilter};
use studio_domain::job::SourceFingerprint;
use studio_domain::preview::{PictureBox, PictureSurface, PictureView};

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

fn overlay_mode(name: &str) -> Option<i32> {
    match name {
        "analyze-overlay" | "analyze-entropy" => Some(1),
        "analyze-transforms" => Some(2),
        "analyze-quantizer" => Some(3),
        _ => None,
    }
}

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
        self.wait_until(|session| !session.analysis().waiting())
    }

    fn wait_until(&mut self, ready: impl Fn(&Session) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready(&self.session) {
            let left = deadline.saturating_duration_since(Instant::now());
            let (route, delivery) = self
                .rx
                .recv_timeout(left)
                .map_err(|e| format!("The engine did not answer ({e})."))?;
            self.session.deliver(route, delivery, Instant::now());
        }
        Ok(())
    }

    pub fn render_picture(
        &mut self,
        window: &studio_app::AppWindow,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mode = overlay_mode(name);
        if mode.is_some()
            || matches!(
                name,
                "analyze-playback" | "analyze-sample" | "analyze-export-setup"
            )
        {
            window.show()?;
            let stage = PictureBox::clamped(
                u32::try_from(window.get_stage_px_width()).unwrap_or(1),
                u32::try_from(window.get_stage_px_height()).unwrap_or(1),
            );
            self.playback(stage, name == "analyze-sample")?;
            if let Some(mode) = mode {
                self.session
                    .block_overlay_input(BlockOverlayInput::Mode(mode));
                self.settle()?;
                self.session.block_overlay_input(BlockOverlayInput::Pick {
                    x: 33,
                    y: 33,
                    width: 100,
                    height: 100,
                });
                self.settle()?;
                studio_app::render_analysis(window, self.session.analysis());
                let overlay = window.global::<studio_app::BlockOverlay>();
                if overlay.get_row_count() == 0 {
                    return Err(
                        "The decoder did not return overlay records for this fixture.".into(),
                    );
                }
            }
            if name == "analyze-entropy" {
                self.input(Input::EvidenceView(7));
                self.input(Input::Entropy(EntropyInput::Read));
                self.settle()?;
                studio_app::render_analysis(window, self.session.analysis());
                let entropy = window.global::<studio_app::EntropyView>();
                if entropy.get_count() == 0 {
                    return Err(
                        format!("No captured entropy symbols. {}", entropy.get_status()).into(),
                    );
                }
            }
            if name == "analyze-export-setup" {
                studio_app::export_setup::Draft::new(
                    studio_app::ExportPurpose::Frame,
                    self.session.export_sources(),
                    self.session
                        .analysis()
                        .shown()
                        .map(studio_app::analysis::Document::id),
                    self.session.export_frame_source().as_ref(),
                )?
                .render(window);
            }
            studio_app::render_preview(
                window,
                self.session.state(),
                self.session.preview(),
                preview::Dirty {
                    picture: true,
                    view: true,
                    thumbs: false,
                },
            );
        }
        Ok(())
    }

    fn playback(&mut self, stage: PictureBox, inspect_sample: bool) -> Result<(), String> {
        let path = std::env::var_os("STUDIO_PLAYBACK_FIXTURE")
            .map(PathBuf::from)
            .ok_or("Set STUDIO_PLAYBACK_FIXTURE to an AV2 file.")?;
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        self.input(Input::Open(vec![path]));
        self.settle()?;
        if self
            .session
            .analysis()
            .shown()
            .and_then(studio_app::analysis::Document::summary)
            .is_none_or(|summary| summary.digest.is_none())
        {
            return Err("The inspection engine could not verify the file for playback.".into());
        }
        self.session.preview_input(preview::Input::Resized {
            stage,
            thumb: PictureBox::clamped(64, 36),
        });
        self.input(Input::TogglePlayback);
        if inspect_sample {
            self.session
                .preview_input(preview::Input::Surface(PictureSurface::Y));
            self.session.preview_input(preview::Input::Zoom(4));
        }
        self.wait_until(|session| {
            matches!(
                session.preview().view().state,
                StageState::Picture | StageState::Unavailable
            )
        })?;
        let view = self.session.preview().view();
        if view.state != StageState::Picture {
            return Err(view.message);
        }
        if inspect_sample {
            return self.sample();
        }
        Ok(())
    }
    fn sample(&mut self) -> Result<(), String> {
        let picture = self
            .session
            .preview()
            .view()
            .picture
            .ok_or("The worker did not return a picture.")?;
        let PictureView::Zoom { x, y, .. } = picture.header.view else {
            return Err("The worker did not return the zoomed region.".into());
        };
        self.session.preview_input(preview::Input::ReadSample {
            x: Some(x + u32::from(picture.header.width) / 2),
            y: Some(y + u32::from(picture.header.height) / 2),
        });
        self.wait_until(|session| {
            let view = session.preview().view();
            !view.viewer.sample_text.is_empty() || view.state == StageState::Unavailable
        })?;
        let view = self.session.preview().view();
        if view.viewer.sample_copy.is_empty() {
            return Err(if view.viewer.sample_text.is_empty() {
                view.message
            } else {
                view.viewer.sample_text
            });
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
            exports: studio_app::export_jobs::ExportJobs::default(),
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
        "analyze-empty" | "analyze-playback" | "analyze-sample" | "analyze-export-setup" => {}
        name if overlay_mode(name).is_some() => {}
        "analyze-layers" => layer_evidence(&mut scenario)?,
        "analyze-blocks-8bit" | "analyze-blocks-10bit" => block_evidence(&mut scenario, name)?,
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

fn block_evidence(scenario: &mut Scenario, name: &str) -> Result<(), String> {
    let (bytes, offset): (&[u8], u64) = if name == "analyze-blocks-8bit" {
        (
            include_bytes!(
                "../../../studio-engine/tests/preview/fixtures/syn-luma-palette2-row-mono-16x16-q0.ivf"
            ),
            59,
        )
    } else {
        (
            include_bytes!(
                "../../../studio-engine/tests/preview/fixtures/syn-mrl-padded-444-67x67-10bit-q120.ivf"
            ),
            62,
        )
    };
    let path = scenario.file(&format!("{name}.ivf"), bytes)?;
    scenario.input(Input::Open(vec![path]));
    scenario.settle()?;
    scenario.input(Input::GoTo(offset.to_string()));
    scenario.settle()?;
    scenario.input(Input::EvidenceView(5));
    scenario.input(Input::InspectBlocks);
    scenario.settle()
}

fn layer_evidence(scenario: &mut Scenario) -> Result<(), String> {
    let bytes = include_bytes!(
        "../../../studio-engine/tests/inspect/fixtures/syn-output-ci-2frame-64x64.ivf"
    );
    let path = scenario.file("content interpretation.ivf", bytes)?;
    scenario.input(Input::Open(vec![path]));
    scenario.settle()?;
    scenario.input(Input::GoTo("83".into()));
    scenario.input(Input::EvidenceView(3));
    scenario.settle()
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
