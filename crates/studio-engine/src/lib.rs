// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `studio-engine` — the Splot Studio engine executable's library.
//!
//! The [`worker`] serves the framed control protocol from `studio-domain`:
//! it probes [`source`]s (Y4M directly, other media files through the
//! [`media`] tools) and runs one [`job`] that encodes the 8-bit or 10-bit
//! frames of one video stream with the pinned Splot encoder into AV2 IVF or
//! an AV2 Annex B length-delimited OBU stream, [`verify`]s the result, and
//! publishes it through a no-clobber [`output`] transaction.
//!
//! Licensed under `PolyForm` Noncommercial 1.0.0; commercial use requires a
//! separate written license from Bartosz Tomczyk.

#![forbid(unsafe_code)]

pub mod cli;
pub mod job;
pub mod media;
pub mod output;
pub mod source;
pub mod verify;
pub mod worker;

use studio_domain::protocol::{BuildIdentity, Capabilities};
use studio_domain::settings::OutputKind;

/// The executable name reported in the handshake and `--version`.
pub const ENGINE_NAME: &str = "splot-studio-engine";

/// This build's identity, as sent in [`Event::Hello`](studio_domain::protocol::Event::Hello).
#[must_use]
pub fn build_identity() -> BuildIdentity {
    BuildIdentity {
        name: ENGINE_NAME.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// What this build can do: both output kinds, and ordinary media files
/// when a usable `FFmpeg` is found ([`media::tool`]).
#[must_use]
pub fn capabilities() -> Capabilities {
    Capabilities {
        encode_outputs: OutputKind::ALL.to_vec(),
        media_tool: media::identity(),
    }
}
