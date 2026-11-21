// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Command-line parsing. Two subcommands with a few optional flags do not
//! justify a parser dependency; unknown input is an error, never ignored.

use std::ffi::OsString;
use std::path::PathBuf;

use studio_domain::protocol::PROTOCOL_VERSION;
use studio_domain::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};
use thiserror::Error;

use crate::ENGINE_NAME;
use crate::media::SYSTEM_OPT_IN;

/// The preset `encode` uses without `--preset`: the codec's default.
pub const DEFAULT_PRESET: SpeedPreset = match SpeedPreset::new(6) {
    Ok(preset) => preset,
    Err(_) => unreachable!(),
};
/// The rate factor `encode` uses without `--crf`: the codec's default.
pub const DEFAULT_CRF: Crf = match Crf::new(20) {
    Ok(crf) => crf,
    Err(_) => unreachable!(),
};

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// Print usage and exit successfully.
    Help,
    /// Print build and protocol identity and exit successfully.
    Version,
    /// Serve the framed protocol on stdin/stdout.
    Worker,
    /// Serve pictures and playback over the framed protocol on stdin/stdout.
    Preview,
    /// Run one job in this process and print its events as text.
    Encode {
        /// The source: Y4M, or a media file `FFmpeg` reads.
        source: PathBuf,
        /// The file to create.
        destination: PathBuf,
        /// The encoder settings.
        settings: EncoderSettings,
        /// The video stream; `None` picks the default one.
        video_stream: Option<u32>,
        /// Leave out the source's other streams and chapters.
        acknowledge_omissions: bool,
    },
}

/// The command line was not understood.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UsageError {
    /// No subcommand or flag was given.
    #[error("missing command")]
    Missing,
    /// An argument that is not a known command or flag.
    #[error("unknown argument {0:?}")]
    Unknown(OsString),
    /// Extra arguments after a complete command.
    #[error("unexpected argument {0:?}")]
    Unexpected(OsString),
    /// A required argument or flag value is absent.
    #[error("missing {0}")]
    MissingValue(&'static str),
    /// A flag value is out of range or not a number.
    #[error("invalid {flag} value {value:?}")]
    Invalid {
        /// The flag.
        flag: &'static str,
        /// The value given.
        value: OsString,
    },
}

/// Parses the arguments after the program name.
///
/// # Errors
/// Returns [`UsageError`] for a missing, unknown, surplus or invalid argument.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Cli, UsageError> {
    let mut args = args.into_iter();
    let first = args.next().ok_or(UsageError::Missing)?;
    let cli = match first.to_str() {
        Some("--help" | "-h") => Cli::Help,
        Some("--version" | "-V") => Cli::Version,
        Some("worker") => Cli::Worker,
        Some("preview") => Cli::Preview,
        Some("encode") => return parse_encode(args),
        _ => return Err(UsageError::Unknown(first)),
    };
    match args.next() {
        None => Ok(cli),
        Some(extra) => Err(UsageError::Unexpected(extra)),
    }
}

fn parse_encode(mut args: impl Iterator<Item = OsString>) -> Result<Cli, UsageError> {
    let source = args.next().ok_or(UsageError::MissingValue("source"))?;
    let destination = args.next().ok_or(UsageError::MissingValue("destination"))?;
    let mut settings = EncoderSettings {
        output: OutputKind::Ivf,
        preset: DEFAULT_PRESET,
        crf: DEFAULT_CRF,
    };
    let mut video_stream = None;
    let mut acknowledge_omissions = false;
    while let Some(flag) = args.next() {
        let flag = match flag.to_str() {
            Some("--acknowledge-omissions") => {
                acknowledge_omissions = true;
                continue;
            }
            Some("--output") => "--output",
            Some("--preset") => "--preset",
            Some("--crf") => "--crf",
            Some("--stream") => "--stream",
            _ => return Err(UsageError::Unknown(flag)),
        };
        let value = args.next().ok_or(UsageError::MissingValue(flag))?;
        let invalid = || UsageError::Invalid {
            flag,
            value: value.clone(),
        };
        let text = value.to_str().ok_or_else(invalid)?;
        match flag {
            "--output" => {
                settings.output = OutputKind::ALL
                    .into_iter()
                    .find(|kind| kind.extension() == text)
                    .ok_or_else(invalid)?;
            }
            "--preset" => {
                settings.preset = text
                    .parse()
                    .ok()
                    .and_then(|n| SpeedPreset::new(n).ok())
                    .ok_or_else(invalid)?;
            }
            "--stream" => {
                video_stream = Some(text.parse().map_err(|_| invalid())?);
            }
            _ => {
                settings.crf = text
                    .parse()
                    .ok()
                    .and_then(|n| Crf::new(n).ok())
                    .ok_or_else(invalid)?;
            }
        }
    }
    Ok(Cli::Encode {
        source: source.into(),
        destination: destination.into(),
        settings,
        video_stream,
        acknowledge_omissions,
    })
}

