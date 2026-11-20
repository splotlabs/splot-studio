# Repository setup and bootstrap

## Safety and authority

Create `splotlabs/splot-studio`, not a folder inside the existing `splot` workspace.
This guide authorizes the coding agent to create a **private empty staging remote**
when invoked through the kickoff prompt. It does not authorize publishing source,
pushing commits, changing visibility, modifying organization settings, or modifying
`splotlabs/splot`.

Check the authenticated account and organization access. Never print tokens. Do
not interpret a permission/network error as proof that a repository is absent.
If the repo exists, inspect it and preserve all existing work; do not reinitialize,
force-push, delete, or replace it. Stop remote mutations on a permission mismatch
and continue locally where possible, recording the exact blocker.

## Manual creation path

These commands are for an authenticated shell with Git and GitHub CLI. Run them
from a parent directory, after verifying the destination does not already exist.
The repository owner/name is explicit so it cannot accidentally become a personal
repository. GitHub CLI documents these flags in [S09](09-SOURCES-AND-OBSERVATIONS.md).

```bash
gh auth status
gh repo view splotlabs/splot-studio --json nameWithOwner,visibility,url
```

If the repository is confirmed absent and you have creation permission:

```bash
gh repo create splotlabs/splot-studio --private --clone \
  --description "Native Rust AV2 encoder workbench. Slint UI. Local processing." \
  --disable-wiki
cd splot-studio
git status --short
git remote -v
```

The backslash syntax above is Bash/Git Bash; in PowerShell use the same `gh repo
create` command on one line. Build/test Windows binaries with a native Windows
Rust toolchain, not merely inside WSL.

For a newly created empty repository, set its unborn branch to `main` before the
first commit (`git symbolic-ref HEAD refs/heads/main`). For an existing repository,
inspect its branch instead; do not rename it casually.

No `--license` template flag: import the exact project licence from the pinned
Splot revision rather than substituting MIT/GPL or generating legal prose.
Do not pass `--push`. After review, the owner can authorize:

```bash
git push -u origin main
```

Publishing a private repository requires a separate deliberate visibility change.
Public source availability does not turn PolyForm Noncommercial into an open-source
licence. GitHub Pages is irrelevant to this application repository.

## Initial local files

Create only these compiling members in the foundation milestone:

```text
splot-studio/
  Cargo.toml                 # virtual workspace, edition 2024, resolver 3
  Cargo.lock
  rust-toolchain.toml
  AGENTS.md                  # installed from this pack's adapted template
  CLAUDE.md                  # short @AGENTS.md import
  README.md
  STATUS.md                  # one concise current handover
  LICENSE.md                 # exact Splot project licence
  THIRD-PARTY-NOTICES.md      # app dependencies; not a duplicate research library
  .cargo/config.toml         # aliases; no unreviewed global CPU flags
  .github/copilot-instructions.md
  .github/workflows/ci.yml
  .gitignore
  .editorconfig
  crates/
    studio-domain/           # job/config/protocol value types
    studio-engine/           # headless executable + reusable engine modules
    studio-app/              # Slint executable, UI-owned interaction state
      ui/
        app-window.slint
        tokens.slint
  xtask/                     # small, real automation commands
  docs/
    planning/studio-v1/      # this pack, copied once
```

Use binary names `splot-studio` and `splot-studio-engine`. Keep internal package
names `studio-app`, `studio-engine`, `studio-domain`, and `xtask` consistent.
Add `crates/studio-media/` only in the FFmpeg phase, when it contains a real
adapter and audited FFI boundary. Add `native/ffmpeg/`, `packaging/`, and fixture
folders as their phases need them. Keep preview in an app module until a separate
crate has a demonstrated ownership/build benefit.

Do not create empty libraries for future audio, muxing, HDR, plugins, or every
screen. Introduce modules for real responsibilities, not a speculative architecture.

## Source baseline

The inspected Splot baseline is:

```text
repository: https://github.com/splotlabs/splot
commit:     c1183fec4b81d72045d861973a9d8e563c6e9e2e
AGENTS blob:daf08ea45c6386223f791b1d2046179a357c92bd
toolchain:  1.100.0
edition:    2024
resolver:   3
```

