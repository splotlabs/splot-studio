# Splot Studio — implementation pack

**Prepared:** 20 November 2026  
**Product:** Splot Studio  
**Repository:** `splotlabs/splot-studio`  
**Status:** implementation instructions, not an implemented application

## The decision

Build a Rust desktop application for Windows, macOS, and Linux, with Slint `.slint`
interfaces and Splot's Rust codec libraries. No browser target. First deliver a
polished **video-only AV2 workbench** that writes IVF and AV2 Annex B OBU streams.
Add multimedia output later, after public AV2-in-Matroska mapping and the chosen
implementation meet the compatibility gate.

“Raw video” in this plan means **elementary compressed AV2 output** when referring
to `.obu`; IVF is a lightweight video wrapper. It does not mean uncompressed YUV.
Y4M is the initial uncompressed input path. Ordinary source containers remain
in scope for the subsequent FFmpeg input milestone.

Audio/subtitle **output** is explicitly deferred by this scope change. In the
first video-only release, enumerate those source tracks and make their omission
visible and acknowledged. Do not label them “preserved” because their metadata
was stored in a project file.

## No blocking product questions remain

Use these provisional operational defaults; they are not additional user approvals:

- Create a private staging repository; publishing it is a separate explicit action.
- Keep the existing PolyForm Noncommercial licence and Bartosz Tomczyk ownership.
- Windows x86_64, macOS arm64, and Linux x86_64 are the first required native lanes.
  Assess macOS x86_64, Windows arm64, and Linux arm64 separately; do not advertise
  a target until its build and application behaviour have been tested.
- Start with progressive, constant-frame-rate 8/10-bit SDR. Detect and reject
  unsupported VFR, HDR/HLG, interlacing, and transformations instead of guessing.
- One active encode, a persistent queue by the reliability milestone, no cloud
  services, no mandatory telemetry, no automatic updates in the initial release.
- Use Slint's attribution-based licence, including the official AboutSlint widget
  in a menu-accessible About dialog. Verify the terms of the pinned version.

The initial operating-system version floor is chosen from the actual dependency
and packaging tests, before release; this does not block the first milestone.

## How to use this pack

1. Extract this directory **outside** any existing Splot checkout. Never run the
   repository-creation phase inside `splotlabs/splot`.
2. Open this directory or its parent in a shell-capable coding agent.
3. Paste [the kickoff prompt](prompts/00-KICKOFF.md). It executes repository setup
   and the first compiling application milestone, then stops.
4. Inspect the result and test the native application. Run one subsequent phase
   prompt at a time. Do not tell an agent to implement the entire roadmap at once.
5. Keep this pack in `docs/planning/studio-v1/` in the new repository. Install the
   adapted templates at the locations named in the bootstrap guide. Generated
   handover information goes into one concise `STATUS.md`, not duplicate reports.

The kickoff prompt can create the private remote using an authenticated `gh`
installation. It must not push commits, publish a release, alter the existing
codec repository, or change repository visibility without explicit permission.
This pack itself has not created a GitHub repository.

## Reading order

| File | Purpose |
|---|---|
| [01-PRODUCT-SCOPE.md](01-PRODUCT-SCOPE.md) | Accepted decisions, non-goals, capability and omission policy |
| [02-REPOSITORY-SETUP.md](02-REPOSITORY-SETUP.md) | Safe GitHub setup, version pinning, repo layout, agent installation |
| [03-ARCHITECTURE.md](03-ARCHITECTURE.md) | Ownership, processes, IPC, data model, output transactions |
| [04-MEDIA-CONTRACT.md](04-MEDIA-CONTRACT.md) | Actual Splot API, IVF/OBU details, timing and 8/10-bit requirements |
| [05-DESIGN-SYSTEM.md](05-DESIGN-SYSTEM.md) | Splot branding, Apple-like interaction, accessibility, preview rules |
| [06-IMPLEMENTATION-ROADMAP.md](06-IMPLEMENTATION-ROADMAP.md) | Ordered milestones and acceptance gates |
| [07-AGENT-RULES-MAPPING.md](07-AGENT-RULES-MAPPING.md) | What is inherited, adapted, or deliberately omitted from Splot AGENTS |
| [08-DEPENDENCIES-AND-RELEASE.md](08-DEPENDENCIES-AND-RELEASE.md) | Dependency policy, FFmpeg/Slint compliance, native delivery |
| [09-SOURCES-AND-OBSERVATIONS.md](09-SOURCES-AND-OBSERVATIONS.md) | Exact inspected source revision and primary-source references |

## Execution order

| Step | Prompt | Result |
|---|---|---|
| 0 + 1 | [00-KICKOFF.md](prompts/00-KICKOFF.md) | New repo plus real Slint shell and minimal CI |
| 2 | [02-RAW-VIDEO.md](prompts/02-RAW-VIDEO.md) | Y4M → Splot → IVF/OBU in an isolated worker |
| 3 | [03-FFMPEG-INPUT.md](prompts/03-FFMPEG-INPUT.md) | Broad, audited input support; still video-only output |
| 4 | [04-PERSISTENCE-AND-RECOVERY.md](prompts/04-PERSISTENCE-AND-RECOVERY.md) | Durable queue, supervisor, reconnect, interruption handling |
| 5 | [05-PREVIEW-AND-POLISH.md](prompts/05-PREVIEW-AND-POLISH.md) | Source/result comparison, rendering, complete interactions |
| 6 | [06-RELEASE-HARDENING.md](prompts/06-RELEASE-HARDENING.md) | Tested packages and publication checklist |
| Later | [07-FUTURE-CONTAINERS.md](prompts/07-FUTURE-CONTAINERS.md) | Evidence-gated MKV/audio/subtitle implementation |

A separate [01-FOUNDATION.md](prompts/01-FOUNDATION.md) lets an agent resume after
repository setup without repeating it. All implementation prompts require the
root `AGENTS.md`, the current phase, and the relevant contract documents.

## Evidence standard

This pack is based on inspected source, not a successful Studio build. Known
source observations are distinguished from proposed requirements. Every phase
must record exact checks run, the machine/target, results, and what remains
untested. Cross-compilation is not runtime testing. Never mark placeholders,
mock progress, or disabled functionality as implemented.
