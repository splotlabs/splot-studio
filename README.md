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

Early foundation (milestone STUDIO-01). **Nothing can be encoded yet.**

- `studio-app` (`splot-studio`): a Slint desktop shell. It does not probe, preview or encode
  media.
- `studio-engine` (`splot-studio-engine`): a headless executable with `--help`, `--version`
  and a `handshake` command that speaks the versioned, length-prefixed JSON control protocol
  from `studio-domain`. It reports an empty capability list: no encode support.
- `studio-domain`: shared settings and protocol values (no GUI, codec or native types).

Planned output is video-only AV2 in IVF and Annex B `.obu`; non-video input tracks are not
carried over. The phased plan is in [`docs/planning/studio-v1/`](docs/planning/studio-v1/).
What has actually been run, and what has not, is recorded in `STATUS.md`.

## Development

Rust is pinned by `rust-toolchain.toml` (1.100.0); rustup installs it on first use. On Linux the
GUI build needs `pkg-config` and the fontconfig development files (`libfontconfig-dev` on
Debian/Ubuntu).

```sh
cargo xtask doctor                 # toolchain and native prerequisites; read-only
cargo xtask ci --fast              # fmt, repo checks, clippy, unit tests
cargo xtask ci                     # full gate, as CI runs it
cargo run -p studio-app            # the desktop shell
cargo run -p studio-engine -- --help
cargo run -p studio-engine -- handshake    # framed protocol on stdin/stdout
```

Rules for contributors and coding agents are in [`AGENTS.md`](AGENTS.md).

## Licence

Source-available, not open source. Original project material is licensed under the
[PolyForm Noncommercial License 1.0.0](LICENSE.md); commercial use requires a separate written
licence from Bartosz Tomczyk. Third-party components keep their own licences, see
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
