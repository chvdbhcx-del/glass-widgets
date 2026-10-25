# Glass Widgets

[![Release](https://img.shields.io/github/v/release/chvdbhcx-del/glass-widgets?color=3b82f6)](../../releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
![Windows 10/11](https://img.shields.io/badge/Windows-10%20%7C%2011-0078d4)

![Glass Widgets on a wallpaper](docs/images/hero.png)

Native desktop widgets for Windows 10/11 with a Liquid-Glass look: translucent, refracting cards that float over your wallpaper
(an animated one too). No WebView, no Electron: one transparent layered window drawn with Direct2D on the GPU, a 0.7 MB installer
and roughly 100 MB of RAM and under 10 % of one CPU core while the wallpaper moves.

*По-русски: нативные виджеты рабочего стола для Windows 10/11 в стиле «жидкого стекла»: полупрозрачные карточки с преломлением
поверх обоев (в том числе живых). Без WebView и Electron, на Direct2D, установщик меньше 1 МБ.*

## Highlights

- **Real glass**: each card refracts and blurs the wallpaper behind it, with a chromatic fringe, a specular highlight that follows
  the light and a soft materialise animation. Cards that touch merge into one blob.
- **Dark or light glass** (`dark` by default) and crisp, smooth Inter typography (bundled, SIL OFL; Nunito is available with `MSW_FONT=nunito`).
- **Live wallpapers work**: the glass reads the wallpaper window itself, so it follows videos and animated wallpapers.
- **Visible to screenshots and screen recorders**: the layer never captures itself.
- **Light**: about 100 MB of RAM and under 10 % of one core with an animated wallpaper.
- **Private**: the network is used only by the widgets that need it (weather, rates, crypto, quote, site icons); no telemetry.

| Dark glass, edit mode and the widget gallery (`Ctrl+Alt+W`) | Light glass |
| --- | --- |
| ![Widget gallery](docs/images/gallery.png) | ![Light appearance](docs/images/light.png) |

## Widgets

Clock, calendar, reminders, music (now playing), batteries, weather, world clock, system monitor, disks, network, processes, notes,
timer, shortcuts (apps, folders and sites with their own icons), clipboard history, photos, currency rates, crypto prices, quote of the day, next event (from a local `.ics` file), countdown to a date and quick
controls. Cards come in S / M / L sizes, snap to a grid, can be dragged, and merge into one blob when they touch.

## Install

Download `GlassWidgets_<version>_x64-setup.exe` from the [releases page](../../releases) and run it. It installs per user (no
administrator rights) and can start Glass Widgets with Windows. The installer is not code-signed, so SmartScreen may ask you to confirm.

| Keys | Action |
| --- | --- |
| `Ctrl+Alt+G` | settings panel |
| `Ctrl+Alt+W` | edit mode: gallery to add widgets, resize, remove |
| `Ctrl+Alt+Q` | quit (if another program already owns this combination, close the process from Task Manager) |

The layer sits above the wallpaper and under your windows, like desktop widgets should. It is visible to screenshots and screen
recorders: the glass reads the wallpaper window itself (Windows Graphics Capture), never the screen, so it does not see itself.

## Tray icon and settings panel

Glass Widgets lives in the notification area (Windows 11 may keep it under the `^` chevron: drag it out to pin it). Left click opens
the settings panel; right click shows a menu: **Settings**, **Edit Widgets**, **Hide / Show Widgets**, **Layouts**, **Start with Windows**, **Quit**.

**Layouts** keeps named desks (cards and glass look): pick one to switch (the desk you leave is saved back into the layout that was active, so nothing is lost), *Save current layout as...* names the current desk, *Delete current layout* forgets the active name (the desk stays).

![Settings panel](docs/images/settings.png)

The panel has sliders that change the look of the widgets live: glass amount, blur, refraction, chromatic fringe, saturation, brightness,
frosting, highlights, glow, corner roundness and card size. It also switches dark / light / auto glass, dimming, live or plain glass,
autostart and the accent colour of card titles (blue, or taken from the wallpaper). "Reset look" brings everything back to the defaults. Drag the panel (or the gallery) by an empty spot: it remembers
where you left it. The sliders also work from the keyboard (`Up` / `Down` / `Tab` pick one, `Left` / `Right` change it by 1 %, with
`Shift` by 10 %, `Home` / `End` jump, `Backspace` resets it) and with the mouse wheel.

**Updates.** "Check for updates" asks GitHub once for the latest release and offers a **Download** button that opens the release page
in your browser. Nothing is downloaded or installed by the app. "Check for updates on start" (off by default) does the same check silently
when Glass Widgets starts and shows a small notice if a newer version exists.

## Settings file

Everything lives in `%APPDATA%\macshell\config.json` (written by the edit mode; you rarely need to touch it):

| Key | Values |
| --- | --- |
| `widgetsLayout` | list of cards: `type`, `size` (`s` `m` `l`), `col`, `row`, `cfg` (per-widget settings) |
| `widgetsGlassMode` | anything but `plain` = live wallpaper behind the glass; `plain` = no capture, cheapest |
| `widgetsAppearance` | `dark` (default), `light` or `auto` (follows the brightness under each card) |
| `widgetsDim` | `auto` (default), `never`, `always` |
| `widgetsGlass` | glass amount, `0`..`1` (default `0.5`) |
| `widgetsLook` | the other sliders of the settings panel (only the changed ones) |
| `widgetsPanels` | where the gallery / settings panel were dragged: `{"gallery":[dx,dy],"settings":[dx,dy]}` (only non-zero ones) |
| `checkUpdates` | `true` = one silent update check at start (default `false`) |
| `accentMode` | `blue` (default) or `wallpaper` (the dominant hue of the desktop wallpaper, re-read when the wallpaper changes) |
| `profiles`, `activeProfile` | saved layouts: `{ name: { items: [...], look: {...} } }` and the checked one (the Layouts submenu of the tray) |

Network is used only by the widgets that need it (weather, rates, crypto, quote, site icons of shortcut tiles) and by the update check, which runs only
when you press "Check for updates" or turn on `checkUpdates`; nothing else phones home.
Advanced knobs are environment variables, for example `MSW_CAPTURE=gdi|dxgi` (older capture paths) and `MSW_TEXT_GAMMA`.

## Build from source

Needs Rust (stable, MSVC toolchain) on Windows.

```
cargo build --release          # target\release\mswidgets.exe
pwsh tools\release.ps1 -NoPublish   # also builds the installer into dist\ (needs NSIS: makensis.exe)
```

Developer guide, widget API and the headless render/test tools are in [NATIVE.md](NATIVE.md). A widget is one file in
`src/widgets/`. `mswidgets --calib` fits the glass against reference screenshots; those are not included in the repository.

## License

MIT, see [LICENSE](LICENSE). This is an independent clean-room implementation and is not affiliated with or endorsed by Apple.
