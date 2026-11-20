# Design system and interaction brief

## Direction

A precise, quiet, native desktop workbench. “Apple-like” means strong hierarchy,
careful typography, predictable interaction, appropriate defaults, keyboard access,
restraint, and clear recovery. It does not mean fake macOS chrome, floating cards
on every surface, glass everywhere, animated gradients, or a marketing hero inside
an application window.

Slint is the selected toolkit. Do not reopen the framework comparison in each
phase. Use `.slint` components and Rust view-model code. Available design-taste,
impeccable, or UI review skills may inform hierarchy, spacing, copy, and critique;
do not let a web-oriented skill introduce HTML/CSS/React/Tailwind into this app.
Use the actual pinned Slint documentation for implementation.

## Existing brand evidence

The inspected `docs/assets/hero.svg` in Splot uses these colors:

| Role in original SVG | Value |
|---|---|
| Wordmark cyan | `#5ee7ff` |
| Wordmark violet | `#a78bfa` |
| Wordmark pink | `#ff7ab6` |
| Dark background | `#05050a` |
| Main text | `#f5f5f7` |
| Muted text | `#a1a1aa` |

Source: [S07](09-SOURCES-AND-OBSERVATIONS.md). These are source colors, not proof
of adequate contrast for every widget or light theme. Adapt them into semantic
tokens and measure contrast. Use the gradient sparingly in branding/About, not
as the fill for every control. Use a restrained solid accent for primary actions.

Reuse the Splot wordmark identity and its original licence. Inspect SVG support;
the README SVG includes CSS animation and fonts that should not be assumed to
work unchanged in Slint. Create an attributed static desktop adaptation when
needed; do not fabricate an unrelated logo or package proprietary font files.
Use OS text fonts on their platforms or an explicitly licensed fallback. The
font name in an SVG is not a redistribution licence for that font.

## Tokens and components

Keep semantic tokens in one small Slint module: background, raised surface,
border, text, secondary text, disabled text, accent, focus ring, warning, error,
and success. Add spacing, typography, radius, and motion tokens. A 4/8-unit spacing
rhythm is a starting proposal, not a reason to misalign native controls.

Support system/light/dark appearance. Default follows the system. Respect reduced
motion, high contrast where available, and user font scaling. Numeric values use
stable alignment where supported. Do not use color as the only status indication.

Build only components needed for real screens. Prefer standard accessible widgets
with careful styling over custom rectangles that reinvent focus, text editing,
selection, and screen-reader semantics. No huge internal design-system package.

## Main window

```text
┌ toolbar: Open · source title · preset · output destination · Encode / Add to Queue ┐
│ source/queue sidebar │ preview or intentional empty state │ settings inspector    │
│                     │ source / result / compare          │ Video                 │
│                     │ frame/time navigation              │ Output                │
│                     │                                    │ Source tracks          │
├ collapsible activity: stage · real progress · cancellation · warning details      ┤
└ status: selected input facts, output format, explicit “Video-only” designation     ┘
```

This is layout guidance, not an instruction to build unavailable functions. In
the first slice show only working sections. Do not display functional-looking
pause, audio preservation, MKV, or HDR controls that do nothing.

Keep advanced details under disclosure. Quality and speed have clear descriptions
and units, not invented “best” presets. Map UI controls to verified encoder APIs.
Never claim visually lossless output from an arbitrary quantizer value.

## Required states

Empty state with one clear Open action. File selected/probing. Invalid input.
Unsupported capability with remedy. Ready with preflight summary. Running with
real counters and stage. Cancelling with truthful status. Finalizing. Verifying.
Completed with reveal-output action. Failed with actionable error and retry.
Interrupted after recovery. Unsaved changes/quit confirmation where needed.

Progress may be indeterminate when a duration/frame count is unknown. Display
processed media time or frame count instead of a fabricated percentage. ETA is
an estimate with a warm-up state, not a promise. Distinguish input decoded from
output encoded when lookahead makes them diverge.

## Input and accessibility

Native menus and open/save dialogs where appropriate. Respect each platform's
shortcuts and window controls. Full keyboard operation, visible focus, logical
focus order, accessible names/roles/value text, Escape dismissal, IME composition,
selection/copy/paste, and help text that is not hover-only.

Test VoiceOver on macOS, NVDA on Windows, and the chosen Linux screen-reader
integration on actual systems. A toolkit accessibility feature flag is not proof
that custom controls are accessible. Test 100/150/200% scaling and moving a window
between monitors. Keep truncation intentional and full paths discoverable.

Use the official AboutSlint widget in a menu-accessible About dialog from phase 1.
Keep it legible and operable, not an offscreen or zero-size compliance artifact.
Review the selected licence in [S08](09-SOURCES-AND-OBSERVATIONS.md).

## Preview contract

Start with frame-level source/result preview in phase 5; add continuous video-only
playback only when scheduling and rendering are tested. Source preview need not
wait for a full encode. Result preview must use decoded Splot output, not the
source frame mislabeled as the result.

The source comparator is the exact processed encoder input, with a separate
original-source option when preprocessing exists. Use the same display transform,
scale, crop, and frame identity on both sides. Difference views are diagnostics,
not a substitute for color-correct viewing.

Preview conversion must not feed encoding. Preserve 10-bit encode data even if
the UI surface presents eight-bit SDR. Label diagnostic limits; do not claim HDR
or reference-monitor accuracy. Handle seek/cancel races, stale frames, renderer
loss, and software fallback. No background decode loop when the view is hidden.

## Performance and design review

Target prompt interactions and no unnecessary idle repainting. Lazy-load input
probing, thumbnail generation, recent-file checks, and heavy preview resources.
Batch model updates. A progress update should not rebuild the full screen.

Capture native screenshots for light/dark, empty/ready/running/error/finished,
small/large windows, and representative scaling. Review on real hardware; annotate
the target, renderer, and build. Browser screenshot tooling is not a substitute.

Review each screen for hierarchy, spacing, typography, alignment, focus, clipping,
contrast, clear units, and useful recovery. Remove decorative complexity that
competes with the media. Keep visual checks reproducible without requiring a paid
UI-test product; Slint's separate system-test tooling has its own licensing.
