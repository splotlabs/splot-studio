// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The Advanced editor in compiled Slint with a real engine and Y4M source.
//! The software renderer needs no display. Build the sibling engine first
//! with `cargo build -p studio-engine` in the same profile.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{self, EventLoopProxy, Platform, WindowAdapter};
use slint::{ComponentHandle, EventLoopError, LogicalSize, Model, PlatformError};
use studio_app::engine::ENGINE_NAME;
use studio_app::journal::{self, Journal, JournalState, Loaded};
use studio_app::{AppWindow, Prepared, Studio};
use studio_domain::options::OptionValue;

const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

type Event = Box<dyn FnOnce() + Send>;

struct Headless {
    window: Rc<MinimalSoftwareWindow>,
    events: Sender<Event>,
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy(self.events.clone())))
    }
}

struct Proxy(Sender<Event>);

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), EventLoopError> {
        Ok(())
    }

    fn invoke_from_event_loop(&self, event: Event) -> Result<(), EventLoopError> {
        self.0
            .send(event)
            .map_err(|_| EventLoopError::EventLoopTerminated)
    }
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn engine() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_splot-studio"))
        .with_file_name(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "Build the engine at {} first.",
        path.display()
    );
    path
}

fn y4m() -> Vec<u8> {
    let mut bytes = b"YUV4MPEG2 W64 H48 F25:1 Ip A1:1 C420jpeg\n".to_vec();
    for sample in [80, 100, 120, 140] {
        bytes.extend_from_slice(b"FRAME\n");
        bytes.extend([sample; 64 * 48]);
        bytes.extend([128; 2 * 32 * 24]);
    }
    bytes
}

struct Rig {
    studio: Studio,
    events: Receiver<Event>,
    platform: Rc<MinimalSoftwareWindow>,
    journal: PathBuf,
}

impl Rig {
    fn new(source: PathBuf, journal: PathBuf) -> Self {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let (tx, events) = mpsc::channel();
        platform::set_platform(Box::new(Headless {
            window: window.clone(),
            events: tx,
        }))
        .unwrap_or_else(|e| unreachable!("{e}"));

        let prepared = Prepared {
            instance: None,
            journal: Some(journal.clone()),
            startup: studio_app::state::Startup::default(),
        };
        let studio = studio_app::build_with_engine(prepared, vec![source], Ok(engine()))
            .unwrap_or_else(|e| unreachable!("{e}"));
        let main = studio.window().window();
        main.set_size(LogicalSize::new(960.0, 680.0));
        studio
            .window()
            .show()
            .unwrap_or_else(|e| unreachable!("{e}"));
        Self {
            studio,
            events,
            platform: window,
            journal,
        }
    }

    fn window(&self) -> &AppWindow {
        self.studio.window()
    }

    fn until(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            platform::update_timers_and_animations();
            if done() {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "Timed out waiting for {what}.");
            let wait = platform::duration_until_next_timer_update()
                .unwrap_or(left)
                .min(left)
                .min(Duration::from_millis(20));
            if let Ok(event) = self.events.recv_timeout(wait) {
                event();
            }
        }
    }

    fn saved(&self, what: &str, done: impl Fn(&Journal) -> bool) -> Journal {
        let read = || match journal::load(&self.journal) {
            Loaded::Found(journal) => Some(journal),
            _ => None,
        };
        self.until(what, || read().as_ref().is_some_and(&done));
        read().unwrap_or_else(|| unreachable!("Cannot read the saved settings."))
    }

    fn pixels(&self) -> Vec<slint::Rgb8Pixel> {
        let size = self.window().window().size();
        let width = size.width as usize;
        let mut buffer = vec![slint::Rgb8Pixel::default(); width * size.height as usize];
        self.window().window().request_redraw();
        assert!(self.platform.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, width);
        }));
        buffer
    }
}

fn keys(window: &AppWindow) -> Vec<String> {
    window
        .get_advanced_options()
        .iter()
        .map(|option| option.key.to_string())
        .collect()
}

fn edit(window: &AppWindow, id: &str, value: &str) {
    assert!(
        window
            .get_advanced_fields()
            .iter()
            .any(|field| field.id == id)
    );
    window.invoke_advanced_edit(id.into(), value.into());
}

