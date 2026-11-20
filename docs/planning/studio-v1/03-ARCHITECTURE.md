# Architecture and reliability contracts

## Keep the permanent boundaries; introduce their machinery in stages

```text
studio-app ───────────────> studio-domain
    │                          ▲
    │ local commands/events    │
    ▼                          │
studio-engine ──────────────────┘
    ├── direct Splot Rust library calls
    └── studio-media (introduced with FFmpeg)
            └── pinned FFmpeg libraries through a narrow adapter

xtask: independent build/test/development automation
```

`studio-domain` has no Slint, FFmpeg, Splot codec, GPU, SQL, or process-management
types. It contains values and pure policy: validated job settings, source inventory,
output kinds, timestamp/geometry/color descriptions, protocol messages, and typed
application errors. Do not create a duplicate AV2 syntax model there.

`studio-app` owns view state, presents engine data, sends commands, and renders
preview. It never encodes or decodes video on the UI thread and never imports a
codec or FFmpeg directly. Disk probing is asynchronous to the interface.

`studio-engine` initially owns a per-job worker. It gains supervisor mode and the
durable queue in the persistence phase. Splot integration is a module, not a new
codec framework. `studio-media` is extracted specifically for the unsafe/native
library boundary, not for every input format.

## Runtime progression

**Raw-video phase:** UI → one child `splot-studio-engine worker` process, over
framed inherited pipes. UI exit stops the job unless a tested policy says
otherwise. State clearly that background continuation is not implemented yet.

**Reliable release:** UI → per-user local supervisor → per-job worker. Supervisor
and worker may be modes of the same engine executable. The supervisor owns SQLite,
queue transitions, job scheduling, process handles, and output-publication state.
The GUI can reconnect after a crash. No system service or TCP listener is needed.

Using the same dynamically linked executable for two roles may map native
libraries into both processes. Do not claim that linking guarantees only workers
load FFmpeg. Keep file parsing and decoding in workers regardless.

One encoding worker at a time initially. Probing/preview workers are short-lived
or explicitly reusable; cap their memory and coordinate their CPU usage. Do not
create a worker per frame or multiple competing unbounded thread pools.

## Protocol

Use a small versioned command/event protocol. Begin with length-prefixed UTF-8 JSON
for control only. Choose one framing scheme, cap message sizes before allocation,
and test partial reads, unknown messages, incompatible versions, invalid lengths,
EOF, and a saturated output pipe. Never parse arbitrary codec console logs.

Include protocol version, request ID, job ID, and typed command/event payloads.
A handshake reports build identity and supported capabilities. A mismatched
installed UI/engine pair must fail clearly, not corrupt saved state.

Control messages include start, cancel, query status, and shutdown. Later add queue
commands only as implemented. Events distinguish preflight, input progress, encoded
frames, finalization, verification, warnings, cancellation, and terminal status.
An acknowledgement of cancel is not a claim that the worker has already stopped.

Progress is coalescible and rate-limited (initial target <=10 updates/second).
Terminal events and important warnings must not be dropped. Read/write protocol
streams independently of a blocking encoder call. Keep stderr for bounded logs;
stdout carries only the framed protocol in worker mode. No busy-wait loops.

Local sockets/named pipes in the supervisor phase must restrict access to the
current user, verify the peer/session as available, and use an unpredictable
session token or equivalent trusted handoff. Keep tokens out of arguments/logs
where exposure is unnecessary. Validate lengths and paths even on local IPC.
Inherited pipe endpoints are preferred for the initial parent-child trust boundary.

Paths must round-trip as native OS paths. Do not use lossy UTF-8 conversion for
Unix filenames or assume all paths fit a UI string. Define a tagged transport
representation or opaque engine-owned source handles; a display label is not a
filesystem authority. Test Unicode and non-UTF8 paths where supported.

## Heavy data

Keep source decode → conversion → Splot encode → output writing within one worker.
Do not send full-resolution encoding frames through the GUI or JSON.

Preview gets an independent bounded data path. Start with small capped binary
images/planes on a separate channel and a bounded latest-frame slot. Shared memory
or native GPU handles are later optimizations, requiring measured benefit and an
ownership/security design. One process cannot safely export an ordinary in-process
pointer as a cross-process texture handle.

Stale preview requests carry generation IDs and are discarded. The UI never
queues every frame of a long encode. Thumbnail generation is on demand, not a full
file scan every time the inspector opens.

