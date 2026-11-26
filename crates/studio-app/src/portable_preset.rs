// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Portable encoder requests. Import produces a preview before it can change settings.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use studio_domain::options::{
    OPTIONS_SCHEMA_VERSION, OptionDescriptor, OptionRequest, OptionValue as Value, UnsignedLimit,
    ValueShape as Shape,
};
use studio_domain::protocol::{MAX_MESSAGE_BYTES, OptionsIdentity};
use studio_domain::settings::{EncoderSettings, OutputKind};

const VERSION: u32 = 1;
const LIMIT: usize = MAX_MESSAGE_BYTES as usize;
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

/// An import or export failure.
#[derive(Debug)]
pub enum PresetError {
    /// The file cannot be read or safely created.
    File(io::Error),
    /// The data exceeds the portable file limit.
    Size,
    /// The file does not have the expected data format.
    Format(serde_json::Error),
    /// The file format or option identity is incompatible.
    Compatibility(String),
    /// A setting is invalid, unknown or cannot be moved between sources.
    Setting {
        /// The canonical option path.
        key: String,
        /// What the user needs to correct.
        reason: String,
    },
}

impl fmt::Display for PresetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                f.write_str("A file already uses this name. Choose a new name.")
            }
            Self::File(error) => write!(f, "Cannot read or create the preset file. {error}"),
            Self::Size => write!(f, "Use a preset file of at most {LIMIT} bytes."),
            Self::Format(error) => write!(f, "Use a valid portable preset file. {error}"),
            Self::Compatibility(message) => f.write_str(message),
            Self::Setting { key, reason } => write!(f, "{key}. {reason}"),
        }
    }
}

impl std::error::Error for PresetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::File(error) => Some(error),
            Self::Format(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for PresetError {
    fn from(error: io::Error) -> Self {
        Self::File(error)
    }
}

impl From<serde_json::Error> for PresetError {
    fn from(error: serde_json::Error) -> Self {
        Self::Format(error)
    }
}

/// One difference between the current request and an imported preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetChange {
    /// The canonical option key, or one of the base controls.
    pub key: String,
    /// The previous explicit value. Absence means inherit.
    pub before: Option<Value>,
    /// The new explicit value. Absence means inherit.
    pub after: Option<Value>,
}

/// An import candidate. The caller applies it after confirmation and engine validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetPreview {
    /// The request to apply.
    pub request: OptionRequest,
    /// The changes that applying the preset would make.
    pub changes: Vec<PresetChange>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetFile {
    version: u32,
    schema: u32,
    codec_revision: String,
    base: EncoderSettings,
    values: BTreeMap<String, Value>,
}

/// Writes a portable preset to a new file.
///
/// Source-specific overrides stay in the request that supplied them. This operation
/// requires a filesystem that can publish a new hard link in the destination folder.
///
/// # Errors
/// Returns an error when the settings are invalid or the file cannot be created.
/// An existing destination is never replaced.
pub fn export(
    path: &Path,
    identity: &OptionsIdentity,
    request: &OptionRequest,
    descriptors: &[OptionDescriptor],
) -> Result<(), PresetError> {
    check_identity(identity)?;
    let values = request
        .values
        .iter()
        .filter(|(key, value)| !local_only(key, Some(value)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let preset = PresetFile {
        version: VERSION,
        schema: identity.schema,
        codec_revision: identity.codec_revision.clone(),
        base: request.base,
        values,
    };
    validate(&preset, identity, descriptors)?;
    let bytes = serde_json::to_vec(&preset)?;
    if bytes.len() > LIMIT {
        return Err(PresetError::Size);
    }
    publish(path, &bytes)?;
    Ok(())
}

/// Reads a portable preset and calculates its changes without modifying the current request.
///
/// # Errors
/// Returns an error for invalid input, incompatible identity or unsupported option values.
pub fn preview_import(
    path: &Path,
    identity: &OptionsIdentity,
    current: &OptionRequest,
    descriptors: &[OptionDescriptor],
) -> Result<PresetPreview, PresetError> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other("Select a regular file.").into());
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("Select a regular file.").into());
    }
    let mut bytes = Vec::new();
    file.take(u64::from(MAX_MESSAGE_BYTES) + 1)
        .read_to_end(&mut bytes)?;
    preview(&bytes, identity, current, descriptors)
}

