// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroUsize};

use splot_encode::config::options::{OPTIONS, OptionClass, OptionDescription, ValueShape};
use splot_encode::y4m::Y4mHeader;
use splot_encode::{
    BitDepth, ChromaSamplePosition, Config, ContentDescription, CroppingWindow, Error,
    FrameMetadata, KeyFrameMetadata, PixelFormat, Preset, PresetError, ProfileIdc, RateControlMode,
    ThreadCount, TileLayout, Tune,
};
use studio_domain::job::{ChromaLayout, ChromaSiting, SourceFacts};
use studio_domain::options::{FieldError, OptionValue};
use studio_domain::protocol::MAX_MESSAGE_BYTES;
use studio_domain::settings::EncoderSettings;

// Each byte can use four JSON characters. The reply can include two copies
// of the record, so reserve half the message for its other values and framing.
pub(super) const MAX_METADATA_BYTES: u32 = MAX_MESSAGE_BYTES / 16;

pub(super) fn from_source(
    source: &SourceFacts,
    base: EncoderSettings,
) -> Result<Config, FieldError> {
    let mut config = base_config(source.width, source.height, source.bit_depth, base)?;
    config.chroma = match source.chroma {
        ChromaLayout::Monochrome => PixelFormat::Monochrome,
        ChromaLayout::Yuv420 => PixelFormat::Yuv420,
        ChromaLayout::Yuv422 => PixelFormat::Yuv422,
        ChromaLayout::Yuv444 => PixelFormat::Yuv444,
    };
    config.frame_rate = (source.frame_rate.num, source.frame_rate.den);
    let color = source.color;
    config.content.color_primaries = color.primaries.unwrap_or(config.content.color_primaries);
    config.content.transfer_characteristics = color
        .transfer
        .unwrap_or(config.content.transfer_characteristics);
    config.content.matrix_coefficients = color.matrix.unwrap_or(config.content.matrix_coefficients);
    config.content.full_range = color.full_range.unwrap_or(config.content.full_range);
    config.content.chroma_sample_position = color.chroma_siting.map(chroma_position);
    config.content.sample_aspect_ratio = source.pixel_aspect.map(|aspect| (aspect.num, aspect.den));
    Ok(config)
}

const fn chroma_position(siting: ChromaSiting) -> ChromaSamplePosition {
    match siting {
        ChromaSiting::Left => ChromaSamplePosition::Left,
        ChromaSiting::Center => ChromaSamplePosition::Center,
        ChromaSiting::TopLeft => ChromaSamplePosition::TopLeft,
        ChromaSiting::Top => ChromaSamplePosition::Top,
        ChromaSiting::BottomLeft => ChromaSamplePosition::BottomLeft,
        ChromaSiting::Bottom => ChromaSamplePosition::Bottom,
    }
}

pub(crate) fn from_header(
    header: &Y4mHeader,
    content: ContentDescription,
    base: EncoderSettings,
) -> Result<Config, FieldError> {
    let mut config = base_config(header.width, header.height, header.bit_depth, base)?;
    config.chroma = header.format;
    config.frame_rate = header.frame_rate;
    config.content = content;
    Ok(config)
}

fn base_config(
    width: u32,
    height: u32,
    bit_depth: u8,
    base: EncoderSettings,
) -> Result<Config, FieldError> {
    let mut config = Config::new(width, height);
    config.bit_depth = match bit_depth {
        8 => BitDepth::Eight,
        10 => BitDepth::Ten,
        bits => {
            return Err(FieldError::new(
                "bit_depth",
                format!(
                    "The encoder supports 8-bit and 10-bit samples. The source uses {bits} bits."
                ),
            ));
        }
    };
    config.rate_control = RateControlMode::Crf {
        crf: base.crf.get(),
    };
    config.preset = Preset::new(base.preset.get()).map_err(|cause| error(&config, &cause))?;
    Ok(config)
}

