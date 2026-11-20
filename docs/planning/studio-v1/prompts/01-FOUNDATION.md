# STUDIO-01 — compiling native foundation

Run through the kickoff, or paste the task below after repository setup is already
complete. Do not recreate an existing remote or reset the checkout.

---

Implement STUDIO-01 only. Read root AGENTS.md and the product, architecture, design,
dependency, and repository guides under `docs/planning/studio-v1/`. Inspect the
current tree/status; preserve work. The goal is a real native shell and sound
boundaries, not a simulated encoder or a full product in one change.

## Work

1. Create a virtual Rust 2024 workspace with explicit resolver 3 and inherited
   metadata/lints. Initial members: studio-domain, studio-engine, studio-app,
   and independent xtask. Package names and binary names match the setup guide.
   Pin the toolchain and Cargo.lock. Add only dependencies used in this phase.

2. Establish typed domain values for output kind, basic requested encoder settings,
   and protocol version/handshake. Validate basic value ranges only when backed
   by the codec contract; do not duplicate a large codec config model. Job and
   stream types can grow when their features arrive. Keep domain free of GUI,
   native library, SQL, and codec dependencies.

3. Create a real engine executable with --help, build/protocol identity, and a
   small framed handshake command suitable for an automated child-process test.
   Do not return fake successful encode events. Reserve pixel processing for the
   later worker implementation; no shell wrapper around Splot.

4. Build the real Slint window from .slint files. Use Splot brand-derived semantic
   tokens, OS-appropriate typography, a restrained toolbar/sidebar/inspector,
   intentional empty state, and light/dark support. Include a menu-accessible
   About dialog containing the official AboutSlint widget and accurate licence
   text. Do not bundle Apple's fonts.

5. Wire a native Open dialog without blocking the event loop. Selecting a file
   can update a source label in this phase; it must not imply probing or validation
   occurred. Encode is clearly unavailable until the next phase. Menus/controls
   that are visible must either work or accurately explain why unavailable.
   No fake preview, task queue, progress, format support, or HDR/MKV toggle.

6. Keep UI interaction state separate from domain values. UI callbacks issue typed
   intents rather than doing filesystem scans or initializing codec worker pools.
   Start cheaply; do not initialize FFmpeg/GPU preview/codec threads on launch.
   Keep accessibility enabled; provide names, focus, and keyboard behaviour.

7. Implement a small xtask ci and ci --fast using real commands, propagating
   failures. Full gate compiles the real Slint app, checks workspace libraries,
   and runs unit/doc/process tests. Fast gate omits heavy tests but does not
   disguise a failure. Integrate minimal header/dependency-direction checks into
   the existing gate when useful, not a dozen empty command names.

8. Configure CI for Windows x86_64, macOS arm64, Linux x86_64 with actual supported
   runner labels and required native GUI build dependencies. Use verified actions,
   read-only permissions, caching keyed on relevant lockfiles/toolchain, and no
   release/signing secrets. Scope jobs; do not change billing or assume runners
   are free in a private repository. If a runner is unavailable, report the gap.

9. Write a short README with actual status, development commands, source-available
   licence, no-web scope, and video-only roadmap. Keep this pack as planning docs;
   don't reproduce it in three new READMEs. Update STATUS.md with facts.

## Acceptance

- `cargo fmt --all -- --check` passes.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` passes for
  supported default features; separately check any selected alternative feature
  combination. Do not blindly enable mutually exclusive backends.
- `cargo test --workspace --all-targets --locked` and doc tests pass, or platform-
  specific inability is stated with the exact failing command, not hidden.
- Both executables build and show truthful help/identity where applicable.
- The handshake is exercised through a real child process, including invalid
  version/oversize/truncated-message cases if the framing implementation exists.
- The native Slint app actually opens on the available host; empty state, dialog,
  AboutSlint, theme, and keyboard interaction are inspected.
- Engine/domain compile without importing Slint. App/domain do not import Splot
  or FFmpeg. There are no placeholders returning success.
- Root AGENTS adaptation and source provenance match the mapping guide.

UI testing tools that require a paid add-on must not become mandatory. Headless
model/backend tests do not replace native screenshots and accessibility checks.
Where a display or other OS is unavailable, say “not tested” and leave its gate
open. No source/branch/push mutation outside this repo.

Stop here. Report exact outcomes and next prompt: 02-RAW-VIDEO.md.
