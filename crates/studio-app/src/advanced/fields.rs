// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Typed edit controls for one descriptor from the engine.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;

use studio_domain::options::{EffectiveOption, OptionDescriptor, OptionMember, UnsignedLimit};
use studio_domain::options::{OptionValue as Value, ValueShape as Shape};

/// One editable or read-only row in an option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRow {
    /// The opaque identifier to return with an edit.
    pub id: String,
    /// The control label.
    pub label: String,
    /// The control kind, either `choice`, `text`, `action` or `readonly`.
    pub kind: String,
    /// The current text, selected choice or action caption.
    pub text: String,
    /// The available labels for a choice control.
    pub choices: Vec<String>,
    /// The help for this control.
    pub help: String,
}

type Setter = Rc<dyn Fn(Option<Value>) -> Option<Value>>;
type Apply = Box<dyn Fn(&str) -> Result<Option<Value>, String>>;

struct Control {
    row: FieldRow,
    apply: Option<Apply>,
}

const INHERIT: &str = "Inherit";
const SET: &str = "Set";
const NONE: &str = "None";
const ALL: &str = "All frames";
const BY_FRAME: &str = "By frame";

/// Lists the controls for the requested value and the engine's effective value.
#[must_use]
pub fn rows(
    descriptor: &OptionDescriptor,
    requested: Option<&Value>,
    effective: Option<&EffectiveOption>,
) -> Vec<FieldRow> {
    controls(descriptor, requested, effective)
        .into_iter()
        .map(|control| control.row)
        .collect()
}

/// Applies one edit and returns the new request. `None` removes the override.
///
/// # Errors
/// Returns a description if the control is unavailable or the text does not fit its shape.
pub fn edit(
    descriptor: &OptionDescriptor,
    requested: Option<&Value>,
    effective: Option<&EffectiveOption>,
    id: &str,
    text: &str,
) -> Result<Option<Value>, String> {
    let control = controls(descriptor, requested, effective)
        .into_iter()
        .find(|control| control.row.id == id)
        .ok_or("This control is unavailable. Select the option again.")?;
    control.apply.ok_or("This control is read-only.")?(text)
}

fn controls(
    descriptor: &OptionDescriptor,
    requested: Option<&Value>,
    effective: Option<&EffectiveOption>,
) -> Vec<Control> {
    let mut controls = Vec::new();
    let contexts = effective.filter(|option| {
        option
            .screen
            .as_ref()
            .is_none_or(|screen| screen == &option.value)
    });
    let effective = effective.map(|option| &option.value);
    let root = Node {
        id: "value".into(),
        label: "Value".into(),
        help: descriptor.help.clone(),
        set: Rc::new(|value| value),
    };
    if let Shape::PerFrame { value, frames } = &descriptor.shape {
        root.frames(
            &mut controls,
            value,
            frames,
            requested,
            contexts.map(|option| &option.value),
        );
    } else if !matches!(descriptor.shape, Shape::Optional { .. } | Shape::ReadOnly) {
        root.state(
            &mut controls,
            &descriptor.shape,
            requested,
            effective,
            false,
        );
        root.shape(&mut controls, &descriptor.shape, requested, effective);
    } else {
        root.shape(&mut controls, &descriptor.shape, requested, effective);
    }
    controls
}

struct Node {
    id: String,
    label: String,
    help: String,
    set: Setter,
}

impl Node {
    fn child(&self, id: &str, label: String, help: String, set: Setter) -> Self {
        Self {
            id: format!("{}/{id}", self.id),
            label,
            help,
            set,
        }
    }

    fn control(
        &self,
        kind: &str,
        text: String,
        choices: Vec<String>,
        apply: Option<Apply>,
    ) -> Control {
        Control {
            row: FieldRow {
                id: self.id.clone(),
                label: self.label.clone(),
                kind: kind.into(),
                text,
                choices,
                help: self.help.clone(),
            },
            apply,
        }
    }

