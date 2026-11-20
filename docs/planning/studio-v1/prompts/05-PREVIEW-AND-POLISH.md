# STUDIO-05 — honest preview and polished native interaction

---

Implement STUDIO-05 only. Read root AGENTS, STATUS, design brief, architecture, and
media contract. Keep Slint and the existing engine boundaries. This phase is not
a new framework evaluation, audio player, HDR implementation, or video editor.

## Frame preview before playback

Build source and result frame preview with precise identity and bounded request
handling. Result frames come from the Splot decoder. A source screenshot must not
stand in for decoded output. The comparison reference is the actual processed
encoder input; expose original-source viewing separately when preprocessing exists.

Use the same display transform/crop/scale for both sides. Preserve source sample
precision; convert only the presentation copy. Handle absent/unknown color facts
explicitly. Do not advertise reference-monitor accuracy, ten-bit display output,
or HDR just because the source is ten-bit or an intermediate texture is floating
point. A transparent SDR preview limitation is better than misleading colors.

## Rendering and transport

Keep parsing/decoding outside the GUI process where possible. Add a bounded binary
preview channel, generation IDs, cancellation, and latest-frame replacement.
No JSON/base64 full-resolution frame traffic; no replay of an entire stream on
every slider movement. Coalesce seek requests and drop stale display work without
dropping encoding frames. Bound thumbnail/cache memory by bytes.

First use the simplest Slint image integration that meets the current interactive
needs. Measure before adding a specialised GPU path. When WGPU interop is needed,
verify the pinned Slint feature and WGPU versions, isolate unstable interfaces,
keep correct texture ownership/thread affinity, and provide a software fallback.
Ordinary GPU texture pointers are not valid cross-process handles.

Add continuous video-only playback only after frame navigation is correct. Use
real media timestamps/clock policy, not an arbitrary animation timer as codec
truth. No audio-sync claim in this release. Raw .obu re-import requests missing
timing rather than silently assuming 30 fps. Unsupported seek behaviour is explicit.

## Complete the application experience

Apply semantic tokens consistently. Refine empty/probing/ready/running/cancelling/
finalizing/verifying/completed/error/interrupted states. Keep advanced settings
progressively disclosed; make video-only output and omitted-stream acknowledgement
unambiguous. UI controls match actual encoder capabilities and units.

Review menus, file dialogs, platform shortcuts, focus order, visible focus,
keyboard-only use, text selection, clipboard, IME, screen-reader names/roles,
reduced motion, high contrast where available, light/dark, mixed-DPI monitors,
small windows, long filenames, localization-friendly strings, and useful errors.
Do not fake Apple window controls on other platforms or embed unlicensed fonts.

Ensure the official AboutSlint widget remains reachable, legible, and interactive.
A web-design skill may critique hierarchy/copy but must not replace the Slint
implementation or invent a new brand palette detached from Splot.

## Testing and evidence

Test exact frame/PTS matching, seek cancellation, stale responses, repeated open/
close, window resize during encode, hidden/minimized preview, memory pressure,
GPU/device loss, software fallback, full/limited-range ramps, and eight/ten-bit
import integrity. Freeze renderer/build details for visual comparisons; allow
expected platform text differences instead of forcing one universal pixel hash.

Use actual native screenshots in light/dark at several window sizes/scales.
Inspect the rendered images; compilation alone is not visual QA. No browser
screenshots masquerading as native tests. Use free/public model/backend tests and
manual/OS automation; do not require an unpurchased Slint testing add-on.

Measure launch, idle CPU/repaint, UI acknowledgement under a busy encode, preview
latency, and memory high-water. Record real hardware/configuration and regressions.
Do not advertise a render-performance guarantee based only on debug startup.

Run full applicable gates, update STATUS with honest platform accessibility/visual
coverage, and stop. Next prompt: 06-RELEASE-HARDENING.md.
