# STUDIO-04 — durable local jobs and recovery

---

Implement STUDIO-04 only. Read AGENTS, STATUS, and architecture, then trace the
existing UI→worker and output-publication code. Keep media processing behaviour
unchanged except for integration needed by the new ownership boundary.

## Target

A per-user local supervisor owns jobs independently of the GUI, starts one encode
worker at a time, persists transitions in SQLite, and reconnects safely. Worker
failure does not destroy the queue. UI crash does not require restarting the
supervisor. Enable continued background encoding only with tested lifecycle rules
and explicit user choice. This is not a system service or an internet server.

## Implementation

1. Add supervisor mode to the engine executable. Keep worker mode reusable from
   prior tests. Move queue and authoritative job state out of UI memory. Use a
   small local IPC mechanism with current-user access, a versioned session,
   handshake, bounded messages, and documented authentication/peer checks.
   Do not replace working typed messages with a general RPC framework.

2. Introduce SQLite/rusqlite persistence with one clear owner. Store immutable
   JobSpec snapshots, source identity, output destination, acknowledgement,
   codec/media-build identity, state/result, and useful bounded diagnostics.
   Use explicit transactions, schema versioning, migrations, and recovery tests.
   Keep the DB in local user application data. Do not place it beside a movie on
   a network share or store every progress tick as a new durable event forever.

3. Implement the state-transition table, idempotent command handling, and one
   active encode. Queue edits affect only not-yet-running jobs and repeat
   preflight when needed. Selection/preset edits do not change an active encode.
   Prevent double supervisors/double execution, including concurrent launch.

4. Retain cancellation responsiveness. Control continues while workers encode;
   terminal states survive UI disconnect. Handle worker exit codes, crashes,
   forced termination, and IPC EOF separately. Do not confuse a reused PID with a
   worker owned by this session. Restrict what the engine can delete or terminate.

5. Add output-publication intent/reconciliation around the existing safe temp
   workflow. Test a crash after file publication but before DB completion.
   Recognize already-published output by reliable identity before rescheduling;
   never overwrite it or mark a random same-name file as this job's result.
   File rename and SQLite commit are not a single transaction.

6. Define normal window close, quit, UI crash, supervisor loss, OS shutdown,
   suspend/wake, and missing destination behaviour. Persist interrupted jobs and
   allow a safe retry. Do not promise automatic mid-bitstream resume or implement
   a misleading pause button. A retry is a new output transaction.

7. Wire the actual queue UI: queued/running/finished/failed/interrupted, reordering
   where permitted, retry, cancel, diagnostics, and reveal output. No invisible
   job activity after an explicit stop/quit choice. Settings and recent files
   must not turn startup into a full disk/media scan.

## Fault tests

Use real child processes plus deterministic injected failures. Kill worker, GUI,
and supervisor at preflight/running/finalizing/verifying/publishing/completed
boundaries. Test disk full, unplugged destination, permission change, duplicate
requests, stale source, incompatible protocol/schema, corrupt settings, failed
migration, simultaneous launches, and cancellation/completion race.

Test that original inputs are untouched, completed output is not clobbered,
nonvideo acknowledgement survives only while valid, failed jobs remain visible,
and a recovered queue does not execute an active job twice. Audit temp cleanup
against symlinks/path replacement and files not owned by the app.

Run full supported CI and a native UI crash/reconnect demonstration. Record what
continues versus what retries. Update STATUS, privacy/diagnostic policy, and any
required migration notes without adding redundant process documents. Stop.
Next prompt: 05-PREVIEW-AND-POLISH.md.
