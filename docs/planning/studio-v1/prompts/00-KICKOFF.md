# Kickoff prompt — repository setup and first compiling application

Paste the following prompt into a shell-capable coding agent with this pack
available. It intentionally stops after the foundation milestone, not after the
entire product. Do not paste every other prompt at the same time.

---

You are creating **Splot Studio**, a new native Rust/Slint AV2 desktop encoder
workbench. Execute **STUDIO-00 and STUDIO-01 only**, then stop with evidence.
You have a shell. Work in a new `splotlabs/splot-studio` repository, not in the
existing `splotlabs/splot` checkout. Preserve all existing user work.

## Read first

Read `00-START-HERE.md`, `01-PRODUCT-SCOPE.md`, `02-REPOSITORY-SETUP.md`,
`03-ARCHITECTURE.md`, `05-DESIGN-SYSTEM.md`, `07-AGENT-RULES-MAPPING.md`,
`08-DEPENDENCIES-AND-RELEASE.md`, and `templates/AGENTS.md` in this pack.
Read `prompts/01-FOUNDATION.md` for the exact implementation deliverable.

Reopen the requested current source:
`https://github.com/splotlabs/splot/blob/main/AGENTS.md`.
Also read the pinned baseline version, toolchain, .cargo/config.toml, project
licence, source-use notices, and hero.svg at commit:
`c1183fec4b81d72045d861973a9d8e563c6e9e2e`.
Use GitHub/read-only source access; never substitute an old attached scaffold.

The pinned AGENTS blob is `daf08ea45c6386223f791b1d2046179a357c92bd`.
Report material upstream changes since the baseline. Do not silently change this
product's scope or adopt a different codec revision. Do not claim a source was
read when the fetch failed. If source access is blocked, preserve completed local
work and report the specific source/licence blocker before marking setup complete.

## Settled product decisions

- Name/repo: Splot Studio / splotlabs/splot-studio.
- Rust desktop only: Windows, macOS, Linux. No browser, WebView, or WASM work.
- Slint `.slint` UI, attribution-based royalty-free route accepted by the owner.
- Apple-like restraint and reliability, using existing Splot identity.
- Initial encode output will be IVF and AV2 Annex B `.obu`, video only.
- Audio/subtitle output and MKV are deferred. No silent omission of source tracks.
- Both eight-bit and ten-bit SDR. HDR/HLG is future work, with detection first.
- Direct reusable Splot libraries later, not parsing CLI logs or copying codec code.

Do not ask the owner to decide these again. Choose conservative nonblocking
implementation details and record them. Ask only for a truly material permission,
unsafe/licensing, or unresolved architectural blocker; otherwise proceed.

## Repository creation and authority

Check working directory, `git status --short` where applicable, `git remote -v`,
`gh auth status`, and access to the target repository. Never print tokens.

You may create an empty **private** `splotlabs/splot-studio` remote if it is confirmed
absent and the account has permission. Use the setup guide's `gh repo create`
procedure. If it exists, inspect and reuse it without destructive operations.
An authentication, permission, or network error is not proof of absence.

Create the local checkout in a separate directory. Copy this pack into
`docs/planning/studio-v1/`. Install the adapted AGENTS/CLAUDE/Copilot templates.
Import the exact source project's LICENSE.md and retain ownership. Record
third-party boundaries without inventing licence terms. Do not copy source project
CI, implementation matrices, or CPU flags wholesale.

No pushes, releases, public visibility changes, billing changes, organization
settings changes, or edits to the codec repository. A local commit is permitted
only when clean, scoped, and checks pass; report it. If GitHub permission is absent,
build locally and explicitly report that remote creation remains undone.

## Foundation implementation

Implement the full requirements in `prompts/01-FOUNDATION.md` after installing
root AGENTS.md. Do not merely create empty crates or an architecture document.
Produce a runnable Slint shell and real executable/typed protocol foundations.
Use verified published dependency releases with a lockfile; do not invent versions.
Pin Rust 1.100.0 as the inspected baseline unless a verified compatibility problem
requires a separately explained decision. No FFmpeg build in this milestone.

Do not begin STUDIO-02 encoding in this run. It has its own integration/test gate.

## Evidence and stop condition

Run actual formatter/compiler/clippy/tests and launch the real UI on the available
native host when possible. Configure appropriate CI, but do not call unexecuted
remote jobs successful. No fake screenshots, real-time percentages, encoder
results, signing, or cross-platform test claims.

Update one concise STATUS.md: source provenance, installed rules, implemented
features, dependencies/versions, commands/results, target/host, visual evidence,
known blockers, and next phase. The final response must state whether the remote
was actually created, its visibility, whether anything was pushed (it must not
be), and whether the app actually launched. Stop after STUDIO-01 and point to
`docs/planning/studio-v1/prompts/02-RAW-VIDEO.md`.