fn preview(
    bytes: &[u8],
    identity: &OptionsIdentity,
    current: &OptionRequest,
    descriptors: &[OptionDescriptor],
) -> Result<PresetPreview, PresetError> {
    if bytes.len() > LIMIT {
        return Err(PresetError::Size);
    }
    let json: UniqueJson = serde_json::from_slice(bytes)?;
    let preset: PresetFile = serde_json::from_value(json.0)?;
    validate(&preset, identity, descriptors)?;

    let mut request = OptionRequest {
        base: preset.base,
        values: preset.values,
    };
    for (key, value) in &current.values {
        if local_only(key, Some(value)) && !request.values.contains_key(key) {
            request.values.insert(key.clone(), value.clone());
        }
    }
    Ok(PresetPreview {
        changes: changes(current, &request),
        request,
    })
}

fn changes(before: &OptionRequest, after: &OptionRequest) -> Vec<PresetChange> {
    let output = |kind| {
        Value::Choice(match kind {
            OutputKind::Ivf => "ivf".into(),
            OutputKind::Av2AnnexB => "av2_annex_b".into(),
        })
    };
    let mut changes = Vec::new();
    let mut compare = |key: &str, before: Option<&Value>, after: Option<&Value>| {
        if before != after {
            changes.push(PresetChange {
                key: key.into(),
                before: before.cloned(),
                after: after.cloned(),
            });
        }
    };
    for (key, before, after) in [
        (
            "output",
            output(before.base.output),
            output(after.base.output),
        ),
        (
            "preset",
            Value::Integer(before.base.preset.get().into()),
            Value::Integer(after.base.preset.get().into()),
        ),
        (
            "crf",
            Value::Integer(before.base.crf.get().into()),
            Value::Integer(after.base.crf.get().into()),
        ),
    ] {
        compare(key, Some(&before), Some(&after));
    }
    for key in before
        .values
        .keys()
        .chain(after.values.keys())
        .collect::<BTreeSet<_>>()
    {
        compare(key, before.values.get(key), after.values.get(key));
    }
    changes
}

fn local_only(key: &str, value: Option<&Value>) -> bool {
    matches!(key, "crop" | "first_pass" | "first_pass_output")
        || (key == "tiles"
            && matches!(value, Some(Value::Record(fields)) if fields.get("mode") == Some(&Value::Choice("explicit".into()))))
}

fn check_identity(identity: &OptionsIdentity) -> Result<(), PresetError> {
    if identity.schema != OPTIONS_SCHEMA_VERSION
        || identity.codec_revision.len() != 40
        || !identity
            .codec_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(PresetError::Compatibility(
            "Connect to an engine with a supported option schema and codec revision.".into(),
        ));
    }
    Ok(())
}

fn validate(
    preset: &PresetFile,
    identity: &OptionsIdentity,
    descriptors: &[OptionDescriptor],
) -> Result<(), PresetError> {
    check_identity(identity)?;
    if preset.version != VERSION {
        return Err(PresetError::Compatibility(format!(
            "Use preset file version {VERSION}. This file uses version {}.",
            preset.version
        )));
    }
    if preset.schema != identity.schema || preset.codec_revision != identity.codec_revision {
        return Err(PresetError::Compatibility(format!(
            "Use a preset for schema {} and codec {}.",
            identity.schema, identity.codec_revision
        )));
    }
    for (key, value) in &preset.values {
        if local_only(key, Some(value)) {
            return Err(setting(
                key,
                "Remove this source-specific setting from the portable file.",
            ));
        }
        let descriptor = descriptors
            .iter()
            .find(|desc| desc.key == *key)
            .ok_or_else(|| {
                setting(
                    key,
                    "Remove this unknown option or use the engine that supports it.",
                )
            })?;
        check_value(key, &descriptor.shape, value)?;
    }
    Ok(())
}