pub(super) fn apply(config: &mut Config, key: &str, value: &OptionValue) -> Result<(), FieldError> {
    match key {
        "rate_control" => config.rate_control = rate_control(&Record::new(key, value)?)?,
        "tiles" => config.tiles = tiles(&Record::new(key, value)?)?,
        "tile_groups" => config.tile_groups = nonzero(key, NonZeroU32::new(integer(key, value)?))?,
        "keyint" => config.keyint = nonzero(key, NonZeroU32::new(integer(key, value)?))?,
        "resize_denominator" => {
            config.resize_denominator = match value {
                OptionValue::None => None,
                _ => Some(integer(key, value)?),
            };
        }
        "crop" => {
            config.crop = match value {
                OptionValue::None => None,
                _ => Some(crop(&Record::new(key, value)?)?),
            };
        }
        "threads" => config.threads = threads(&Record::new(key, value)?)?,
        "content" => {
            let record = Record::new(key, value)?;
            record.check_record()?;
            if let Some(value) = record.values.get("timing_info") {
                config.content.timing_info = boolean("content.timing_info", value)?;
            }
        }
        "key_frame_metadata" => {
            config.key_frame_metadata = metadata(&Record::new(key, value)?)?;
        }
        "frame_metadata" => config.frame_metadata = frame_metadata(&Record::new(key, value)?)?,
        "film_grain" => config.film_grain = boolean(key, value)?,
        "cbr_padding" => config.cbr_padding = boolean(key, value)?,
        "fast_decode" => config.fast_decode = boolean(key, value)?,
        "multi_frame_header" => config.multi_frame_header = boolean(key, value)?,
        "disable_loopfilters_across_tiles" => {
            config.disable_loopfilters_across_tiles = boolean(key, value)?;
        }
        "profile" => config.profile = profile(key, value)?,
        "tune" => {
            config.tune = match value {
                OptionValue::Choice(choice) if choice == "psnr" => Tune::Psnr,
                OptionValue::Choice(choice) if choice == "ssim" => Tune::Ssim,
                OptionValue::Choice(choice) if choice == "vq" => Tune::Vq,
                _ => return Err(FieldError::new(key, "Select psnr, ssim, or vq.")),
            };
        }
        "width" | "height" | "bit_depth" | "chroma" | "frame_rate" | "first_pass"
        | "preset_overrides" | "preset" | "output" => {
            return Err(FieldError::new(key, "This option is read-only here."));
        }
        _ => return Err(FieldError::new(key, "The encoder option is unknown.")),
    }
    Ok(())
}

/// Reads a `seq_profile_idc` choice. The codec refuses a reserved profile.
fn profile(key: &str, value: &OptionValue) -> Result<Option<ProfileIdc>, FieldError> {
    let bits = match value {
        OptionValue::None => return Ok(None),
        // The syntax element has five bits.
        OptionValue::Choice(choice) => choice.parse().ok().filter(|bits: &u8| *bits < 32),
        _ => None,
    };
    bits.map(|bits| Some(ProfileIdc::from_bits(bits)))
        .ok_or_else(|| FieldError::new(key, "Select a profile from the list."))
}

fn boolean(key: &str, value: &OptionValue) -> Result<bool, FieldError> {
    match value {
        OptionValue::Bool(value) => Ok(*value),
        _ => Err(FieldError::new(key, "Use true or false.")),
    }
}

fn integer<T: TryFrom<i64>>(key: &str, value: &OptionValue) -> Result<T, FieldError> {
    match value {
        OptionValue::Integer(value) => T::try_from(*value)
            .map_err(|_| FieldError::new(key, "Use an integer in the supported range.")),
        _ => Err(FieldError::new(key, "Use an integer.")),
    }
}

fn nonzero<T>(key: &str, value: Option<T>) -> Result<T, FieldError> {
    value.ok_or_else(|| FieldError::new(key, "Use an integer greater than zero."))
}

