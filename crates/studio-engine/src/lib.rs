// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `studio-engine` — the Splot Studio engine executable's library.
//!
//! This build speaks the framed control protocol from `studio-domain` and
//! nothing else: it answers a handshake and reports an empty capability list.
//! It cannot encode, and it never claims to. Encoding arrives with a later
//! milestone, behind a capability the app can check.

#![forbid(unsafe_code)]

pub mod cli;
pub mod session;

use studio_domain::protocol::BuildIdentity;

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
