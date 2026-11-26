// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! First pass, final pass, replay and Clone Settings through the desktop controller.

use std::cell::Cell;
use std::error::Error;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::language::ColorScheme;
use slint::platform::{Key, WindowEvent, update_timers_and_animations};
use slint::{ComponentHandle, LogicalSize, Model, ModelRc, SharedString, VecModel};
use studio_app::journal::{JournalItem, JournalState};
use studio_app::{JobAction, Palette};
use studio_domain::job::{JobPurpose, JobReport, JobSpec};
use studio_domain::options::{OptionRequest, OptionValue};

use super::{Rig, edit, source_fixture, y4m};

fn completed(rig: &Rig, index: usize) -> JournalItem {
    let mut saved = rig.saved("the related job to finish", |journal| {
        journal.items.get(index).is_some_and(|item| {
            matches!(
                item.state,
                JournalState::Completed { .. }
                    | JournalState::Failed { .. }
                    | JournalState::Cancelled { .. }
                    | JournalState::Lost { .. }
            )
        })
    });
    let item = saved.items.swap_remove(index);
    assert!(
        matches!(item.state, JournalState::Completed { .. }),
        "{:?}",
        item.state
    );
    item
}

fn result(item: &JournalItem) -> (&JobSpec, &JobReport) {
    match &item.state {
        JournalState::Completed { job, report } => (job, report),
        state => unreachable!("The job did not complete. {state:?}"),
    }
}

fn snapshot(rig: &Rig, title: &str) -> Result<(), Box<dyn Error>> {
    let Some(folder) = std::env::var_os("SPLOT_STUDIO_TEST_SNAPSHOTS") else {
        return Ok(());
    };
    let (name, scheme, scale) = match title {
        "Generate first pass" => ("first-pass-light-1x.ppm", ColorScheme::Light, 1.0),
        "Encode with first pass" => ("final-pass-dark-2x.ppm", ColorScheme::Dark, 2.0),
        "Re-encode" => ("replay-light-1x.ppm", ColorScheme::Light, 1.0),
        _ => return Ok(()),
    };
    let main = rig.window();
    let window = main.window();
    let palette = main.global::<Palette>();
    let previous = palette.get_color_scheme();
    let logical = window.size().to_logical(window.scale_factor());
    let old_scale = window.scale_factor();
    palette.set_color_scheme(scheme);
    window.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    window.set_size(logical);
    update_timers_and_animations();
    let result = write_snapshot(rig, &PathBuf::from(folder), name);
    window.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: old_scale,
    });
    window.set_size(logical);
    palette.set_color_scheme(previous);
    result
}

fn write_snapshot(rig: &Rig, folder: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(folder)?;
    let size = rig.window().window().size();
    let pixels = rig.pixels();
    let mut image = std::io::BufWriter::new(fs::File::create(folder.join(name))?);
    write!(image, "P6\n{} {}\n255\n", size.width, size.height)?;
    for pixel in pixels {
        image.write_all(&[pixel.r, pixel.g, pixel.b])?;
    }
    image.flush()?;
    Ok(())
}

fn approve(rig: &Rig, title: &str) {
    let window = rig.window();
    rig.until(title, || {
        window.get_job_related_ready() || window.get_job_related_failed()
    });
    assert!(
        window.get_job_related_ready(),
        "{}",
        window.get_job_related_detail()
    );
    assert_eq!(window.get_job_related_title(), title);
    snapshot(rig, title).unwrap_or_else(|error| unreachable!("{error}"));
    window.invoke_job_related_approve();
}

fn choose_job(rig: &Rig, action: JobAction, title: &str) {
    let window = rig.window();
    let row = || {
        window.get_jobs().iter().position(|job| match action {
            JobAction::FinalPass => job.can_encode_with_pass,
            JobAction::Reencode => job.can_reencode,
            JobAction::CloneSettings => job.can_clone_settings && job.can_reencode,
        })
    };
    rig.until(title, || {
        window.get_job_actions_enabled() && row().is_some()
    });
    window.invoke_show_jobs();
    let index = row()
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or_else(|| unreachable!("Cannot find the job action."));
    window.invoke_job_action(index, action);
    approve(rig, title);
}

fn set_vbr(rig: &Rig, bitrate: i64) -> OptionRequest {
    let window = rig.window();
    window.invoke_row_selected(0);
    window.invoke_preset_edited(10);
    window.set_advanced_open(true);
    rig.until("the option catalog", || {
        window.get_advanced_options().row_count() > 0
    });
    window.invoke_advanced_search("rate_control".into());
    window.invoke_advanced_select("rate_control".into());
    rig.until("the current rate control", || {
        !window.get_advanced_effective().is_empty()
    });
    edit(window, "value/@mode", "vbr");
    edit(window, "value/bitrate_kbps", &bitrate.to_string());
    let displayed = format!("bitrate kbps = {bitrate}");
    rig.until("validated VBR", || {
        window.get_advanced_error().is_empty()
            && window.get_advanced_status().is_empty()
            && window.get_advanced_effective().contains(displayed.as_str())
    });
    rig.saved("the VBR draft", |journal| {
        matches!(journal.settings.values.get("rate_control"), Some(OptionValue::Record(values))
            if values.get("bitrate_kbps") == Some(&OptionValue::Integer(bitrate)))
    })
    .settings
}

