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
