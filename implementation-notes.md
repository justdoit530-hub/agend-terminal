# Implementation notes — TUI keystroke activity write

## Debounce

`INPUT_ACTIVITY_DEBOUNCE` is 250ms.

The window is the short end of the 250ms–1s range in the task. A burst of composing keys becomes one metadata fsync. The deadline is set by the first key of the burst and is not pushed out by later keys, so continuous typing still lands about once per 250ms instead of being deferred until the operator pauses. The value written is the keystroke's own `timestamp_millis`, not the flush time.

250ms stays inside the slack of the 1.5s typing-quiet window and the daemon idle/stuck readers. A longer window would make those cross-process readers staler without making the hot path any shorter.

## Same-process reads

`record_input_activity` updates an in-memory timestamp before it enqueues the disk write. `read_input_submit_timestamps` returns the newer of that cache and the file. Draft gating in this process (`draft_state`, `operator_typing_recent`) therefore still sees the keystroke immediately.

The daemon is another process. It keeps reading the file and can be up to one debounce window behind. That is the delay the task budget allows.

The cache and the writer both ignore an older timestamp, so a coalesced or reordered enqueue cannot move `last_input_epoch_ms` backwards.

## What stayed synchronous

`record_submit_activity` (Enter) still writes immediately. It is not on the per-character path. The debounced input write is monotonic with it: a late input timestamp older than the last successful write is dropped, so a delayed burst cannot rewind a newer submit/input pair.

## Deviations

- `write_to_pane` shares `record_input_activity`, so mouse-forwarded composing input takes the same path as `write_to_focused`.
- `store.rs` and the metadata schema are unchanged.
- The #1680 regression test flushes and drops the cache before it reads, so it still pins the on-disk UUID path rather than the in-memory overlay.

# TuiEvent resize gate

`AppState::handle_tui_event` used to set `needs_resize = true` for every
successful `TuiEvent`. The next loop then ran `apply_pending_resize`, which
calls `resize_panes` and `terminal.clear()`.

## Variant classification

| Variant | needs_resize | Why |
| --- | --- | --- |
| `InstanceCreated` | yes | Adds a tab or splits a pane. Geometry is wrong until `resize_panes`. |
| `InstanceDeleted` | yes | Closes a pane or a tab. Remaining panes must be reflowed. |
| `TeamCreated` | yes | Opens a team tab and attaches member panes. |
| `TeamMembersChanged` | yes | Moves members into or out of the team tab. |
| `PaneMoved` | yes | Relocates a pane across tabs (split or new tab). |
| `ScreenshotRequest` | no | Draws into a throwaway `TestBackend` and returns SVG. The live grid is unchanged. |

`Event::Resize` (real terminal window drag) is not a `TuiEvent`. It stays in
`handle_crossterm_event`, which already calls `resize_panes` and
`terminal.clear()` inline. This change does not touch that arm.

A new `TuiEvent` variant fails to compile until it is listed in
`tui_event_changes_layout`, so the next author has to classify it.

## Assumptions

- `InstanceDeleted` / `PaneMoved` can no-op when the agent is not on screen.
  Those events are operator or API actions, not the notification flood. They
  still request a resize. One extra clear on a rare no-op is cheaper than
  missing a real reflow. The handler does not report whether it mutated the
  layout, and this change does not thread a new return value through it.
- Every non-layout event still sets `dirty = true` before the match, so the
  existing 33ms frame throttle still repaints badges and pane output.

## Inbox notifications are not TuiEvents

The report that tied screen flash to inbox traffic assumed inbox delivery
arrived as a `TuiEvent`. It does not. `TuiEvent` is only the API lifecycle
channel (`Instance*` / `Team*` / `PaneMoved` / `ScreenshotRequest`), produced
by `TuiNotifier` from `ApiEvent`.

Inbox and badge updates take two other paths, both of which already avoid
`terminal.clear()`:

- `pre_select` → `sync_badges` → `sync_notification_state` rewrites
  `pending_notification_count`. No `needs_resize`.
- Queued text is injected into the pane PTY. The forwarder wakes the loop via
  `handle_wakeup`, which only sets `dirty = true`.

So this gate stops `ScreenshotRequest` (and any future non-layout variant)
from clearing the screen. It does not change the inbox redraw path, because
that path never called `terminal.clear()`. If the photographed ghosts continue
after inbox bursts, the next place to look is a spurious `Event::Resize` on
the crossterm reader, which clears inline and is outside this task.

# Event::Resize dimension dedup

## `last_resize_dims` initial value

`AppState::new()` sets the field to `crossterm::terminal::size().unwrap_or((0, 0))`.

