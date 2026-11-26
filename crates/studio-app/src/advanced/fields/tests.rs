// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use super::*;
use studio_domain::options::OptionMode;

fn field(shape: Shape) -> OptionDescriptor {
    OptionDescriptor {
        key: "test".into(),
        group: String::new(),
        help: String::new(),
        shape,
        expert: false,
    }
}

fn desc(shape: Shape) -> OptionMember {
    OptionMember {
        shape,
        help: String::new(),
        applicability: String::new(),
        read_only: None,
    }
}

fn record<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Record(
        pairs
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn nums(values: &[i64]) -> Value {
    Value::List(values.iter().copied().map(Value::Integer).collect())
}

fn effective(value: Value) -> EffectiveOption {
    EffectiveOption {
        key: "test".into(),
        requested: None,
        value,
        screen: None,
        origin: Value::Choice("preset".into()),
        screen_origin: None,
    }
}

#[test]
fn explicit_zero_false_and_null_are_distinct_from_inherit() -> Result<(), String> {
    let field = field(Shape::Optional {
        value: Box::new(Shape::Integer { min: 0, max: 8 }),
    });
    let current = effective(Value::Integer(4));
    assert_eq!(
        edit(&field, None, Some(&current), "value", "0")?,
        Some(Value::Integer(0))
    );
    assert_eq!(
        edit(&field, None, Some(&current), "value/@state", NONE)?,
        Some(Value::None)
    );
    assert_eq!(
        edit(
            &field,
            Some(&Value::None),
            Some(&current),
            "value/@state",
            INHERIT
        )?,
        None
    );

    let flag = super::tests::field(Shape::Bool);
    assert_eq!(
        edit(&flag, None, None, "value", "Off")?,
        Some(Value::Bool(false))
    );
    Ok(())
}

#[test]
fn mode_switch_retains_shared_members_and_removes_inactive_members() -> Result<(), String> {
    let number = desc(Shape::Integer { min: 1, max: 100 });
    let field = field(Shape::Modes {
        modes: BTreeMap::from([
            (
                "first".into(),
                OptionMode {
                    help: String::new(),
                    fields: BTreeMap::from([
                        ("shared".into(), number.clone()),
                        ("old".into(), number.clone()),
                    ]),
                },
            ),
            (
                "second".into(),
                OptionMode {
                    help: String::new(),
                    fields: BTreeMap::from([
                        ("shared".into(), number.clone()),
                        ("new".into(), number),
                    ]),
                },
            ),
        ]),
    });
    let requested = record([
        ("mode", Value::Choice("first".into())),
        ("shared", Value::Integer(40)),
        ("old", Value::Integer(5)),
    ]);
    let result = edit(&field, Some(&requested), None, "value/@mode", "second")?;
    assert_eq!(
        result,
        Some(record([
            ("mode", Value::Choice("second".into())),
            ("shared", Value::Integer(40)),
            ("new", Value::Integer(1)),
        ]))
    );
    assert!(edit(&field, result.as_ref(), None, "value/old", "6").is_err());
    Ok(())
}

#[test]
fn mixed_content_does_not_copy_source_facts_into_request() -> Result<(), String> {
    let field = field(Shape::Record {
        fields: BTreeMap::from([
            ("timing_info".into(), desc(Shape::Bool)),
            (
                "full_range".into(),
                OptionMember {
                    read_only: Some("Use the source value.".into()),
                    ..desc(Shape::Bool)
                },
            ),
        ]),
    });
    let readback = effective(record([
        ("timing_info", Value::Bool(true)),
        ("full_range", Value::Bool(true)),
    ]));
    let result = edit(&field, None, Some(&readback), "value/timing_info", "Off")?;
    assert_eq!(result, Some(record([("timing_info", Value::Bool(false))])));
    assert!(edit(&field, None, Some(&readback), "value/full_range", "Off").is_err());
    Ok(())
}

#[test]
fn frame_edits_keep_other_frame_values_and_require_explicit_replacement() -> Result<(), String> {
    let frames = ["key", "base", "mid", "leaf"].map(str::to_owned);
    let field = field(Shape::PerFrame {
        value: Box::new(Shape::Integer { min: 0, max: 8 }),
        frames: frames.to_vec(),
    });
    let readback = effective(Value::Record(
        frames
            .iter()
            .enumerate()
            .flat_map(|(index, frame)| {
                ["low", "normal", "high"].map(|level| {
                    (
                        format!("{frame}.{level}"),
                        Value::Integer(i64::try_from(index).unwrap_or(0)),
                    )
                })
            })
            .collect(),
    ));
    assert!(edit(&field, None, Some(&readback), "value/@frames", ALL).is_err());
    assert_eq!(
        edit(&field, None, Some(&readback), "value/base", "6")?,
        Some(nums(&[0, 6, 2, 3]))
    );
    assert_eq!(
        edit(&field, None, Some(&readback), "value/@all", "8")?,
        Some(Value::Integer(8))
    );

    let mut readback = readback;
    readback.screen = Some(Value::Record(BTreeMap::new()));
    assert!(edit(&field, None, Some(&readback), "value/@frames", BY_FRAME).is_err());
    let controls = rows(&field, None, Some(&readback));
    assert!(
        controls
            .iter()
            .any(|row| row.id == "value/key" && row.kind == "readonly")
    );
    Ok(())
}

#[test]
fn fixed_lists_bytes_and_variable_lists_use_typed_values() -> Result<(), String> {
    let int = Shape::Integer { min: 0, max: 255 };
    let fixed = field(Shape::List {
        element: Box::new(int.clone()),
        length: Some(3),
    });
    assert_eq!(
        edit(&fixed, Some(&nums(&[1, 2, 3])), None, "value/1", "5")?,
        Some(nums(&[1, 5, 3]))
    );
    assert!(edit(&fixed, None, None, "value/@add", "").is_err());

    let variable = field(Shape::List {
        element: Box::new(int),
        length: None,
    });
    assert_eq!(
        edit(&variable, Some(&nums(&[1, 2])), None, "value/@remove0", "")?,
        Some(nums(&[2]))
    );
    assert_eq!(
        edit(&variable, Some(&nums(&[1, 2])), None, "value/@add", "")?,
        Some(nums(&[1, 2, 0]))
    );

    let bytes = field(Shape::Bytes { max_length: 2 });
    assert_eq!(
        edit(&bytes, None, None, "value", "00 fF")?,
        Some(nums(&[0, 255]))
    );
    assert!(edit(&bytes, None, None, "value", "a").is_err());
    assert!(edit(&bytes, None, None, "value", "00 ff 01").is_err());
    Ok(())
}
