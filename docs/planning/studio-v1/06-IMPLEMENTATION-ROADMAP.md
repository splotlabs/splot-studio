# Ordered roadmap and acceptance gates

Each phase is a compiling, reviewable change. The agent reports results and stops.
An unimplemented next phase is not a reason to add placeholders that pretend to
work. A prerequisite failure is not permission to waive it silently.

## STUDIO-00 — Repository and source audit

Read current and pinned Splot AGENTS, the source-use notices, toolchain, build
configuration, and reusable APIs. Create or safely open the private Studio repo.
Install adapted instructions and this pack. Pin and document dependency choices.

Gate: correct remote/visibility, preserved user changes, exact provenance, imported
project licence, no modifications to Splot, no accidental push. Continue into
STUDIO-01 in the kickoff only when safe.

## STUDIO-01 — Real native shell and foundations

Create the three initial crates and independent xtask. Real Slint window,
semantic tokens, menu-accessible AboutSlint, native file-dialog wiring, basic typed
settings, and a truthful unavailable-encoding state. Engine `--help` and a protocol
version/handshake smoke path. Add only used dependencies. No simulated encode.

Gate: fmt/clippy/unit tests, both executables build, native UI launched on the
available host, initial CI configured for the required targets. Report other
platforms as untested until their runners actually pass. No FFmpeg dependency yet.

## STUDIO-02 — Isolated raw-input encoder

Y4M → real Splot → IVF/OBU. Reuse upstream reader/writer. Both depths. Work runs in
a child worker, with bounded protocol, progress, cancellation, EOF drain, safe
output publication, and structural/decode verification. UI opens a Y4M file,
configures a job, encodes it, and reveals the actual file.

Gate: tiny valid 8/10-bit jobs, independent AVM check where installed, parser and
same-codebase decode tests separately labeled, invalid/truncated input, path
collision, writer failure, worker death, cancellation, complete output drain, and
IVF/OBU framing tests. No media files from an unlicensed third-party corpus.

## STUDIO-03 — Native-media input and honest preflight

Introduce studio-media with the pinned, bundled FFmpeg library build and `rsmpeg`
adapter. Enumerate all tracks, support tested decode formats, preserve/explicitly
interpret SDR properties, validate timestamps, and record omitted tracks before
video-only export. Open ordinary source containers without waiting for an AV2
Matroska output muxer.

Gate: library-build reproducibility, one representative compressed 8-bit input and
one 10-bit SDR input on each required release target, native dependency discovery,
negative/odd strides as appropriate, unsupported HDR/VFR paths, damaged input,
metadata/geometry treatment, no accidental eight-bit conversion, omission warning
and acknowledgement persistence. First user-authored unsafe blocks need review.

Do not expand the container/audio output scope here.

## STUDIO-04 — Durable queue and recovery

Add supervisor mode, SQLite transactions/migrations, per-user IPC, worker lifecycle,
reconnect, immutable jobs, one active encode, queue edits, output-publication journal,
and explicit quit/background behaviour. Initial UI restart may recover the queue;
background encode survival is enabled only once its lifecycle tests pass.

Gate: process kills at each meaningful state, no double execution, recovery across
a file-publication crash window, disk-full/removable-drive failure, DB migration
failure without data loss, stale sessions/PIDs, concurrent launch, cancellation
races, corrupted settings, and retry that does not overwrite a prior result.
Do not implement partial-bitstream resume or claim it.

## STUDIO-05 — Preview and interaction quality

Correct source/result frame comparison, bounded thumbnail/preview data path,
responsive seeking, tested SDR display conversion, software fallback, UI state
completion, accessible input, polished light/dark layouts, and actual native
screenshots. GPU integration is optional for the first frame preview but required
where needed to meet measured interactive performance. Avoid unstable APIs unless
isolated and pinned with a fallback.

Gate: frame identity and color-transform matching, no encoding from preview pixels,
10-bit source integrity, stale-request rejection, repeated seeks, GPU loss/fallback,
small-window/high-DPI behaviour, keyboard/IME and screen-reader review, CPU/memory
measurements under encode load. No requirement for synchronized audio yet.

## STUDIO-06 — Distribution and release hardening

Bundle libraries, licences, About attributions, native asset manifest, and build
identities. Package Windows/macOS/Linux deliverables, choose OS/CPU floors, test
on clean systems, sign/notarize where credentials are provided, and document
manual update/install/uninstall procedures. No secret extraction or invented signing.

Gate: install/open/encode/cancel/verify/reopen on every advertised target, portable
launcher/worker compatibility, clean-machine library loading, full multimedia
omission policy, test-corpus regression, accessible AboutSlint, native-source/legal
artifacts, privacy inspection, and a human release approval. Signing unavailable
means unsigned test build, not a production-ready release.

## STUDIO-07 — Evidence-gated multimedia output (later, not part of initial build)

Proceed only after the owner's decision to start this phase and the public
AV2-in-Matroska mapping plus chosen implementation pass the evidence gate. The
trigger is not a calendar date or finding a patch on a forum.

Then implement complete-media jobs: selected video encoding, compatible audio
copy/transcode, compatible text/bitmap subtitle handling, chapters, dispositions,
attachments, and timing. Unsupported combinations must stay explicit. Test actual
independent demux/seek/decode and supported external players; self-playback is not
ecosystem interoperability. Keep IVF/OBU as independent supported outputs.

HDR/HLG remains a distinct subsequent feature even if the muxer can store HDR
metadata. Do not combine those projects accidentally.

## Common validation ladder

Fast developer loop: fmt, targeted clippy/check, changed-crate tests, and one
runnable application/engine check for changed nontrivial behaviour. Full CI:
workspace checks for supported feature combinations, unit/integration/doc tests,
actual Slint compilation, engine process tests, and applicable platform lanes.

Only use `--all-features` when the chosen feature set is compatible. Do not combine
mutually exclusive renderer/backend configurations to satisfy a copied command.
Keep the feature matrix explicit and small. Heavy corpus, independent AVM, fuzz,
packaging, and real hardware/UI checks can be separate jobs, but the release gate
must report them by name and cannot silently skip required coverage.

## Required phase handover

Update one concise STATUS.md with: implemented capabilities; actual dependencies
and versions; target/host; exact commands and outcomes; screenshots/artifacts where
relevant; remaining blockers; known unsupported cases; and the next prompt to run.
Use code-backed statements. Do not claim an acceptance criterion passed because a
test file was added or a remote workflow was merely configured.
