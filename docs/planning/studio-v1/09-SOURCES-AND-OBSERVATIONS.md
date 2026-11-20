# Sources, observations, and limits

Checked 20 November 2026. This pack contains proposed architecture and requirements;
only the observations below describe inspected existing implementation. The pack
has not compiled or executed Splot Studio, FFmpeg, Slint, or the codec on a target.

## Exact Splot baseline

`c1183fec4b81d72045d861973a9d8e563c6e9e2e` was the main-branch commit returned
by the GitHub connector during preparation. Use immutable references below. Do not
assume the moving main branch still matches it when implementation begins.

### S01 — Agent instructions

- [Pinned AGENTS.md](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/AGENTS.md)
- [Requested moving source](https://github.com/splotlabs/splot/blob/main/AGENTS.md)
- Blob SHA: `daf08ea45c6386223f791b1d2046179a357c92bd`.

Observed: behavior/lazy-developer rules, scoped diffs, reuse, measurable performance,
8/10-bit equality, safe source use, codec-specific SIMD rules, real-hardware checks,
and named repo-specific automation. The root template adapts these, not the older
uploaded validator-first scaffold. See the mapping file for intentional changes.

### S02 — Public encoder exports

[lib.rs](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/crates/splot-encode/src/lib.rs)

Observed: reusable Encoder/Config/SourceFrame/Packet API, public Y4M/IVF modules,
re-exported ThreadCount, BitDepth, PixelFormat, presets and rate-control types.
Packet is documented as an AV2 Annex B temporal unit. This is a library integration,
not a request to implement another encoder or shell out to the Splot CLI.

### S03 — Actual IVF writer

[ivf.rs](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/crates/splot-encode/src/ivf.rs)

Observed: AV02 fourcc, inverse configured frame rate, per-packet PTS, zero header
frame count, 16-bit dimensions, write/finish methods. Reuse this writer and verify
its documented constraints. Do not assume MP4/MKV support from this source.

### S04 — Toolchain and build configuration

- [rust-toolchain.toml](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/rust-toolchain.toml)
- [.cargo/config.toml](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/.cargo/config.toml)

Observed: Rust 1.100.0; codec checkout's x86 flags include x86-64-v3 and
`disable_dispatch_avx512`; it sets RUST_MIN_STACK for particular debug tests.
These are not automatically Studio policy or evidence of portable startup.

### S05 — Y4M reader

[y4m.rs](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/crates/splot-encode/src/y4m.rs)

Observed inspected lines 1–240: streaming reader, header dimensions/rate/aspect/
interlace/layout/depth; frame planes are owned u16; header limit; typed truncated
input errors. The public header does not by itself expose a complete SDR/HDR color
model. Additional implementation must not silently infer missing properties.

### S06 — Owned source frame

[frame.rs](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/crates/splot-encode/src/frame.rs)

Observed inspected lines 1–190: packed owned u16 planes for both sample depths;
`new` takes ownership and validates sizes/range; strided constructors copy and
use sample strides. No zero-copy promise is supported by these constructors.

### S07 — Brand asset and source-use policy

- [hero.svg](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/docs/assets/hero.svg)
- [Third-party notices](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/docs/references/THIRD-PARTY-NOTICES.md)
- [Project licence, to import and review during bootstrap](https://github.com/splotlabs/splot/blob/c1183fec4b81d72045d861973a9d8e563c6e9e2e/LICENSE.md)

Observed: cyan/violet/pink brand gradient; dark background; light/muted text;
CSS animations and font references. Notices distinguish project material from
upstream research and the quarantined AV2 spec mirror. Do not relicense dependencies
or copy third-party code/prose/tables by default. The licence text is to be retrieved
exactly, not reconstructed from this pack.

## Primary external documentation

### S08 — Slint licence and application model

- [Licence overview](https://github.com/slint-ui/slint/blob/master/LICENSE.md)
- [Royalty-free v2.0 terms](https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md)
- [Slint getting started](https://docs.slint.dev/latest/docs/slint/guide/getting-started/)
- [Rust API](https://docs.slint.dev/latest/docs/rust/slint/)
- [Cargo features](https://docs.slint.dev/latest/docs/rust/slint/docs/cargo_features/)

Observed: .slint frontend with Rust logic; selectable royalty-free licence;
AboutSlint/public-badge attribution paths; accessibility and backend features.
Pin released versions and read their own terms/features before integration.

### S09 — GitHub repository creation

[GitHub CLI gh repo create](https://cli.github.com/manual/gh_repo_create)

Observed: explicit OWNER/REPO, visibility choice, --clone/--source, optional --push.
The setup commands deliberately omit --push and do not create a default MIT/GPL
licence. Verify org permission without leaking credentials.

### S10 — Cargo configuration and workspace policy

- [Configuration](https://doc.rust-lang.org/stable/cargo/reference/config.html)
- [Workspaces](https://doc.rust-lang.org/stable/cargo/reference/workspaces.html)

Observed: configuration lookup is based on invocation hierarchy, and workspace
root owns shared lockfile/profile decisions. Studio must set its own deliberate
build policy; a codec dependency's development checkout policy is not inherited
wholesale.

### S11 — FFmpeg compliance and native adapter candidates

- [FFmpeg licence and legal checklist](https://www.ffmpeg.org/legal.html)
- [FFmpeg libraries and tools documentation](https://ffmpeg.org/documentation.html)
- [rsmpeg upstream, to inspect at selected release](https://github.com/larksuite/rsmpeg)

Observed from FFmpeg's legal guidance: LGPL default, optional GPL components,
recommendation to avoid --enable-gpl/--enable-nonfree and use dynamic linking with
source/build/notices obligations. The complete shipped configuration still needs
review. rsmpeg is a proposed adapter choice, not a verified Studio build.

### S12 — UI testing boundaries

- [Slint testing backend](https://docs.slint.dev/latest/docs/rust/i_slint_backend_testing/)
- [Slint UI testing guide](https://docs.slint.dev/latest/docs/slint/guide/development/ui-testing/)
- [Slint system UI test documentation](https://testing.slint.dev/)

Observed: the testing backend can exercise UI behaviour without a windowing system
but does not render real pixels. The separate system UI-testing product is an
add-on and requires its own access/licence. Do not assume the user's royalty-free
runtime choice purchases that tool.

### S13 — Release platform documentation (implementation references)

- [Apple notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- [Microsoft app signing](https://learn.microsoft.com/windows/msix/package/sign-app-package-using-signtool)

Consult current platform instructions when packaging. No target signing or
notarization was performed while preparing this pack.

## Deliberately not asserted

This pack does not assert that AV2 Matroska support is currently stable or absent,
or that a specific FFmpeg release can mux it. That research is deferred with the
container phase. The gate is defined without perpetuating an old time-sensitive
claim. Likewise no exact latest Slint/rsmpeg/FFmpeg version, codec throughput,
zero-copy path, correct HDR display, or universal input compatibility is claimed.
