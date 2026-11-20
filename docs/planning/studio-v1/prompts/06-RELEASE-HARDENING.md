# STUDIO-06 — distribution and reliability release gate

---

Implement STUDIO-06 only. Read root AGENTS, STATUS, dependencies/release guide,
architecture, and the actual source. Audit claimed capabilities against tests.
Do not add MKV/HDR/audio-output features to fill marketing gaps.

## Establish the release matrix

Choose concrete supported OS/CPU versions based on Slint, FFmpeg, Rust, graphics,
and packaging requirements. Windows x86_64, macOS arm64, and Linux x86_64 are the
initial required lanes. Other targets are optional until independently built and
tested. No “multiplatform” claim based only on Cargo target availability.

Audit CPU instructions and startup. No globally native-CPU release build. The GUI
must open or produce a clear unsupported-platform message without executing
unsupported codec instructions. Review separate worker builds only if actually
needed; do not invent dispatch support or redesign SIMD in Studio.

## Package

Make reproducible native packaging recipes for app, engine, assets, and shared
libraries with exact build/source manifests. Test loading without developer PATH,
DYLD/LD_LIBRARY_PATH overrides, system FFmpeg, or a Rust installation. Avoid DLL
search-path hijacking and incorrect macOS rpaths.

macOS bundles: nested engine/library signing, app signing, notarization, and
validation when owner credentials exist. Windows: installers/executables and
signatures where authorized; do not promise reputation from signing alone. Linux:
a documented delivery format/ABI baseline tested with portals/fonts/window systems
and renderer fallback. Avoid adding several untested installer ecosystems.

Use signed manual downloads first. No background downloader/updater or release
publication added implicitly. Signing tokens/certificates are never printed,
committed, or exposed to PR jobs. Without credentials, produce accurately labeled
unsigned test packages and an outstanding release checklist.

## Licensing and privacy

Verify the actual Slint release's selected terms and menu-accessible AboutSlint.
Verify bundled FFmpeg flags, each external dependency, native source/build/patch
artifacts, replacement/debugging obligations, and notices. Cargo.lock is not an
inventory of every native component. Do not relicense third-party code as PolyForm.
Do not ship proprietary fonts or unlicensed media fixtures.

No network access for local encoding by default. Inspect dependencies and runtime
traffic rather than relying only on absent upload UI. Restrict media protocols,
log sizes, metadata sizes, private paths, and crash-data collection. Diagnostic
export is user initiated and previews what it includes.

## Clean-machine acceptance

Install, launch, open eight/ten-bit SDR sources, inspect excluded tracks, encode
both IVF and OBU, cancel/retry, verify result, reveal files, quit/reopen, recover
interrupted queue, and uninstall without deleting user media. Test Unicode paths,
low disk space, output collisions, removable destination loss, and graphics fallback.
Run real keyboard/IME/screen-reader spot checks on each advertised OS.

Run corpus regressions and independent AVM differential checks for the release
fixture set. Preserve clear not-run/unsupported/failure results. Full postflight
and signing status must be truthful. Validate package identity/UI-engine protocol
matching and test upgrades/migrations from the preceding test build if one exists.

Prepare release notes describing video-only output and supported/rejected media.
List system requirements, source availability/licences, known limitations, build
identities, checksums, and manual installation/update instructions. Website changes
are a separate scoped task in the website repo, not edits hidden in Studio work.

Do not push/tag/publish or change repository visibility without explicit human
approval. Update STATUS with exact artifact locations and tested matrix. Stop with
an actionable release-ready versus still-blocked assessment, not a claim of
perfection. Future container work is a separate authorization and prompt.
