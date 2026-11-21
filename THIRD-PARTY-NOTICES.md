# Third-party notices

Original Splot Studio material is licensed under the PolyForm Noncommercial License 1.0.0,
copyright (c) 2026 Bartosz Tomczyk (see `LICENSE.md`). That licence does not cover third-party
components, which keep their own licences and copyright notices.

## Direct dependencies

Declared licence expressions below are taken from each crate's `Cargo.toml` metadata as
resolved in `Cargo.lock`, not from a licence audit. Transitive dependencies are not listed
here. A package made by `cargo xtask package` appends the complete list of crates linked
into its executables, generated from `cargo metadata` for its target, and carries their
licence texts in `licenses/RUST-CRATES.txt`; the build fails if a shipped crate's licence
is missing or not among the reviewed identifiers in `xtask/src/notices.rs`.

| Crate | Version | Declared licence | Used by |
|---|---|---|---|
| `serde` | 1.0.229 | MIT OR Apache-2.0 | studio-domain, studio-engine, studio-app |
| `serde_json` | 1.0.154 | MIT OR Apache-2.0 | studio-domain, studio-engine, studio-app, xtask |
| `thiserror` | 2.0.21 | MIT OR Apache-2.0 | studio-domain, studio-engine |
| `slint` | 1.19.0 | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | studio-app |
| `slint-build` | 1.19.0 | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | studio-app (build) |
| `rfd` | 0.18.0 | MIT | studio-app |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 | studio-engine |
| `directories` | 6.0.0 | MIT OR Apache-2.0 | studio-app |
| `splot-encode`, `splot-decode`, `splot-validate`, `splot-core` | git `main`, commit in `Cargo.lock` | PolyForm-Noncommercial-1.0.0 (same owner) | studio-engine |

## Slint

Splot Studio uses Slint under the **Slint Royalty-free License 2.0** option, which requires
attribution. Project policy is to show the official `AboutSlint` widget in an About screen
reachable from the top-level menu. The licence terms are Slint's; read them rather than any summary here:

- <https://github.com/slint-ui/slint/blob/v1.19.0/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md>
- <https://slint.dev>

The same `LICENSES/` directory is shipped inside the `slint` crate.

## Inter font (embedded by Slint)

Slint embeds the Inter variable font (`i-slint-common`, `sharedfontique/`) as its default
font; it is part of the `splot-studio` executable. Copyright 2020 The Inter Project
Authors, licensed under the **SIL Open Font License 1.1** (Reserved Font Name). Packages
reproduce the crate's notice for it.

## FFmpeg (bundled in packages only)

Packages ship `ffmpeg` and `ffprobe`, built by `native/ffmpeg/build.sh` from the
unmodified FFmpeg and dav1d release archives pinned in `native/ffmpeg/versions.env`:
FFmpeg under the **GNU Lesser General Public License 2.1 or later** (configured without
`--enable-gpl`, `--enable-nonfree` or `--enable-version3`), dav1d under the BSD 2-clause
licence. They are separate programs that the engine runs; they are not covered by Splot
Studio's licence. Each package carries the build's `MANIFEST.json` (source URLs and
SHA-256, configuration, toolchain) and the licence texts under `ffmpeg/`. The source
repository contains no FFmpeg binaries or sources.

