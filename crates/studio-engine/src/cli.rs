// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Command-line parsing. Two subcommands and two flags do not justify a parser
//! dependency; unknown input is an error, never silently ignored.

use std::ffi::OsString;

use studio_domain::protocol::PROTOCOL_VERSION;
use thiserror::Error;

use crate::ENGINE_NAME;

/// What the command line asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cli {
    /// Print usage and exit successfully.
    Help,
    /// Print build and protocol identity and exit successfully.
    Version,
    /// Serve the framed protocol on stdin/stdout.
    Handshake,
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
}

/// Parses the arguments after the program name.
///
/// # Errors
/// Returns [`UsageError`] for a missing, unknown or surplus argument.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Cli, UsageError> {
    let mut args = args.into_iter();
    let first = args.next().ok_or(UsageError::Missing)?;
    let cli = match first.to_str() {
        Some("--help" | "-h") => Cli::Help,
        Some("--version" | "-V") => Cli::Version,
        Some("handshake") => Cli::Handshake,
        _ => return Err(UsageError::Unknown(first)),
    };
    match args.next() {
        None => Ok(cli),
        Some(extra) => Err(UsageError::Unexpected(extra)),
    }
}

/// The `--help` text.
#[must_use]
pub fn help_text() -> String {
    format!(
        "\
{ENGINE_NAME} - Splot Studio engine

USAGE:
    {ENGINE_NAME} handshake    serve the framed control protocol on stdin/stdout
    {ENGINE_NAME} --version    print build and protocol identity
    {ENGINE_NAME} --help       print this help

This build cannot encode yet. 'handshake' reads length-prefixed JSON commands
until end of input; stdout carries protocol frames only, diagnostics go to stderr.
"
    )
}

/// The `--version` text. Truthful about what this build can do.
#[must_use]
pub fn version_text() -> String {
    format!(
        "\
{ENGINE_NAME} {}
protocol version: {PROTOCOL_VERSION}
encoding: not implemented in this build
",
        env!("CARGO_PKG_VERSION")
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
        assert_eq!(parse(args(&["handshake"])), Ok(Cli::Handshake));
    }

    #[test]
    fn bad_input_is_an_error() {
        assert_eq!(parse(args(&[])), Err(UsageError::Missing));
        assert!(matches!(
            parse(args(&["worker"])),
            Err(UsageError::Unknown(_))
        ));
        assert!(matches!(
            parse(args(&["handshake", "x"])),
            Err(UsageError::Unexpected(_))
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