    fn state(
        &self,
        rows: &mut Vec<Control>,
        shape: &Shape,
        requested: Option<&Value>,
        effective: Option<&Value>,
        optional: bool,
    ) {
        let current = match requested {
            None => INHERIT,
            Some(Value::None) if optional => NONE,
            Some(_) => SET,
        };
        let node = self.child(
            "@state",
            format!("{} source", self.label),
            self.help.clone(),
            self.set.clone(),
        );
        let set = self.set.clone();
        let value = seed(shape, requested.or(effective));
        let mut choices = vec![INHERIT.into(), SET.into()];
        if optional {
            choices.insert(1, NONE.into());
        }
        rows.push(node.control(
            "choice",
            current.into(),
            choices,
            Some(Box::new(move |text| match text {
                INHERIT => Ok(set(None)),
                NONE if optional => Ok(set(Some(Value::None))),
                SET => Ok(set(Some(value.clone()))),
                _ => Err("Select a value source from the list.".into()),
            })),
        ));
    }

    fn shape(
        &self,
        rows: &mut Vec<Control>,
        shape: &Shape,
        requested: Option<&Value>,
        effective: Option<&Value>,
    ) {
        let value = requested.or(effective);
        match shape {
            Shape::Optional { value: inner } => {
                self.state(rows, inner, requested, effective, true);
                if !matches!(value, Some(Value::None)) {
                    self.shape(rows, inner, requested, effective);
                }
            }
            Shape::Modes { modes } => {
                let current = member(value, "mode");
                let shape = shape.clone();
                let set = self.set.clone();
                let value = value.cloned();
                let node = self.child("@mode", "Mode".into(), self.help.clone(), set.clone());
                let active = match current {
                    Some(Value::Choice(key)) => modes.get(key),
                    _ => None,
                };
                rows.push(node.control(
                    "choice",
                    summary(current),
                    modes.keys().cloned().collect(),
                    Some(Box::new(move |text| {
                        let Shape::Modes { modes } = &shape else {
                            return Err("Use a mode from the list.".into());
                        };
                        let mode = modes.get(text).ok_or("Select a mode from the list.")?;
                        let mut record = seed_members(&mode.fields, value.as_ref());
                        record.insert("mode".into(), Value::Choice(text.into()));
                        Ok(set(Some(Value::Record(record))))
                    })),
                ));
                if let Some(mode) = active {
                    self.members(rows, &mode.fields, requested, effective, true);
                }
            }
            Shape::Record { fields } => self.members(rows, fields, requested, effective, false),
            Shape::List { element, length } => {
                self.list(rows, element, *length, requested, effective);
            }
            Shape::PerFrame { value, frames } => {
                self.frames(rows, value, frames, requested, effective);
            }
            Shape::ReadOnly => {
                rows.push(self.control("readonly", summary(value), Vec::new(), None));
            }
            shape => self.scalar(rows, shape, value),
        }
    }

