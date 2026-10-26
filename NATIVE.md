# mswidgets - native widget renderer: developer guide

`mswidgets` draws the macshell desktop widgets without a WebView: one transparent layered window, Direct2D 1.1 on the GPU,
Liquid Glass built from D2D effects, the wallpaper behind each card captured with DXGI Desktop Duplication.
The core (glass, layers, scheduling, input, persistence) is finished. **A widget is ONE file, `src/widgets/<id>.rs`.**
You never edit anything else: `widgets/mod.rs` already declares every widget module and calls its `register`.

```
src/widget.rs     the widget API (Widget trait, Cx, Tick, Style, Rect, palette, Pending ...)   <- read this first
src/registry.rs   WidgetDef + Registry
src/icons.rs      the built-in vector glyphs (enum Glyph)
src/sys.rs        hidden processes, HTTP (curl.exe), PowerShell, image decoding, Action executor
src/tz.rs         civil-date maths + a fixed world time-zone table (london, tokyo, new_york, ...)
src/layout.rs     config.json widgetsLayout read / per-card cfg write-back / --cards parser
src/cards.rs      Scene: per-card glass layer + cached static command list + overlay compositing + scheduling
src/glass.rs      Liquid Glass renderer + GlassParams (section 13)         src/motion.rs  pure motion state machines (light spring, materialise, appearance, merge)
src/liquid.rs     the merge pass (liquid neck between close cards)         src/calib.rs   `--calib`: fit the glass against the Apple references
src/gfx.rs backdrop.rs host.rs main.rs     core: DO NOT TOUCH
src/widgets/*.rs  one file per widget (clock + calendar are finished and are your style reference; the rest are stubs)
```

## 1. Build and verify (this is your whole feedback loop)

Use a private target dir so parallel builds do not fight over locks:

```
cd <repo>
cargo build --target-dir <repo>\target-<yourname>          # debug, a few seconds incremental
cargo build --release --target-dir ...                                           # must stay ZERO warnings
```

Never run live mode (`mswidgets.exe` without `--dump/--sheet`) and never `--bench`: the lead does that. Everything below is headless,
needs no window and no network, and renders in well under a second.

**Contact sheet, every size of your widget (the main check):**
```
target-<you>\debug\mswidgets.exe --sheet out\<you>\sheet.png --backdrop out\bd_sunset2.png --only weather
```
`--only id1,id2` limits it; without it EVERY registered widget at EVERY supported size is drawn (bands S, M, L; caption `id:size` above each card).
Then READ the PNG (the Read tool shows images). Other backdrops: `out\bd_grid2.png` (shows the lens distortion), `out\bd_bw.png` (bright/dark noise, tests
legibility), `out\bd_sunset.png`. A backdrop of another aspect ratio is cropped to cover, never stretched. Omit `--backdrop` for the plain dark base.

**Cards at chosen cells, with settings, scale and time:**
```
mswidgets.exe --dump out\<you>\d.png --backdrop out\bd_sunset2.png --cards "clock:s@0,0;calendar:m@1,0;weather:l@0,1" [--scale 1.5] [--w W --h H] [--time "2026-10-24 10:08:35"]
--cards "notes:m@0,0{\"text\":\"hello\"}"     optional JSON cfg right after the cell: test your settings paths
```
`type:size@col,row` (size optional = default size). The canvas auto-fits unless `--w/--h`. Use `--scale 2` to inspect details (it is also a test that you
use logical px everywhere). Previews use the FIXED time 2026-10-24 10:08:35 unless `--time` is given, so two renders are comparable.
`--no-config` makes a `--dump` without `--cards` use the default layout instead of the user's config.json.

`--cards "_dev:l@0,0;_dev:l@2,0{\"mode\":\"icons\"}"` renders the primitive gallery (text styles, rings, bars, sparkline, clip, bitmap, rotation)
and all glyphs numbered in `Glyph::ALL` order.

Unit tests (`cargo test --target-dir ...`) cover tz.rs and layout.rs; add a `#[cfg(test)]` block to your file for pure logic (parsers, formatting).

## 2. Add or replace a widget

1. Open `src/widgets/<id>.rs` (it exists as a stub that draws the title and "скоро").
2. Replace the body. Keep `pub fn register(r: &mut Registry)`, but call `r.register(...)` (not `register_stub`) and keep the metadata
   (`type_id`, names, sizes, default size) unless the product contract says otherwise. Delete the `use super::Stub`.
3. Implement `Widget` for your struct, `make: || Box::new(MyWidget::default())`.
4. Build, `--sheet --only <id>`, read the PNG, iterate. Check every declared size, bright and dark backdrops, `--scale 2`.
5. Done when the sheet looks like the references, the release build has zero warnings and the performance rules below hold.

`WidgetDef { type_id, name: (ru, en), desc: (ru, en), group, icon: Glyph, sizes: &[Size], default_size, make }`.
`group` = gallery sidebar family (clock, calendar, reminders, music, batteries, weather, system, notes, controls, utilities, photos, info).
An id starting with `_` is hidden from sheet/gallery (developer tools only).

## 3. The Widget trait

```rust
pub trait Widget {
    fn draw(&mut self, cx: &mut Cx) -> Result<()>;                     // STATIC content, recorded into a cached command list
    fn overlay(&mut self, _cx: &mut Cx) -> Result<()> { Ok(()) }       // ANIMATED content, painted every frame over the cached layer
    fn tick(&mut self, cx: &Cx) -> Tick;                               // state updates + the next wake-up
    fn mouse(&mut self, _ev: &MouseEv, _cx: &mut Cx) -> bool { false } // true = something visible changed
    fn version(&self) -> u64 { 0 }                                     // bump when draw() would produce something different
}
```
* **`draw`** runs again only when `version()` changed, the day changed, or `cx.invalidate()` was called (the card size never changes while the instance lives).
  It must be a pure function of your state + `cx` (+ `cx.now`). Anything that changes at most once a minute belongs here.