struct Record<'a> {
    key: &'a str,
    values: &'a BTreeMap<String, OptionValue>,
}

impl<'a> Record<'a> {
    fn new(key: &'a str, value: &'a OptionValue) -> Result<Self, FieldError> {
        match value {
            OptionValue::Record(values) => Ok(Self { key, values }),
            _ => Err(FieldError::new(key, "Use a record.")),
        }
    }

    fn get(&self, field: &str) -> Result<&'a OptionValue, FieldError> {
        self.values
            .get(field)
            .ok_or_else(|| FieldError::new(self.path(field), "Enter a value."))
    }

    fn path(&self, field: &str) -> String {
        format!("{}.{field}", self.key)
    }

    fn integer<T: TryFrom<i64>>(&self, field: &str) -> Result<T, FieldError> {
        integer(&self.path(field), self.get(field)?)
    }

    fn uints(&self, field: &str) -> Result<Vec<u32>, FieldError> {
        let path = self.path(field);
        let OptionValue::List(values) = self.get(field)? else {
            return Err(FieldError::new(path, "Use a list of integers."));
        };
        values.iter().map(|value| integer(&path, value)).collect()
    }

    fn shape(&self) -> Result<ValueShape, FieldError> {
        OPTIONS
            .iter()
            .find(|description| description.key == self.key)
            .map(|description| match description.shape {
                ValueShape::Optional(shape) => *shape,
                shape => shape,
            })
            .ok_or_else(|| self.unsupported_shape())
    }

    fn unsupported_shape(&self) -> FieldError {
        FieldError::new(self.key, "Studio cannot edit this option shape.")
    }

    fn unsupported_mode(&self) -> FieldError {
        FieldError::new(self.path("mode"), "Select a supported mode.")
    }

    fn mode(&self) -> Result<&'a str, FieldError> {
        let OptionValue::Choice(mode) = self.get("mode")? else {
            return Err(FieldError::new(self.path("mode"), "Select a mode."));
        };
        let ValueShape::Choice(variants) = self.shape()? else {
            return Err(self.unsupported_shape());
        };
        let variant = variants
            .iter()
            .find(|variant| variant.key == mode)
            .ok_or_else(|| self.unsupported_mode())?;
        self.check_fields(variant.fields, true)?;
        Ok(mode)
    }

    fn check_record(&self) -> Result<(), FieldError> {
        let ValueShape::Record(fields) = self.shape()? else {
            return Err(self.unsupported_shape());
        };
        self.check_fields(fields, false)
    }

    fn check_fields(&self, fields: &[OptionDescription], tagged: bool) -> Result<(), FieldError> {
        for key in self.values.keys() {
            if tagged && key == "mode" {
                continue;
            }
            match fields.iter().find(|field| field.key == key) {
                Some(field) if field.class == OptionClass::Setting => {}
                Some(_) => return Err(FieldError::new(self.path(key), "This field is read-only.")),
                None => return Err(FieldError::new(self.path(key), "Remove this field.")),
            }
        }
        Ok(())
    }
}

fn rate_control(record: &Record<'_>) -> Result<RateControlMode, FieldError> {
    let mode = record.mode()?;
    Ok(match mode {
        "constant_qp" => RateControlMode::ConstantQp {
            qp: record.integer("qp")?,
        },
        "crf" => RateControlMode::Crf {
            crf: record.integer("crf")?,
        },
        "capped_crf" => RateControlMode::CappedCrf {
            crf: record.integer("crf")?,
            max_bitrate_kbps: record.integer("max_bitrate_kbps")?,
            buffer_ms: record.integer("buffer_ms")?,
            initial_ms: record.integer("initial_ms")?,
        },
        "vbr" => RateControlMode::Vbr {
            bitrate_kbps: record.integer("bitrate_kbps")?,
        },
        "cbr" => RateControlMode::Cbr {
            bitrate_kbps: record.integer("bitrate_kbps")?,
            buffer_ms: record.integer("buffer_ms")?,
            initial_ms: record.integer("initial_ms")?,
        },
        _ => return Err(record.unsupported_mode()),
    })
}

