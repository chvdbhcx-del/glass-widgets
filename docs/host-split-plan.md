# host.rs split plan

Planning only, no code moved. `src/host.rs` is 4553 lines. Line numbers are those of commit 742ff81 and will drift.
In every case `host.rs` gets `mod <name>;` (file `src/host/<name>.rs`); a child module sees the private items of `host`, so `App`'s
fields, the `T_*` / `WM_*` consts and the free helpers need no visibility changes. `use super::*;` at the top of the new file.

## 1. Win32 window helpers (cleanest: free functions, no `App`)

* Lines: 4325-4335 (`same_rect`, `step_toward`), 4337-4344 (`cpu_time_100ns`), 4346-4486 (`place_above_desktop`, `app_in_front`,
  `input_desktop_open`, `Cover` + `cover_cb`, `covering_windows`, `foreground_is_fullscreen`); optionally also 972-981 `force_foreground`
  and 983-1003 `clipboard_text` / 3826-3846 `set_clipboard_text`.
* Dependencies: only `windows` crate imports, `RECT` and (for `foreground_is_fullscreen`) `same_rect`. Nothing reads `App`.
  Callers are `App` methods (the guard / dim / cover logic) and `tests` (`same_rect` is used 6 times, `step_toward` 5, `cpu_time_100ns` 5);
  re-export with `use winutil::*;` in host.rs so the call sites stay unchanged.
* Module name: `winutil` (`src/host/winutil.rs`). About 170 lines out.

## 2. Canvas text prompt and canvas context menu (largest, still one `impl App`)

* Lines: 3823-4125 (prompt: `on_prompt_request`, `prompt_tick`, `finish_prompt`, `modal_begin`, `apply_act`, key / char handlers) and
  4127-4323 (menu: `show_menu`, `menu_tick`, `menu_act`, `finish_menu`, `abort_menu`, `menu_key`). The two share `modal_begin` and the
  foreground hand-off fields (`prompt_fg`, `prompt_was_fg`, `prompt_t0`), so move them together. Plus the native-popup fallback
  `run_menu` (917-970, uses `wide`, `force_foreground`, `CMD_*`) if module 1 is done first.
* Dependencies: `impl App` methods only; reads `self.ui`, `self.scene`, `self.gfx`, `self.mon`, `self.work`, `self.s`, `self.visible`,
  `self.drag`, `self.reduced` and the `prompt_*` / `menu_*` fields (declared in `App`, lines 297-315 - leave the struct where it is);
  timers `T_PROMPT` / `T_MENU`, `force_foreground`, `reduced_motion`, `input::`, `menu::`, `prompt::`. It calls back into `App` helpers
  that stay in host.rs (`leave`, `boost_capture`, `set_window`, `sync_regions`, `frame`, `now`). Rust allows a second `impl App` in a
  child module, so no trait or signature change is needed.
* Module name: `modal` (`src/host/modal.rs`). About 500 lines out.

## 3. Accent from the wallpaper and layout profiles

* Lines: 3661-3822 (`resolve_accent`, `apply_scope`, `refresh_accent`, `switch_profile`, `ask_profile_name`, `profile_named`, `delete_profile`; `notify` at 3746 is called from all over host.rs, leave it behind).
* Dependencies: `impl App` using `self.settings`, `self.scene`, `self.gfx`, `self.accent_last`, `self.ui.lang`, `self.frame`, `self.all_dirty`; free fn `resolve_accent` is used by the startup path (`run` / `rebuild`) and must stay reachable
  (`pub(super)` or re-exported). Pure JSON handling already lives in `layout.rs` and `accent.rs`, so this chunk is thin glue.
* Module name: `profiles` (`src/host/profiles.rs`). About 160 lines out; do it last, it has the most call sites back into `App`.

## Not worth splitting yet

* `App` struct + `run` + `wndproc` dispatch (lines 201-340, 761-900): everything else hangs off it.
* `impl App` "the host" block (1007-3660, about 2650 lines: placement, drag, edit mode, capture, frame): needs a design pass
  (state groups such as drag / edit / capture could become sub-structs) before any mechanical move.
