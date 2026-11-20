// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Compiles the `.slint` interface into Rust.

fn main() -> Result<(), slint_build::CompileError> {
    // "native" resolves per target without Qt: cupertino on macOS, fluent on
    // Windows and Linux. `SLINT_STYLE` is ignored so builds stay reproducible.
    let config = slint_build::CompilerConfiguration::new().with_style("native".into());
    slint_build::compile_with_config("ui/app-window.slint", config)
}