fn tiles(record: &Record<'_>) -> Result<TileLayout, FieldError> {
    match record.mode()? {
        "uniform" => Ok(TileLayout::Uniform {
            cols_log2: record.integer("cols_log2")?,
            rows_log2: record.integer("rows_log2")?,
        }),
        "explicit" => Ok(TileLayout::Explicit {
            widths: record.uints("widths")?,
            heights: record.uints("heights")?,
        }),
        _ => Err(record.unsupported_mode()),
    }
}

fn threads(record: &Record<'_>) -> Result<ThreadCount, FieldError> {
    match record.mode()? {
        "auto" => Ok(ThreadCount::Auto),
        "fixed" => {
            let count = nonzero("threads.count", NonZeroUsize::new(record.integer("count")?))?;
            Ok(ThreadCount::Fixed(count))
        }
        _ => Err(record.unsupported_mode()),
    }
}

/// Reads the optional byte payloads of one metadata record within one
/// [`MAX_METADATA_BYTES`] budget.
fn payloads<'a>(
    record: &'a Record<'a>,
) -> impl FnMut(&str) -> Result<Option<Vec<u8>>, FieldError> + 'a {
    let mut remaining = MAX_METADATA_BYTES as usize;
    move |field| {
        let path = record.path(field);
        match record.values.get(field) {
            None | Some(OptionValue::None) => Ok(None),
            Some(OptionValue::List(values)) if values.len() <= remaining => {
                remaining -= values.len();
                values
                    .iter()
                    .map(|byte| integer(&path, byte))
                    .collect::<Result<Vec<u8>, _>>()
                    .map(Some)
            }
            Some(OptionValue::List(_)) => Err(FieldError::new(
                path,
                format!("Use at most {MAX_METADATA_BYTES} bytes in total for this metadata."),
            )),
            Some(_) => Err(FieldError::new(path, "Use a list of bytes or null.")),
        }
    }
}

fn metadata(record: &Record<'_>) -> Result<KeyFrameMetadata, FieldError> {
    record.check_record()?;
    let mut bytes = payloads(record);
    let mut metadata = KeyFrameMetadata::default();
    metadata.icc_profile = bytes("icc_profile")?;
    metadata.itut_t35 = bytes("itut_t35")?;
    metadata.user_data_unregistered = bytes("user_data_unregistered")?;
    Ok(metadata)
}

fn frame_metadata(record: &Record<'_>) -> Result<FrameMetadata, FieldError> {
    record.check_record()?;
    // An absent flag keeps the codec default, as an absent payload does.
    let flag = |field| {
        record
            .values
            .get(field)
            .map_or(Ok(false), |value| boolean(&record.path(field), value))
    };
    let mut metadata = FrameMetadata::default();
    metadata.timecode = flag("timecode")?;
    metadata.temporal_point_info = flag("temporal_point_info")?;
    metadata.scan_type = flag("scan_type")?;
    metadata.banding_hints = payloads(record)("banding_hints")?;
    Ok(metadata)
}

fn crop(record: &Record<'_>) -> Result<CroppingWindow, FieldError> {
    record.check_record()?;
    Ok(CroppingWindow {
        left: record.integer("left")?,
        right: record.integer("right")?,
        top: record.integer("top")?,
        bottom: record.integer("bottom")?,
    })
}

