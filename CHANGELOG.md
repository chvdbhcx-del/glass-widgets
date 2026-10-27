# Changelog

## [Unreleased]

## [0.5.13] - 2026-10-27

### Changed
- Smooth data everywhere: rings, bars and numbers glide to each new reading instead of jumping (System, Disks, Batteries, Processes, Network, Crypto, Rates, Weather). Reduced motion and previews jump as before; a settled card costs nothing.
- Processes: rows move to their new rank (a process that falls out of the top slides down and fades at the bottom edge, one that climbs slides up from below); near-equal low rows keep their order, a row overtakes another only when it is clearly bigger.
- Network: the big speeds show the mean of the last ~4 s; the graph is current (its right edge is the newest sample) and scrolls continuously between samples. System graphs scroll the same way.
- Sparklines of the System and Network cards animate at 60 fps while the card is visible (about 3 % of one core measured with both open).

## [0.5.12] - 2026-10-27

### Added
- The equalizer of the Music card shows the real spectrum of what is playing: 24 bands from 50 Hz to 14 kHz from a WASAPI loopback capture of the default output device (2048-point FFT every ~16 ms, auto-gain, fast attack and slow release). The capture runs on one thread only while the card is on screen and playing and stops 2 s after; the sound is analysed in memory and never stored or sent. Previews, `"eq_audio": false` in the card's `cfg` and a device that cannot be captured keep the old animated bars.
- Click action menu: the card menu has a "Click action" submenu: Default, Nothing, the presets of that widget (for example Task Manager for System) or a custom target (a program, link or `ms-settings:` page, validated before it is stored in the card's `cfg.click`). World clock: a click on the title opens Alarms & Clock.
- Network: a click on the unit label flips MB/s and Mbit/s (decimal bits, as Task Manager and speed tests show them); stored in `cfg.unit`. The MB/s figure is MiB/s, like the other widgets.
- Smooth S/M/L resize: changing a card's size springs the glass rectangle to the new size (the old and the new content cross-fade inside it, the bridges to neighbour cards follow); reduced motion jumps, nothing runs once it has settled.
- Music card track change: the title and artist cross-fade, the last track stays on the card for 3 s while the player gives no session or empty properties (no more "Not Playing" flash between tracks) and the previous cover stays until the new one arrives.
- Widgets open Task Manager / Explorer / calendar / weather site / settings on click. A left click on a card now opens the matching Windows tool: System, Network and Processes open Task Manager, Disks opens This PC, Clock opens Alarms & Clock, Batteries opens the Bluetooth settings, Calendar and Next event open the calendar app (or Google Calendar when none is installed), Weather opens a web search for the weather in the shown city. In Quick Actions the glyph left of the volume slider opens the Sound settings and the one left of the brightness slider the Display settings (the slider itself is not touched). Nothing opens in edit mode, while a card is dragged or in previews.
- Weather: the city is now changed by clicking the city name (it lights up under the pointer); the rest of the card opens the weather site. Without a forecast ("City not found") the whole card still asks for a city.
- Next event: with a working calendar a click opens the calendar, Ctrl+click changes the .ics path or link; an unset or broken source keeps the plain click that asks for it.

### Changed
- Less work per redraw, same frame rates. When the wallpaper moves, a card now recomputes only the lens (the part that depends on the backdrop): the two blurred shadows, the inner glow and the soft text shadow of the card content are kept as bitmaps and redrawn as plain copies (the shadows follow the 2 Hz luminance probe, not the 15 Hz capture). Pixel-identical to before (checked on the headless renders), about a third less CPU per redraw of the whole desk (8 cards: 3.2 -> 2.1 ms, 3.3 -> 2.4 ms while the luminance changes every 7th redraw). `MSWIDGETS_NO_GLASS_CACHE=1` returns to the old path.
- Background jobs (`cx.spawn`: weather, rates, volume, PowerShell, file scans) run on a small worker pool (4 threads, a few more only while a job blocks) instead of a new OS thread per job; the NVIDIA driver kept a small object for every thread ever created. Same API, same wake-ups, jobs start in submission order.
- The lens colour matrix and blur sigma are written only when they changed; the wallpaper-colour probe keeps its bitmaps between calls.

### Fixed
- A card whose content holds clipped layers (the music cover) could stop redrawing with `D2DERR_LAYER_ALREADY_IN_USE` after a few backdrop updates (seen in a preview desk with the sample cover): the text shadow effect and the direct drawing of the same recorded content shared one draw session. The cached text shadow is rendered in its own session.

## [0.5.11] - 2026-10-26

### Fixed
- The widgets used about half a core and about 5 MB more memory every minute (1.2 GB after three hours). The Quick Actions poll ("every 2 s") had no time gate: each finished job woke the card, which started the next one at once, about 480 short-lived threads a second (audio endpoint, radios, a monitor brightness read over DDC/CI), and the NVIDIA driver keeps a small object for every thread it has seen. The poll now starts at most once per 1.9 s, so the radios are read about every 10 s and the brightness every 2 minutes. Idle CPU with a video wallpaper: about 50 % -> about 8 % of one core, memory stays flat.
- A prompt thread that panicked left the text prompt "busy" for good; the GDI capture fallback leaked a bitmap when it stopped.

### Changed
- The desk sleeps while nobody can see it: the session is locked or on the secure desktop (UAC), the monitors are off, the PC suspends, or other windows cover every card completely (a card that is even partly visible keeps everything running; opening the editor, the menu or a prompt wakes it within 100 ms). The capture and the live glass stop, and everything resumes at once on unlock / wake. `MSWIDGETS_NO_AUTOPAUSE=1` turns the new reasons off.
- Capture: the Windows capture session gets a minimum update interval that matches the look rate, so the compositor no longer delivers frames at the video's rate while only 15 a second are used; the capture threads wait on a parked timeout instead of 25 ms naps (stop and boost are immediate).
- `accent.txt` is rewritten only when the colour changed (it was rewritten every 8-10 s).

## [0.5.10] - 2026-10-26

### Changed
- Hover: a card now reacts to the pointer for as long as it is over it. The rim glint no longer vanishes 62 px from the edge (it stays at half strength or more inside the card and follows the pointer along the rim), a soft light spot trails the pointer, the card lifts a little (about 1 %) and eases back out when the pointer leaves. Pressing the left button presses the card in and it springs back on release; the widget still gets every click.
- Weather: the hourly labels read "02:00" instead of a bare "2" (which looked like a day of the month); in M the hours block is wider so "Сейчас" and "02:00" do not touch.
- The accent from the wallpaper follows what is really on the screen: a grey / black / white picture gives a neutral light accent instead of the blue, and with the live glass source the accent starts from the last live colour instead of the colour of the wallpaper file.

- The text prompt (new reminder, note, city, timer, shortcut, calendar path...) is no longer a separate Windows popup: the host draws it on its own canvas, next to the cursor, in the look of the settings panel and the gallery: a neutral glass panel (no accent colour), a text field like the gallery's search field (white 12 % layer, white focus ring, dimmed placeholder, blinking caret), the pill buttons "Cancel" and a white "Done", the same fade and a few px of glide as the other panels. The field has its own text model: selection (Shift+arrows, mouse drag, double click on a word, Ctrl+A), Ctrl+C / X / V, Ctrl+arrows / Backspace / Delete by word, Home / End; a note (multiline) wraps softly to the field, Enter is a new line (Ctrl+Enter confirms), Up / Down / Home / End walk the visual lines, the wheel and the caret scroll it. Enter confirms, Esc or a click outside cancels, Tab walks field > Done > Cancel. The cards behind are not reachable while it is open (a faint veil marks it as modal); it also works on the live desk, where the layer takes the keyboard for the time and gives the foreground back afterwards. The Win32 popup of 0.5.10's first restyle is only the fallback now (no host such as `--prompt-demo`, the layer hidden by a fullscreen app, or `MSWIDGETS_PROMPT_WIN32=1`).
- `--dump-ui` renders the prompt too: `--state prompt|prompt-multi [--search text] [--hover 1]`.
- Settings panel motion: hover on rows, buttons and segments eases in (120 ms) and out (180 ms); the selected-segment pill slides to the new option and the label under it darkens with it; the two switches cross-fade their fill (170 ms) and the knob glides and stretches while pressed; a slider handle swells under the pointer, glides to a value set by a click on the track, Reset or the keyboard, and still follows a drag 1:1; buttons shrink to 97 % on a spring while pressed; the update status line cross-fades when its kind changes and "Release page" / "Update now" fade in. Only the controls that move are repainted, the animation timer stops once everything has settled, reduced motion applies the final values at once. `--dump-ui` shows a motion still with `MSW_SA_MS=<ms>`.
- Toasts fade in with an 8 px rise (180 ms, ease-out) and fade out (220 ms); the toast and the monitor-wide canvas it needs in live mode are kept until the fade-out has ended.
- Widget motion: a shared `HoverAnim` helper (ramps stepped in `tick`, painted in `overlay`, 60 fps only while moving) eases the Shortcuts hover plate and icon scale, the Calendar L chevrons (fade in with the pointer, light up under it), the Clipboard row / "Clear" highlight and the "Copied" flash (fade in and out). Calendar L month changes slide the old month out and the new one in (200 ms). Quick Actions: the slider knob and the percent follow the real value on an exponential approach (the real volume / brightness write is never delayed), the knob swells under the pointer, button plates ease in / out on hover and press, and toggles and the armed "Click again" pill cross-fade (170 ms). Music: the play / pause icon swaps with a 120 ms scale cross-fade, and in M the previous cover fades out over the new one (250 ms).
- Gallery: a new list of previews (group, size chip, search) fades in tile by tile with a 20 ms stagger.
- The widget context menu is drawn by the app in the same glass look instead of the stock Windows popup: a neutral glass panel (no accent colour) at the cursor, kept inside the work area (it opens to the left of / above the cursor when there is no room), with hover pills, a check mark on the current size, a chevron on "Size" whose submenu opens to the side after a short hover (or on click, Right, Enter), the greyed sizes the widget does not offer, the red "Remove widget", the same fade and a short grow as the other panels. Esc, a click or a right click outside close it; Up / Down / Home / End walk the rows, Enter / Space / Right choose or open, Left closes the submenu. It also works on the live desk (the layer covers the monitor and takes the keyboard for the time). The native popup remains only as the fallback (`MSWIDGETS_NATIVE_MENU=1`, or while the text prompt / the settings panel is up). `--dump-ui --state menu|menu-size|menu-one` renders it.

### Fixed
- Every animation crawled while the pointer moved (the host reset its animation clock on each mouse move).

## [0.5.9] - 2026-10-25

### Changed
- Weather L: the glyph, the condition and high / low sit next to the temperature instead of at the far right edge; "Today" in the daily list no longer truncates to "Сегод…".

## [0.5.8] - 2026-10-25

### Changed
- Weather M: a little more air between the hour, the glyph and the temperature.

## [0.5.7] - 2026-10-25

### Changed
- Weather M: the hourly forecast is one tight block (hour, glyph, temperature close together, centred on the card) instead of three rows spread over the full height, which did not read as columns.

## [0.5.6] - 2026-10-25

### Fixed
- Edit mode: the jiggle has the same swing of the edges for every card size (the angle of M and L cards is smaller, it used to be the same 1.2 degrees and looked much wider on big cards).

## [0.5.5] - 2026-10-25

### Fixed
- Edit mode: the jiggle of the cards was jerky. It ran at 10 fps once the gallery had opened, started at full strength on the first frame and stopped dead when the settings opened. It now runs at 30 fps and swells in and fades out in 0.4 s.

## [0.5.4] - 2026-10-25

### Changed
- The gallery, the strip and the settings panel are neutral: "Accent on: Everywhere" no longer tints their text and icons, and the selection, the Done button, the add badge and the focus rings are white glass instead of the wallpaper accent. The accent stays in the widgets.

## [0.5.3] - 2026-10-25

### Changed
- Calendar: the circle of today in the S grid has radius 9 and the digits sit in its middle (the ink was half a pixel right and down of the centre).

## [0.5.2] - 2026-10-25

### Changed
- Calendar S: the circle of today is a little smaller (radius 9.5).

## [0.5.1] - 2026-10-25

### Changed
- Big numbers: round commas (Inter's stylistic set 3, the default comma hung far below the baseline) and proportional figures from 36 px up (a tabular "9,0" had wide holes around the comma); small numbers that change in place stay tabular.
- Calendar: the circle of today is larger, the digits no longer touch its edge.

## [0.5.0] - 2026-10-25

### Added
- Setting "Accent on: Main / Everywhere" (`accentScope`). Everywhere is Apple's "Tinted" style: all text, icons and graphics of the cards take the accent (the red / orange warnings keep their colour); Main keeps the accent on titles, rings and bars.
- Developer aid: `MSW_TEXT_LINT=1` + `tools/text-lint.py` report how evenly the text sits in every card and which sizes it uses.

### Changed
- One type scale for the whole desk: every font size snaps to 10 / 11 / 12 / 13 / 15 / 17 / 20 / 24 / 28 / 34 / 40 / 46 / 54 / 64 / 76 px (no more 14.276 or 9.5).

## [0.4.0] - 2026-10-25

### Changed
- The wallpaper accent is now taken from the captured desktop (what is really on the screen, also a dynamic or slideshow wallpaper), not from the wallpaper file.
- Rings and bars of Disks / System / Network follow the accent below the warning levels (orange and red stay); the calendar dims weekends and past days like Apple's.

### Added
- One-click self-update: when the update check finds a newer version, "Update now" downloads `GlassWidgets_<version>_x64-setup.exe` (HTTPS, github.com and its release CDN only, at most 20 MB, progress in the status line), verifies its SHA-256 against the `digest` GitHub publishes for the asset and, only if it matches, runs it with `/S /UPDATE`; the installer replaces the app and starts it again. A missing / malformed digest or a mismatch deletes the file and shows an error. "Release page" keeps the old behaviour (opens the release in the browser). Opt-in by the click, never automatic.

### Changed
- The accent colour (blue, or from the wallpaper) now also colours the chrome of the panels: the "Done" button, the selected gallery group, the add badge, the focus rings and the search selection (a deeper shade of the accent where white text sits on it). The switches stay green.
- Next event: the `ics` setting may also be a calendar subscription link (`https://` or `webcal://`). Fetched by the card's worker only while it is on the desktop, at most every 30 min (conditional GET with ETag / Last-Modified, 15 s timeout, 2 MB limit, redirects re-checked at every hop), cached in `%APPDATA%\macshell\cache
extevent-<hash>.ics` for offline use; the card shows a hint when the first fetch fails and nothing is cached. Plain http, `user@` tricks, `localhost` / `*.local` and private addresses are refused; the link (often a secret) is never logged, only its host. The prompt now reads "Путь к .ics или ссылка на календарь".
- `captureFps` in config.json: `auto` (default) or a fixed wallpaper capture rate `1`..`30`.

### Changed
- Lighter wallpaper capture: a GPU-downscaled sample of the area under each card is compared with the previous one, so a card is redrawn only when its own backdrop changed, and the capture rate backs off 15 -> 8 -> 4 -> 2 fps while nothing changes (the GDI fallback too). The first change, a card move, edit mode, a drag or a look / appearance change snaps it back to full rate at once, so a video or slideshow wallpaper stays smooth. `--bench` prints `capture_rate_hz_at_exit`.

## [0.3.0] - 2026-10-25

### Added
- Accent colour of card titles from the wallpaper (settings: Accent, Blue / Wallpaper; `accentMode`), re-read when the wallpaper changes.
- Layout profiles: a "Layouts" submenu in the tray saves the desk and the look under a name and switches between them (`profiles`, `activeProfile`).
- Multi-monitor: the desk can live on any monitor (`widgetsMonitor`: primary / cursor / a monitor number / its device name), chosen with the new "Monitor" control of the settings panel or the "Monitor" submenu of the tray. The work area, DPI, wallpaper capture and window follow the chosen monitor; each non-primary monitor has its own layout (`monitorLayouts`, an empty desk the first time), `widgetsLayout` stays the primary monitor's. An unplugged monitor falls back to the primary one and the desk returns when it is plugged in again. A fullscreen app on another monitor no longer hides the widgets.
- The gallery and the settings panel remember where you dragged them (`widgetsPanels` in config.json).
- Settings sliders work from the keyboard (Up / Down / Tab move the focus, Left / Right 1 %, Shift 10 %, Home / End, Backspace / Delete = reset) and with the mouse wheel (2 % a notch); the glass slider under the gallery too (Tab from the search box).
- Update check, opt-in: a "Check for updates" button and a "Check for updates on start" switch (`checkUpdates`, off by default) in the settings panel. One request to the GitHub releases API; a newer version is offered as a "Download" button that opens the release page. Nothing is downloaded or installed.
- Next event widget (S / M / L): the upcoming events of a local `.ics` file (`ics` setting; click to set the path). UTC, local and all-day times, simple DAILY / WEEKLY / MONTHLY / YEARLY repeats.
- Crypto widget (S / M / L): prices and the 24 h change of a few coins from the public CoinGecko API (coins / currency in the settings). Network only while the card is on the desktop; previews use sample data.
- Countdown widget (S / M / L): days left to a date you set, with a progress ring.

### Fixed
- Text that was cut with an ellipsis although it could fit: the weather condition (shorter wording), the music title in S (wider, steps down in size), long reminder titles in M (smaller font or two lines), disk rows (shorter "free of" wording), big prices in rates.

## [0.2.1] - 2026-10-25

### Changed
- Text is smoother: unhinted symmetric grayscale rendering by default (the hinted mode made edges look blocky; `MSW_TEXT_MODE=natural` brings it back).
- The typeface is now Inter (bundled, SIL OFL), crisper at small sizes than Nunito. `MSW_FONT=nunito` selects Nunito, `MSW_FONT=system` Segoe UI.

## [0.2.0] - 2026-10-25

### Changed
- Typography: the bundled Nunito typeface (SIL OFL, Latin + Cyrillic) is used everywhere, hinted grayscale text with even digits and stems, heavier captions, brighter secondary text. `MSW_FONT=system` falls back to Segoe UI.
- Disks widget: roomier ring, clearer M / L rows.
- Text prompt (new reminder, note, timer...): rounded glass-style panel with a field well, placeholder and Add / Cancel buttons.
- New installer: artwork, app icon, English and Russian pages, optional desktop shortcut and autostart, project link on the last page.
- Wider slider ranges, so every slider visibly changes the look; the icon is embedded into the exe.

### Fixed
- Changing the card size left the glass without a backdrop.
- Dimming no longer turns the widgets grey when another window is active (it is off by default).

### Added
- Tray icon with a menu (settings, edit widgets, hide / show, start with Windows, quit); it is added again after Explorer restarts.
- Settings panel (`Ctrl+Alt+G`, tray, or the edit-mode strip) with 11 live sliders for the look of the widgets, appearance / dimming / glass-source switches, an autostart switch and "Reset look". Slider values are stored in `widgetsLook`.

## [0.1.0] - 2026-10-25

First public release.

- 19 widgets, Liquid-Glass renderer on Direct2D, drag / edit mode / gallery, light and dark appearance, merge of touching cards.
- The glass reads the wallpaper through Windows Graphics Capture, so the widgets are visible to screenshots and recorders.
- Shortcut tiles for sites show the site's own icon.
- Per-user installer (NSIS) with an optional "start with Windows" entry.
