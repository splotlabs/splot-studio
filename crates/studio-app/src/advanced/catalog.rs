// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Presentation of the engine's Advanced option descriptions.

use std::fmt::Write as _;

use studio_domain::options::{OptionDescriptor, ValueShape};

const QUALITY: &str = "Quality and rate control";
const SEARCH: &str = "Speed and search";
const TEMPORAL: &str = "Temporal structure";
const PREDICTION: &str = "Prediction";
const TRANSFORMS: &str = "Transforms and quantization";
const FILTERS: &str = "Filters and grain";
const PARALLEL: &str = "Parallelism";
const OUTPUT: &str = "Output";
const SOURCE: &str = "Source";
const OTHER: &str = "Other options";

/// Section order in the Advanced editor.
pub(crate) const GROUPS: &[&str] = &[
    QUALITY, SEARCH, TEMPORAL, PREDICTION, TRANSFORMS, FILTERS, PARALLEL, OUTPUT, SOURCE, OTHER,
];

/// The location and visibility of an option in Advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Classification {
    group: &'static str,
    title: Option<&'static str>,
    expert: bool,
}

impl Classification {
    const fn named(group: &'static str, title: &'static str, expert: bool) -> Self {
        Self {
            group,
            title: Some(title),
            expert,
        }
    }
}

/// Classifies a known key or preset group. New top-level keys need an explicit entry.
pub(crate) fn classification(descriptor: &OptionDescriptor) -> Option<Classification> {
    use Classification as Class;

    if descriptor.key.starts_with("preset.") {
        let group = match descriptor.group.as_str() {
            "partition" | "mode_decision" => SEARCH,
            "intra" | "inter" | "screen_content" => PREDICTION,
            "transform" | "lossless" => TRANSFORMS,
            "filters" => FILTERS,
            "gop" => TEMPORAL,
            "threading" => PARALLEL,
            _ => return None,
        };
        return Some(Classification {
            group,
            title: None,
            expert: true,
        });
    }
    Some(match descriptor.key.as_str() {
        "rate_control" => Class::named(QUALITY, "Rate control", false),
        "cbr_padding" => Class::named(QUALITY, "Constant-bitrate padding", false),
        "first_pass" => Class::named(QUALITY, "First-pass statistics", false),
        "tune" => Class::named(QUALITY, "Quality metric", false),
        "preset" => Class::named(SEARCH, "Speed preset", false),
        "preset_overrides" => Class::named(SEARCH, "Preset overrides", true),
        "fast_decode" => Class::named(SEARCH, "Fast decoding", false),
        "keyint" => Class::named(TEMPORAL, "Keyframe interval", false),
        "film_grain" => Class::named(FILTERS, "Film grain synthesis", false),
        "threads" => Class::named(PARALLEL, "Worker threads", false),
        "tiles" => Class::named(PARALLEL, "Tile layout", false),
        "tile_groups" => Class::named(OUTPUT, "Tile groups", true),
        "crop" => Class::named(OUTPUT, "Crop window", false),
        "resize_denominator" => Class::named(OUTPUT, "Output resize", false),
        "multi_frame_header" => Class::named(OUTPUT, "Multi-frame header", true),
        "key_frame_metadata" => Class::named(OUTPUT, "Keyframe metadata payloads", true),
        "width" => Class::named(SOURCE, "Source width", false),
        "height" => Class::named(SOURCE, "Source height", false),
        "bit_depth" => Class::named(SOURCE, "Source bit depth", false),
        "chroma" => Class::named(SOURCE, "Chroma subsampling", false),
        "frame_rate" => Class::named(SOURCE, "Source frame rate", false),
        "content" => Class::named(SOURCE, "Source description and timing", false),
        _ => return None,
    })
}

/// The section shown in Advanced.
pub(crate) fn group(descriptor: &OptionDescriptor) -> &'static str {
    classification(descriptor).map_or(OTHER, |class| class.group)
}

/// Whether Advanced hides this control until the user opens Expert.
pub(crate) fn expert(descriptor: &OptionDescriptor) -> bool {
    classification(descriptor).is_none_or(|class| class.expert)
}

/// The readable option name. The search also accepts the canonical key.
pub(crate) fn title(descriptor: &OptionDescriptor) -> String {
    match classification(descriptor) {
        Some(Classification {
            title: Some(title), ..
        }) => title.to_owned(),
        Some(_) => preset_title(descriptor.key.trim_start_matches("preset.")),
        None => descriptor.key.clone(),
    }
}

