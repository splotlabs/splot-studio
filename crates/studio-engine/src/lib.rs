// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `studio-engine` — the Splot Studio engine executable's library.
//!
//! The [`worker`] serves the framed control protocol from `studio-domain`:
//! it probes [`source`]s (Y4M directly, other media files through the
//! [`media`] tools) and runs one [`job`] that encodes the 8-bit or 10-bit
//! frames of one video stream with the pinned Splot encoder into AV2 IVF or
//! an AV2 Annex B length-delimited OBU stream, [`verify`]s the result, and
//! publishes it through a no-clobber [`output`] transaction. While it runs,
//! [`live`] pictures of the packets written so far can be sent.
//!
//! The [`preview`] process serves pictures and playback of sources and of
//! finished outputs ([`stream`] decodes them with the Splot decoder), all
//! through one display transform, [`picture`].
//!
//! The [`inspect`] process opens local AV2 IVF and Annex B files read-only
//! and serves their envelope and header facts a page at a time.
//!
//! Licensed under `PolyForm` Noncommercial 1.0.0; commercial use requires a
//! separate written license from Bartosz Tomczyk.

#![forbid(unsafe_code)]

mod artifact;
pub mod cli;
mod export;
pub mod inspect;
pub mod job;
pub mod live;
pub mod media;
mod options;
pub mod output;
pub mod picture;
pub mod preview;
pub mod source;
pub mod stream;
pub mod verify;
pub mod worker;

use studio_domain::analysis::Framing;
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

/// What this build can do: both output kinds, ordinary media files when a
/// usable `FFmpeg` is found ([`media::tool`]), and inspection of both
/// framings.
#[must_use]
pub fn capabilities() -> Capabilities {
    Capabilities {
        encode_outputs: OutputKind::ALL.to_vec(),
        media_tool: media::identity(),
        analysis_framings: vec![Framing::Ivf, Framing::AnnexB],
        analysis_evidence: None,
        options: None,
    }
}
