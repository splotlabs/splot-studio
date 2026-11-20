// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Renders the real window once and writes a renderer snapshot as binary PPM.
//!
//! ```text
//! cargo run -p studio-app --example snapshot -- OUT.ppm light|dark [about] [source=PATH]
//! ```
//!
//! This is Slint's own `Window::take_snapshot`, not an OS screenshot: no
//! window decorations and no compositor. `source=PATH` sets the display
//! properties a picked file would set, without running the file dialog. The
//! engine handshake is real; from `target/*/examples/` it reports the engine
//! as not found, because the engine binary is not next to the example.

use std::io::Write;
use std::time::Duration;

use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use studio_app::Palette;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, scheme, rest @ ..] = args.as_slice() else {
        return Err("usage: snapshot OUT.ppm light|dark [about] [source=PATH]".into());
    };
    let scheme = match scheme.as_str() {
        "light" => slint::language::ColorScheme::Light,
        "dark" => slint::language::ColorScheme::Dark,
        other => return Err(format!("unknown scheme {other:?}").into()),
    };

    let window = studio_app::build()?;
    window.global::<Palette>().set_color_scheme(scheme);
    for arg in rest {
        if arg == "about" {
            window.invoke_show_about();
        } else if let Some(path) = arg.strip_prefix("source=") {
            let path = std::path::Path::new(path);
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            window.set_has_source(true);
            window.set_source_name(name.unwrap_or_default().into());
            window.set_source_path(path.display().to_string().into());
        } else {
            return Err(format!("unknown argument {arg:?}").into());
        }
    }
    window.show()?;

    let weak = window.as_weak();
    let out = out.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    // Long enough for the first frames and the engine handshake to land.
    slint::Timer::single_shot(Duration::from_millis(1500), move || {
        let result = weak
            .upgrade()
            .ok_or_else(|| "window closed".to_owned())
            .and_then(|w| w.window().take_snapshot().map_err(|e| e.to_string()))
            .and_then(|pixels| write_ppm(&out, &pixels).map_err(|e| e.to_string()));
        let _ = tx.send(result);
        let _ = slint::quit_event_loop();
    });
    slint::run_event_loop()?;
    rx.recv()??;
    Ok(())
}

fn write_ppm(path: &str, pixels: &SharedPixelBuffer<Rgba8Pixel>) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(file, "P6\n{} {}\n255\n", pixels.width(), pixels.height())?;
    for p in pixels.as_slice() {
        file.write_all(&[p.r, p.g, p.b])?;
    }
    file.flush()
}
