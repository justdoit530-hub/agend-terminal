# Spike Manifest — freeze-3 RCA (operator 渲染凍結殘留,預-#2386)

_task t-20260621062045374111-50793-0 · base origin/main @ 21bf6f53(含 wave-1)· read-only · instrument-first_

## ① 探針覆蓋(charter 第一要務)— GAP 確認

- **`#freeze-loop-summary` 不在當前 binary**:`grep -rn 'freeze-loop-summary\|max_wakeup_burst\|loop_iters' src/` = 0 hit。它來自 **commit e2f671c7**(branch `spike/freeze-residual-instrument`,我 6/20 的 instrument spike,**未 merge**)。→ 08:xx 那台是該 spike binary;**當前 wave-1 binary 沒有此探針**,所以 `#freeze-loop-summary` 在 08:16:50 後就停(979 行全在 08:00–08:16),而 `#freeze-drain`(#2385 merged,pane.rs:170)記到 14:22。**這是覆蓋缺口,不是凍結停了。**
- **#freeze-drain 仍在記(當前 binary)**:11:51→14:22 共 11 筆。關鍵:**14:12:03 drain_us=12152 more=true**、**14:22:15 連兩筆 more=true(0.2s 內)** = cap 生效(107ms→12ms)但 backlog 沒一次清完、re-arm 多幀。
- **補法(env-gated、zero-behavior,AGEND_FREEZE_INSTRUMENT)**:restore e2f671c7 的 `FreezeInstrument`(1s 窗 #freeze-loop-summary:draws/fps/wakeups/max_wakeup_burst/loop_iters/inputs/max_draw_us/max_input_us)**+ 新增針對 H2 的欄位**:
  - **per-active-pane `rx.len()` backlog**(切 tab 當下 + 每窗 max)——量「切到的 pane 累積多少 backlog」。
  - **consecutive `more=true` 連續幀數**(catch-up 長度)——量「一次切 tab 要刷幾幀才乾」。
  - **tab-switch marker**(active tab 改變時記一筆 + 該 tab 各 pane rx backlog)。
  → operator build 此 branch、**重現(切到一個背景跑很久的忙碌 tab)**、收 `#freeze-loop-summary`+`#freeze-backlog` → 證 H2。

## ② RCA(code-backed,標記 needs-probe-confirm;此 freeze 已誤診兩次故不拍板)

**資料流(已讀碼確認)**:PTY read thread → `pane.rx`(**unbounded** channel)+ 每 chunk 一個 `wakeup_tx` 信號。`drain_output(32KiB)`(pane.rs:142,主執行緒、在 `terminal.draw` 內)把 rx → `vterm.process`。`render_pane`(core_render.rs:399/410)**只對它 render 的 pane** 呼叫 drain_output = **只有 active(可見)tab 的 pane 被 drain**;`active_tab_has_pending_output`(core_render.rs:70)也**只看 active tab**。

### H1 — wakeup burst(charter 主嫌 #1):**code 上被 #2346 coalesce 壓住,非凍結主因**
- mod.rs:948 `while wakeup_rx.try_recv().is_ok() {}` = 一次 select 把**整個 burst** 排乾 → 一個 `dirty=true` → 一個 frame-capped draw(33ms cap)。08:16 數據 wakeups=55 → **draws=23**(coalesce 在做事)。
- 背景忙碌 pane 灌 `wakeup_tx`(每 chunk 一個)→ 喚醒 loop → 重畫 **可見 tab**(即使可見內容沒變)≤30fps = **CPU 浪費、非凍結**(每次可見-tab draw 很快、無 backlog)。
- ⟹ H1 不是 operator 的「凍結」主因;是背景輸出造成的無謂重畫(bounded)。**needs-probe**:確認 post-#2385 wakeup 沒繞過 coalesce。

### H2 — 背景 tab unbounded rx backlog → 切 tab catch-up(**PRIMARY**,強 code-backed)
- 背景 tab 的 pane **rx 無上限累積**(PTY thread 一直送、drain_output 不對非可見 pane 跑)→ 背景 agent 跑越久、backlog 越大、其 vterm 越舊。
- **切到該 tab** → 它的 pane 開始被 render → drain_output 以 **32KiB / 33幀 ≈ 970 KiB/s** 追(core_render.rs:64 + mod.rs:206)→ 大 backlog(agent 跑一陣子累積數 MB)= **數秒可見 catch-up**(畫面一直往前刷)= operator 的「**切 tab 一直刷新 / 凍結**」。
- **#2385 把「一次 107ms input-stall」變成「每幀 12ms、不卡輸入」,但「總 catch-up 時間 = backlog / 970KiB」仍 unbounded、∝ 背景時長** → 殘留 freeze。14:12/14:22 的 more=true 連續幀正是此 catch-up。
- 完全吻合 operator:**「一陣子」發作**(背景累積)+ **切 tab**(顯露)+ **非輸入法** + **預-#2386**(與 find-storm 無關)。
- cite:core_render.rs:399/410(render_pane 只 drain 可見 pane)、:70(active_tab_has_pending_output 只看 active tab)、pane.rs:128-178(rx unbounded、drain→vterm、more re-arm、#freeze-2 註解自證「main thread / vterm.process / boot backlog」)、mod.rs:206(FRAME_INTERVAL=33ms)、:765-811(frame-cap + #2385 re-arm dirty)、core_render.rs:64(32KiB)。
- **needs-probe-confirm**:量「切 tab 當下該 pane rx backlog bytes」+「連續 more=true 幀數」對上「可見 freeze 秒數」。

## ③ 修法草案(候選,lead 選;render/lock 敏感 → DUAL)

**前置(必做)**:補 ① 的 augmented probe → operator 重現 → 證 H2(別跳過,已誤診兩次)。

確認 H2 後候選(root:背景 pane 的 vterm 沒持續更新 + rx unbounded):
- **(A) 背景 pane 也持續 drain rx→vterm**(tmux 模型,root-fix):用 tick / 專屬路徑把**所有** pane 的 rx 持續餵進各自 vterm(vterm 大小有界=螢幕+scrollback,持續餵=有界工作)→ 切 tab 即時顯示當前 vterm、無 backlog。⚠ perf-R1:餵 vterm 持 core.lock,背景餵 vs render 讀——但 #2380 已把 render 改 lock-free snapshot,**需重評** feed-under-lock + lock-free-read 是否安全(我 perf-R1 判 feed_with_fg lock-shrink UNSAFE,此處不同:不是 shrink lock,是把餵的時機從 draw 內移到背景)。最大 blast。
- **(B/C) 切 tab fast-forward**(較小 blast,tab-switch-scoped):切到某 tab 的那幀,對其 pane 用**大一次性 budget drain 到乾**(接受切換瞬間一次性短 stall ~背景 backlog 大小,例如數 MB→~100ms),之後回 steady 32KiB。把「10s 一直刷」換成「切換時一下卡一下就好」。correctness 簡單(只是 budget 變大、仍 lossless FIFO)。
- **(D) bound the rx / 上游**:rx 給上限(滿了…?)——⚠ **不可naive drop**:vterm 狀態是 escape-seq 累積、丟 rx bytes 會壞畫面。若要 cap 須是「fast-forward 餵 vterm」非「丟」。列出但不建議單獨用。
- 各候選評:blast / correctness(lossless FIFO 不破)/ perf-R1 + #2380 + #2385 + #2346 互動 / 是否需 operator restart 驗。
- **建議起點**:(B/C) tab-switch fast-forward(小、直擊「切 tab」症狀、correctness 單純),(A) 列為 root-fix 但 perf 重評後再定。先 probe 證 H2 + 量 backlog 典型大小(決定 fast-forward budget)。

## Evidence(read-only spike @ 21bf6f53)
- ran:`grep -rn 'freeze-loop-summary' src/` → 0(gap);`git log --all -S max_wakeup_burst` → e2f671c7 `spike/freeze-residual-instrument`(未 merge);app.log `grep -ac '#freeze-loop-summary'`=979(全 08:xx)、`#freeze-drain`=11(到 14:22,14:12 drain_us=12152 more=true);`gh pr view 2390` → **MERGED**(離題確認:我的 denylist 已進 main)。
- cited(行號見 ②):mod.rs:206/765-811/943-950、core_render.rs:64/70/399/410、pane.rs:128-178。
- 未跑單測(read-only RCA spike);probe 補上後才有當前-binary 凍結現場數據。