/// The `--help` text.
#[must_use]
pub fn help_text() -> String {
    format!(
        "\
{ENGINE_NAME} - Splot Studio engine

USAGE:
    {ENGINE_NAME} worker       serve the framed control protocol on stdin/stdout
    {ENGINE_NAME} preview      serve pictures and playback on stdin/stdout
    {ENGINE_NAME} encode <source> <destination> [--output ivf|obu] [--preset 0-10] [--crf 1-63]
                       [--stream N] [--acknowledge-omissions]
                              print the source's inventory, run one job here and
                              print its events
    {ENGINE_NAME} --version    print build and protocol identity
    {ENGINE_NAME} --help       print this help

Input is one progressive, constant-frame-rate, 8-bit or 10-bit SDR video: a Y4M
file, or an MP4, MOV, Matroska (MKV, WebM), MPEG-TS or AVI file decoded by FFmpeg.
--stream picks the video stream (default: the first one that is not cover art).
Output is AV2 IVF (ivf, the default) or an AV2 Annex B length-delimited OBU
stream (obu), video only: a source with audio, subtitles, other streams or
chapters needs --acknowledge-omissions. The destination must not exist; it
appears only after the output is verified. 'encode' defaults to preset {} and
crf {}.

FFmpeg is looked for beside this executable and in its ffmpeg/ directory; with
{SYSTEM_OPT_IN}=1 set, also on PATH.

'worker' reads length-prefixed JSON commands until end of input; stdout carries
protocol frames only, diagnostics go to stderr. It runs one job, and with live
pictures asked for, also decodes what the job has written (8-bit RGB, at most
1280x720, about four a second).

'preview' speaks the same protocol but runs no jobs: it answers Picture, Play
and StopPlayback with 8-bit RGB pictures of a source (exactly the frames the
encoder receives) or of a finished IVF/OBU file (decoded by the Splot decoder),
and plays video only, at the stream's frame timing. A raw OBU file has no
timing of its own. Pictures are an SDR approximation, not colour-managed.
",
        DEFAULT_PRESET.get(),
        DEFAULT_CRF.get()
    )
}

/// The `--version` text.
#[must_use]
pub fn version_text() -> String {
    format!(
        "\
{ENGINE_NAME} {}
protocol version: {PROTOCOL_VERSION}
encoding: {} and {}, from 8-bit or 10-bit Y4M{}
",
        env!("CARGO_PKG_VERSION"),
        OutputKind::Ivf.label(),
        OutputKind::Av2AnnexB.label(),
        match crate::media::tool() {
            Ok(tool) => format!(
                "\nmedia files: FFmpeg {} ({:?}, {})",
                tool.identity.version,
                tool.identity.origin,
                tool.ffmpeg.display()
            ),
            Err(stop) => format!(" only\nmedia files: not readable: {stop}"),
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn known_commands_parse() {
        assert_eq!(parse(args(&["--help"])), Ok(Cli::Help));
        assert_eq!(parse(args(&["-V"])), Ok(Cli::Version));
        assert_eq!(parse(args(&["worker"])), Ok(Cli::Worker));
        assert_eq!(parse(args(&["preview"])), Ok(Cli::Preview));
    }

    #[test]
    fn encode_parses_flags_in_any_order() {
        let got = parse(args(&[
            "encode", "in.y4m", "out.obu", "--crf", "30", "--output", "obu", "--preset", "10",
        ]));
        let Ok(Cli::Encode {
            settings,
            video_stream: None,
            acknowledge_omissions: false,
            ..
        }) = got
        else {
            unreachable!("{got:?}");
        };
        assert_eq!(settings.output, OutputKind::Av2AnnexB);
        let got = parse(args(&[
            "encode",
            "in.mkv",
            "out.ivf",
            "--acknowledge-omissions",
            "--stream",
            "3",
        ]));
        assert!(
            matches!(
                got,
                Ok(Cli::Encode {
                    video_stream: Some(3),
                    acknowledge_omissions: true,
                    ..
                })
            ),
            "{got:?}"
        );
        assert_eq!((settings.preset.get(), settings.crf.get()), (10, 30));
        let Ok(Cli::Encode { settings, .. }) = parse(args(&["encode", "a", "b"])) else {
            unreachable!("defaults");
        };
        assert_eq!(settings.output, OutputKind::Ivf);
        assert_eq!((settings.preset.get(), settings.crf.get()), (6, 20));
    }

    #[test]
    fn bad_input_is_an_error() {
        assert_eq!(parse(args(&[])), Err(UsageError::Missing));
        assert!(matches!(
            parse(args(&["handshake"])),
            Err(UsageError::Unknown(_))
        ));
        assert!(matches!(
            parse(args(&["worker", "x"])),
            Err(UsageError::Unexpected(_))
        ));
        assert_eq!(
            parse(args(&["encode", "a"])),
            Err(UsageError::MissingValue("destination"))
        );
        for bad in [
            &["encode", "a", "b", "--crf", "0"][..],
            &["encode", "a", "b", "--preset", "11"],
            &["encode", "a", "b", "--output", "mkv"],
            &["encode", "a", "b", "--stream", "-1"],
        ] {
            assert!(
                matches!(parse(args(bad)), Err(UsageError::Invalid { .. })),
                "{bad:?}"
            );
        }
        assert!(matches!(
            parse(args(&["encode", "a", "b", "--crf"])),
            Err(UsageError::MissingValue("--crf"))
        ));
        assert!(matches!(
            parse(args(&["encode", "a", "b", "--fast"])),
            Err(UsageError::Unknown(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_is_unknown_not_a_panic() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        assert!(matches!(parse([bad]), Err(UsageError::Unknown(_))));
    }
}
