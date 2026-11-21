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

Milestone STUDIO-04: it converts **ordinary local video files** (MP4, MOV, MKV, WebM, TS, AVI)
and Y4M to video-only AV2. Sources must be progressive, constant frame rate and SDR; anything
else is refused with a reason. Several files queue up and the list survives restarts.
Encoding is slow: the codec is young.

- `studio-app` (`splot-studio`): the Slint desktop app. Open a video, pick Quality and
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
Debian/Ubuntu). Reading files other than Y4M, and the test suite, need `ffmpeg` and `ffprobe`
on `PATH` during development; packages ship their own LGPL build instead (see
`native/ffmpeg/README.md`).

```sh
cargo xtask doctor                 # toolchain and native prerequisites; read-only
cargo xtask ci --fast              # fmt, repo checks, clippy, unit tests
cargo xtask ci                     # full gate, as CI runs it
cargo xtask run --release          # the desktop app, with its engine built to match
cargo run -p studio-engine -- --help
cargo run --release -p studio-engine -- encode in.y4m out.ivf
cargo xtask package --smoke        # unsigned test package for this OS in target/package/
```

`cargo xtask package` needs the FFmpeg pair from `native/ffmpeg/build.sh <target>`; it builds
the app for plain x86-64 and the engine for x86-64-v3 (the app refuses older processors
before starting it), writes `BUILD.json` and the notices, and archives a `.tar.gz` (Linux) or
`.zip` (Windows, and macOS with a `.app` bundle). `cargo xtask smoke` unpacks it and runs it
with an emptied environment. Packages are unsigned test builds; see `docs/release/`.

`cargo run -p studio-app` rebuilds only the app, not the engine it starts; use `cargo xtask run`.

Rules for contributors and coding agents are in [`AGENTS.md`](AGENTS.md).

## Licence

Source-available, not open source. Original project material is licensed under the
[PolyForm Noncommercial License 1.0.0](LICENSE.md). Uses outside that licence, which includes
typical commercial deployments, need a separate written licence from Bartosz Tomczyk
(bartekplus@gmail.com). SplotLabs and splot are project names he uses, not a separate company.
Third-party components keep their own licences, see
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md); a commercial licence for Splot Studio does
not cover them. Splot Studio uses Slint under the Slint Royalty-free License 2.0. How
contributions to Splot Studio are licensed does not change that, or the licence of FFmpeg or any
other dependency.

## Contributing

Read the [contribution policy](https://github.com/splotlabs/.github/blob/main/CONTRIBUTING.md)
first. Other people's contributions are licensed by their authors under the unmodified MIT No
Attribution License (MIT-0), through the
[SplotLabs Contribution Terms](https://github.com/splotlabs/.github/blob/main/CLA.md). You keep
your copyright. MIT-0 lets everyone who receives your contribution reuse it, including
commercially. It covers only your own original material. The rest of Splot Studio, including the
file you edit, keeps its licence.

A contribution is merged only after each rights holder's acceptance of the approved terms is
recorded. This link, a ticked box in a pull request and a green CI run are not that acceptance.
