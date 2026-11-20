# Repository instructions

Read [AGENTS.md](../AGENTS.md); it is the canonical policy.

Splot Studio is a Rust/Slint desktop app. Keep codec and FFmpeg work outside the UI
thread and isolated in the engine. Reuse pinned Splot APIs; no copied codec logic.
Initial output is video-only IVF/AV2 Annex B, with explicit nonvideo omissions.
Both 8/10-bit paths need tests. Preserve source files, bound memory/IPC, report real
progress, and never invent successful verification. Use only the current phase
under docs/planning/studio-v1/. Do not push or publish without permission.
