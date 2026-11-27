// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Saves bounded Analyze reports selected by the user.

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use slint::ComponentHandle;

use crate::ui::{AppWindow, CoefficientView, TimelineUi};

#[derive(Default)]
pub(super) struct ReportWriters {
    tasks: RefCell<Vec<JoinHandle<()>>>,
}

impl ReportWriters {
    pub(super) fn finish(&self) {
        for task in self.tasks.take() {
            let _ = task.join();
        }
    }
}

pub(super) fn bind(window: &AppWindow, writers: &Rc<ReportWriters>) {
    let timeline_writers = Rc::downgrade(writers);
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
        save_report(
            &window,
            timeline_writers.clone(),
            snapshot,
            "Save Timeline Statistics",
            "timeline-statistics.txt",
            ("Text report", "txt"),
            |window, status| {
                let view = window.global::<TimelineUi>();
                view.set_exporting(false);
                view.set_export_status(status);
            },
        );
    });

    let weak = window.as_weak();
    let coefficient_writers = Rc::downgrade(writers);
    window.global::<CoefficientView>().on_save(move || {
        let Some(window) = weak.upgrade() else { return };
        let view = window.global::<CoefficientView>();
        let snapshot = view.get_report().to_string();
        if snapshot.is_empty() || view.get_exporting() {
            return;
        }
        view.set_exporting(true);
        view.set_export_status("".into());
        save_report(
            &window,
            coefficient_writers.clone(),
            snapshot,
            "Save Transform Coefficients",
            "transform-coefficients.json",
            ("JSON report", "json"),
            |window, status| {
                let view = window.global::<CoefficientView>();
                view.set_exporting(false);
                view.set_export_status(status);
            },
        );
    });
}

fn save_report(
    window: &AppWindow,
    writers: Weak<ReportWriters>,
    snapshot: String,
    title: &str,
    file_name: &str,
    filter: (&str, &str),
    finish: fn(&AppWindow, slint::SharedString),
) {
    let dialog = rfd::AsyncFileDialog::new()
        .set_parent(&window.window().window_handle())
        .set_title(title)
        .set_file_name(file_name)
        .add_filter(filter.0, &[filter.1])
        .save_file();
    let weak = window.as_weak();
    if slint::spawn_local(async move {
        let Some(chosen) = dialog.await else {
            if let Some(window) = weak.upgrade() {
                finish(&window, "".into());
            }
            return;
        };
        let Some(writers) = writers.upgrade() else {
            return;
        };
        let path = chosen.path().to_path_buf();
        let completion = weak.clone();
        let result = thread::Builder::new()
            .name("analysis-report".into())
            .spawn(move || {
                let outcome = write_report(&path, snapshot.as_bytes());
                let _ = completion.upgrade_in_event_loop(move |window| {
                    finish(&window, save_status(outcome));
                });
            });
        match result {
            Ok(task) => {
                let mut tasks = writers.tasks.borrow_mut();
                tasks.retain(|task| !task.is_finished());
                tasks.push(task);
            }
            Err(error) => {
                if let Some(window) = weak.upgrade() {
                    finish(&window, save_status(Err(error)));
                }
            }
        }
    })
    .is_err()
    {
        finish(window, "Cannot open the save dialog.".into());
    }
}

fn save_status(outcome: io::Result<()>) -> slint::SharedString {
    match outcome {
        Ok(()) => "Report saved.".into(),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            "Choose a new file name. The destination already exists.".into()
        }
        Err(error) => format!("Cannot save the report. {error}").into(),
    }
}

fn write_report(path: &Path, contents: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stage = path.with_file_name(format!(
        ".splot-report-{}-{}.tmp",
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
