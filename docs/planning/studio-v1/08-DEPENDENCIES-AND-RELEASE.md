# Dependencies, licences, build reproducibility, and release

## Selection policy

Use a small stack and add each dependency when a real feature uses it. Exact
versions are selected from published releases and tested against the pinned
Rust/Slint/FFmpeg combination during bootstrap; do not invent version numbers.
Commit Cargo.lock and the native dependency/build manifest. Record declared
features, licence choice, source, and purpose. An approved candidate is not a
requirement to add it before needed.

| Stage | Candidate | Purpose / boundary |
|---|---|---|
| Foundation | `slint`, matching `slint-build` | Compiled desktop interface; accessibility enabled |
| Foundation | `serde`, `serde_json` | Versioned job/protocol values, not pixel buffers |
| Foundation | `thiserror` | Typed library/application errors |
| Foundation | `clap`, `anyhow` | Thin engine/xtask executable boundaries only |
| Foundation or raw slice | `tracing`, `tracing-subscriber` | Structured bounded logs; no sensitive input by default |
| File selection | `rfd` | Native dialog integration, checked on chosen platforms |
| Raw slice | Pinned `splot-*` Git crates | Actual encoder/decoder/validation; one revision |
| Raw slice/testing | `tempfile` | Safe exclusive temporary files; inspect publication guarantees |
| Testing as needed | `proptest`, `insta` | Small parser/state properties and meaningful snapshots |
| FFmpeg input | `rsmpeg` + compatible FFmpeg build | Thin native input adapter; scrutinize unsafe ownership |
| Persistence | `rusqlite` | Local durable jobs and migrations; bundled SQLite is an option |
| Persistence | `directories` or existing equivalent | Correct per-user app-data paths |
| Persistence if required | `interprocess` | Current-user local IPC; select one mechanism, not many |
| Preview if justified | compatible `wgpu` | Isolated GPU preview; verify Slint interop features/version |
| Later only | `cpal`, `libass`, extra audio codecs | Not required for a silent video-only initial product |

Do not introduce Tokio, SQLx, GStreamer, a web framework, a custom job DSL,
notification/updater framework, or both FFmpeg bindings by default. The small
synchronous control plane plus bounded threads is sufficient until evidence says
otherwise. Reassess candidates when actual APIs are unsuitable rather than forcing
a dependency because it appeared in a previous recommendation.

The selected renderer/backend is pinned and tested. WGPU is not automatically
required merely because the GUI renders with a GPU. Do not enable Slint's complete
feature set or all internal backends indiscriminately. Prefer a stable desktop
backend with accessibility and a tested software fallback.

## Slint licence and attribution

The user accepts the attribution-based royalty-free route. Verify the actual
licence files of the pinned release. The inspected royalty-free v2.0 terms allow
the official AboutSlint widget in an About screen reachable from the top-level
menu, or the specified public webpage badge. Use the AboutSlint route from the
first application milestone; a download-page badge can be added too. [S08]

Do not silently choose GPL for the combined source-available application. Keep
Slint's copyright and licence notices. Teach licence tooling the specific reviewed
LicenseRef expression/clarification rather than disabling all licence checks.
The project's PolyForm licence covers original project material, not third-party
libraries, SVGs, fonts, legal texts, or standard mirrors.

Slint's separate commercial/system UI-testing tool is an add-on. Do not make CI
require an unpurchased token. Use ordinary Rust/model/process tests, the pinned
public testing backend where appropriate, screenshot/manual platform checks, and
separately evaluated test tools. The testing backend alone does not render pixels
or prove native accessibility. [S12]

## FFmpeg distribution policy

The initial FFmpeg deliverable is input decode/probe, not AV2 muxing or audio
encoding. Build a controlled LGPL-compatible variant with shared libraries, without
`--enable-gpl` and without `--enable-nonfree`. Prefer a limited, documented build
and intentional additions over a downloaded “full” binary of unknown provenance.
Audit every external library and native dependency. [S11]

Record FFmpeg commit/release, source archive hash, configure flags, toolchains,
patches, native library versions, target, and transitive native licences. Provide
corresponding source/build instructions/notices with distribution as required;
dynamic linking is not a complete compliance checklist on its own. Preserve the
ability and rights to replace/debug LGPL components in the chosen packaging model.
Use a qualified release review for the actual shipped combination; this document
is an engineering policy, not a legal opinion.

Do not relicense FFmpeg under PolyForm or apply the noncommercial restriction to
its separately licensed components. Do not add patent/royalty guarantees merely
because an input decoder is compiled. Do not copy external encoder source as a
shortcut to implementing Splot functionality.

## FFI policy

The GUI/domain/engine logic forbids unsafe. The native adapter uses the smallest
reviewed unsafe module with explicit ownership, lifetime, thread-affinity,
allocation/free, buffer-size, and error-handling invariants. No safe API may expose
a way to violate those invariants. A new user-authored unsafe block needs human
review before merge; if the agent cannot obtain it, prepare the safe boundary and
review note and keep the feature disabled rather than quietly weakening the rule.

Do not blanket-allow unsafe at workspace scope to satisfy generated code. Isolate
necessary generated bindings and upstream code with narrowly scoped configuration;
keep app-owned unsafe review separate from transitive dependency implementation.

## Runtime loading and packaging

Ship the tested native library bundle. Never require ordinary users to install
FFmpeg separately, and do not search arbitrary writable PATH entries for production
DLLs. Set correct per-platform loader paths/rpaths and test from an installed
location without development environment variables. Prevent dependency hijacking.

macOS: package an application bundle with engines/libraries in suitable locations;
sign nested code, sign the app, notarize and validate when credentials are supplied.
Windows: signed installer/executables where available; test names, Unicode paths,
DLL loading, and uninstall. Linux: choose a documented packaging target and ABI
baseline; test portals, fonts, window systems, renderer fallback, and audio only
when implemented. Cross-built binaries are not considered tested packages.

Pin OS runner/SDK floors deliberately. Keep release jobs separate from untrusted
PR jobs, protect signing credentials, and avoid runtime downloading of executable
components. Do not enable auto-update or auto-publication as a convenience.

## CPU policy

Do not compile the whole app with `target-cpu=native`. Start with a portable UI and
engine control path. Inspect/test Splot's actual dispatch and requirements for the
pinned revision. If a specialised worker build is necessary, compile it separately
with a documented baseline and choose/check it before launch; never execute
unsupported instructions just to discover the CPU is incompatible.

SIMD dispatch redesign belongs in Splot and a separate reviewed change. Studio
must not duplicate kernels or claim AVX-512 availability from a repository name.
Measure both 8/10-bit, startup, copy overhead, and UI latency on real hardware.

## Release checklist

A release requires all advertised targets tested, exact build identities, complete
attribution, matching native source artifacts, clean-machine installation tests,
codec/preview verification, omitted-stream warning, safe cancellation/recovery,
privacy review, and human publication approval. No uploads by default. Logs must
not include frame contents, credentials, or unnecessary full private paths.

Unsigned local artifacts can be useful test builds. Label them accurately and do
not claim notarization, signing, cross-platform validation, or an independent
codec test that was not performed. The website supplies downloads/documentation;
no encoding server or browser runtime is needed.

Source references [S08, S11, S12, S13] are indexed in
[09-SOURCES-AND-OBSERVATIONS.md](09-SOURCES-AND-OBSERVATIONS.md).
