// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Presentation of the option catalog and typed edits to the current draft.

pub(crate) mod catalog;
pub(crate) mod fields;

use studio_domain::options::{EffectiveOption, OptionDescriptor, OptionRequest, OptionValue};

pub(crate) struct OptionRow {
    pub key: String,
    pub title: String,
    pub group: String,
    pub modified: bool,
}

pub(crate) struct Detail {
    pub title: String,
    pub help: String,
    pub origin: String,
    pub effective: String,
    pub screen: String,
    pub fields: Vec<fields::FieldRow>,
}

pub(crate) enum Reset<'a> {
    Field(&'a str),
    Group(&'a str),
    All,
}

pub(crate) fn options(
    catalog: &[OptionDescriptor],
    request: &OptionRequest,
    query: &str,
    modified_only: bool,
    expert: bool,
) -> Vec<OptionRow> {
    let query = query.trim();
    let mut rows: Vec<_> = catalog
        .iter()
        .filter_map(|field| {
            let modified = request.values.contains_key(&field.key);
            if (modified_only && !modified)
                || (!expert && query.is_empty() && !modified && catalog::expert(field))
                || !catalog::matches(field, query)
            {
                return None;
            }
            Some(OptionRow {
                key: field.key.clone(),
                title: catalog::title(field),
                group: catalog::group(field).into(),
                modified,
            })
        })
        .collect();
    rows.sort_by_cached_key(|row| {
        (
            catalog::GROUPS
                .iter()
                .position(|name| *name == row.group)
                .unwrap_or(usize::MAX),
            row.title.clone(),
        )
    });
    rows
}

pub(crate) fn inspect(
    descriptor: &OptionDescriptor,
    request: &OptionRequest,
    policy: &[EffectiveOption],
) -> Detail {
    let effective = policy.iter().find(|row| row.key == descriptor.key);
    let requested = request.values.get(&descriptor.key);
    Detail {
        title: catalog::title(descriptor),
        help: catalog::help(descriptor),
        origin: effective.map_or_else(String::new, |r| display(&r.origin)),
        effective: effective.map_or_else(String::new, |r| display(&r.value)),
        screen: effective
            .filter(|row| {
                row.screen != Some(row.value.clone())
                    || row.screen_origin != Some(row.origin.clone())
            })
            .and_then(|row| {
                row.screen.as_ref().map(|screen| {
                    let value = display(screen);
                    row.screen_origin.as_ref().map_or(value.clone(), |origin| {
                        format!("{value} ({})", display(origin))
                    })
                })
            })
            .unwrap_or_default(),
        fields: fields::rows(descriptor, requested, effective),
    }
}

pub(crate) fn edit(
    descriptor: &OptionDescriptor,
    request: &OptionRequest,
    policy: &[EffectiveOption],
    id: &str,
    text: &str,
) -> Result<OptionRequest, String> {
    let value = fields::edit(
        descriptor,
        request.values.get(&descriptor.key),
        policy.iter().find(|row| row.key == descriptor.key),
        id,
        text,
    )?;
    let mut next = request.clone();
    if let Some(value) = value {
        next.values.insert(descriptor.key.clone(), value);
    } else {
        next.values.remove(&descriptor.key);
    }
    Ok(next)
}

pub(crate) fn reset(
    request: &OptionRequest,
    catalog: &[OptionDescriptor],
    scope: Reset<'_>,
) -> OptionRequest {
    let mut next = request.clone();
    match scope {
        Reset::Field(key) => {
            next.values.remove(key);
        }
        Reset::Group(group) => {
            for descriptor in catalog.iter().filter(|d| catalog::group(d) == group) {
                next.values.remove(&descriptor.key);
            }
        }
        Reset::All => next.values.clear(),
    }
    next
}

fn display(value: &OptionValue) -> String {
    match value {
        OptionValue::None => "None".into(),
        OptionValue::Bool(true) => "On".into(),
        OptionValue::Bool(false) => "Off".into(),
        OptionValue::Integer(n) => n.to_string(),
        OptionValue::Choice(name) => name.replace('_', " "),
        OptionValue::List(items) => {
            let mut text = items
                .iter()
                .take(16)
                .map(display)
                .collect::<Vec<_>>()
                .join(", ");
            if items.len() > 16 {
                use std::fmt::Write as _;
                let _ = write!(text, " … ({} values)", items.len());
            }
            if text.is_empty() {
                "Empty list".into()
            } else {
                text
            }
        }
        OptionValue::Record(members) => {
            if let Some((key, first)) = members.first_key_value()
                && key.contains('.')
                && members.values().all(|v| v == first)
            {
                return format!("{} (all contexts)", display(first));
            }
            members
                .iter()
                .map(|(key, value)| format!("{} = {}", key.replace('_', " "), display(value)))
                .collect::<Vec<_>>()
                .join(" · ")
        }
    }
}

pub(crate) fn differences(before: &[EffectiveOption], after: &[EffectiveOption]) -> String {
    let mut lines = Vec::new();
    for next in after {
        if let Some(prior) = before.iter().find(|row| row.key == next.key) {
            if prior.value != next.value {
                lines.push(format!(
                    "{}   {} → {}",
                    next.key,
                    display(&prior.value),
                    display(&next.value)
                ));
            } else if prior.origin != next.origin {
                lines.push(format!(
                    "{}   origin {} → {}",
                    next.key,
                    display(&prior.origin),
                    display(&next.origin)
                ));
            }
            if prior.screen != next.screen || prior.screen_origin != next.screen_origin {
                lines.push(format!(
                    "{}   screen content {} → {}",
                    next.key,
                    prior
                        .screen
                        .as_ref()
                        .map_or_else(|| display(&prior.value), display),
                    next.screen
                        .as_ref()
                        .map_or_else(|| display(&next.value), display)
                ));
            }
        }
    }
    if lines.is_empty() {
        "Effective settings stay the same.".into()
    } else {
        lines.join("\n")
    }
}