fn publish(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let folder = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for _ in 0..32 {
        let index = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let sibling = folder.join(format!(
            ".splot-preset-{}-{index}.partial",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sibling)
        {
            Ok(mut file) => {
                let temporary = Temporary(sibling);
                let written = file.write_all(bytes).and_then(|()| file.sync_all());
                drop(file);
                written?;
                return fs::hard_link(&temporary.0, path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(
        "Choose a folder where Studio can create a temporary file.",
    ))
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn setting(key: &str, reason: impl Into<String>) -> PresetError {
    PresetError::Setting {
        key: key.into(),
        reason: reason.into(),
    }
}

fn check_value(key: &str, shape: &Shape, value: &Value) -> Result<(), PresetError> {
    match (shape, value) {
        (Shape::Bool, Value::Bool(_)) | (Shape::Optional { .. }, Value::None) => Ok(()),
        (Shape::Integer { min, max }, Value::Integer(n)) if (min..=max).contains(&n) => Ok(()),
        (Shape::Unsigned { min, max, .. }, Value::Integer(n)) => {
            let max = match max {
                UnsignedLimit::Fixed { value } => *value,
                UnsignedLimit::PerBitDepth { eight, ten } => (*eight).max(*ten),
                UnsignedLimit::Field { .. } => u64::MAX,
            };
            if u64::try_from(*n).is_ok_and(|n| (*min..=max).contains(&n)) {
                Ok(())
            } else {
                Err(setting(key, "Use a whole number in the supported range."))
            }
        }
        (Shape::Choice { values }, Value::Choice(choice)) if values.contains(choice) => Ok(()),
        (Shape::Optional { value: inner }, value) => check_value(key, inner, value),
        (Shape::Bytes { max_length }, Value::List(values))
            if values.len() <= *max_length as usize
                && values
                    .iter()
                    .all(|value| matches!(value, Value::Integer(n) if (0..=255).contains(n))) =>
        {
            Ok(())
        }
        (Shape::List { element, length }, Value::List(values))
            if length.is_none_or(|length| length as usize == values.len()) =>
        {
            for (index, value) in values.iter().enumerate() {
                check_value(&format!("{key}.{index}"), element, value)?;
            }
            Ok(())
        }
        (Shape::Record { fields }, Value::Record(values)) => {
            check_members(key, fields, values, false)
        }
        (Shape::Modes { modes }, Value::Record(values)) => {
            let mode_key = format!("{key}.mode");
            let Some(Value::Choice(mode)) = values.get("mode") else {
                return Err(setting(&mode_key, "Select a mode."));
            };
            let mode = modes
                .get(mode)
                .ok_or_else(|| setting(&mode_key, "Select a supported mode."))?;
            check_members(key, &mode.fields, values, true)
        }
        (
            Shape::PerFrame {
                value: inner,
                frames,
            },
            value,
        ) => {
            if check_value(key, inner, value).is_ok() {
                return Ok(());
            }
            if let Value::List(values) = value
                && values.len() == frames.len()
            {
                for (frame, value) in frames.iter().zip(values) {
                    check_value(&format!("{key}.{frame}"), inner, value)?;
                }
                return Ok(());
            }
            Err(setting(
                key,
                "Use one supported value or one value for each frame kind.",
            ))
        }
        (Shape::ReadOnly, _) => Err(setting(key, "Remove this read-only option.")),
        _ => Err(setting(
            key,
            "Use a value of the type and range that the engine describes.",
        )),
    }
}

fn check_members(
    key: &str,
    fields: &BTreeMap<String, studio_domain::options::OptionMember>,
    values: &BTreeMap<String, Value>,
    tagged: bool,
) -> Result<(), PresetError> {
    for (name, value) in values {
        if tagged && name == "mode" {
            continue;
        }
        let path = format!("{key}.{name}");
        let field = fields
            .get(name)
            .ok_or_else(|| setting(&path, "Remove this unknown member."))?;
        if field.read_only.is_some() {
            return Err(setting(&path, "Remove this read-only member."));
        }
        check_value(&path, &field.shape, value)?;
        if let Shape::Unsigned {
            max: UnsignedLimit::Field { key: limit },
            ..
        } = &field.shape
            && let (Value::Integer(value), Some(Value::Integer(limit))) = (value, values.get(limit))
            && value > limit
        {
            return Err(setting(&path, format!("Use a value of {limit} or less.")));
        }
    }
    if key != "content" {
        for (name, field) in fields {
            if field.read_only.is_none()
                && !matches!(field.shape, Shape::Optional { .. } | Shape::ReadOnly)
                && !values.contains_key(name)
            {
                return Err(setting(&format!("{key}.{name}"), "Enter a value."));
            }
        }
    }
    Ok(())
}

// Reject duplicates before deriving typed structures. Map deserializers can otherwise keep the last value.
struct UniqueJson(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;

        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueJson;

            fn expecting(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
                writer.write_str("JSON with unique keys and whole numbers")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(serde_json::Value::Null))
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueJson(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(UniqueJson(serde_json::Value::Array(values)))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, UniqueJson(value))) = map.next_entry::<String, UniqueJson>()? {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("Each preset field must occur only once."));
                    }
                }
                Ok(UniqueJson(serde_json::Value::Object(values)))
            }
        }

        deserializer.deserialize_any(UniqueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::options::{OptionMember, OptionMode};
    use studio_domain::settings::{Crf, SpeedPreset};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn identity() -> OptionsIdentity {
        OptionsIdentity {
            schema: OPTIONS_SCHEMA_VERSION,
            codec_revision: "a".repeat(40),
        }
    }

    fn base() -> EncoderSettings {
        EncoderSettings {
            output: OutputKind::Ivf,
            preset: SpeedPreset::new(6).unwrap_or_else(|_| unreachable!()),
            crf: Crf::new(20).unwrap_or_else(|_| unreachable!()),
        }
    }

    fn descriptor(key: &str, shape: Shape) -> OptionDescriptor {
        OptionDescriptor {
            key: key.into(),
            group: String::new(),
            help: String::new(),
            shape,
            expert: false,
        }
    }

    fn member(shape: Shape) -> OptionMember {
        OptionMember {
            shape,
            help: String::new(),
            applicability: String::new(),
            read_only: None,
        }
    }

    fn record<const N: usize>(fields: [(&str, Value); N]) -> Value {
        Value::Record(
            fields
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }

    fn described() -> Vec<OptionDescriptor> {
        let level = Shape::Integer { min: 0, max: 7 };
        let triple = Shape::List {
            length: Some(3),
            element: Box::new(level.clone()),
        };
        vec![
            descriptor("film_grain", Shape::Bool),
            descriptor(
                "tune",
                Shape::Choice {
                    values: vec!["ssim".into(), "vq".into()],
                },
            ),
            descriptor("preset.offset", Shape::Integer { min: -7, max: 7 }),
            descriptor(
                "resize_denominator",
                Shape::Optional {
                    value: Box::new(level),
                },
            ),
            descriptor(
                "key_frame_metadata",
                Shape::Record {
                    fields: BTreeMap::from([(
                        "data".into(),
                        member(Shape::Bytes { max_length: 16 }),
                    )]),
                },
            ),
            descriptor(
                "preset.budget",
                Shape::PerFrame {
                    value: Box::new(triple),
                    frames: ["key", "base", "mid", "leaf"].map(str::to_owned).into(),
                },
            ),
            descriptor(
                "rate_control",
                Shape::Modes {
                    modes: BTreeMap::from([(
                        "cbr".into(),
                        OptionMode {
                            help: String::new(),
                            fields: BTreeMap::from([
                                (
                                    "buffer_ms".into(),
                                    member(Shape::Unsigned {
                                        min: 1,
                                        max: UnsignedLimit::Fixed { value: 1000 },
                                        unit: None,
                                    }),
                                ),
                                (
                                    "initial_ms".into(),
                                    member(Shape::Unsigned {
                                        min: 0,
                                        max: UnsignedLimit::Field {
                                            key: "buffer_ms".into(),
                                        },
                                        unit: None,
                                    }),
                                ),
                            ]),
                        },
                    )]),
                },
            ),
            descriptor("width", Shape::ReadOnly),
        ]
    }

    fn document(values: BTreeMap<String, Value>) -> PresetFile {
        let identity = identity();
        PresetFile {
            version: VERSION,
            schema: identity.schema,
            codec_revision: identity.codec_revision,
            base: base(),
            values,
        }
    }

    fn temporary_folder() -> io::Result<PathBuf> {
        let index = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let folder = std::env::temp_dir().join(format!(
            "splot-portable-test-{}-{index}",
            std::process::id()
        ));
        fs::create_dir(&folder)?;
        Ok(folder)
    }

    struct TestFolder(PathBuf);
    impl Drop for TestFolder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn round_trip_preserves_requested_shapes_and_inheritance() -> TestResult {
        let folder = TestFolder(temporary_folder()?);
        let path = folder.0.join("studio.splot-preset");
        let mut request = OptionRequest::from(base());
        let nums =
            |values: &[i64]| Value::List(values.iter().copied().map(Value::Integer).collect());
        request.values = BTreeMap::from([
            ("film_grain".into(), Value::Bool(false)),
            ("preset.offset".into(), Value::Integer(0)),
            ("resize_denominator".into(), Value::None),
            (
                "key_frame_metadata".into(),
                record([("data", nums(&[0, 255]))]),
            ),
            (
                "preset.budget".into(),
                Value::List(vec![
                    nums(&[1, 2, 3]),
                    nums(&[2, 3, 4]),
                    nums(&[2, 2, 3]),
                    nums(&[3, 4, 5]),
                ]),
            ),
            (
                "rate_control".into(),
                record([
                    ("mode", Value::Choice("cbr".into())),
                    ("buffer_ms", Value::Integer(500)),
                    ("initial_ms", Value::Integer(100)),
                ]),
            ),
            ("crop".into(), record([("left", Value::Integer(17))])),
            ("first_pass".into(), nums(&[123, 456])),
            (
                "tiles".into(),
                record([
                    ("mode", Value::Choice("explicit".into())),
                    ("widths", nums(&[3, 4])),
                ]),
            ),
        ]);
        let descriptors = described();
        export(&path, &identity(), &request, &descriptors)?;
        let saved: PresetFile = serde_json::from_slice(&fs::read(&path)?)?;
        assert!(!saved.values.contains_key("crop"));
        assert!(!saved.values.contains_key("first_pass"));
        assert!(!saved.values.contains_key("tiles"));
        assert!(!saved.values.contains_key("tune"));

        let mut current = OptionRequest::from(base());
        current.values.insert("crop".into(), Value::Integer(4));
        current
            .values
            .insert("tune".into(), Value::Choice("vq".into()));
        let result = preview_import(&path, &identity(), &current, &descriptors)?;
        assert_eq!(result.request.values.get("crop"), Some(&Value::Integer(4)));
        for (key, value) in &saved.values {
            assert_eq!(result.request.values.get(key), Some(value));
        }
        assert!(result.changes.contains(&PresetChange {
            key: "tune".into(),
            before: Some(Value::Choice("vq".into())),
            after: None,
        }));
        assert!(result.changes.contains(&PresetChange {
            key: "resize_denominator".into(),
            before: None,
            after: Some(Value::None),
        }));
        assert_eq!(
            current.values.get("tune"),
            Some(&Value::Choice("vq".into()))
        );
        Ok(())
    }

    #[test]
    fn incompatible_hostile_or_unbounded_imports_fail() -> TestResult {
        let current = OptionRequest::from(base());
        let descriptors = described();
        let decode = |bytes: &[u8]| preview(bytes, &identity(), &current, &descriptors);
        assert!(matches!(
            decode(&vec![b' '; LIMIT + 1]),
            Err(PresetError::Size)
        ));
        let mut file = document(BTreeMap::new());
        file.version += 1;
        assert!(matches!(
            decode(&serde_json::to_vec(&file)?),
            Err(PresetError::Compatibility(_))
        ));
        file.version = VERSION;
        file.schema += 1;
        assert!(matches!(
            decode(&serde_json::to_vec(&file)?),
            Err(PresetError::Compatibility(_))
        ));
        file.schema = OPTIONS_SCHEMA_VERSION;
        file.codec_revision = "b".repeat(40);
        assert!(matches!(
            decode(&serde_json::to_vec(&file)?),
            Err(PresetError::Compatibility(_))
        ));

        let json = serde_json::to_string(&document(BTreeMap::new()))?;
        let duplicate = json.replace(
            r#""values":{}"#,
            r#""values":{"key_frame_metadata":{"data":[1],"data":[2]}}"#,
        );
        let extra = json.replace(
            r#""values":{}"#,
            r#""values":{},"source":"/tmp/private.y4m""#,
        );
        let non_integer = json.replace(r#""values":{}"#, r#""values":{"preset.offset":0.0}"#);
        for hostile in [duplicate, extra, non_integer] {
            assert!(
                matches!(decode(hostile.as_bytes()), Err(PresetError::Format(_))),
                "{hostile}"
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_unknown_read_only_local_and_wrong_shapes() -> TestResult {
        let current = OptionRequest::from(base());
        let descriptors = described();
        for (key, value) in [
            ("unknown", Value::Bool(true)),
            ("width", Value::Integer(64)),
            ("crop", Value::None),
            ("first_pass", Value::List(vec![Value::Integer(1)])),
            ("film_grain", Value::Integer(0)),
            ("tune", Value::Choice("unknown".into())),
            ("preset.offset", Value::Integer(-8)),
            ("preset.budget", Value::List(vec![Value::Integer(1)])),
            (
                "key_frame_metadata",
                record([("data", Value::List(vec![Value::Integer(256)]))]),
            ),
            (
                "rate_control",
                record([
                    ("mode", Value::Choice("cbr".into())),
                    ("buffer_ms", Value::Integer(2)),
                    ("initial_ms", Value::Integer(3)),
                ]),
            ),
            (
                "rate_control",
                record([
                    ("mode", Value::Choice("cbr".into())),
                    ("extra", Value::Integer(1)),
                ]),
            ),
        ] {
            let file = document(BTreeMap::from([(key.into(), value)]));
            let result = preview(
                &serde_json::to_vec(&file)?,
                &identity(),
                &current,
                &descriptors,
            );
            assert!(matches!(result, Err(PresetError::Setting { .. })), "{key}");
        }
        Ok(())
    }

    #[test]
    fn export_is_no_clobber_and_cleans_its_temporary_file() -> TestResult {
        let folder = TestFolder(temporary_folder()?);
        let target = folder.0.join("preset.json");
        fs::write(&target, b"keep this file")?;
        let request = OptionRequest::from(base());
        let result = export(&target, &identity(), &request, &[]);
        assert!(
            matches!(result, Err(PresetError::File(error)) if error.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(&target)?, b"keep this file");
        assert_eq!(fs::read_dir(&folder.0)?.count(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn preset_io_refuses_symbolic_links() -> TestResult {
        let folder = TestFolder(temporary_folder()?);
        let target = folder.0.join("target");
        let link = folder.0.join("linked.json");
        fs::write(&target, b"keep this file")?;
        std::os::unix::fs::symlink(&target, &link)?;
        let request = OptionRequest::from(base());
        assert!(preview_import(&link, &identity(), &request, &[]).is_err());
        assert!(export(&link, &identity(), &request, &[]).is_err());
        assert_eq!(fs::read(&target)?, b"keep this file");
        assert!(fs::symlink_metadata(&link)?.is_symlink());
        assert_eq!(fs::read_dir(&folder.0)?.count(), 2);
        Ok(())
    }
}