The primary seed is the terminal size at startup, not a hardcoded `(0, 0)`. On Unix, crossterm turns `SIGWINCH` into `Event::Resize`. A tmux/screen pane change, or dragging a window edge, delivers events whose size did not change. A `(0, 0)` seed would treat the first of those as a change and still `resize_panes` + `terminal.clear()`. Seeding the live size skips that first duplicate.

`(0, 0)` is only the fallback when `crossterm::terminal::size()` fails (no TTY, ioctl error). It is not a usable terminal size, so the first real `Resize` still counts as a change and the #1140 clear still runs. That path does not drop the first real resize.

## Why the first real resize is not skipped

The arm compares `(cols, rows) != last_resize_dims`.

- Ioctl succeeded at startup and the size later changed: the event differs from the seed, so the arm runs `resize_panes` and `terminal.clear()`, then stores the new size. The #1140 wide-char ghost clear stays.
- Ioctl succeeded and the event size matches: the whole arm is skipped, including `resize_panes`.
- Ioctl failed and the seed is `(0, 0)`: the first non-zero event differs, so it still clears.

`apply_pending_resize` (`needs_resize`) is unchanged. It clears from its own `crossterm::terminal::size()` read at the top of the loop. If that read differs from the seed, a later `Event::Resize` of the new size still clears, because it does not match the seed. That is an extra clear, not a missed real resize.

## Out of scope

No debounce or coalescing timer. A drag that changes the size on every event still clears each time. This change only drops events whose size is the same as the last one applied.

# typed-inject large-payload file fallback

Task: `t-20261104082809734341-20435-1`

## Deviations

- The pre-flight check runs only when `InjectTarget.typed_inject` is true, inside `inject_with_target` after ANSI strip and `trim_end`. The bulk path is unchanged. It does not chunk per byte and it does not latch `typed_inject_contaminated`, so spilling those payloads would change Claude's fast path without hitting the bug this fixes.
- "Over threshold" is `len() > 2048` bytes of that stripped text, not chars and not the raw pre-strip buffer. A payload of exactly 2048 bytes stays inline.
- Spill files go to `std::env::temp_dir()` with the prefix `agend_inject_`. That matches `image_paste`'s system-temp convention (`agend_paste_`). This repo has no inject scratchpad directory to reuse.
- The typed line is `[AGEND-INJECT-FILE] ... Path: <abs>`. It does not keep a leading `[AGEND-MSG]` or `[from:]` header. Those headers are inside the file. Keeping them on the typed line would put the short instruction through the system-header atomic-write path and would not match the "replace the inline text" contract.
- A spill I/O error, or a generated instruction that is itself over 2048 bytes, returns `PtyWrite` and does not latch contamination and does not type the original payload.

## Edge Cases

- Deferred injects (`inject_with_target_gated`) still enqueue the original text. The spill happens when the flush calls `inject_with_target`, so the file exists at the moment the agent is told to read it.
- Invalid UTF-8 is lossy-converted before the size check, same as the existing typed path.
- Each spill uses pid + atomic sequence + nanoseconds. Files older than one hour and named `agend_inject_*.txt` are removed best-effort on the next spill in that directory.
- The short instruction stays on one line so `inject_sentinel` / readback-confirm keep working. `READBACK_TIMEOUT`, chunking, `inject_sentinel`, and `observe_post_submit` are untouched.
- `src/agent/mod.rs` is grandfathered at 3205 lines. This change leaves it at 3164.

## Blocked

- None.

# CI: audit advisories and macOS spawn path

PR #119 run 44455147810 failed audit, then Check (macos-latest).

## Audit

- `RUSTSEC-2026-0459` is `libsqlite3-sys` 0.28.0 (C SQLite CVE-2026-119375), patched in `>= 0.39.0`. `rusqlite` 0.41.1 is the first release that depends on that line. It declares rustc 1.89, so `rust-version` and the CI and release MSRV gates move from 1.88 to 1.89. `default-features = false` keeps 0.41's `ffi-sqlite-wasm-rs` default off; `bundled` stays on.
- `RUSTSEC-2026-0538` is `quinn-udp` 0.5.14 via `reqwest` → `quinn` 0.11.9, patched in `>= 0.5.16, < 0.6.0`. quinn requires `^0.5`, so the lockfile moves to 0.5.16 and quinn stays 0.11.9.
- Those IDs are not added to the audit ignore list. The CI job still fails closed on vulnerability-class advisories, and both have a patched release.

## macOS

`tests/integration.rs` found the daemon by walking `current_exe()` out of `deps/`. Nextest 0.9.152 ran that test binary from a temp copy, `Command::spawn` returned ENOENT, and fail-fast stopped the suite on `bridge_rejects_malformed_mcp_json` and `daemon_shutdown_cleans_up_three_agents`. The path is now `CARGO_BIN_EXE_agend-terminal`, which the harness tests already use.