## Job model

Suggested values, not a demand for one source file per type:

- `SourceId`, source identity/fingerprint, complete `SourceInventory`.
- `VideoSelection`, `ColorDescription`, `VideoGeometry`, rational `TimeBase`.
- `EncoderSettings`, `OutputKind::{Ivf, Av2AnnexB}`, `OutputDestination`.
- Immutable resolved `JobSpec`, including acknowledgement of omissions.
- `JobState`, typed diagnostics, `VerificationReport`, build/provenance information.

Serialize a versioned application DTO, not arbitrary internal Splot structs.
Translate to the pinned public codec API inside the engine. Do not serialize Rust
type names or rely on unstable enum debug strings as a durable format.

Validate at UI boundaries for helpful feedback, then authoritatively in the
engine. The latter cannot assume a well-behaved GUI. No floats for authoritative
media timestamps. Use checked rational arithmetic and explicit rounding policies.

## State transitions

```text
Draft → Preflight → Ready → Queued → Running → Finalizing → Verifying → Completed
                         ↘ Cancelled        ↘ Failed       ↘ Interrupted
```

The first slice may omit Queued persistence. The reliable release needs an explicit
transition table and tests, including racing cancel/completion, disconnect, crash,
and duplicate requests. A terminal result has an idempotent durable identity.
“Completed with warnings” can be a result flag; do not invent another state for
every diagnostic.

Preset changes do not mutate a running job. Store resolved values, preset version,
codec revision, native-media build identity, and stream selection. Editing an
already queued job creates a revised spec and repeats preflight.

## Cancellation and process lifecycle

A control thread can request cancellation while a synchronous encode call runs.
Check a cancellation flag at safe boundaries. Do not falsely promise immediate
cooperative cancellation inside an opaque codec call; measure worst-case latency.
An escalation path can terminate an unresponsive job worker and mark its output
unfinished. Killing the worker must not kill the queue supervisor.

Do not fake “pause” with zero progress or promise byte-exact resume. Initially
support cancel and retry; queue recovery is not mid-bitstream checkpointing.

Track real process handles, not only stored PIDs; PIDs are reused. Prevent double
supervisors with a tested per-user single-instance mechanism. Define UI close,
normal quit, shutdown, and lost-supervisor policies, including orphan cleanup.
Background continuation requires explicit user consent and visible status.

## Safe file publication

1. Resolve and validate the destination; detect source/destination identity,
   including aliases/hard links where possible, not only identical path strings.
2. Create a unique temporary file in the destination directory using exclusive
   creation. No user source or existing final file is ever truncated by preflight.
3. Stream output; handle partial writes, flush errors, full disks, and device loss.
4. Finalize the output and perform the configured verification on the temporary file.
5. Record a publication intent once persistence exists. Close handles, sync as the
   documented durability policy requires, and publish with a platform-appropriate
   no-clobber operation. A pre-check followed by a clobbering rename is not safe.
6. Mark completion only after publication succeeds. Reconcile crash windows using
   output identity and the publication record; do not re-encode over a completed file.

Do not claim a filesystem rename and a SQLite transaction are one atomic operation.
Keep the queue DB on a local application-data filesystem. Network/removable output
filesystems may have weaker rename/durability behaviour: detect failures and state
the supported guarantees. Retain or remove unfinished files according to a visible
policy, with ownership checks. Do not recursively delete arbitrary user paths.

## Memory and performance

Budget in bytes, not just frames. Include decoded input, packed encoder input,
lookahead/references, verification, preview textures, and IPC buffers. Splot's
current owned SourceFrame stores u16 for both bit depths; eight-bit input does
not imply one byte/sample in the encoder. [S06](09-SOURCES-AND-OBSERVATIONS.md)

Validate untrusted dimensions/strides/counts before multiplication/allocation.
Use bounded queues and streaming reads. No whole-movie raw intermediate. Reuse
buffers where the APIs permit it; do not label an ownership-converting copy
“zero-copy.” Do not redesign the codec's frame type just to prettify the wrapper.

Measure wrapper overhead separately from codec throughput, at both bit depths.
Measure idle CPU, cold/warm launch, UI response under full encode load, memory
high-water, preview cancellation, and finalization. Performance numbers need a
hardware/configuration description. Treat UI response <=100 ms for basic command
acknowledgements as an initial engineering target, not an unmeasured guarantee.