    fn members(
        &self,
        rows: &mut Vec<Control>,
        fields: &BTreeMap<String, OptionMember>,
        requested: Option<&Value>,
        effective: Option<&Value>,
        tagged: bool,
    ) {
        let mut record = seed_members(fields, requested.or(effective));
        if tagged && let Some(mode) = member(requested.or(effective), "mode") {
            record.insert("mode".into(), mode.clone());
        }
        for (key, field) in fields {
            let set = self.set.clone();
            let key_to_set = key.clone();
            let mut base = record.clone();
            let label = title(key);
            let help = [
                field.help.as_str(),
                field.applicability.as_str(),
                field.read_only.as_deref().unwrap_or_default(),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            if field.read_only.is_some() {
                let node = self.child(key, label, help, set);
                rows.push(node.control(
                    "readonly",
                    summary(member(effective, key).or(member(requested, key))),
                    Vec::new(),
                    None,
                ));
                continue;
            }
            // Only optional members can be removed from a compound value.
            if requested.is_none() {
                base.retain(|key, _| {
                    !fields
                        .get(key)
                        .is_some_and(|field| optional_member(&field.shape))
                });
            }
            let node = self.child(
                key,
                label,
                help,
                Rc::new(move |value| {
                    let mut record = base.clone();
                    match value {
                        Some(value) => record.insert(key_to_set.clone(), value),
                        None => record.remove(&key_to_set),
                    };
                    set((!record.is_empty()).then_some(Value::Record(record)))
                }),
            );
            node.shape(
                rows,
                &member_shape(&field.shape, &record),
                member(requested, key),
                member(effective, key),
            );
        }
    }

    fn list(
        &self,
        rows: &mut Vec<Control>,
        element: &Shape,
        length: Option<u32>,
        requested: Option<&Value>,
        effective: Option<&Value>,
    ) {
        let mut values = items(requested.or(effective)).to_vec();
        if let Some(length) = length {
            values.resize_with(length as usize, || seed(element, None));
        }
        let values = Rc::new(values);
        for (index, value) in values.iter().enumerate() {
            let node = self.child(
                &index.to_string(),
                format!("{} {}", self.label, index + 1),
                self.help.clone(),
                item_setter(&self.set, &values, index),
            );
            node.shape(rows, element, Some(value), None);
            if length.is_none() {
                let id = format!("@remove{index}");
                self.action(rows, &id, format!("Remove item {}", index + 1), node.set);
            }
        }
        if length.is_none() {
            let value = seed(element, None);
            let set = self.set.clone();
            self.action(
                rows,
                "@add",
                format!("Add {} item", self.label.to_lowercase()),
                Rc::new(move |_| {
                    let mut values = values.as_ref().clone();
                    values.push(value.clone());
                    set(Some(Value::List(values)))
                }),
            );
        }
    }

    fn action(&self, rows: &mut Vec<Control>, id: &str, label: String, set: Setter) {
        let node = self.child(id, label, String::new(), set.clone());
        rows.push(node.control(
            "action",
            node.label.clone(),
            Vec::new(),
            Some(Box::new(move |_| Ok(set(None)))),
        ));
    }

    fn scalar(&self, rows: &mut Vec<Control>, shape: &Shape, value: Option<&Value>) {
        let (kind, choices, text) = match shape {
            Shape::Bool => (
                "choice",
                vec!["On".into(), "Off".into()],
                match value {
                    Some(Value::Bool(true)) => "On".into(),
                    Some(Value::Bool(false)) => "Off".into(),
                    _ => String::new(),
                },
            ),
            Shape::Choice { values } => (
                "choice",
                values.clone(),
                value.map_or_else(String::new, |v| summary(Some(v))),
            ),
            Shape::Bytes { .. } => {
                let mut hex = String::new();
                for (index, byte) in items(value).iter().enumerate() {
                    if let Value::Integer(byte) = byte {
                        if index > 0 {
                            hex.push(' ');
                        }
                        let _ = write!(hex, "{byte:02x}");
                    }
                }
                ("text", Vec::new(), hex)
            }
            _ => (
                "text",
                Vec::new(),
                value.map_or_else(String::new, |v| summary(Some(v))),
            ),
        };
        let display_shape = shape;
        let shape = shape.clone();
        let bytes_max = if let Shape::Bytes { max_length } = &shape {
            Some(*max_length)
        } else {
            None
        };
        let set = self.set.clone();
        let mut control = self.control(
            kind,
            text,
            choices,
            Some(Box::new(move |text| {
                parse(&shape, text).map(|value| set(Some(value)))
            })),
        );
        if let Shape::Unsigned {
            unit: Some(unit), ..
        } = display_shape
        {
            let _ = write!(control.row.help, " Unit is {unit}.");
        }
        if let Some(max_length) = bytes_max {
            let _ = write!(
                control.row.help,
                " Enter up to {max_length} bytes as hexadecimal pairs."
            );
        }
        rows.push(control);
    }

    fn frames(
        &self,
        rows: &mut Vec<Control>,
        shape: &Shape,
        frames: &[String],
        requested: Option<&Value>,
        effective: Option<&Value>,
    ) {
        let expanded: Vec<_> = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                frame_value(shape, requested, index).or_else(|| uniform_frame(effective, frame))
            })
            .collect();
        let all = uniform(&expanded);
        let current = match requested {
            None => INHERIT,
            Some(value) if fits(shape, value) => ALL,
            Some(_) => BY_FRAME,
        };
        let all_value = all.cloned();
        let by_frame = expanded
            .iter()
            .map(|value| value.cloned())
            .collect::<Option<Vec<_>>>();
        let selection = by_frame.clone();
        let set = self.set.clone();
        let node = self.child(
            "@frames",
            "Frame override".into(),
            "Inherit every frame, set one value for all frames, or set four frame values.".into(),
            set.clone(),
        );
        rows.push(node.control(
            "choice",
            current.into(),
            vec![INHERIT.into(), ALL.into(), BY_FRAME.into()],
            Some(Box::new(move |text| match text {
                INHERIT => Ok(set(None)),
                ALL => all_value.clone().map(|v| set(Some(v))).ok_or_else(|| {
                    "The frame values differ. Enter a value in Set all frames to replace them.".into()
                }),
                BY_FRAME => selection.clone().map(|v| set(Some(Value::List(v)))).ok_or_else(|| {
                    "The inherited values can vary by coefficient level or content. Set all frames before you edit individual frames.".into()
                }),
                _ => Err("Select a frame override from the list.".into()),
            })),
        ));
        self.all_frames(rows, shape, all);
        if let Some(values) = by_frame {
            let values = Rc::new(values);
            for (index, (frame, value)) in frames.iter().zip(values.iter()).enumerate() {
                let node = self.child(
                    frame,
                    title(frame),
                    self.help.clone(),
                    item_setter(&self.set, &values, index),
                );
                node.shape(rows, shape, Some(value), None);
            }
        } else {
            for frame in frames {
                let node = self.child(frame, title(frame), self.help.clone(), self.set.clone());
                rows.push(node.control(
                    "readonly",
                    uniform_frame(effective, frame).map_or_else(
                        || "No single inherited value".into(),
                        |value| summary(Some(value)),
                    ),
                    Vec::new(),
                    None,
                ));
            }
        }
    }

    fn all_frames(&self, rows: &mut Vec<Control>, shape: &Shape, all: Option<&Value>) {
        let node = self.child(
            "@all",
            "Set all frames".into(),
            "An edit applies to every frame and replaces frame-specific overrides.".into(),
            self.set.clone(),
        );
        if all.is_none()
            && matches!(
                shape,
                Shape::List {
                    length: Some(_),
                    ..
                }
            )
        {
            let shape = shape.clone();
            let set = node.set.clone();
            rows.push(node.control(
                "text",
                String::new(),
                Vec::new(),
                Some(Box::new(move |text| {
                    let Shape::List {
                        element,
                        length: Some(length),
                    } = &shape
                    else {
                        return Err("Enter a list of frame values.".into());
                    };
                    let values = text
                        .split(',')
                        .map(|text| parse(element, text.trim()))
                        .collect::<Result<Vec<_>, _>>()?;
                    if values.len() != *length as usize {
                        return Err(format!("Enter {length} values separated by commas."));
                    }
                    Ok(set(Some(Value::List(values))))
                })),
            ));
        } else {
            node.shape(rows, shape, all, None);
        }
    }
}