pub(super) fn value(config: &Config, key: &str) -> Option<OptionValue> {
    use OptionValue::Bool;

    Some(match key {
        "width" => number(config.width),
        "height" => number(config.height),
        "bit_depth" => choice(match config.bit_depth {
            BitDepth::Eight => "8",
            BitDepth::Ten => "10",
        }),
        "chroma" => choice(config.chroma.as_str()),
        "frame_rate" => ratio(config.frame_rate),
        "content" => content_value(&config.content),
        "key_frame_metadata" => record([
            (
                "icc_profile",
                byte_value(config.key_frame_metadata.icc_profile.as_deref()),
            ),
            (
                "itut_t35",
                byte_value(config.key_frame_metadata.itut_t35.as_deref()),
            ),
            (
                "user_data_unregistered",
                byte_value(config.key_frame_metadata.user_data_unregistered.as_deref()),
            ),
        ]),
        "frame_metadata" => {
            let metadata = &config.frame_metadata;
            record([
                ("timecode", Bool(metadata.timecode)),
                ("temporal_point_info", Bool(metadata.temporal_point_info)),
                ("scan_type", Bool(metadata.scan_type)),
                (
                    "banding_hints",
                    byte_value(metadata.banding_hints.as_deref()),
                ),
            ])
        }
        "preset" => number(config.preset.get()),
        "rate_control" => rate_control_value(config.rate_control)?,
        "tiles" => tile_value(&config.tiles),
        "tile_groups" => number(config.tile_groups.get()),
        "keyint" => number(config.keyint.get()),
        "resize_denominator" => config.resize_denominator.map_or(OptionValue::None, number),
        "crop" => config.crop.map_or(OptionValue::None, |crop| {
            record([
                ("left", number(crop.left)),
                ("right", number(crop.right)),
                ("top", number(crop.top)),
                ("bottom", number(crop.bottom)),
            ])
        }),
        "threads" => match config.threads {
            ThreadCount::Auto => record([("mode", choice("auto"))]),
            ThreadCount::Fixed(count) => record([
                ("mode", choice("fixed")),
                ("count", number(i64::try_from(count.get()).ok()?)),
            ]),
        },
        "film_grain" => Bool(config.film_grain),
        "cbr_padding" => Bool(config.cbr_padding),
        "fast_decode" => Bool(config.fast_decode),
        "multi_frame_header" => Bool(config.multi_frame_header),
        "disable_loopfilters_across_tiles" => Bool(config.disable_loopfilters_across_tiles),
        "profile" => config.profile.map_or(OptionValue::None, |profile| {
            choice(&profile.get().to_string())
        }),
        "tune" => choice(match config.tune {
            Tune::Psnr => "psnr",
            Tune::Ssim => "ssim",
            Tune::Vq => "vq",
        }),
        _ => return None,
    })
}

pub(super) fn content_origin(source: &SourceFacts, requested: Option<&OptionValue>) -> OptionValue {
    let color = source.color;
    let origin = |known| choice(if known { "source" } else { "codec default" });
    let explicit_timing = matches!(requested, Some(OptionValue::Record(values)) if values.contains_key("timing_info"));
    record([
        ("color_primaries", origin(color.primaries.is_some())),
        ("transfer_characteristics", origin(color.transfer.is_some())),
        ("matrix_coefficients", origin(color.matrix.is_some())),
        ("full_range", origin(color.full_range.is_some())),
        (
            "chroma_sample_position",
            origin(color.chroma_siting.is_some()),
        ),
        ("sample_aspect_ratio", origin(source.pixel_aspect.is_some())),
        (
            "timing_info",
            choice(if explicit_timing {
                "explicit"
            } else {
                "codec default"
            }),
        ),
        ("mastering_display", origin(false)),
        ("content_light_level", origin(false)),
    ])
}

fn content_value(content: &ContentDescription) -> OptionValue {
    use OptionValue::Bool;

    record([
        ("color_primaries", number(content.color_primaries)),
        (
            "transfer_characteristics",
            number(content.transfer_characteristics),
        ),
        ("matrix_coefficients", number(content.matrix_coefficients)),
        ("full_range", Bool(content.full_range)),
        (
            "chroma_sample_position",
            content
                .chroma_sample_position
                .map_or(OptionValue::None, |position| {
                    choice(match position {
                        ChromaSamplePosition::Left => "left",
                        ChromaSamplePosition::Center => "center",
                        ChromaSamplePosition::TopLeft => "top_left",
                        ChromaSamplePosition::Top => "top",
                        ChromaSamplePosition::BottomLeft => "bottom_left",
                        ChromaSamplePosition::Bottom => "bottom",
                    })
                }),
        ),
        (
            "sample_aspect_ratio",
            content.sample_aspect_ratio.map_or(OptionValue::None, ratio),
        ),
        ("timing_info", Bool(content.timing_info)),
        ("mastering_display", OptionValue::None),
        ("content_light_level", OptionValue::None),
    ])
}

