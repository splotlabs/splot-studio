// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use super::*;
use studio_domain::export::{ExportFormat, ExportRange, ExportRequest, ExportSampleFormat};
use studio_domain::job::{ChromaLayout, WirePath};
use studio_domain::preview::GrainPolicy;

pub(super) fn shared_worker_and_restore(rig: &mut Rig) -> Result<(), Box<dyn std::error::Error>> {
    use studio_app::jobs::Work;

    let scratch = Scratch::new("export-jobs");
    let input = scratch.media("export-source.y4m");
    let pending = scratch.media("waiting-encode.y4m");
    write_y4m(&input, 32, 32, 3);
    write_y4m(&pending, 64, 48, 4);
    let source_path = open_export_fixture(rig, &scratch, input);
    let source = rig
        .session()
        .export_sources()
        .pop()
        .ok_or("no complete source digest")?;
    let request = |path: &Path| ExportRequest {
        source: source.source.clone(),
        range: ExportRange::Inclusive { first: 1, last: 2 },
        layer: None,
        grain: GrainPolicy::Skip,
        frame_rate: None,
        expected: ExportSampleFormat {
            bit_depth: 8,
            chroma: ChromaLayout::Yuv420,
        },
        format: ExportFormat::Hashes {
            path: WirePath::from_path(path),
        },
    };
    rig.dispatch(Intent::FilesOpened(vec![pending]));
    rig.settle("the second encode was probed", |s| {
        s.state().startable_count() == 1
    });
    rig.dispatch(Intent::ConvertRequested);
    let first = scratch.media("first.hashes.json");
    rig.session()
        .export_start(vec![request(&first)], Instant::now());
    assert!(rig.session().export_notice().is_none());
    let last = |s: &Session| {
        s.jobs()
            .into_iter()
            .rfind(|j| matches!(j.work, Work::Export(_)))
            .map(|j| j.state)
    };
    assert_eq!(last(rig.session()), Some(State::Queued));
    rig.settle("Encode released the worker and the export finished", |s| {
        !s.export_active()
    });
    assert_eq!(last(rig.session()), Some(State::Completed));
    let original = fs::read(&first)?;
    assert_ne!(original, [] as [u8; 0]);

    let retry = scratch.media("retry-source.y4m");
    write_y4m(&retry, 32, 32, 2);
    rig.dispatch(Intent::FilesOpened(vec![retry]));
    rig.settle("the third encode was probed", |s| {
        s.state().startable_count() == 1
    });
    let second = scratch.media("second.hashes.json");
    rig.session()
        .export_start(vec![request(&second)], Instant::now());
    rig.dispatch(Intent::ConvertRequested);
    assert_eq!(rig.session().state().items()[2].state(), ItemState::Waiting);
    assert_eq!(last(rig.session()), Some(State::Running));
    rig.settle("Export released the worker and Encode finished", |s| {
        !s.export_active() && s.state().items()[2].state() == ItemState::Completed
    });
    assert_eq!(last(rig.session()), Some(State::Completed));
    assert_eq!(fs::read(&second)?, original);

    rig.session()
        .export_start(vec![request(&first)], Instant::now());
    rig.settle("the existing output was refused", |s| !s.export_active());
    assert_eq!(last(rig.session()), Some(State::Failed));
    assert_eq!(fs::read(&first)?, original);

    #[cfg(unix)]
    {
        let destination = scratch.media("interrupted.hashes.json");
        kill_before_proceed(rig, request(&destination), &destination, &source_path)?;
    }

    rig.start(&scratch);
    let states: Vec<_> = rig
        .session()
        .jobs()
        .into_iter()
        .filter(|job| matches!(job.work, Work::Export(_)))
        .map(|job| job.state)
        .collect();
    assert_eq!(
        states.get(..3),
        Some([State::Completed, State::Completed, State::Failed].as_slice())
    );
    #[cfg(unix)]
    assert_eq!(states.get(3), Some(&State::Interrupted));
    assert_eq!(fs::read(&first)?, original);
    rig.stop();
    Ok(())
}

fn open_export_fixture(rig: &mut Rig, scratch: &Scratch, input: PathBuf) -> PathBuf {
    rig.start(scratch);
    rig.dispatch(Intent::FilesOpened(vec![input]));
    rig.settle("the export fixture was probed", |s| {
        s.state().startable_count() == 1
    });
    rig.dispatch(Intent::ConvertRequested);
    rig.settle("the export fixture was encoded", |s| {
        s.state().items()[0].state() == ItemState::Completed
    });
    let source_path = rig.session().state().items()[0].destination().path.clone();
    rig.dispatch(Intent::WorkspaceRequested(Workspace::Analyze));
    rig.input(Input::Open(vec![source_path.clone()]));
    rig.settle("the export source was read", |s| !reading(s));
    source_path
}

#[cfg(unix)]
fn kill_before_proceed(
    rig: &mut Rig,
    request: ExportRequest,
    destination: &Path,
    source_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    rig.session().export_start(vec![request], Instant::now());
    let (temporary, can_remove) = loop {
        let (route, delivery) = rig.rx.recv_timeout(ANSWER_TIMEOUT)?;
        let partial = match &delivery {
            Delivery::Event(event) => match &event.body {
                studio_domain::protocol::Event::ExportPrepared { files } => {
                    let file = files.first().ok_or("no temporary file")?;
                    let known = file.identity.as_ref().and_then(|id| id.created).is_some()
                        && file
                            .parent_identity
                            .as_ref()
                            .and_then(|id| id.created)
                            .is_some();
                    Some((file.partial.to_path_buf()?, known))
                }
                _ => None,
            },
            _ => None,
        };
        rig.session().deliver(route, delivery, Instant::now());
        if let Some(partial) = partial {
            break partial;
        }
    };
    // The worker is waiting for the journal reply. The test has not
    // delivered that reply, so the destination cannot have changed.
    assert!(temporary.is_file());
    let children = engine_children("worker").ok_or("could not list workers")?;
    let [pid] = children.as_slice() else {
        return Err("expected one export worker".into());
    };
    assert!(
        std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()?
            .success()
    );
    rig.settle("the export worker exited", |s| !s.export_active());
    assert_eq!(
        rig.session().jobs().last().map(|job| job.state),
        Some(State::Interrupted)
    );
    assert_eq!(temporary.exists(), !can_remove);
    assert!(!destination.exists());
    assert!(fs::read(source_path)?.starts_with(b"DKIF"));
    Ok(())
}
