// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command as Process, Stdio};

use slint::Model;
use studio_domain::options::{MAX_OPTIONS_PAGE_ITEMS, OptionDescriptor, OptionValue, ValueShape};
use studio_domain::protocol::{
    BuildIdentity, Command, Envelope, Event, read_message, write_message,
};

use super::{Rig, edit, engine, keys};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn describe_worker() -> TestResult<BTreeMap<String, OptionDescriptor>> {
    let mut worker = Process::new(engine())
        .arg("worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut input = worker.stdin.take().ok_or("Expected worker input.")?;
    let mut output = worker.stdout.take().ok_or("Expected worker output.")?;
    write_message(
        &mut input,
        &Envelope::new(
            1,
            Command::Hello {
                client: BuildIdentity {
                    name: "option coverage".into(),
                    version: "0".into(),
                },
            },
        ),
    )?;
    let Some(Envelope {
        body: Event::Hello { capabilities, .. },
        ..
    }) = read_message(&mut output)?
    else {
        return Err("Expected the worker handshake.".into());
    };
    let schema = capabilities
        .options
        .ok_or("Expected the option schema.")?
        .schema;
    let mut descriptors = BTreeMap::new();
    loop {
        let first = u32::try_from(descriptors.len())?;
        let request_id = u64::from(first) + 2;
        write_message(
            &mut input,
            &Envelope::new(
                request_id,
                Command::DescribeOptions {
                    schema,
                    first,
                    count: MAX_OPTIONS_PAGE_ITEMS,
                },
            ),
        )?;
        let Some(Envelope {
            request_id: answered,
            body:
                Event::OptionsDescribed {
                    first: start,
                    total,
                    options,
                },
            ..
        }) = read_message(&mut output)?
        else {
            return Err("Expected a page of worker options.".into());
        };
        assert_eq!(answered, request_id);
        assert_eq!(start, first);
        assert!(!options.is_empty());
        for option in options {
            assert!(descriptors.insert(option.key.clone(), option).is_none());
        }
        if descriptors.len() == usize::try_from(total)? {
            break;
        }
    }
    drop(input);
    assert!(worker.wait()?.success());
    Ok(descriptors)
}

pub(super) fn check_catalog(rig: &Rig) -> TestResult {
    let descriptors = describe_worker()?;
    let window = rig.window();
    window.invoke_advanced_expert(true);
    rig.until("the complete option catalog", || {
        keys(window).len() >= descriptors.len()
    });
    let shown: BTreeSet<_> = keys(window).into_iter().collect();
    assert_eq!(shown, descriptors.keys().cloned().collect());
    assert_eq!(shown.len(), window.get_advanced_options().row_count());

    let readonly: BTreeSet<_> = [
        "width",
        "height",
        "bit_depth",
        "chroma",
        "frame_rate",
        "first_pass",
        "preset",
        "preset_overrides",
    ]
    .into();
    for row in window.get_advanced_options().iter() {
        let key = row.key.as_str();
        assert_ne!(
            row.group, "Other options",
            "Classify {key} before exposing it."
        );
        let descriptor = &descriptors[key];
        let excluded = readonly.contains(key);
        assert_eq!(descriptor.shape == ValueShape::ReadOnly, excluded, "{key}");

        window.invoke_advanced_select(row.key.clone());
        let fields = window.get_advanced_fields();
        assert!(fields.row_count() > 0, "{key} has no controls");
        assert_eq!(
            fields.iter().all(|field| field.kind == "readonly"),
            excluded,
            "{key}"
        );
        assert!(fields.iter().all(|field| matches!(
            field.kind.as_str(),
            "readonly" | "text" | "choice" | "action"
        )));
    }
    window.invoke_advanced_expert(false);
    Ok(())
}

pub(super) fn check_explicit_values(rig: &Rig) {
    let window = rig.window();
    for (key, field, input, expected) in [
        ("film_grain", "value", "Off", OptionValue::Bool(false)),
        ("tune", "value", "ssim", OptionValue::Choice("ssim".into())),
        (
            "resize_denominator",
            "value/@state",
            "None",
            OptionValue::None,
        ),
    ] {
        window.invoke_advanced_select(key.into());
        edit(window, field, input);
        rig.saved("the explicit value", |journal| {
            journal.settings.values.get(key) == Some(&expected)
        });
        rig.until("the explicit origin", || {
            window.get_advanced_origin() == "explicit" && window.get_advanced_error().is_empty()
        });
        window.invoke_advanced_reset("selected".into());
        rig.saved("the inherited value after reset", |journal| {
            !journal.settings.values.contains_key(key)
        });
        rig.until("the inherited origin", || {
            window.get_advanced_origin() == "codec default"
                && window.get_advanced_error().is_empty()
        });
    }
}