fn rate_control_value(mode: RateControlMode) -> Option<OptionValue> {
    Some(match mode {
        RateControlMode::ConstantQp { qp } => {
            record([("mode", choice("constant_qp")), ("qp", number(qp))])
        }
        RateControlMode::Crf { crf } => record([("mode", choice("crf")), ("crf", number(crf))]),
        RateControlMode::CappedCrf {
            crf,
            max_bitrate_kbps,
            buffer_ms,
            initial_ms,
        } => record([
            ("mode", choice("capped_crf")),
            ("crf", number(crf)),
            ("max_bitrate_kbps", number(max_bitrate_kbps)),
            ("buffer_ms", number(buffer_ms)),
            ("initial_ms", number(initial_ms)),
        ]),
        RateControlMode::Vbr { bitrate_kbps } => record([
            ("mode", choice("vbr")),
            ("bitrate_kbps", number(bitrate_kbps)),
        ]),
        RateControlMode::Cbr {
            bitrate_kbps,
            buffer_ms,
            initial_ms,
        } => record([
            ("mode", choice("cbr")),
            ("bitrate_kbps", number(bitrate_kbps)),
            ("buffer_ms", number(buffer_ms)),
            ("initial_ms", number(initial_ms)),
        ]),
        _ => return None,
    })
}

fn tile_value(layout: &TileLayout) -> OptionValue {
    let list = |items: &[u32]| OptionValue::List(items.iter().copied().map(number).collect());
    match layout {
        TileLayout::Uniform {
            cols_log2,
            rows_log2,
        } => record([
            ("mode", choice("uniform")),
            ("cols_log2", number(*cols_log2)),
            ("rows_log2", number(*rows_log2)),
        ]),
        TileLayout::Explicit { widths, heights } => record([
            ("mode", choice("explicit")),
            ("widths", list(widths)),
            ("heights", list(heights)),
        ]),
    }
}

fn ratio((num, den): (u32, u32)) -> OptionValue {
    record([("numerator", number(num)), ("denominator", number(den))])
}

fn byte_value(bytes: Option<&[u8]>) -> OptionValue {
    bytes.map_or(OptionValue::None, |bytes| {
        OptionValue::List(bytes.iter().copied().map(number).collect())
    })
}