Use this revision initially. A newer revision requires a deliberate review and
one consistent dependency update, not an unpinned `branch = "main"`. At bootstrap,
read both the pinned instructions and current upstream AGENTS for material changes;
report differences, do not silently replace this pack's scope or baseline.

The source project is active; this is an inspected baseline, not a claim that it
remains latest. Do not use the attached May scaffold as current architecture or
revert to its Rust 1.96/validator-placeholder assumptions.

When the codec milestone starts, centralize dependencies, for example:

```toml
[workspace.dependencies]
splot-encode = { git = "https://github.com/splotlabs/splot", rev = "c1183fec4b81d72045d861973a9d8e563c6e9e2e" }
splot-decode = { git = "https://github.com/splotlabs/splot", rev = "c1183fec4b81d72045d861973a9d8e563c6e9e2e" }
splot-validate = { git = "https://github.com/splotlabs/splot", rev = "c1183fec4b81d72045d861973a9d8e563c6e9e2e" }
```

Add `splot-core` or `splot-recon` at that same revision only when actually used.
Do not duplicate types already publicly re-exported by `splot-encode`. No Git
submodule or copied codec source is needed. An explicit local `[patch]` may be
used for a separate codec PR, but never ship an uncommitted developer-path patch.

## Build policy

Pin the verified toolchain; do not guess the latest Rust or Slint version. Resolve
an actual compatible published Slint release and matching `slint-build`, pin it,
and commit Cargo.lock. Use the current stable APIs of that release, not examples
from another release or Slint master. Record the chosen features and native SDKs.

Cargo configuration and build profiles require explicit attention in this new
root. Splot's dependency checkout does not automatically impose its own root
`.cargo/config.toml` and profile policy on Studio builds. Inspect them; adopt only
what is relevant. [S04, S10](09-SOURCES-AND-OBSERVATIONS.md)

In particular, do not globally copy `target-cpu=x86-64-v3`, `target-cpu=native`,
`disable_dispatch_avx512`, or large-stack settings into the GUI. Establish a
portable launcher and test codec requirements separately. Configure a worker
stack explicitly if codec calls require it; `RUST_MIN_STACK` is not a policy for
all threads or the main thread.

For usable debug iteration, add targeted codec package `opt-level` overrides at
the Studio workspace root when measurements show they are needed. Keep assertions
and overflow checks for development/tests. Do not optimize every dependency or
add release-only flags to the whole workspace without a measured reason.

## Unsafe lint implementation detail

Set a deny-by-default workspace policy when narrowly scoped exceptions are needed;
use `#![forbid(unsafe_code)]` in wholly safe crates such as domain and engine.
Do not set an irrevocable inherited `forbid` in a crate and then try to override it
with a module `allow`. The later media adapter uses crate-level deny plus a human-
reviewed allow in its specific FFI module. Keep hand-authored GUI code safe. If the
pinned Slint compiler emits code that needs an exception, confine the generated
binding module and document the exact reason; do not relax all app-owned code.

## Agent rules and contribution plumbing

Install [templates/AGENTS.md](templates/AGENTS.md) as root AGENTS.md. Install
[templates/CLAUDE.md](templates/CLAUDE.md) and
[templates/copilot-instructions.md](templates/copilot-instructions.md) at their
named locations. Use a real file rather than a symlink for Windows reliability.
Review [the mapping](07-AGENT-RULES-MAPPING.md); this is an adaptation, not blind
inheritance of validator/kernel instructions.

Use Conventional Commits and small reviewed phases. No mandatory number of
commits, artificial line-budget games, or code-generation ceremony. CI uses least
privilege, verified action revisions, lockfile-based caching, and no production
signing secrets in PR jobs. Never add default private CI spending/billing changes.
Native matrices may incur runner costs; keep jobs scoped and document this.

Implement `cargo xtask ci` and `cargo xtask ci --fast` before documenting them as
working commands. A minimal `doctor` can check installed tools without downloading,
modifying a machine, or hiding failures. Do not clone Splot's many repository-specific
checker commands unless their implementation and policy are actually needed here.