fn item_setter(set: &Setter, values: &Rc<Vec<Value>>, index: usize) -> Setter {
    let set = set.clone();
    let values = values.clone();
    Rc::new(move |value| {
        let mut list = values.as_ref().clone();
        if let Some(value) = value {
            list[index] = value;
        } else {
            list.remove(index);
        }
        set(Some(Value::List(list)))
    })
}

fn member<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    match value {
        Some(Value::Record(fields)) => fields.get(key),
        _ => None,
    }
}

fn items(value: Option<&Value>) -> &[Value] {
    match value {
        Some(Value::List(values)) => values,
        _ => &[],
    }
}

fn frame_value<'a>(shape: &Shape, value: Option<&'a Value>, index: usize) -> Option<&'a Value> {
    value
        .filter(|value| fits(shape, value))
        .or_else(|| items(value).get(index).filter(|value| fits(shape, value)))
}

fn uniform<'a>(values: &[Option<&'a Value>]) -> Option<&'a Value> {
    let first = values.first().copied().flatten()?;
    values
        .iter()
        .all(|value| *value == Some(first))
        .then_some(first)
}

fn uniform_frame<'a>(value: Option<&'a Value>, frame: &str) -> Option<&'a Value> {
    uniform(&["low", "normal", "high"].map(|level| member(value, &format!("{frame}.{level}"))))
}

