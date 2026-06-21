# SPIKE MANIFEST — freeze-4 restart-flood (task t-20260621100425339369-2324-0)

_Author: fixup-dev. Base: origin/main @ 45138e53 (incl. #2393). spike-first → this manifest → lead VET → impl (DUAL, render-sensitive) → review._

## 1. Symptom (operator-confirmed, 2026-06-21, on a #2393 build)
**(a) Restart moment still ~1s freeze — input + tab-switch lag, reproducible every restart.** Distinct from (b) the occasional in-use freeze (#2393 should have fixed; awaiting probe recurrence; NOT this task).

Existing evidence — `#freeze-backlog` probe @ 17:57:24–25 (the probe I added in #2393): `panes_with_backlog` 1–3, `max_rx_chunks` up to **132**, `budget_spent` maxed at **64 KiB/frame**, decreasing 132→100→68→36→4 over the first frames; ~15 interactive frames (~1s) to clear.

## 2. Architecture map (code-structure; to be CONFIRMED by instrumentation — see §5)
Mode: **the fleet daemon runs in app/OWNED mode** (`run_app`, never `run_core` — mod.rs:145), so the **render-first** path (#2343, commit 0af4573c) applies.

Restart sequence (owned/render-first):
1. `run_app` → `session::restore_with_reconciliation` builds **placeholder** panes (`pane.rx = fwd_rx`, empty) synchronously (µs) + collects `attach_jobs`. (mod.rs:477)
2. `restore-complete` logged → **interactive render loop entered immediately** (render-first shows the shell fast).
3. A bounded **W=3 background pool** runs `spawn_and_subscribe` per agent (re-attach surviving agent → `subscribe_with_dump` → `(rx, dump)`); results returned via `attach_rx`. (mod.rs:524–562)
4. **In the loop**, the `attach_rx` arm → `apply_attach_outcome` → `apply_attachment` (pane_factory.rs:354):
   - **(A)** `pane.vterm.process(&dump)` — the initial screen dump processed **DIRECTLY into vterm, in ONE shot, in the loop, UN-bounded** (NOT through `drain_output`'s budget; NOT measured by `#freeze-backlog`).
   - **(B)** starts the forwarder thread: agent subscriber `rx` → `fwd_tx` → `pane.rx`.
5. Post-subscribe agent output (buffered-during-downtime replay and/or active output) floods `pane.rx` → `drain_all_panes` (64 KiB/frame, #2385/#2393) drains it over ~N frames → **`#freeze-backlog` (B)**.

Key correction to the naive model: **the dump is NOT the rx backlog.** There are TWO distinct in-loop restart costs:
- **(A) dump → vterm**: unbounded, one-shot, per pane, in `apply_attachment`. Currently **uninstrumented** — `#freeze-backlog` (rx) does not see it.
- **(B) rx backlog drain**: bounded 64 KiB/frame, measured by `#freeze-backlog`, ~15 frames ≈ 1s.

## 3. RCA
At restart, the main thread is busy in the interactive loop doing (A)+(B) over the first ~15 frames → each frame's `terminal.draw` (drain + render; probe showed up to ~107 ms draws) blocks the `select!` loop → input + tab-switch events queue behind the current frame → perceived ~1s freeze. #2393 BOUNDED (B) per-frame (was unbounded pre-#2385 = longer), but the catch-up still runs in the INTERACTIVE loop, so it reads as a freeze rather than a load.

⚠ **(A) is a code-traced HYPOTHESIS, not yet measured** — it could be negligible (small dumps) or co-equal with (B). MUST measure before sizing the fix (§5). The charter's data only covers (B). (This freeze has been misjudged twice — no mechanism claim without a probe.)

## 4. Design tension + recommended approach

**Tension:** the charter asks to "move the drain to the boot/restore phase, BEFORE the interactive loop." But render-first (#2343) **deliberately defers attach into the loop** to show the shell fast — so in owned mode the dump (A) and backlog (B) do not exist before the loop; they arrive as attaches complete *in* the loop. A literal pre-loop drain would force pre-loop attach = undo #2343 = re-introduce the boot freeze it removed.

**Resolution — absorb the flood as a bounded LOADING PHASE inside the early loop** (achieves the charter's intent without undoing render-first):

Add a `booting` state to the render loop, active from loop entry until **(attaches all applied AND every pane's rx drained) OR `MAX_BOOT_CATCHUP` elapsed** (e.g. 1500 ms hard cap). While `booting`:
- **Bigger catch-up budget**: `drain_all_panes` uses a larger per-frame budget bounded by a **per-frame TIME cap** (e.g. each frame may drain up to ~80–100 ms before yielding) — finishes the flood in a few frames instead of ~15, while still yielding to `select!`/input every frame (laggy-but-not-frozen, and labeled loading).
- **Bound the dump (A)**: stop the unbounded one-shot `pane.vterm.process(&dump)`. Option A1 (preferred): push `dump` into `pane.rx` ahead of the forwarder (FIFO preserves the "seed-before-stream" order) so the SAME budgeted/time-capped drain handles dump+backlog uniformly and bounded. Option A2: process `dump` in time-capped slices. (A1 also deletes the special-case + the only unbounded vterm.process on the restart path.)
- **Visible progress**: a "loading — attaching M/N agents, draining…" indicator (overlay or status line) so the phase reads as loading, not a freeze.
- (Optional) defer non-quit input during `booting` with a hint, or keep input live (time cap already keeps it serviceable).

On exit → normal interactive loop with the **untouched steady-state 32/64 KiB cap** (#2385/#2393 unchanged). `MAX_BOOT_CATCHUP` guarantees no unbounded boot hang even with a pathological backlog (remainder finishes under the steady cap, as today).

This keeps render-first's fast first frame (the loading screen IS the first frame), bounds boot, shows progress, and confines the bigger budget to the bounded boot window.

## 5. Instrumentation plan (instrument-first — settle before/with impl)
Reuse `#freeze-backlog` (B). ADD (all env-gated `AGEND_FREEZE_INSTRUMENT`, zero behavior off):
1. **`#freeze-dump`** in `apply_attachment`: `dump.len()` + `vterm.process` µs per pane → measures (A), currently invisible. Decides whether (A) needs bounding or is negligible.
2. **`#freeze-boot-timeline`**: timestamps for `restore-complete`, first interactive frame, each `attach` applied, and "all panes drained" → confirms WHEN (A)/(B) enter relative to the first interactive frame (the charter's open question), and the true boot-window length.
3. Collect by asking the operator to restart 2–3× with the env set (can't restart the operator's daemon from here). Confirms (A) magnitude + boot-window size BEFORE finalizing `MAX_BOOT_CATCHUP` / the time-cap constants.

## 6. Deterministic tests (gate, no PTY)
1. **Boot window drains a restart backlog**: build a layout of N panes, pre-load each with a large `dump` + rx backlog (mirror 132 chunks); run the `booting` drain logic; assert the backlog reaches empty WITHIN the boot window (bounded # of catch-up frames), and that after exit `booting==false`.
2. **No frame starves input**: assert a single boot-phase catch-up step never exceeds the per-frame TIME/byte cap (bounded main-thread work per frame → `select!` serviced).
3. **Steady-state cap restored**: after the boot window, `drain_all_panes` uses the 64 KiB cap (assert via a post-boot pane that a single frame drains ≤64 KiB) — proves #2385/#2393 untouched.
4. **MAX_BOOT_CATCHUP bound**: with an oversized backlog, `booting` exits at the deadline (no unbounded hang); remainder drains under steady cap.
5. (If A1 chosen) **dump-via-rx ordering**: dump bytes precede post-subscribe bytes in the vterm (FIFO preserved).

## 7. Constraints honored
- ✅ Bounded boot (per-frame time cap + `MAX_BOOT_CATCHUP`); no unbounded hang.
- ✅ Visible loading progress.
- ✅ Steady-state interactive cap (#2385/#2393) untouched — bigger budget confined to the bounded boot window.
- ✅ Render-first (#2343) preserved — fast first (loading) frame; attaches stay deferred.
- ✅ instrument-first — (A) measured before sizing; no unproven mechanism claim.

## 8. Open questions for lead VET
1. **Approve the "bounded loading phase IN the loop"** framing over a literal pre-loop drain (which would undo render-first #2343)? 
2. **Bound the dump (A)?** A1 (dump→rx, unifies + removes the unbounded process) vs A2 (time-sliced) vs defer until `#freeze-dump` shows (A) is significant.
3. **Constants**: `MAX_BOOT_CATCHUP` (~1.5s?) + per-frame time cap (~80–100 ms?) — finalize after probe data, or pick provisional + tune?
4. **Input during boot**: keep live (time-cap makes it serviceable) vs defer-with-hint?
5. Add the probes (#freeze-dump, #freeze-boot-timeline) in the impl PR and gate the fix sizing on operator restart data, or proceed with provisional constants + probes together?