* **`overlay`** is for what moves continuously (second hand, equalizer, a blinking dot, a progress arc, a ticking seconds counter).
  It is called only while the card is visible and `tick` returned an animating `Tick`, plus once per composite in previews
  (so `--sheet/--dump` show it frozen at `cx.now`). Keep it tiny: it runs at up to 60 Hz. It is clipped to the card rectangle.
  Draw in `draw` whatever does not move.
* **`tick`** is called once at start (start your first background job here), whenever the schedule it returned is due, and right after one of
  your background jobs finished. Poll `Pending`s here, update state, bump your version counter. It may call `cx.invalidate()` / `cx.request_frame()`
  / `cx.save_cfg()` (they take `&self`). Never draw here.
* **`Tick`**: `Never` (static: zero cost, the loop sleeps), `Every(Duration)`, `Fps(n)` (1..60), `Second` / `Minute` (aligned to the wall clock),
  `Poll(Duration)` (data refresh timer: `tick` runs but NO overlay frame is painted unless `version()` changed or you called `request_frame()`).
  The framework wakes the loop only as often as the fastest visible card needs. Return `Never`/`Poll` unless something truly animates.
  A frame is produced only for a due animating card, so `Second` for a countdown costs one composite per second.
* **`mouse`**: card-local LOGICAL px. `MouseEv { kind: Move|Down|Up|Wheel|Leave, x, y, button: Left|Right|Middle|None, delta }` (wheel notches, + = up).
  Helpers: `ev.inside(r)`, `ev.pressed(r)`, `ev.clicked(r)` (left button released inside), `ev.right_clicked(r)`, `ev.wheel()`. Events only arrive while the
  cursor is over the card (the window is click-through elsewhere). `cx.hover` / `cx.hovering(rect)` give the cursor position. A hover look that lives in `draw`
  needs a version bump when the hovered thing changes (calendar.rs: chevrons appear on hover). Return true only when something changed.
  Do not draw in `mouse`: change state, return true, the framework re-records/recomposes.

## 4. Cx reference

All coordinates are LOGICAL px, origin = top-left of the card. The size is `cx.w x cx.h` = 170x170 (S), 356x170 (M), 356x356 (L) at scale 1; the framework applies the UI scale
(`cx.scale` is informational). Card padding is 16 px: `cx.content()` is the card inset by 16, `cx.bounds()` the whole card.

Fields: `size: Size`, `w, h`, `scale`, `now: Now` (y, mo, d, h, mi, sec, ms; `dow()` 0=Mon, `day_key()`, `minute_key()`), `preview: bool`, `lum` (backdrop luminance 0..1),
`dark_backdrop: bool`, `lang: Lang::{Ru,En}`, `cfg: &mut serde_json::Value`, `hover: Option<(f32,f32)>`, `dc` / `gfx` (raw D2D, UI thread only).

Services:

| call | meaning |
|---|---|
| `cx.invalidate()` | re-record `draw` on the next frame |
| `cx.request_frame()` | one overlay frame soon even if `tick` is `Never` |
| `cx.save_cfg()`, `cx.cfg_set(key, json!(v))` | persist settings (section 7); read with `cx.cfg_str/f64/bool(key)` or `cx.cfg` |
| `cx.spawn(\|\| -> T)` returns `Pending<T>` | run on a worker thread; `pending.take()` in `tick` yields the result once; `pending.busy()` |
| `cx.waker()` | `Waker` (Copy, Send): `.wake()` from your own thread makes the framework call `tick` |
| `cx.invoke_system(Action)` | `Open(url_or_path)`, `Run(exe, args)` (hidden), `Power(Lock/Sleep/Restart/Shutdown/SignOut)`, `Media(PlayPause/Next/Prev/Stop/VolUp/VolDown/Mute)`; no-op in preview |
| `cx.t("ru", "en")` | string by UI language; also `weekday_name`, `month_name`, `month_genitive`, `plural(n, one, few, many)` |
| `cx.utc_secs()`, `cx.zone_time(&tz::Zone)`, `tz::find("tokyo")` | world time without any OS call (table in tz.rs) |
| `cx.image(&Decoded)`, `cx.image_file(path, max_w, max_h)` | build a device `Bitmap` (= ID2D1Bitmap1) on the UI thread |

Drawing helpers (colour = a palette constant or `Color::rgba(..)`; `.alpha(a)`, `.fade(k)`, `.mix(other, t)`):

| call | draws |
|---|---|
| `text(rect, str, Style, color)` | DirectWrite text inside the rect (Segoe UI Variable); `measure(str, style, max_w)` returns `(w, h)` |
| `rect`, `rrect(rect, radius, c)`, `rrect_stroke(rect, radius, width, c)`, `pill(rect, c)` | fills / outline |
| `circle(c, r, color)`, `circle_stroke(c, r, w, color)`, `line(a, b, w, color)` (round caps), `hr(rect)` | basics |
| `arc(c, r, w, start_deg, sweep_deg, color)` | arc, 0 deg = 12 o'clock, clockwise (negative sweep = counter-clockwise) |
| `ring(c, r, stroke, frac, color)` | gauge: faint track + arc of `frac` (battery, disk, timer) |
| `bar(rect, frac, color)` | progress pill |
| `sparkline(rect, &[f32], color)`, `sparkline_range(rect, v, lo, hi, color)` | line chart with a soft area |
| `gradient_rrect(rect, radius, c0, c1, angle_deg)` | CSS-like linear gradient (180 = top to bottom) |
| `icon(Glyph, rect, color)` | vector glyph in the square fitted to `rect` |
| `bitmap(rect, &Bitmap, Fit)`, `bitmap_round(rect, &Bitmap, Fit, radius)` | `Fit::Cover` (crop) / `Contain` / `Stretch` |
| `with_clip_round(rect, r, \|cx\| ..)`, `clip_round(rect, r)` + `pop_clip()`, `clip(rect)` | anti-aliased clipping |
| `rotated(center, deg, \|cx\| ..)` | rotate everything drawn inside (clockwise) |

