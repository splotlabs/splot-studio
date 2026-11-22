# AGENTS.md — Splot Studio

Canonical rules for humans and coding agents. CLAUDE.md and Copilot instructions
point here.

## Behaviour

Understand intent and the real call/data flow before editing. Ask about material
unresolved choices, not decisions already accepted in the current task. For safe,
nonblocking details choose the simplest reasonable default and record it. Flag
uncertainty and use a small runnable experiment when that resolves it.

Work like a lazy senior developer: efficient, not careless. Reuse existing code,
standard library/platform features, and installed dependencies before adding new
code. Prefer deletion, boring implementation, and the smallest correct change.
Do not add speculative abstractions, dependency stacks, boilerplate, empty crates,
or frameworks for hypothetical future formats. Do not fix unrelated issues in a
feature diff. Inspect callers and fix shared root causes once.

Never economize on input validation at trust boundaries, data-loss prevention,
security, accessibility, precision, ownership, or real hardware verification.
Nontrivial logic needs a runnable check. A short diff in the wrong place is not a
better solution. Cleanup must reduce complexity; do not add new budget reports,
ratchets, command names, or documentation to justify a cleanup.

## Product boundaries

Splot Studio is a Rust/Slint desktop AV2 workbench for Windows, macOS, and Linux.
No browser target.

