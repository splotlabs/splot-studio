<p align="center">
  <img src="./docs/assets/hero.svg" alt="splot studio — The AV2 encoder workbench." width="100%">
</p>

<p align="center">
  <b>Native Rust AV2 encoder workbench.</b><br>
  Slint UI. Local processing.
</p>

A native desktop workbench for encoding video to AV2, built on the
[Splot](https://github.com/splotlabs/splot) codec. Rust and [Slint](https://slint.dev), for
Windows, macOS and Linux. Processing is local; there is no web or browser target.

## Status

First usable slice (milestone STUDIO-02): it converts **progressive 8-bit or 10-bit Y4M** to
AV2. Ordinary files (MP4, MKV, MOV) are not read yet. Encoding is slow: the codec is young.

- `studio-app` (`splot-studio`): the Slint desktop app. Open a Y4M file, pick Quality and
  Priority, press Convert, watch real progress, cancel, reveal the result.
- `studio-engine` (`splot-studio-engine`): the headless engine. `worker` runs one job per
  process over the versioned, length-prefixed JSON control protocol from `studio-domain`;
  `encode` runs the same job from the command line. Output is verified, then published
  without ever replacing an existing file.
- `studio-domain`: shared settings, job and protocol values (no GUI, codec or native types).

Output is video-only AV2 in IVF or Annex B `.obu`; non-video input tracks are not carried over. The phased plan is in [`docs/planning/studio-v1/`](docs/planning/studio-v1/).
What has actually been run, and what has not, is recorded in `STATUS.md`.

## Development

Rust is pinned by `rust-toolchain.toml` (1.100.0); rustup installs it on first use. On Linux the
GUI build needs `pkg-config` and the fontconfig development files (`libfontconfig-dev` on
Debian/Ubuntu).

```sh
cargo xtask doctor                 # toolchain and native prerequisites; read-only
cargo xtask ci --fast              # fmt, repo checks, clippy, unit tests
cargo xtask ci                     # full gate, as CI runs it
cargo run -p studio-app            # the desktop app
cargo run -p studio-engine -- --help
cargo run --release -p studio-engine -- encode in.y4m out.ivf
```

Rules for contributors and coding agents are in [`AGENTS.md`](AGENTS.md).

## Licence

Source-available, not open source. Original project material is licensed under the
[PolyForm Noncommercial License 1.0.0](LICENSE.md); commercial use requires a separate written
licence from Bartosz Tomczyk. Third-party components keep their own licences, see
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