fn change_key_interval(rig: &Rig) {
    let window = rig.window();
    for query in ["keyint", "Keyframe interval"] {
        window.invoke_advanced_search(query.into());
        assert_eq!(keys(window), ["keyint"]);
    }
    window.invoke_advanced_select("keyint".into());
    rig.until("the inherited keyframe interval", || {
        !window.get_advanced_effective().is_empty()
    });
    let inherited = window.get_advanced_effective();
    assert_ne!(inherited, "3");
    assert!(
        window
            .get_advanced_fields()
            .iter()
            .any(|field| { field.id == "value/@state" && field.text == "Inherit" })
    );

    edit(window, "value", "0");
    rig.until("the keyframe interval error", || {
        !window.get_advanced_error().is_empty()
    });
    edit(window, "value", "3");
    rig.until("the valid keyframe interval", || {
        window.get_advanced_effective() == "3" && window.get_advanced_error().is_empty()
    });
    assert_eq!(window.get_advanced_origin(), "explicit");
    window.invoke_advanced_search("".into());
    window.invoke_advanced_modified(true);
    assert_eq!(keys(window), ["keyint"]);

    window.invoke_advanced_reset("selected".into());
    assert_eq!(keys(window), Vec::<String>::new());
    assert!(window.get_advanced_can_undo());
    window.invoke_advanced_undo();
    assert_eq!(keys(window), ["keyint"]);
    assert!(window.get_advanced_can_redo());
    window.invoke_advanced_redo();
    assert_eq!(keys(window), Vec::<String>::new());
    window.invoke_advanced_undo();
    window.invoke_advanced_modified(false);
}

fn check_context_edits(rig: &Rig) {
    let window = rig.window();
    let key = "preset.min_block_log2";
    assert!(!keys(window).iter().any(|shown| shown == key));
    window.invoke_advanced_search(key.into());
    assert_eq!(keys(window), [key]);
    window.invoke_advanced_select(key.into());
    edit(window, "value/@all", "4");
    edit(window, "value/key", "3");

    let requested = OptionValue::List([3, 4, 4, 4].map(OptionValue::Integer).to_vec());
    rig.saved("the four frame values", |journal| {
        journal.settings.values.get(key) == Some(&requested)
    });
    edit(window, "value/@frames", "All frames");
    assert!(!window.get_advanced_error().is_empty());
    let current = |id: &str| {
        window
            .get_advanced_fields()
            .iter()
            .find(|field| field.id == id)
            .map(|field| field.text)
    };
    assert_eq!(current("value/key").as_deref(), Some("3"));
    assert_eq!(current("value/base").as_deref(), Some("4"));
    window.invoke_advanced_reset("group".into());
    rig.saved("the group reset", |journal| {
        !journal.settings.values.contains_key(key)
            && journal.settings.values.get("keyint") == Some(&OptionValue::Integer(3))
    });
}

fn check_nested_error_focus(rig: &Rig) {
    let window = rig.window();
    window.invoke_advanced_search("rate control".into());
    window.invoke_advanced_select("rate_control".into());
    edit(window, "value/@mode", "constant_qp");
    edit(window, "value/qp", "300");
    window.invoke_advanced_search("keyint".into());
    rig.until("the quantizer field error focus", || {
        window.get_advanced_selected() == "rate_control"
            && window.get_advanced_focus_field() == "value/qp"
            && !window.get_advanced_error().is_empty()
    });
    assert!(keys(window).iter().any(|option| option == "rate_control"));
    window.invoke_advanced_reset("selected".into());
    rig.until("the rate control reset", || {
        window.get_advanced_error().is_empty()
    });
}

fn source_fixture(dir: &Path) -> PathBuf {
    let source = dir.join("advanced.y4m");
    fs::write(&source, y4m()).unwrap_or_else(|e| unreachable!("{e}"));
    source
}