fn fits(shape: &Shape, value: &Value) -> bool {
    match (shape, value) {
        (Shape::Bool, Value::Bool(_))
        | (Shape::Integer { .. } | Shape::Unsigned { .. }, Value::Integer(_))
        | (Shape::Bytes { .. }, Value::List(_))
        | (Shape::Record { .. } | Shape::Modes { .. } | Shape::Ratio, Value::Record(_))
        | (Shape::Optional { .. }, Value::None) => true,
        (Shape::Choice { values }, Value::Choice(value)) => values.contains(value),
        (Shape::List { element, length }, Value::List(values)) => {
            length.is_none_or(|length| values.len() == length as usize)
                && values.iter().all(|value| fits(element, value))
        }
        (Shape::Optional { value: shape }, value) => fits(shape, value),
        _ => false,
    }
}

fn seed(shape: &Shape, existing: Option<&Value>) -> Value {
    match shape {
        Shape::Record { fields } => Value::Record(seed_members(fields, existing)),
        Shape::Modes { modes } => {
            let mode = match member(existing, "mode") {
                Some(Value::Choice(mode)) => modes.get_key_value(mode),
                _ => None,
            };
            mode.or_else(|| modes.first_key_value())
                .map_or(Value::None, |(mode, desc)| {
                    let mut record = seed_members(&desc.fields, existing);
                    record.insert("mode".into(), Value::Choice(mode.clone()));
                    Value::Record(record)
                })
        }
        _ if existing.is_some_and(|value| fits(shape, value)) => {
            existing.cloned().unwrap_or(Value::None)
        }
        Shape::Bool => Value::Bool(false),
        Shape::Integer { min, .. } => Value::Integer(*min),
        Shape::Unsigned { min, .. } => Value::Integer(wire_limit(*min)),
        Shape::Choice { values } => values.first().cloned().map_or(Value::None, Value::Choice),
        Shape::Optional { .. } | Shape::ReadOnly | Shape::PerFrame { .. } => Value::None,
        Shape::Bytes { .. } => Value::List(Vec::new()),
        Shape::List { element, length } => Value::List(
            (0..length.unwrap_or_default())
                .map(|_| seed(element, None))
                .collect(),
        ),
        Shape::Ratio => Value::Record(
            ["numerator", "denominator"]
                .into_iter()
                .map(|key| (key.into(), Value::Integer(1)))
                .collect(),
        ),
    }
}

fn seed_members(
    fields: &BTreeMap<String, OptionMember>,
    existing: Option<&Value>,
) -> BTreeMap<String, Value> {
    fields
        .iter()
        .filter(|(_, desc)| desc.read_only.is_none())
        .filter_map(|(key, desc)| {
            let value = member(existing, key);
            if value.is_none() && optional_member(&desc.shape) {
                None
            } else {
                Some((key.clone(), seed(&desc.shape, value)))
            }
        })
        .collect()
}

fn optional_member(shape: &Shape) -> bool {
    matches!(shape, Shape::Optional { .. })
}