fn record(fields: impl IntoIterator<Item = (&'static str, OptionValue)>) -> OptionValue {
    OptionValue::Record(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn number(value: impl Into<i64>) -> OptionValue {
    OptionValue::Integer(value.into())
}

fn choice(value: &str) -> OptionValue {
    OptionValue::Choice(value.into())
}

pub(super) fn error(config: &Config, error: &Error) -> FieldError {
    let field = match error {
        Error::InvalidDimensions { width, .. } => {
            if (1..=splot_encode::MAX_FRAME_DIMENSION).contains(width) {
                "height"
            } else {
                "width"
            }
            .into()
        }
        Error::UnsupportedChroma { .. } => "chroma".into(),
        Error::InvalidResize { .. } => "resize_denominator".into(),
        Error::InvalidCrop { .. } => "crop".into(),
        Error::InvalidFrameRate { .. } => "frame_rate".into(),
        Error::InvalidQp { .. } => "rate_control.qp".into(),
        Error::InvalidCrf { .. } => "rate_control.crf".into(),
        Error::InvalidBitrate {
            what: "bitrate_kbps",
            ..
        } if matches!(config.rate_control, RateControlMode::CappedCrf { .. }) => {
            "rate_control.max_bitrate_kbps".into()
        }
        Error::InvalidBitrate { what, .. } => format!("rate_control.{what}"),
        Error::InvalidInitialBuffer { .. } => "rate_control.initial_ms".into(),
        Error::LosslessWith { what } if what == "film grain" => "film_grain".into(),
        Error::LosslessWith { what } if what == "resize" => "resize_denominator".into(),
        Error::LosslessWith { what } => format!("preset.{what}"),
        Error::PaddingWithoutCbr => "cbr_padding".into(),
        Error::InvalidContentDescription { .. } | Error::InvalidMasteringDisplay { .. } => {
            "content".into()
        }
        Error::InvalidMetadata { field, .. } => (*field).into(),
        Error::InvalidProfile { .. } => "profile".into(),
        Error::InvalidTiles { reason, .. } => return FieldError::new("tiles", *reason),
        Error::InvalidPreset { .. } => "preset".into(),
        Error::Preset(PresetError::UnknownField { name }) => format!("preset.{name}"),
        Error::Preset(
            PresetError::InvalidValue { field, .. } | PresetError::OutOfRange { field, .. },
        ) => format!("preset.{field}"),
        Error::BitDepthMismatch { .. } => "bit_depth".into(),
        _ => "config".into(),
    };
    FieldError::new(field, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_error_uses_codec_reason_without_copying_the_request() {
        let config = Config::new(64, 64);
        let cause = Error::InvalidTiles {
            tiles: TileLayout::Explicit {
                widths: vec![1; 22_000],
                heights: vec![],
            },
            reason: "tiles beyond the frame edge",
        };
        assert_eq!(
            error(&config, &cause),
            FieldError::new("tiles", "tiles beyond the frame edge")
        );
    }

    #[test]
    fn readback_preserves_each_rate_control_shape() -> Result<(), FieldError> {
        let mut config = Config::new(64, 64);
        for mode in [
            RateControlMode::ConstantQp { qp: 0 },
            RateControlMode::Crf { crf: 25 },
            RateControlMode::CappedCrf {
                crf: 25,
                max_bitrate_kbps: 1000,
                buffer_ms: 500,
                initial_ms: 250,
            },
            RateControlMode::Vbr { bitrate_kbps: 1000 },
            RateControlMode::Cbr {
                bitrate_kbps: 1000,
                buffer_ms: 500,
                initial_ms: 250,
            },
        ] {
            let request = rate_control_value(mode)
                .ok_or_else(|| FieldError::new("rate_control", "Readback needs a value."))?;
            apply(&mut config, "rate_control", &request)?;
            assert_eq!(config.rate_control, mode);
            assert_eq!(value(&config, "rate_control"), Some(request));
        }
        Ok(())
    }

    #[test]
    fn metadata_has_one_budget_and_round_trips_explicit_absence() -> Result<(), FieldError> {
        let mut config = Config::new(64, 64);
        let bytes = OptionValue::List(vec![number(0); MAX_METADATA_BYTES as usize]);
        let accepted = record([("icc_profile", bytes.clone())]);
        apply(&mut config, "key_frame_metadata", &accepted)?;
        assert_eq!(
            value(&config, "key_frame_metadata"),
            Some(record([
                ("icc_profile", bytes.clone()),
                ("itut_t35", OptionValue::None),
                ("user_data_unregistered", OptionValue::None),
            ]))
        );
        let excessive = record([
            ("icc_profile", bytes),
            ("itut_t35", OptionValue::List(vec![number(1)])),
        ]);
        assert!(matches!(
            apply(&mut config, "key_frame_metadata", &excessive),
            Err(error) if error.field == "key_frame_metadata.itut_t35"
        ));
        assert!(config.key_frame_metadata.itut_t35.is_none());
        Ok(())
    }

    #[test]
    fn profile_round_trips_and_codec_errors_name_it() -> Result<(), FieldError> {
        let mut config = Config::new(64, 64);
        assert_eq!(value(&config, "profile"), Some(OptionValue::None));
        for (request, refused) in [("0", false), ("3", false), ("5", true)] {
            apply(&mut config, "profile", &choice(request))?;
            assert_eq!(value(&config, "profile"), Some(choice(request)));
            // The codec refuses a reserved profile (Annex A.2 Table A.1).
            let field = config
                .validate()
                .map_err(|cause| error(&config, &cause).field);
            assert_eq!(
                field.err().as_deref(),
                refused.then_some("profile"),
                "{request}"
            );
        }
        for request in [choice("32"), choice("main"), OptionValue::Bool(true)] {
            assert!(apply(&mut config, "profile", &request).is_err());
        }
        apply(&mut config, "profile", &OptionValue::None)?;
        assert_eq!(config.profile, None);
        Ok(())
    }

    #[test]
    fn frame_metadata_round_trips_and_codec_errors_name_the_failed_record() -> Result<(), FieldError>
    {
        let mut config = Config::new(64, 64);
        let request = record([
            ("timecode", OptionValue::Bool(true)),
            ("temporal_point_info", OptionValue::Bool(true)),
            ("scan_type", OptionValue::Bool(false)),
            ("banding_hints", byte_value(Some(&[0xA8, 0xA6]))),
        ]);
        apply(&mut config, "frame_metadata", &request)?;
        assert_eq!(value(&config, "frame_metadata"), Some(request));

        // Temporal point info counts timing-info ticks, which are off by default.
        let failed = |config: &Config| config.validate().map_err(|cause| error(config, &cause));
        assert!(matches!(failed(&config), Err(error) if error.field == "frame_metadata"));
        config.key_frame_metadata.icc_profile = Some(vec![0; 1]);
        assert!(matches!(failed(&config), Err(error) if error.field == "key_frame_metadata"));
        config.key_frame_metadata = KeyFrameMetadata::default();
        config.content.timing_info = true;
        assert!(failed(&config).is_ok());
        Ok(())
    }

    #[test]
    fn capped_rate_errors_identify_the_maximum_bitrate_field() -> Result<(), FieldError> {
        let mut config = Config::new(64, 64);
        apply(
            &mut config,
            "rate_control",
            &record([
                ("mode", choice("capped_crf")),
                ("crf", number(20)),
                ("max_bitrate_kbps", number(0)),
                ("buffer_ms", number(500)),
                ("initial_ms", number(0)),
            ]),
        )?;
        assert!(matches!(
            config.validate(),
            Err(cause) if error(&config, &cause).field == "rate_control.max_bitrate_kbps"
        ));
        Ok(())
    }

    #[test]
    fn reject_invalid_record_fields_and_preserve_config() {
        let mut config = Config::new(64, 64);
        let original = config.clone();
        let requests = [
            (
                "rate_control",
                record([
                    ("mode", choice("crf")),
                    ("crf", number(20)),
                    ("qp", number(80)),
                ]),
                "rate_control.qp",
            ),
            (
                "content",
                record([("full_range", OptionValue::Bool(true))]),
                "content.full_range",
            ),
            (
                "content",
                record([
                    ("timing_info", OptionValue::Bool(true)),
                    ("color_primaries", number(1)),
                ]),
                "content.color_primaries",
            ),
            (
                "content",
                record([("chroma_sample_position", choice("left"))]),
                "content.chroma_sample_position",
            ),
            (
                "content",
                record([("sample_aspect_ratio", ratio((1, 1)))]),
                "content.sample_aspect_ratio",
            ),
            ("keyint", number(0), "keyint"),
            ("bit_depth", number(10), "bit_depth"),
        ];
        for (key, value, field) in requests {
            assert!(matches!(apply(&mut config, key, &value), Err(error) if error.field == field));
            assert_eq!(config, original);
        }
    }
}
