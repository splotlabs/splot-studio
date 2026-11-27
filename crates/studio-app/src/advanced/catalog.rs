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

    if let Some(key) = descriptor.key.strip_prefix("preset.") {
        let codec_group = preset_group(key)?;
        if codec_group != descriptor.group {
            return None;
        }
        let group = match codec_group {
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
        "disable_loopfilters_across_tiles" => {
            Class::named(FILTERS, "In-loop filters at tile edges", true)
        }
        "threads" => Class::named(PARALLEL, "Worker threads", false),
        "tiles" => Class::named(PARALLEL, "Tile layout", false),
        "tile_groups" => Class::named(OUTPUT, "Tile groups", true),
        "crop" => Class::named(OUTPUT, "Crop window", false),
        "resize_denominator" => Class::named(OUTPUT, "Output resize", false),
        "multi_frame_header" => Class::named(OUTPUT, "Multi-frame header", true),
        "profile" => Class::named(OUTPUT, "Sequence profile", true),
        "key_frame_metadata" => Class::named(OUTPUT, "Keyframe metadata payloads", true),
        "frame_metadata" => Class::named(OUTPUT, "Per-frame metadata", true),
        "width" => Class::named(SOURCE, "Source width", false),
        "height" => Class::named(SOURCE, "Source height", false),
        "bit_depth" => Class::named(SOURCE, "Source bit depth", false),
        "chroma" => Class::named(SOURCE, "Chroma subsampling", false),
        "frame_rate" => Class::named(SOURCE, "Source frame rate", false),
        "content" => Class::named(SOURCE, "Source description and timing", false),
        _ => return None,
    })
}

// A new key in a known codec group needs a decision.
#[rustfmt::skip]
const PRESET_GROUPS: &[(&str, &[&str])] = &[
    ("partition", &[
        "breakout_area", "ext_four_way_margin", "ext_partitions", "ext_three_way_margin",
        "extended_sdp", "leaf_reuse", "max_block_log2", "min_block_log2", "none_breakout",
        "partition_early_exit", "pd0_leaf_bits", "pd_window", "ref_intra_margin",
        "ref_intra_refresh", "sb256_min_height", "sdp", "sdp_chroma_search",
    ]),
    ("intra", &[
        "cfl_filter", "cfl_zero_alpha", "dip", "dip_trials", "intra_modes", "mhccp", "mrl_trials",
    ]),
    ("transform", &[
        "cctx", "cctx_candidates", "full_trials", "inter_ddt", "inter_ist", "inter_ist_rd",
        "inter_tx_split", "intra_ist_rd", "ist_sets", "parity_hiding", "rdoq", "rdoq_every_type",
        "tcq", "tcq_search_trellis", "tcq_type_trellis", "tx_partition_search", "tx_type_depth",
        "tx_type_model_keep",
    ]),
    ("inter", &[
        "bawp", "bawp_search", "cdf_avg_across_tiles", "cdf_carry", "cdf_update_tile",
        "compound_amvd", "compound_candidates", "compound_local_warp", "compound_refs",
        "compound_skip_sad", "compound_types", "disable_cdf_update", "drl_bits",
        "drl_reorder_always", "eighth_pel_max_qindex", "explicit_refs", "global_motion",
        "global_motion_predict", "hme_range", "implicit_mask", "implicit_output", "interintra",
        "interintra_search", "interp_filter_search", "intra_only", "jmvd_scales", "large_inter",
        "long_term_id_bits", "max_tlayer_id", "me_range", "me_seeds", "motion_range",
        "mv_prediction", "mv_traj", "mvd_tools", "newmv_ref_margin", "num_ref_frames",
        "open_loop_key", "opfl",
        "opfl_refine_all", "primary_default_cdfs", "primary_ref_choose", "quant_matrices",
        "refinemv", "same_ref_modes", "sef_repeat", "seq_tile_info", "short_refresh",
        "subpel_depth", "switch_frames", "switch_type", "tip", "tip_amvd", "tip_explicit_qp",
        "tip_hole_fill", "tip_newmv",
        "tip_output", "tip_output_search", "tip_refinemv", "warp", "warp_interintra",
    ]),
    ("filters", &[
        "ccso_bo_band_log2", "ccso_candidates", "ccso_reuse", "cdef_damping_search",
        "cdef_on_skip", "cdef_pairs", "cdef_strengths", "deblock_luma_split",
        "deblock_offset_bits", "df_sub_pu", "gdf_candidates", "loop_filter_search",
        "lr_chroma_unit_sizes", "lr_unit_sizes", "wiener_ns", "wiener_ns_chroma",
        "wiener_ns_classes",
    ]),
    ("screen_content", &[
        "fsc", "idtx_intra", "integer_mv", "intrabc", "intrabc_drl", "intrabc_inter",
        "intrabc_local", "intrabc_morph", "intrabc_small", "intrabc_subpel", "palette",
        "screen_detect",
    ]),
    ("lossless", &[
        "lossless_dpcm", "lossless_inter_tx",
    ]),
    ("gop", &[
        "bitrate_mini_gop_size", "delta_q_res", "key_qindex_boost", "lookahead_frames",
        "mini_gop_size", "segmentation", "ssim_aq", "tf_frames", "tf_strength", "tpl_delta_q",
        "u_delta_q", "v_delta_q", "y_dc_delta_q",
    ]),
    ("mode_decision", &[
        "light_leaf", "mds0_budgets", "psy_rd",
    ]),
    ("threading", &[
        "wavefront",
    ]),
];

fn preset_group(key: &str) -> Option<&'static str> {
    PRESET_GROUPS
        .iter()
        .find_map(|(group, keys)| keys.contains(&key).then_some(*group))
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

        for key in ["key_frame_metadata", "frame_metadata"] {
            let payload = descriptor(key, "Encoder", ValueShape::ReadOnly);
            assert_eq!(group(&payload), OUTPUT, "{key}");
            assert!(expert(&payload), "{key}");
        }
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
        assert_eq!(
            classification(&descriptor(
                "preset.new_knob",
                "transform",
                ValueShape::Bool
            )),
            None
        );
        assert_eq!(
            classification(&descriptor("preset.rdoq", "partition", ValueShape::Bool)),
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
