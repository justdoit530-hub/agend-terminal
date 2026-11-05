# Event::Resize 尺寸去重

## `last_resize_dims` 初始值

`AppState::new()` 把欄位設成 `crossterm::terminal::size().unwrap_or((0, 0))`。

主路徑是啟動當下的實際終端尺寸，不是寫死的 `(0, 0)`。crossterm 在 Unix 上把 `SIGWINCH` 轉成 `Event::Resize`，tmux/screen 裡其他 pane 變動、或拖曳視窗邊框，都會送出「尺寸其實沒變」的事件。若初始值是 `(0, 0)`，第一個這種事件一定不等於初始值，仍會 `resize_panes` + `terminal.clear()`，畫面還是會跳一次。用啟動時的真實尺寸，第一個相同尺寸的事件就整段跳過。

`(0, 0)` 只在 `crossterm::terminal::size()` 失敗時使用（沒有 TTY、ioctl 失敗）。它不是一個可用的終端尺寸，所以第一個真實 `Resize` 仍會被當成變化，`terminal.clear()` 照跑。這條路徑不會把第一次真實 resize 誤判成「沒變化」。

## 為什麼第一次真實 resize 不會被跳過

比較的是 `(cols, rows) != last_resize_dims`。

- 啟動時 ioctl 成功，之後尺寸真的變了：事件尺寸 ≠ 種子，執行 `resize_panes` 與 `terminal.clear()`，然後把欄位更新成新尺寸。#1140 的寬字元殘影清除還在。
- 啟動時 ioctl 成功，事件尺寸相同：整段跳過，連 `resize_panes` 都不呼叫。
- 啟動時 ioctl 失敗，種子是 `(0, 0)`：第一個非零尺寸事件一定不相等，仍會 clear。

`apply_pending_resize`（`needs_resize` 那條）沒有改。它在迴圈開頭依當時的 `crossterm::terminal::size()` 自己 clear。若那次讀到的尺寸和種子不同，之後到來的 `Event::Resize` 仍會因為不等於種子而再 clear 一次。多清一次，不會漏掉真實 resize。

## 不做的事

沒有 debounce / coalescing timer。連續不同尺寸（拖曳過程）仍每次都 clear。這次只去掉尺寸相同的重複事件。
