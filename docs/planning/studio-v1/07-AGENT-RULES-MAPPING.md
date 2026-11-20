# Adapting Splot AGENTS for Studio

## Provenance and scope

Requested source: `https://github.com/splotlabs/splot/blob/main/AGENTS.md`.
Inspected on 20 November 2026; corresponding pinned commit:
`c1183fec4b81d72045d861973a9d8e563c6e9e2e`.
AGENTS blob: `daf08ea45c6386223f791b1d2046179a357c92bd`.
Source links: [S01](09-SOURCES-AND-OBSERVATIONS.md).

The new root template is a **Studio adaptation**, not a verbatim copy and not an
instruction for modifying the original repository. If a separate change is made
inside Splot, its own current AGENTS governs that change.

## Carry over

| Source rule | Studio treatment |
|---|---|
| Ask when material; proceed on safe nonblocking assumptions | Keep; do not reopen decisions accepted in this pack |
| Fit solution to problem, scoped diff, explicit uncertainty | Keep |
| Lazy senior developer: reuse, standard library, minimum new code | Keep as an engineering rule, not an excuse for incomplete product behaviour |
| No unnecessary abstractions/dependencies/boilerplate | Keep; approved phase dependencies are bounded, not mandatory all at once |
| Root-cause fixes and caller inspection | Keep |
| Do not be lazy about safety, loss prevention, accessibility, real hardware | Keep and emphasize |
| `git status`, preserve work, Conventional Commits | Keep |
| Thin entrypoints, typed errors, strong public values, docs, SPDX | Keep |
| Measure before optimizing, byte-exact preservation where required | Keep with Studio-specific workload and correctness definitions |
| 8-bit and 10-bit are equal targets | Keep for conversion, encoding, verification, and tests |
| AV2 syntax honesty and AVM differential checks | Keep; route syntax changes to Splot, not GUI copies |
| Source-use notices and third-party boundary | Keep; dependencies are not relicensed under PolyForm |
| Small/high-signal docs and comments | Keep; no generated bureaucracy or filler |

## Adapt deliberately

| Source detail | Studio decision |
|---|---|
| Validator-first project description | Replace with desktop encoder workbench and video-only first release |
| Codec crate graph | Replace with domain/app/engine/media graph; never depend on the CLI |
| Feature IDs from codec implementation matrix | Use STUDIO-00…07 phase/issue IDs; do not clone a huge codec status matrix |
| `anyhow` only CLI/xtask | Permit only executable/automation error reporting; library boundaries stay typed |
| Unsafe forbidden except named codec areas | Keep safe defaults; studio-media/platform interop needs separately reviewed narrow exceptions |
| Native 8/10-bit CPU kernel rules | Keep inside Splot; Studio measures frame copies, UI responsiveness, memory, and process overhead |
| Decoder-specific module naming | Use actual Studio domains: jobs, media, protocol, storage, preview, presentation |
| Fast/full xtask commands | Implement a small real Studio equivalent; do not reference nonexistent commands |
| Reference gate before encoder-facing work | Read upstream notices/API contracts; codec algorithm research requires upstream references and separate PR |
| Rust file size guidance | Prefer cohesive small modules; inherited 1000-line target/2500-line ceiling is a review guard, not a gaming exercise |
| Human approval for each dependency | Listed phase-specific dependencies are proposed/preapproved by the plan's execution; other additions require justification and sign-off |

## Do not import into Studio wholesale

The source's no-scalar-runtime sample-processing rule, exact architecture-kernel
count, x86-64-v3 floor, unsafe-intrinsics policy, stream-specific benchmark commands,
and retired-instruction measurement prescription are codec rules. Applying them
to GUI layout, UTF-8 parsing, FFmpeg conversion glue, or software preview would be
incorrect. Respect them when changing Splot itself.

Likewise, do not copy `check-duplication`, `check-feature-status`,
`check-diagnostic-registry`, `check-doc-budget`, `check-ai-slop`, or
`check-comment-density` command names into Studio as though implemented. Start
with compiler/tests and minimal repo checks. Add enforcement only when there is a
real policy and demonstrated value; do not re-create every codec gate for an app.

Do not copy the codec `.cargo/config.toml` globally. At inspection it includes
x86-64-v3 and disable_dispatch_avx512 flags. The launcher needs a deliberate CPU
policy; downstream configuration is not inherited automatically.

## Explicit distinction: lazy developer versus lazy runtime

The source phrase concerns efficient engineering. This plan **also proposes**
lazy runtime initialization: open without codec pools/FFmpeg probing/preview decode,
create heavy state only on demand, release resources when no longer used, and avoid
idle polling/repaint. That is a Studio requirement derived for this product, not a
claim that the original AGENTS already specified this exact application behaviour.

## Template installation

Copy `templates/AGENTS.md` to the repository root, `templates/CLAUDE.md` to the
root, and `templates/copilot-instructions.md` to `.github/copilot-instructions.md`.
Root rules are canonical. The pack remains a task specification, not a second
contradictory instruction hierarchy. Record future deviations once, deliberately.