Approved direction (owner, November 2026; roadmap #33): one application with
three task workspaces, Encode, Analyze and Compare, and shared Jobs. This is a
roadmap, not shipped capability. Today only Encode exists. Do not show Analyze,
Compare or Jobs as available, or describe them as working, until their flows
land. Analyze starts read-only: no bitstream rewriting, repair or source
mutation. AV2 parsing, validation and codec instrumentation belong in Splot,
not in a second Studio implementation. The direction does not relax any rule
below.

Encode writes IVF and AV2 Annex B `.obu`.
This is video-only output; nonvideo input tracks are inventoried and explicitly
excluded with acknowledgement. MKV/audio/subtitle output is a later gated phase.
Eight-bit and ten-bit SDR are equal targets. HDR/HLG, unproven VFR, live capture,
network sources, and partial-bitstream resume are not silently implemented.

Planned work, known defects and release blockers are GitHub issues (label
`release-blocker`). Do one issue per change. Do not reopen the
Slint/toolkit choice.

## Before editing

Run `git status --short`; inspect touched files, callers, and applicable instructions.
Preserve user changes. Confirm this is splot-studio, not the codec checkout. Use a
phase/issue ID for nontrivial changes and Conventional Commit subjects. Do not
change visibility, publish a release, or change organization settings without
explicit permission. Never expose credentials or signing material.

## Autonomous work

The owner turns autonomous mode on in the task ("autonomous", "take it to
merge") or by handing over a GitHub issue. Without it, a commit, push, PR or
merge needs the task to name it.

In autonomous mode, do the whole path without asking or reporting progress:
branch, implement, commit, run the gate and a code review, push, open the PR
(linked to its issue, if any), address every review comment (fix it, or reply
with the reason it stays), and merge when CI is green. Diagnose failing checks
and conflicts, and continue.

Stop only for a decision that is the owner's:

- The requirement is unclear.
- The best solution is a larger restructure than the task implies, or several
  sound designs exist.
- A rule in this file says to ask or needs explicit permission.
- The action is destructive or cannot be undone.

Ask once, with the options, their pros, cons and effort, and a recommendation.
Continue the work that does not depend on the answer. Never reach the merge
with a hack, workaround, weakened check, skipped test or dismissed comment.

## Crate boundaries

```text
studio-domain   pure job/media/protocol values and policy; no GUI/native/codec/SQL
studio-app      Slint UI and preview presentation -> studio-domain
studio-engine   worker/supervisor/codec orchestration -> studio-domain, splot-* APIs
studio-media    audited FFmpeg input adapter -> studio-domain; added only when used
xtask           standalone development automation
```

Keep codec integration in the engine, not in UI callbacks. No package depends on
the app executable or Splot CLI. No copy of codec algorithms, AV2 parsers/tables,
Y4M reader, or IVF writer when a usable upstream API exists. Preview may be an app
module; extract a crate only for a real boundary. No raw FFmpeg pointers outside
its adapter and no Slint types in the media/job model.

## Toolchain and dependencies

Use the pinned rust-toolchain.toml, edition 2024, resolver 3, and committed Cargo.lock.
Until Splot has releases,
splot-* Git sources track `main` (owner decision, 2026-11-20): Cargo.lock holds the
one exact commit for all of them, and it moves only by a deliberate `cargo update`
with the full gate. No other moving branch dependency, uncommitted path patch,
random prebuilt FFmpeg, or generated latest-version guess. Codec limitations are
reported and fixed in the Splot repository, not worked around here.

Add only dependencies that are used. New dependencies or crate-graph changes
require justification and maintainer approval. Candidate
selection includes compatibility/licence review; “approved candidate” does not
mean every feature or future release is approved. Never weaken licence checks to
silence an unfamiliar LicenseRef.

Do not copy upstream .cargo flags globally. No target-cpu=native in distributed
builds. Establish portable startup and an explicit worker CPU policy. Codec SIMD
redesign belongs in a separate Splot change under Splot's own instructions.

## Coding

Thin binaries, typed library errors with thiserror, strong boundary values, public
API and crate docs. Anyhow only at executable/automation reporting boundaries.
No reachable library unwrap/expect/panic/todo/unimplemented. Test-only assertions
and narrow test allowances are permitted. Never return fake success for a stub.

Original .rs and .slint files carry project SPDX ownership/licence headers.
Comments explain non-obvious invariants/decisions, not the line immediately below.
Prefer cohesive small files; 1000 physical Rust lines is a review target and 2500
a ceiling, not a reason to move code mechanically. No miscellaneous grab-bag module.

Forbid unsafe in domain/engine and prohibit it in app-owned logic. Use a
deny-by-default workspace policy where reviewed exceptions must be possible;
crate-level forbid cannot be undone by a module allow. Necessary generated code
or FFI is isolated. A new app-owned unsafe block requires human sign-off, a narrow module
exception, safety invariants, and boundary tests. No workspace-wide unsafe allowance.

## Media and correctness

Use exact Splot public APIs and documented packet framing. Never invent AV2 syntax,
metadata signalling, constants, or AV1 substitutions. Read upstream source-use
notices before encoder integration; algorithm/spec work follows Splot AGENTS and
its reference gate in a separate PR. AVM is an independent differential oracle;
Splot decode alone is not independent proof.

Preserve 8/10-bit samples without a preview-RGB round trip. Track range, matrix,
primaries, transfer, chroma siting, timing, and geometry separately. Detect unsupported
HDR/HLG. Explicit unknowns beat guesses. VFR must not become frame_index/average_fps
silently. Compare lossy reconstructions to the appropriate decoder/recon oracle,
not to original samples as though encoding were lossless.

Every source stream is accounted for. IVF/OBU never promise audio/subtitle retention.
Acknowledgements are tied to immutable source/job identity. A metadata report is
not a backup of omitted media. Preserve source files in all cases.

## Reliability

No heavy work on the Slint thread. Process-isolate media work. Use bounded queues,
byte budgets, throttled progress, and typed IPC with preallocation length checks.
Keep pixels out of JSON and full-resolution encoding frames out of the GUI.
No blocking codec calls in the protocol reader. No busy polling/idle repaint loops.

Source/destination identity, exclusive temp creation, write/flush failures,
verification, no-clobber publication, and interruption cleanup are mandatory.
Do not equate SQLite transactions with atomic filesystem publication. Queue recovery
is distinct from midstream resume. Do not advertise pause/background continuation
before their semantics and lifecycle tests exist. Process separation is not a
security sandbox; restrict paths/protocols/access deliberately.

## Design and accessibility

Use the Splot branding and semantic tokens in the design brief. Apple-like means
restraint and behaviour, not copied Apple window chrome or bundled Apple fonts.
Use `.slint`, not a web framework. Keyboard/IME, focus, accessible names/roles,
light/dark/high-DPI, clear errors, and responsive cancellation are part of features.
Use AboutSlint visibly through the top-level About entry. Never hide attribution.

## Performance

Measure before tuning. Compare both depths, UI latency under encode load, idle
CPU, startup, memory high-water, copies, and wrapper overhead. Lazy-initialize
heavy processing state and release unused resources. Reuse buffers where possible,
not unsafely. No “zero overhead,” “zero-copy,” speed-win, or portability claim
without evidence and configuration details.

The codec's no-scalar-runtime/SIMD kernel-count rules are not GUI rules. Do not
invent SIMD wrappers in Studio or apply stream-specific performance hacks. Keep
output-equivalent paths correct and report each meaningful regression.

## Validation and handover

`cargo xtask ci` is the full gate and `cargo xtask ci --fast` plus touched tests
is the local loop. Compile real Slint; don't mock it away to pass CI. Check supported
feature combinations rather than blindly --all-features when backends conflict.

Tests include malformed input, truncation, delayed flush, both depths, protocol
errors, worker death, cancellation, file failures, and actual UI behaviour.
Record host/target/renderer/build. Cross-compile is not runtime testing; configured
CI is not executed CI; missing independent tests are not passes.

Report exact checks and results in the pull request. File remaining risks as GitHub
issues. Keep documentation for people: no agent plans, prompts, status reports,
generated matrices or evidence dumps in the repository, and no fake screenshots
or unrun benchmark numbers.

## Licences and approvals

Original project material: PolyForm Noncommercial 1.0.0, Bartosz Tomczyk
<bartekplus@gmail.com>. Third-party components retain their licences. Slint uses
the reviewed attribution-based route; FFmpeg build/licensing is audited separately.
No copied research code/tables/substantial prose by default. No blanket claim that
private visibility, dynamic linking, or source availability resolves licensing.

Ask before new unsafe, unapproved dependencies, codec algorithm changes, ambiguous
AV2 interpretation, licence changes, public publication, or paid services. A blocker
must not be bypassed by disabling tests or silently weakening safety requirements.
