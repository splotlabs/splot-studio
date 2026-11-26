// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Writes the exact text the user has selected in the timeline view.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use slint::ComponentHandle;

use crate::ui::{AppWindow, TimelineUi};

pub(super) fn bind(window: &AppWindow) {
    let weak = window.as_weak();
    window.global::<TimelineUi>().on_save_statistics(move || {
        let Some(window) = weak.upgrade() else { return };
        let timeline = window.global::<TimelineUi>();
        let snapshot = timeline.get_exact_statistics().to_string();
        if snapshot.is_empty() || timeline.get_exporting() {
            return;
        }
        timeline.set_exporting(true);
        timeline.set_export_status("".into());
        let dialog = rfd::AsyncFileDialog::new()
            .set_parent(&window.window().window_handle())
            .set_title("Save Timeline Statistics")
            .set_file_name("timeline-statistics.txt")
            .add_filter("Text report", &["txt"])
            .save_file();
        let weak = weak.clone();
        if slint::spawn_local(async move {
            let Some(chosen) = dialog.await else {
                if let Some(window) = weak.upgrade() {
                    window.global::<TimelineUi>().set_exporting(false);
                }
                return;
            };
            let path = chosen.path().to_path_buf();
            let completion = weak.clone();
            let result = thread::Builder::new()
                .name("timeline-statistics".into())
                .spawn(move || {
                    let outcome = write_report(&path, snapshot.as_bytes());
                    let _ = completion.upgrade_in_event_loop(move |window| {
                        finish(&window, outcome);
                    });
                });
            if let Err(error) = result
                && let Some(window) = weak.upgrade()
            {
                finish(&window, Err(error));
            }
        })
        .is_err()
        {
            timeline.set_exporting(false);
            timeline.set_export_status("Cannot open the save dialog.".into());
        }
    });
}

fn finish(window: &AppWindow, outcome: io::Result<()>) {
    let timeline = window.global::<TimelineUi>();
    timeline.set_exporting(false);
    timeline.set_export_status(match outcome {
        Ok(()) => "Statistics saved.".into(),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            "Choose a new file name. The destination already exists.".into()
        }
        Err(error) => format!("Cannot save the statistics. {error}").into(),
    });
}

fn write_report(path: &Path, contents: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stage = path.with_file_name(format!(
        ".splot-timeline-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)?;
    let written = file.write_all(contents).and_then(|()| file.sync_all());
    drop(file);
    // Publishing a hard link refuses every existing destination, including the source.
    let published = written.and_then(|()| fs::hard_link(&stage, path));
    let cleanup = fs::remove_file(&stage);
    published.and(cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_save_refuses_to_replace_a_source() {
        let folder =
            std::env::temp_dir().join(format!("splot-timeline-save-{}", std::process::id()));
        fs::create_dir(&folder).unwrap_or_else(|error| unreachable!("{error}"));
        let source = folder.join("source.ivf");
        fs::write(&source, b"source").unwrap_or_else(|error| unreachable!("{error}"));
        let report = folder.join("report.txt");
        let refusal = write_report(&source, b"statistics").err();
        let actual_source = fs::read(&source);
        let saved = write_report(&report, b"statistics");
        let actual_report = fs::read(&report);
        let kept_files = fs::read_dir(&folder).map(Iterator::count);
        fs::remove_dir_all(folder).unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(
            refusal.map(|error| error.kind()),
            Some(io::ErrorKind::AlreadyExists)
        );
        assert_eq!(actual_source.ok().as_deref(), Some(b"source".as_slice()));
        assert!(saved.is_ok());
        assert_eq!(
            actual_report.ok().as_deref(),
            Some(b"statistics".as_slice())
        );
        assert_eq!(kept_files.ok(), Some(2));
    }
}