`Style` presets (chain `.center() .right() .top() .bottom() .mono() .ellipsis() .wrap() .size(px) .weight(w) .track(px)`):
`TITLE` 13/600, `BODY` 13/400, `CAPTION` 11/400, `BIG` 44/300, `HUGE` 64/300, `MONO` 13 tabular digits, `LABEL` 12/700 letter-spaced (upper-case the text yourself; `accent()` or RED).
Text is vertically centred in its rect by default (`.top()` / `.wrap()` change that). `.mono()` = tabular figures: use it for anything that changes in place (clocks, counters).

Palette: `TEXT` (white .96), `DIM` (.62, secondary), `FAINT` (.38, tertiary), `FILL` (.18, tracks/separators), `HAIR`, `INK` (dark text on cream), `accent()` (the accent colour: light blue by default, the wallpaper's dominant hue with `accentMode` "wallpaper"; the const `ACCENT` is only its default - never use it in a widget),
`RED GREEN BLUE ORANGE YELLOW PINK TEAL PURPLE WHITE BLACK`.

`Rect::new(x, y, w, h)`: `inset(d)`, `inset_xy`, `offset`, `centered(w, h)`, `cut_top(h)` / `cut_bottom` / `cut_left` / `cut_right` returning `(a, b)`, `row(i, n, gap)`, `col(i, n, gap)`, `cx() cy() right() bottom() contains()`.

Glyphs (`Glyph::...`): Play Pause Next Prev Plug Bolt Cloud Sun PartlyCloudy Rain Snow Thunder Moon Drop Cpu Ram Gpu Disk NetUp NetDown Note Check CheckCircle Circle Plus Trash Bell Power Sleep Lock
Restart Wifi Bluetooth Volume Brightness Mic ChevL ChevR ChevUp ChevDown X Clock Calendar Laptop Headphones Keyboard Mouse Phone Battery Clipboard Copy Folder Image Star Pin Search Location.
`src/icons.rs` is shared, so a missing glyph is not yours to add: draw it in your own file with `line/arc/rrect` and tell the lead.

## 5. Complete minimal example (compile-checked; copy into `widgets/<id>.rs`, change `type_id`)

```rust
//! Minimal example: a click counter with a pulsing overlay dot and a background job.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;

#[derive(Default)]
struct Counter {
    count: u32,
    ver: u64,
    started: bool,
    time: String,
    job: Option<Pending<String>>,
}

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "quote", // must be the id of YOUR file in widgets/
        name: ("Счётчик", "Counter"),
        desc: ("Считает клики", "Counts clicks"),
        group: "demo",
        icon: Glyph::Plus,
        sizes: &[Size::S, Size::M],
        default_size: Size::S,
        make: || Box::new(Counter::default()),
    });
}

impl Widget for Counter {
    // STATIC part: recorded once per version() / day / invalidate.
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let (head, body) = cx.content().cut_top(18.0);
        cx.text(head, cx.t("Счётчик", "Counter"), Style::TITLE, TEXT);
        cx.text(body, &self.count.to_string(), Style::BIG.mono(), TEXT);
        cx.text(body.cut_bottom(16.0).1, &self.time, Style::CAPTION.ellipsis(), DIM);
        Ok(())
    }

    // ANIMATED part: every frame while tick() returns an animating Tick.
    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        let t = cx.now.sec as f32 + cx.now.ms as f32 / 1000.0;
        let k = (t * std::f32::consts::TAU).sin() * 0.5 + 0.5;
        cx.circle((cx.w - 24.0, 24.0), 4.0 + 2.0 * k, GREEN.alpha(0.5 + 0.5 * k));
        Ok(())
    }

    // Schedule + state updates. Also called after a job finished and once at start.
    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            self.count = cx.cfg_f64("count").unwrap_or(0.0) as u32;
            if cx.preview {
                self.time = "10:08:35".into(); // preview: sample data, no I/O
            } else {
                self.job = Some(cx.spawn(|| powershell("Get-Date -Format HH:mm:ss").unwrap_or_default().trim().to_string()));
            }
            self.ver += 1;
        }
        if let Some(t) = self.job.as_ref().and_then(|j| j.take()) {
            self.time = t;
            self.ver += 1;
        }
        if cx.preview { Tick::Never } else { Tick::Fps(15) }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if ev.clicked(cx.bounds()) {
            self.count += 1;
            self.ver += 1;
            cx.cfg_set("count", json!(self.count)); // persisted into config.json
            return true;
        }
        false
    }

    fn version(&self) -> u64 {
        self.ver
    }
}
```

## 6. Preview mode and the sample-data rule

`cx.preview == true` in `--sheet`, `--dump` and (later) the gallery. In preview your widget MUST
* show deterministic SAMPLE data that makes the widget look its best (realistic names, a full list, a playing track, 3 reminders, a 5-day forecast...), derived from `cx.now` only,
* do NO I/O: no files, no network, no PowerShell, no registry, no clipboard, no WMI (`cx.spawn` and `cx.invoke_system` already do nothing in preview),
* `Tick::Never` is fine: overlays are painted once anyway.

Live mode (`preview == false`) shows real data, and a sensible EMPTY / LOADING state until the first job returns (never a blank card, never a crash on an error: show "нет данных").

## 7. Settings

Each card has a JSON object `cx.cfg` (starts `{}`), stored in `%APPDATA%\macshell\config.json` under `widgetsLayout[i].cfg`.
Read it in `tick`/`draw`; change it with `cx.cfg_set("key", json!(value))` (or edit `cx.cfg` and call `cx.save_cfg()`). The framework writes ONLY that one item's `cfg`
(read-modify-write, temp file + rename; all unknown keys and the other items stay untouched; an unparsable file is never overwritten). The Tauri app writes the same file:
never write it yourself, never cache it. Document your keys at the top of your file (see clock.rs: `zones`).

## 8. Background work

```rust
// in tick(), UI thread:
if self.job.is_none() && !cx.preview {
    self.job = Some(cx.spawn(|| fetch()));            // worker thread; 'static + Send closure, no Direct2D inside
}
if let Some(data) = self.job.as_ref().and_then(|j| j.take()) { self.data = data; self.ver += 1; self.job = None; }
Tick::Poll(Duration::from_secs(600))                   // refresh later: tick runs again, no frames are painted
```
Helpers usable ON ANY THREAD (all windowless; a console window must never appear): `http_get(url, timeout_s)` returns `Result<String, String>` (curl.exe), `powershell(script)` (UTF-8 stdout; end with
`| ConvertTo-Json -Compress`), `run_hidden(exe, &[args])`, `spawn_hidden`, `decode_image_file(path, max_w, max_h)` / `decode_image_bytes(bytes, ..)` returning `Decoded` (premultiplied BGRA).
Images: decode on the worker, then `cx.image(&decoded)` on the UI thread (in `tick`), keep the `Bitmap` in your struct, draw it with `cx.bitmap`.
`serde_json` is available (`Value`, `json!` are re-exported). The `windows` crate (0.58) already enables Shell, Power, Time, Storage_FileSystem, NetworkManagement_IpHelper, System_DataExchange,
Media_Audio(+Endpoints), WinRT Media_Control / Storage_Streams, Performance, ToolHelp ... use them from workers. If you truly need another feature flag, tell the lead (Cargo.toml is shared).
Always use timeouts (curl `--max-time`) and never block in `tick` / `draw` / `mouse`.

## 9. Visual rules (match the Apple Liquid Glass references)

References: `calib/ref/` (not in the repository) - `cmp01.png` (calendar + weather on a warm wallpaper),
`dark_z.png` (batteries rings, calendar, weather, reminders in the dark tint), `clear.jpg`, `dark.jpg`, `gallery.png`. Look at them before you design.
Reference implementations here: `clock.rs`, `calendar.rs`. Product contract: `..\WIDGETS.md`.
* The framework draws the glass (shadow, lens refraction, tint, rim); you draw content only, directly on it. No card background, no borders.
* Padding 16 px (`cx.content()`). Nested shapes have radius ~8-12. Text is white with alpha steps: primary `TEXT`, secondary `DIM`, tertiary `FAINT`. Accent colour only for headers / active things (`accent()`; `RED` weekday / alarm; `GREEN` ok; `ORANGE` warning).
* Typography: title 13 semibold, or a 12 bold letter-spaced upper-case `LABEL`; body 13; captions 11; big numbers 44/64 LIGHT. Numbers that change in place: `.mono()`. At most 3-4 sizes in one card.
* **Grid and type system, in one place.** Desk grid (`grid.rs`): a cell is 170 px, the gap 16 (`PITCH` 186), the origin (20, 50); S = 1x1 = 170x170, M = 2x1 = 356x170, L = 2x2 = 356x356, the desk is `dims(work area)` cells; the card padding is 16 (`cx.content()`: S 138x138, M 324x138, L 324x324). There is no layout engine: every widget places its text with hand-written rectangles. What is shared: the palette (`TEXT` 1.0, `SUBTLE` .80, `DIM` .70, `FAINT` .50 white, `accent()`), the presets (`Style::TITLE` 13/600, `BODY` 13/400, `CAPTION` 11/400, `LABEL` 12/700 tracked caps, `BIG` 44/300), the size-dependent tracking (`auto_track`), Inter, and the **type scale**: every requested size snaps to 10, 11, 12, 13, 15, 17, 20, 24, 28, 34, 40, 46, 54, 64, 76 (`snap_size`, off with `MSW_TEXT_SCALE=off`), so fit loops and one-off sizes (14.276, 11.5, 9.5) land on a step. Text is rendered unhinted grayscale (`MSW_TEXT_MODE`). `MSW_TEXT_LINT=1 mswidgets --sheet x.png 2> lint.log` then `python tools/text-lint.py lint.log` prints per card the insets of the text block and the sizes in use.
* S (170x170) = ONE idea: a glanceable value (ring, big number, icon + label). M (356x170) = the S idea plus context side by side (visual left, text right) or a row of 2-4 items.
  L (356x356) = a list or grid (4-6 rows of ~44 px, a month, a chart) with a title; lists anchor to the top, summaries to the bottom. Empty states are dim centred text, never blank.
* List rows are 44 px with a 0.6 px `FILL` separator (see the world-clock rows in clock.rs); checkbox circles are 11 px with a 1.5 px `FAINT` stroke.
* Everything must stay legible over a bright AND a dark backdrop: check the sheet on `bd_sunset2.png` and `bd_bw.png`.
* `.ellipsis()` on every variable-length string; `.wrap()` only where designed. Russian first, `cx.t(ru, en)` for both.

## 10. Threading and performance rules

* ONLY the UI thread touches Direct2D/DirectWrite (`draw`, `overlay`, `tick`, `mouse`, `cx.image`). Workers get plain data and return plain data (`Send + 'static`).
* A static scene costs ~0 CPU: all static visuals in `draw`, animation only in `overlay`, `Tick::Never` (or `Poll`) unless something actually moves. `Fps(30)` only for hands / waveforms; `Second` for per-second text.
* Do not call `invalidate()` per frame: every re-record re-renders the glass layer. Bump `version()` only when the displayed value really changed (compare, then bump).
* Hover / press easing inside a widget: `HoverAnim` (widget.rs). `set(cell, cx)` from `mouse` (it pokes `tick` for you), `if hv.step(cx) { Tick::Fps(60) } else { <idle schedule> }` in `tick`, `hv.get(cell)` 0..1 in `overlay`. It requests the last frame itself, so a settled widget costs nothing. A value that must change look for ~150 ms (a toggle, a month change) is left out of `draw` meanwhile and painted by `overlay` (old and new look at complementary opacity: `cx.with_opacity(k, ..)`, `cx.scaled(c, k, ..)`), then `version()` is bumped once to bring it back. Honour `reduced_motion()` (the system's "show animations" switch): jump to the end value.
* `overlay` must not allocate bitmaps, create fonts or parse anything; precompute in `tick` / `draw`. Typical budget: far under 1 ms.
* No unbounded growth (histories are fixed-length ring buffers), no polling loops, no `sleep` on the UI thread; `cx.spawn` threads end by themselves.
* A panic in `draw` / `overlay` / `tick` / `mouse` is caught per call (release profile `panic = "unwind"`): the card keeps its last picture and the process lives, but still: no `unwrap()` on I/O results, parse defensively, `.get()` instead of indexing.
* Colours and sizes are logical: never multiply by `cx.scale` yourself.

## 11. Known limits

* Cards are moved, resized (a fresh `Widget` instance with the same `cfg`: a `Widget` never changes size), added and removed at run time by the host (section 12); `cx.waker()` stays valid across all of that.
* At most 62 cards on the monitor the desk lives on (section 12). `lum` changes do not re-record `draw` (the framework tints the text shadow itself); call `invalidate()` if you really colour by luminance.
* A one-shot frame from `request_frame()` on a `Tick::Never` card is lost if a neighbour repaints that area afterwards; anything that must persist belongs in `draw` + `version()`.

## 12. Host features (arranging the desk; widget authors need none of this)

* **Drag & drop**: press on a card, the widget gets `Down` first; if it reports no change and the pointer moves > 5 px the card is lifted (scale 1.03, bigger shadow, glass follows live), the nearest free cell (spiral search, S 1x1 / M 2x1 / L 2x2) is outlined, on release the card springs (~250 ms) into it and `widgetsLayout` is saved. The layered window covers the whole monitor meanwhile and shrinks back to the card bounding box 150 ms after the settle. The widget never gets the `Up` of a press that became a drag.
* **Context menu** (right click, if the widget did not use it): native popup, "Изменить виджеты…" / "Размер" (S/M/L, check mark, only supported sizes; a new size keeps the cell or moves to the nearest free one) / "Удалить виджет"; ru/en by `lang`.
* **Edit mode**: menu item, `Ctrl+Alt+W`, leave with `Esc`, "Готово" or the hotkey. Window = whole monitor, activatable (keyboard for the search box), raised above apps, light dim, cards jiggle +-1.2 deg (10 fps, off with "animation effects" disabled in Windows), "−" on every card (pop-out), widgets get no clicks. Leaving hands the foreground back; switching to another app leaves edit mode.
* **Gallery** (opens with edit mode, bottom-left 780x430 at 6 %): group sidebar, search (name / description, ru + en; typing, caret, Backspace/Delete/arrows/Home/End, Ctrl+V, Ctrl+A), S/M/L chips, LIVE previews (one preview-mode scene per (widget, size), cached, <= 5 fps only when something animates), click / "+" = add at the first free cell (pop), drag a preview onto the desk = drop at that cell. Settings strip next to it: glass slider ("Стекло") and "Фон: Живое / Простой".
* **Settings panel** (`Ctrl+Alt+G`, tray menu, the "Settings" button of the edit-mode strip; Esc / Done / a click outside closes): the edit-mode machinery without the gallery (activatable, no jiggle, no "−", light dim), a glass panel at the right edge. Look sliders change the cards LIVE (drag = preview at <= 10 apply/s through `Scene::set_params` = fresh effect graphs, release = persist): Glass (`widgetsGlass`), Blur, Refraction, Chromatic fringe (`chroma_r` + `chroma_b`), Saturation, Brightness, Frosting (`tint0_g` + `tint1_g`), Highlights (`spec` + `sheen` + `sheen_mid`), Glow, Corner roundness (`radius_k`), Card size (`card_k`, applied on release: a new desk grid / capture through `rebuild`). Table: `glass::LOOK` (sub-range per slider, followers keep their ratio). Behaviour: Appearance / Dimming / Glass source segments, "Start with Windows" (`autostart`), Edit widgets, Quit. The look is stored as ONE object `widgetsLook` (only the changed keys; effective params = defaults -> glass.json -> widgetsLook). Headless: `--dump-ui out.png --state settings [--look '{"blur":9}'] [--hover k | 100+k] [--update-state idle|checking|latest|available|update-failed|downloading|verifying|failed]` (`--hover` on a slider also draws its keyboard focus ring).
  Panels: drag by an empty spot, the offsets (`Ui::gal_off` / `set_off`) are stored on release as `widgetsPanels` (`{"gallery":[dx,dy],"settings":[dx,dy]}`, only non-zero ones).
  Keyboard / wheel (`Ui::slider_key`, `ui::wheel_pos`, host `nudge_slider`): Up / Down / Tab / Shift+Tab move the focus, Left / Right 1 % (Shift 10 %), Home / End, Backspace / Delete = base look, wheel 2 % a notch; the value applies live like a drag and is committed with `commit_look` 400 ms after the last key. In the gallery Tab goes between the search box and the strip's glass slider.
  Update check (`update.rs`, opt-in): the button / the `checkUpdates` switch start ONE WinHTTP GET of `api.github.com/repos/chvdbhcx-del/glass-widgets/releases/latest` on a worker thread (`WM_UPDATE` back to the host); `tag_name` is compared with `CARGO_PKG_VERSION` (semver, pre-release < release), `html_url` must be on `https://github.com/`. From the same JSON `parse_release` takes the asset named exactly `GlassWidgets_<version>_x64-setup.exe` (`browser_download_url` must start with `https://github.com/chvdbhcx-del/glass-widgets/releases/download/`) and its `digest` (`sha256:<64 hex>`, parsed by `parse_digest`); a release without a well-formed digest gets `asset: None` and "Update now" refuses with a message.
  One-click update (only after the user presses "Update now"; "Release page" opens `html_url`): `update::start_install` runs a worker that `net::download`s the asset over HTTPS only into `%TEMP%\GlassWidgets-update\` (cap 20 MB; WinHTTP redirects are switched off and followed by hand, every hop must pass `host_ok` = github.com / objects.githubusercontent.com / release-assets.githubusercontent.com, https, no userinfo / port; at most 4 hops), posting the percent as `WM_UPDATE_DL` (wParam 0..=100, `DL_VERIFYING`, `DL_DONE`) which the host shows in the status line (`UpdateState::Downloading(p)` / `Verifying`). `verify_file` hashes the file with the own `sha256.rs` (streaming FIPS 180-4, tested against the standard vectors) through a handle opened `FILE_SHARE_READ` and compares it with the digest; on a mismatch the file is deleted and the panel shows the error (`Available { error }`, the button stays for a retry). Only the resulting `Verified` token can be passed to `launch`, which starts `setup.exe /S /UPDATE` (CreateProcessW through `Command`) while the verified handle is still open; the host then flushes its look and destroys its window. `installer/glass-widgets.nsi` kills the old copy, replaces the files and, only for a silent run that got `/UPDATE` (`${GetParameters}` / `${GetOptions}` of FileFunc.nsh), starts the new exe; a plain `/S` install does not start it. The downloaded installer stays in `%TEMP%` until the next update replaces it. Nothing is downloaded or run without the click.
* **Config keys** (`%APPDATA%\macshell\config.json`): `widgetsAppearance` (auto / dark / light glass, section 13), `widgetsLayout` (only written on user edits: temp file + rename, unknown per-item keys and foreign widget types are kept, an unparsable file is never touched), `widgetsGlass` 0..1, `widgetsGlassMode` ("plain" = no desktop capture, else live), `widgetsDim` (auto / always / never: cards go monochrome while an app window is in front), `widgetsLook` (settings sliders, see above), `widgetsPanels` (dragged panel offsets), `checkUpdates` (bool, default false), `accentMode` (`blue` | `wallpaper`: `accent.rs` decodes the wallpaper at <= 64 px, takes the hue with the most saturation x brightness weight in a 36 bin histogram and clamps saturation / lightness; grey wallpapers give the default blue; re-checked every 2 s by wallpaper path + file time; headless `--accent RRGGBB` forces a colour, `--accent-of image` prints the pick), `profiles` / `activeProfile` (layout profiles: `{name: {items, look}}`, the tray's "Layouts" submenu; switching writes the current `widgetsLayout` + `widgetsLook` back into the active profile first, then loads the chosen one and rebuilds the desk; pure JSON transformations in layout.rs). Polled every 2 s while visible. New ids: `<type>-<base36 millis>`.
  Update check (`update.rs`, opt-in): the button / the `checkUpdates` switch start ONE WinHTTP GET of `api.github.com/repos/chvdbhcx-del/glass-widgets/releases/latest` on a worker thread (`WM_UPDATE` back to the host); `tag_name` is compared with `CARGO_PKG_VERSION` (semver, pre-release < release), `html_url` must be on `https://github.com/`; "Download" only ShellExecutes that page. Never downloads or runs anything.
* **Config keys** (`%APPDATA%\macshell\config.json`): `widgetsAppearance` (auto / dark / light glass, section 13), `widgetsLayout` (only written on user edits: temp file + rename, unknown per-item keys and foreign widget types are kept, an unparsable file is never touched), `widgetsGlass` 0..1, `widgetsGlassMode` ("plain" = no desktop capture, else live), `widgetsDim` (auto / always / never: cards go monochrome while an app window is in front), `widgetsLook` (settings sliders, see above), `widgetsPanels` (dragged panel offsets), `checkUpdates` (bool, default false), `captureFps` (`auto` | 1..30), `accentMode` (`blue` | `wallpaper`: `accent.rs` decodes the wallpaper at <= 64 px, takes the hue with the most saturation x brightness weight in a 36 bin histogram and clamps saturation / lightness; grey wallpapers give the default blue; re-checked every 2 s by wallpaper path + file time; headless `--accent RRGGBB` forces a colour, `--accent-of image` prints the pick), `profiles` / `activeProfile` (layout profiles: `{name: {items, look}}`, the tray's "Layouts" submenu; switching writes the current `widgetsLayout` + `widgetsLook` back into the active profile first, then loads the chosen one and rebuilds the desk; pure JSON transformations in layout.rs). Polled every 2 s while visible. New ids: `<type>-<base36 millis>`.
* **Capture pace** (`backdrop/pace.rs`, pure + unit tested): `Pace::observe(changed, now)` returns the next polling interval; auto = the ceiling (WGC 15 fps, `--backdrop-fps` for the others) then 8 -> 4 -> 2 fps, one rung per 2 s without a change, any change (or `snap()`) back to the ceiling; `captureFps` = a number gives a fixed rate (WGC up to 30). WGC copies every frame it gets into the shared texture (so it is always current) but flags a card dirty only when the hash of its region changed: the crop is copied into a mip chain texture (`GenerateMips`), level 3 (1/8 size) goes to a staging texture and the covered pixels are FNV-hashed per region (`wgc::region_hashes`, `changed_mask`); a failing sampler means "everything changed" (the old behaviour). Changes below one 8-bit step of a 8x8 average are not seen. The thread naps in 25 ms slices (`Shared::nap`) that end on stop, on moved regions and on `Backdrop::boost()` (host: edit mode, drag, glass / appearance / config changes). DXGI duplication is event driven and unchanged. Measure: Task Manager (Details, mswidgets.exe CPU) with a still wallpaper before / after, or `mswidgets --bench 30` (`cpu_percent_steady_state`, `capture_rate_hz_at_exit`, `achieved_backdrop_fps`).
* **Next event link** (`widgets/nextevent.rs`): `ics` = path or `https://` / `webcal://` link. `normalize_url` (webcal -> https, https only, no userinfo / backslash / non-ASCII / IPv6 / odd IPv4 spellings / single-label or `.local`-like hosts / private, loopback, link-local, CGNAT, multicast ranges) validates the first link and every redirect hop (`net::http_get_once` does not follow redirects; <= 5 hops, 15 s in total, 2 MB). Worker order: cached copy first (instant, offline), then the conditional GET (`nextevent-<fnv64 of the link>.ics` + `.meta` with ETag / Last-Modified in %APPDATA%\macshell\cache, written with temp + rename); a non-calendar answer is never cached; 304 or a failure falls back to the cache and retries in 5 min; refresh every 30 min. Errors and logs carry only the host. Known gap: a public name that resolves to a private address is not caught.
* **Monitors** (`monitors.rs`, pure + unit tested on synthetic monitor lists; `host::monitor()` is the only caller of EnumDisplayMonitors / GetMonitorInfoW): `widgetsMonitor` = `primary` (default) | `cursor` (resolved once at start, then pinned by device name) | a number (display order: the primary monitor is 1, the others follow left to right) | a device name (`\.\DISPLAY2`, `MONITORINFOEXW::szDevice`, the stable key; the settings panel's "Monitor" control and the tray's "Monitor" submenu store the primary one as `primary`, the others by name). A choice that does not exist (any more) resolves to the primary monitor, so an unplugged display falls back through the normal `WM_DISPLAYCHANGE` -> `replace_monitor` path and the desk returns when it comes back. Rect / work area (`rcWork`) / DPI (`GetDpiForMonitor`) / DXGI output (matched by `DXGI_OUTPUT_DESC::DeviceName`) all come from the chosen monitor. WGC captures the whole `Progman` window, which spans the virtual desktop; the crop is `wgc::crop(monitor origin - window origin, ...)` (negative origins for monitors left of / above the primary one). A monitor on another GPU than the UI device (LUID compared) gets the GDI capture, because the WGC / duplication textures are shared with the UI device. Only a fullscreen app on the desk's monitor hides the widgets.
  Layouts per monitor: `widgetsLayout` is the PRIMARY monitor's layout (nothing changes for existing users); any other monitor's cards are `monitorLayouts["\.\DISPLAYn"]` (an empty desk the first time). `layout::with_slot` swaps the active monitor's layout in as `widgetsLayout` around any config transformation (save, per-card cfg, profiles), so all of those work on the active monitor without knowing about monitors; `monitorProfiles[name]` is that monitor's `activeProfile` (profiles themselves, `widgetsLook`, panels and every other key are global). Choosing a monitor writes the current layout back into its own slot, stores `widgetsMonitor` and rebuilds the desk there (`set_monitor` -> `replace_monitor` -> `rebuild`, which reloads `items` when the slot changed); an unparsable config.json aborts the switch.
* **Process**: single instance (named mutex, a second launch exits silently), `Ctrl+Alt+Q` quits, `--no-hotkeys` registers neither hotkey (a taken hotkey is only logged), display / DPI / work-area changes re-place everything, host panics are caught per message.
* **Headless**: `mswidgets --dump-ui out.png --backdrop out\bd_sunset2.png --state live|edit|gallery|menu --size 1600x900 [--scale 1.5] [--cards "clock:s@0,0;..."] [--search text] [--group weather] [--psize l] [--hover 1] [--dim]` renders the same scene + UI code (jiggle at a fixed phase). `menu` shows the desk only: the real menu is a native popup. Unit tests: `cargo test` (grid.rs, layout.rs, ui.rs, host.rs, cards.rs).

## 13. Liquid Glass (renderer, motion, calibration)

Everything below lives in `glass.rs` (GPU), `motion.rs` (pure logic, unit tested), `liquid.rs` (merge pass), `calib.rs` (fit) and the plumbing in `cards.rs` / `host.rs`.
Widgets know nothing about it except that their palette whites turn into dark ink in the light appearance (`Cx::ink`, applied in `Cx::brush` and gradients).

**Pipeline.** Static layer per card (`Glass::draw`, re-rendered only when the backdrop, the widget version, the glass slider or the appearance changes):
shadow (two blurred rounded rects, alpha by backdrop luminance) -> chromatic lens: the displacement map is applied THREE times (R, G, B at `1 - chroma_r`, `1`, `1 + chroma_b` of the
strength), each pass isolated with a colour matrix and recombined with Composite PLUS, then blur, then one colour matrix (saturation x brightness) -> white tint gradient -> adaptive dark veil
(dark look) or milky veil (light look) -> inner glow -> sheen (towards the side opposite the light) -> thin dark edge line + thin base rim.
Dynamic highlights (`Glass::draw_hi`, painted over the finished card inside the frame / scratch compose, a handful of gradient strokes, no allocation, no layer): the specular rim is a
linear-gradient stroke along the LIGHT AXIS (bright and thin at the lit extreme, a fainter counter highlight at the opposite one), plus soft inner bands, the pointer glint and the materialise sweep (radial gradient strokes at a contour point).

**Parameters.** `GlassParams` (glass.rs) holds every constant (blur, refraction strength / band / exponent, chromatic split, saturation / brightness, tint, veils, sheen, glow, rim, edge line, specular,
glint, light angle, light-appearance multipliers, shadow). Override with `--glass-params file.json` or `%APPDATA%\macshell\glass.json` (absent = built-in defaults; unknown keys are reported and ignored).
`mswidgets --calib defaults` prints the defaults as JSON (`calib/glass_defaults.json`).

**Light.** Default direction `light_angle` = 315 deg (from the upper left). While a card is dragged or springs into its cell its light is pushed against the card velocity and relaxes back with a spring
(~400 ms, `LightSpring`); only that card's rectangle is repainted.

**Hover / press** (`Scene::mouse` -> `step_fx`, pure parts in `motion.rs`). Live mode only (edit mode and a drag send `Leave`). The hover progress ramps in 180 ms / out 250 ms (smoothstep) and is
held while the pointer stays on the card (no timeout). While hovered: a soft white light spot under the pointer (`hover_spot` 0.10 alpha on dark glass, x1.6 on light, radius `hover_spot_r`
78 px, filled into the card shape, trails the pointer with a 70 ms low-pass) and the rim glint at the contour point nearest to that smoothed pointer (strength `hover_rim`: 0.5 floor inside,
1 on the edge; the contour fraction is low-passed the short way round, so a jump across the card slides along the rim). The card scale springs (`motion::Bump`, omega 24) to 1.012 on
hover with a soft overshoot, to 0.985 while the left button is held (critically damped, the spot dims 40 %) and back with a small overshoot on release; the widget still receives every
event unchanged. The scale is a draw-time factor on top of `Xf` (not part of the merge distance), composed through the scratch path with the cubic filter (crisp text).
Nothing animates once the ramps are done and the spring rests: a pointer resting on a hovered card arms no timer.
There is NO idle drift: a static scene arms no timer. Everything animates on the host's single 16 ms animation timer, which stops when `Scene::fx_busy()` is false.

**Materialise.** A card that appears (start-up staggered by 70 ms, gallery add, resize) condenses: scale 0.92 -> 1 with a spring overshoot, lens blur sigma 24 -> normal, fade in, the rim sweeps once around the contour
(450 ms); removal reverses it (250 ms, blur out + fade). With "animation effects" off in Windows (SPI_GETCLIENTAREAANIMATION) it is a plain 180 ms fade. The diffusion re-renders the glass layer per frame and softens the composed card.

**Merge.** Two cards closer than 15 px (logical; the grid gap is 16, so resting cards never merge) pull a liquid neck: the two rounded rects are blurred (sigma 11 px) and thresholded (LinearTransfer) = metaballs,
the glass (lens + tint + veils + a rim along the neck contour) is poured into the neck, clipped to the zone between the cards minus their interiors. It is a separate pass that only runs for close pairs; the normal path is untouched otherwise.

**Appearance.** Per card, from the 8x8 probe of the backdrop (mean, 25th percentile, share of bright cells, contrast under the text): `light_score = 0.5 mean + 0.5 p25 - 0.5 max(0, contrast - 0.10)`;
flip to light at >= 0.66, back to dark at <= 0.56, the score is low-passed and a live flip needs two probes in a row (probes run <= 2 Hz). The glass cross-fades in 250 ms (the content is recorded once per ink and cross-faded).
Config `widgetsAppearance`: `auto` (default) | `dark` | `light`; CLI `--appearance auto|dark|light` (also for the headless renders); per card in `--cards`: `{"_appearance":"light"}`.

**Headless knobs.** `--dump` / `--sheet` / `--dump-ui` accept `--appearance` and `--glass-params`. Card cfg keys starting with `_` drive the renderer (`--cards "calendar:s@0,0{\"_light\":90}"`):
`_light` deg (fixed light direction), `_mat` ms / `_mout` ms (a card that far into its materialise / dematerialise), `_move` [dx, dy] logical px off its cell (merge tests), `_hover` [x, y] logical card px (spot + glint + lift + widget hover; `_hover_t` 0..1 = that far into the ease-in, default 1; `_press` true = button held, the widget hears no click), `_appearance`.
`--anim-strip out.png --cards "calendar:s@0,0" [--times 0,40,...] [--anim-out]` renders the first card once per time step side by side (filmstrip).

**Glass calibration** (`calib.rs`, `calib/ref/*.png` = the Apple reference screenshots, `calib/glass_*.json`). Reproduce: `mswidgets --calib eval [--glass-params p.json] [--calib-dump dir]` (table + side by side PNGs
reference | render | |diff| x4), `--calib fit --calib-rounds 8 --calib-out out.json` (search), `--calib scan-light` (error vs light angle); run from `native\` (references are read from `calib\ref`, `--calib-dir` overrides).
Method: for every reference crop a synthetic backdrop is rebuilt from the wallpaper beside the card (per row: blend of the strip left and right of it), the glass is rendered over it at the reference pixel size and
reduced to features: (a) edge profiles, RGB perpendicular to every usable edge from 10 px outside to 24 inside (median along the middle half of the edge): rim peak / width, inner fall-off, shadow outside;
(b) the vertical profile of the per-row MEDIAN colour over the middle 60 % of the card (removes the text): tint, contrast against the backdrop, blur (how a backdrop gradient is smeared), colour matrix;
(c) corner profiles along the 45 deg diagonals. `distance` = mean absolute RGB difference (0..1) of edges + rows + 0.6 corners. References: cmp01 and cmp03 (sharp, calendar + weather S, edges + rows + corners),
clear_z / dark_z (blurry zoomed video frames: interior tone only, weight .35), gallery.png (Apple's light frosted panel: the light appearance).
Search: coordinate descent (step 25 % of the range, halved per round, small prior towards the defaults) over blur, bright, sat, tint0/1, veil_*, rim_w, edge_dark, glow_sigma (dark), then milk, light_* (light).
NOT searched (kept at design values): specular rim, sheen, inner glow, shadow shape, base rim alpha - the references are older flat-lit widgets without them and a free fit simply deletes them (measured: spec, sheen, glow and the shadow all go to
their lower bound / zero) - and refraction / chromatic constants: no reference has a grid or text crossing a card edge, so the displacement itself is unconstrained (only its colour effect on the edge profiles is seen).
Result (weighted distance, lower is better): old constants 0.272 -> baked defaults 0.118 (-57 %); the sharp cmp cards 0.25-0.40 -> 0.09-0.16 (edges 0.11-0.16 -> 0.045-0.067, rows 0.08-0.14 -> 0.025-0.042).
The bright-green clear_z crops got worse (0.044 / 0.064 -> 0.12 / 0.14): they want a lighter glass than the warm references (one `bright` for all backdrops; Apple tints per backdrop, we use the luminance veil).
Residual: the references show a 1 px DARK outline on the left / right edges and a bright rim on top / bottom (lit from above), which a uniform rim + a 315 deg light cannot reproduce pixel-exact (edge error stays ~0.05);
the light angle is nearly irrelevant to the fit (error 0.116-0.140 over 0..330 deg, best 150 deg, 315 deg is 2 % worse), so the product default 315 deg stays.