pub(super) fn workflow(rig: Rig, scratch: &Path) -> Result<(), Box<dyn Error>> {
    let data = scratch.join("pass-workflow");
    fs::create_dir(&data)?;
    let source = source_fixture(&data);
    let rig = rig.reopen(&data, vec![source.clone()]);
    rig.until("the new source to be ready", || {
        rig.window().get_can_encode()
    });
    let requested = set_vbr(&rig, 35);
    rig.until("first pass is available", || {
        rig.window().get_job_can_generate_pass()
    });
    rig.window().invoke_show_jobs();
    rig.window().invoke_job_generate_pass();
    approve(&rig, "Generate first pass");
    let first = completed(&rig, 1);
    let (first_job, artifact) = result(&first);
    assert_eq!(first_job.purpose, JobPurpose::FirstPass);
    assert_eq!(first_job.settings, requested);
    assert!(first_job.pass_artifact.is_none());
    let digest = artifact
        .pass_digest
        .as_deref()
        .ok_or("The artifact has no digest.")?;
    assert_eq!(digest.len(), 64);
    assert_eq!(artifact.frames, 4);
    let artifact_path = artifact.destination.to_path_buf()?;
    let artifact_bytes = fs::read(&artifact_path)?;
    assert_eq!(u64::try_from(artifact_bytes.len())?, artifact.bytes);

    let rig = rig.reopen(&data, Vec::new());
    choose_job(&rig, JobAction::FinalPass, "Encode with first pass");
    let final_pass = completed(&rig, 2);
    let (final_job, encoded) = result(&final_pass);
    assert_eq!(final_job.purpose, JobPurpose::Encode);
    assert_eq!(final_job.settings, requested);
    assert_eq!(final_job.identity, first_job.identity);
    assert_eq!(
        final_job.pass_artifact.as_ref(),
        Some(&artifact.destination)
    );
    assert_eq!(final_job.pass_digest.as_deref(), Some(digest));
    assert_eq!(encoded.pass_digest.as_deref(), Some(digest));
    assert_eq!(encoded.frames, artifact.frames);
    assert_eq!(final_pass.attempt_policy, first.attempt_policy);
    let final_path = encoded.destination.to_path_buf()?;
    let final_bytes = fs::read(&final_path)?;
    assert_eq!(final_bytes.get(8..12), Some(b"AV02".as_slice()));

    let rig = rig.reopen(&data, Vec::new());
    let newer = set_vbr(&rig, 70);
    assert_ne!(newer, requested);
    choose_job(&rig, JobAction::Reencode, "Re-encode");
    let replay = completed(&rig, 3);
    let (replay_job, replayed) = result(&replay);
    let mut expected = final_job.clone();
    expected.destination.clone_from(&replay_job.destination);
    assert_eq!(*replay_job, expected);
    assert_ne!(replayed.destination, encoded.destination);
    assert_eq!(replayed.pass_digest.as_deref(), Some(digest));
    assert_eq!(replay.attempt_policy, final_pass.attempt_policy);
    assert!(fs::read(replayed.destination.to_path_buf()?)? == final_bytes);

    choose_job(&rig, JobAction::CloneSettings, "Clone settings");
    let saved = rig.saved("the cloned draft", |journal| journal.settings == requested);
    assert_eq!(saved.items.len(), 4);
    assert_eq!(saved.items[1], first);
    assert_eq!(saved.items[2], final_pass);
    assert_eq!(saved.items[3], replay);
    assert!(fs::read(artifact_path)? == artifact_bytes);
    assert!(fs::read(final_path)? == final_bytes);
    assert!(fs::read(source)? == y4m());
    long_jobs(&rig)?;
    Ok(())
}

fn long_jobs(rig: &Rig) -> Result<(), Box<dyn Error>> {
    let window = rig.window();
    let native = window.window();
    let press = |key: Key| {
        let text: SharedString = key.into();
        native.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        native.dispatch_event(WindowEvent::KeyReleased { text });
    };
    const ROWS: usize = 16;
    let template = window
        .get_jobs()
        .row_data(0)
        .ok_or("The Jobs list is empty.")?;
    let shown = Rc::new(Cell::new(None));
    window.on_job_show({
        let shown = shown.clone();
        move |index| shown.set(Some(index))
    });
    // Test only the list layout here. Name each copy so the capture shows the last row.
    let rows = (1..=ROWS)
        .map(|number| {
            let mut row = template.clone();
            row.name = format!("Keyboard row {number:02}").into();
            row.can_cancel = false;
            row.can_clone_settings = false;
            row.can_reencode = false;
            row.can_encode_with_pass = false;
            row
        })
        .collect::<Vec<_>>();
    let folder = std::env::var_os("SPLOT_STUDIO_TEST_SNAPSHOTS").map(PathBuf::from);
    window.set_max_window_width(640.0);
    press(Key::Escape);
    for (scheme, scale, label) in [
        (ColorScheme::Light, 1.0, "jobs-keyboard-light-1x.ppm"),
        (ColorScheme::Dark, 2.0, "jobs-keyboard-dark-2x.ppm"),
    ] {
        shown.set(None);
        window.global::<Palette>().set_color_scheme(scheme);
        native.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        native.set_size(LogicalSize::new(640.0, 600.0));
        update_timers_and_animations();
        window.set_jobs(ModelRc::new(VecModel::from(rows.clone())));
        window.invoke_show_jobs();
        for _ in 0..ROWS {
            press(Key::Tab);
        }
        rig.until("the Jobs list to scroll to the focused action", || {
            !native.has_active_animations()
        });
        if let Some(folder) = &folder {
            write_snapshot(rig, folder, label)?;
        }
        press(Key::Return);
        assert_eq!(shown.get(), Some(i32::try_from(ROWS - 1)?));
    }
    Ok(())
}