#[test]
fn advanced_edits_and_submits_the_displayed_policy() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("studio-app-advanced-{}", std::process::id())));
    fs::create_dir(&scratch.0).unwrap_or_else(|e| unreachable!("{e}"));
    let source = source_fixture(&scratch.0);
    let rig = Rig::new(source.clone(), scratch.0.join(journal::FILE_NAME));
    let window = rig.window();
    rig.saved("the source probe", |journal| {
        journal
            .items
            .first()
            .is_some_and(|item| item.facts.is_some())
    });
    rig.until("the Convert action", || window.get_can_encode());
    let simple = rig.pixels();
    window.set_advanced_open(true);
    rig.until("the option catalog", || !keys(window).is_empty());
    assert!(simple != rig.pixels(), "The window did not show Advanced.");

    change_key_interval(&rig);
    check_context_edits(&rig);
    check_nested_error_focus(&rig);

    window.invoke_advanced_search("keyint".into());
    window.invoke_advanced_select("keyint".into());
    let original_preset = window.get_preset();
    let next_preset = 10;
    assert_ne!(original_preset, next_preset);
    window.invoke_advanced_preview_preset(next_preset);
    rig.until("the preset preview", || {
        window.get_advanced_can_apply_preset()
    });
    assert_eq!(window.get_preset(), original_preset);
    assert!(!window.get_advanced_preset_preview().is_empty());
    window.invoke_advanced_apply_preset();
    rig.until("the new preset", || {
        window.get_preset() == next_preset && window.get_advanced_effective() == "3"
    });
    let edited = rig.saved("the custom settings", |journal| {
        i32::from(journal.settings.base.preset.get()) == next_preset
            && journal.settings.values.get("keyint") == Some(&OptionValue::Integer(3))
    });

    window.set_advanced_open(false);
    platform::update_timers_and_animations();
    assert!(window.get_advanced_custom());
    window.set_advanced_open(true);
    rig.until("the restored validated interval", || {
        window.get_advanced_effective() == "3"
            && window.get_advanced_error().is_empty()
            && window.get_advanced_status().is_empty()
    });
    assert!(window.get_can_encode());
    edit(window, "value", "abc");
    window.invoke_advanced_search("tile groups".into());
    window.set_advanced_open(false);
    platform::update_timers_and_animations();
    window.invoke_encode_requested();
    rig.until("the invalid field to reopen", || {
        window.get_advanced_open()
            && window.get_advanced_selected() == "keyint"
            && window.get_advanced_focus_field() == "value"
    });
    assert!(keys(window).iter().any(|option| option == "keyint"));
    assert!(!window.get_advanced_error().is_empty());
    assert!(
        window
            .get_advanced_fields()
            .iter()
            .any(|field| field.id == "value" && field.text == "abc")
    );
    assert!(window.get_can_encode());
    assert!(!source.with_extension("ivf").exists());
    edit(window, "value", "3");
    rig.until("the corrected keyframe interval", || {
        window.get_advanced_error().is_empty()
    });
    window.invoke_encode_requested();
    let completed = rig.saved("the encode to finish", |journal| {
        journal.items.first().is_some_and(|item| {
            matches!(
                &item.state,
                JournalState::Completed { .. }
                    | JournalState::Failed { .. }
                    | JournalState::Lost { .. }
            )
        })
    });
    let item = &completed.items[0];
    let JournalState::Completed { job, report } = &item.state else {
        unreachable!("The encode did not complete. {:?}", item.state);
    };
    assert_eq!(report.frames, 4);
    assert_eq!(job.settings, edited.settings);
    let interval = item
        .attempt_policy
        .iter()
        .find(|option| option.key == "keyint")
        .unwrap_or_else(|| unreachable!("The job must record its keyframe interval."));
    assert_eq!(interval.requested, Some(OptionValue::Integer(3)));
    assert_eq!(interval.value, OptionValue::Integer(3));
    assert_eq!(interval.origin, OptionValue::Choice("explicit".into()));
    let output = fs::read(source.with_extension("ivf")).unwrap_or_default();
    assert_eq!(output.get(0..4), Some(&b"DKIF"[..]));
    assert_eq!(output.get(8..12), Some(&b"AV02"[..]));
    assert_eq!(u64::try_from(output.len()), Ok(report.bytes));
    assert_eq!(fs::read(source).unwrap_or_default(), y4m());
}
