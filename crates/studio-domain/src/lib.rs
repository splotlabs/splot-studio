// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `studio-domain` — values shared by the Splot Studio app and engine.
//!
//! Requested encoder [`settings`], [`job`] values, [`preview`] pictures,
//! read-only [`analysis`] documents and the versioned control [`protocol`] the
//! app and engine speak over inherited pipes. This crate holds values and pure
//! policy only: no Slint, codec, `FFmpeg`, SQL or process-management types.
//!
//! Licensed under `PolyForm` Noncommercial 1.0.0; commercial use requires a
//! separate written license from Bartosz Tomczyk.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod job;
pub mod options;
pub mod preview;
pub mod protocol;
pub mod settings;