fn preset_title(key: &str) -> String {
    match key {
        "rdoq" => return "Rate-distortion optimized quantization".to_owned(),
        "psy_rd" => return "Psychovisual rate-distortion tuning".to_owned(),
        "ssim_aq" => return "SSIM adaptive quantization".to_owned(),
        "screen_detect" => return "Screen content detection".to_owned(),
        _ => {}
    }
    let mut title = String::new();
    for word in key.split('_') {
        if !title.is_empty() {
            title.push(' ');
        }
        match word {
            "cdf" | "cdef" | "ccso" | "gdf" | "gop" | "ibp" | "mrl" | "obmc" | "rdo" | "scc"
            | "sb" | "tip" | "tx" => title.push_str(&word.to_ascii_uppercase()),
            _ if title.is_empty() => {
                let mut chars = word.chars();
                if let Some(first) = chars.next() {
                    title.extend(first.to_uppercase());
                    title.push_str(chars.as_str());
                }
            }
            _ => title.push_str(word),
        }
    }
    title
}

/// Finds an option by canonical key or by its readable name.
pub(crate) fn matches(descriptor: &OptionDescriptor, query: &str) -> bool {
    let key = descriptor.key.to_ascii_lowercase();
    let title = title(descriptor).to_lowercase();
    query
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || matches!(c, '_' | '-' | '.'))
        .filter(|part| !part.is_empty())
        .all(|part| key.contains(part) || title.contains(part))
}

/// Combines the engine description and units with short guidance for common settings.
pub(crate) fn help(descriptor: &OptionDescriptor) -> String {
    let mut help = descriptor.help.clone();
    if let Some(unit) = unit(&descriptor.shape) {
        let _ = write!(help, " Enter the value in {unit}.");
    }
    let tradeoff = match descriptor.key.as_str() {
        "rate_control" => "Quality modes target image quality. Bitrate modes target a data rate.",
        "keyint" => {
            "A shorter interval can make seeking easier. A longer interval can improve compression."
        }
        "tiles" => "More tiles can improve parallelism. They can also reduce compression.",
        "resize_denominator" => {
            "A higher denominator reduces both dimensions. The output keeps the resized dimensions."
        }
        "fast_decode" => "This setting limits some coding tools to reduce work for the decoder.",
        "film_grain" => "The decoder applies the grain model to the output frames.",
        _ => return help,
    };
    if !help.is_empty() {
        help.push(' ');
    }
    help.push_str(tradeoff);
    help
}

fn unit(shape: &ValueShape) -> Option<&str> {
    match shape {
        ValueShape::Unsigned { unit, .. } => unit.as_deref(),
        ValueShape::Optional { value } | ValueShape::PerFrame { value, .. } => unit(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(key: &str, group: &str, shape: ValueShape) -> OptionDescriptor {
        OptionDescriptor {
            key: key.to_owned(),
            group: group.to_owned(),
            help: "Set an encoder option.".to_owned(),
            shape,
            expert: true,
        }
    }

    #[test]
    fn common_settings_and_research_controls_have_distinct_placement() {
        let rate = descriptor("rate_control", "Encoder", ValueShape::Bool);
        assert_eq!(group(&rate), QUALITY);
        assert!(!expert(&rate));

        let research = descriptor("preset.rdoq", "transform", ValueShape::Bool);
        assert_eq!(group(&research), TRANSFORMS);
        assert!(expert(&research));

        let depth = descriptor("bit_depth", "Source", ValueShape::ReadOnly);
        assert_eq!(group(&depth), SOURCE);
        assert!(!expert(&depth));

        let payload = descriptor("key_frame_metadata", "Encoder", ValueShape::ReadOnly);
        assert_eq!(group(&payload), OUTPUT);
        assert!(expert(&payload));
    }

    #[test]
    fn search_finds_friendly_names_and_canonical_keys() {
        let option = descriptor("keyint", "Encoder", ValueShape::Bool);
        assert!(matches(&option, "  KEYFRAME interval  "));
        assert!(matches(&option, "keyint"));
        assert!(!matches(&option, "keyframe threads"));

        let research = descriptor("preset.rdoq", "transform", ValueShape::Bool);
        assert!(matches(&research, "preset.rdoq"));
        assert!(matches(&research, "rate distortion quantization"));
        assert!(matches(&research, " "));
    }

    #[test]
    fn a_new_key_or_preset_group_requires_classification() {
        let unknown = descriptor("new_option", "Source", ValueShape::ReadOnly);
        assert_eq!(classification(&unknown), None);
        assert_eq!(group(&unknown), OTHER);
        assert!(expert(&unknown));
        assert_eq!(title(&unknown), "new_option");
        assert_eq!(
            classification(&descriptor("preset.knob", "new_group", ValueShape::Bool)),
            None
        );
    }

    #[test]
    fn help_uses_the_engine_unit_and_explains_the_tradeoff() {
        use studio_domain::options::UnsignedLimit;

        let option = descriptor(
            "keyint",
            "Encoder",
            ValueShape::Unsigned {
                min: 1,
                max: UnsignedLimit::Fixed { value: 100 },
                unit: Some("frames".to_owned()),
            },
        );
        let help = help(&option);
        assert!(help.contains(&option.help));
        assert!(help.contains("in frames"));
        assert!(help.contains("seeking"));
    }
}