fn member_shape(shape: &Shape, parents: &BTreeMap<String, Value>) -> Shape {
    if let Shape::Unsigned {
        min,
        max: UnsignedLimit::Field { key },
        unit,
    } = shape
        && let Some(Value::Integer(value)) = parents.get(key)
    {
        Shape::Unsigned {
            min: *min,
            max: UnsignedLimit::Fixed {
                value: value.unsigned_abs(),
            },
            unit: unit.clone(),
        }
    } else {
        shape.clone()
    }
}

fn parse(shape: &Shape, text: &str) -> Result<Value, String> {
    match shape {
        Shape::Bool => match text {
            "On" => Ok(Value::Bool(true)),
            "Off" => Ok(Value::Bool(false)),
            _ => Err("Select On or Off.".into()),
        },
        Shape::Choice { values } if values.iter().any(|value| value == text) => {
            Ok(Value::Choice(text.into()))
        }
        Shape::Choice { .. } => Err("Select a choice from the list.".into()),
        Shape::Integer { min, max } => integer(text, *min, *max),
        Shape::Unsigned { min, max, .. } => {
            let max = match max {
                UnsignedLimit::Fixed { value } => wire_limit(*value),
                UnsignedLimit::PerBitDepth { eight, ten } => wire_limit((*eight).max(*ten)),
                UnsignedLimit::Field { .. } => i64::MAX,
            };
            integer(text, wire_limit(*min), max)
        }
        Shape::Bytes { max_length } => hex(text, *max_length as usize),
        Shape::Ratio => {
            let (numerator, denominator) =
                text.split_once('/').ok_or("Enter a ratio such as 1/1.")?;
            Ok(Value::Record(BTreeMap::from([
                (
                    "numerator".into(),
                    integer(numerator, 1, i64::from(u32::MAX))?,
                ),
                (
                    "denominator".into(),
                    integer(denominator, 1, i64::from(u32::MAX))?,
                ),
            ])))
        }
        _ => Err("Use the controls for the components of this value.".into()),
    }
}

fn wire_limit(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn integer(text: &str, min: i64, max: i64) -> Result<Value, String> {
    let value = text
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|value| (min..=max).contains(value));
    value
        .map(Value::Integer)
        .ok_or_else(|| format!("Enter a whole number from {min} through {max}."))
}

fn hex(text: &str, max_length: usize) -> Result<Value, String> {
    let mut digits = text.bytes().filter(|byte| !byte.is_ascii_whitespace());
    let mut values = Vec::new();
    while let Some(high) = digits.next() {
        if values.len() == max_length {
            return Err(format!("Enter at most {max_length} bytes."));
        }
        let low = digits.next().ok_or("Enter hexadecimal digits in pairs.")?;
        let digit = |byte: u8| {
            char::from(byte)
                .to_digit(16)
                .map(i64::from)
                .ok_or("Use hexadecimal digits from 0 to 9 and A to F.")
        };
        values.push(Value::Integer(digit(high)? * 16 + digit(low)?));
    }
    Ok(Value::List(values))
}

fn summary(value: Option<&Value>) -> String {
    match value {
        None => "Unavailable".into(),
        Some(Value::None) => NONE.into(),
        Some(Value::Bool(value)) => if *value { "On" } else { "Off" }.into(),
        Some(Value::Integer(value)) => value.to_string(),
        Some(Value::Choice(value)) => value.clone(),
        Some(Value::List(values)) => values
            .iter()
            .map(|value| summary(Some(value)))
            .collect::<Vec<_>>()
            .join(", "),
        Some(Value::Record(fields)) => match (fields.get("numerator"), fields.get("denominator")) {
            (Some(Value::Integer(num)), Some(Value::Integer(den))) => format!("{num}/{den}"),
            _ => fields
                .iter()
                .map(|(key, value)| format!("{} {}", title(key), summary(Some(value))))
                .collect::<Vec<_>>()
                .join(", "),
        },
    }
}

fn title(key: &str) -> String {
    let mut label = key.replace('_', " ");
    if let Some(first) = label.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    label
}

#[cfg(test)]
#[path = "fields/tests.rs"]
mod tests;
