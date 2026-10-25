# Glass Widgets

Native desktop widgets for Windows 10/11 with a Liquid-Glass look: translucent, refracting cards that float over your wallpaper
(an animated one too). No WebView, no Electron: one transparent layered window drawn with Direct2D on the GPU, a ~4 MB installer
and roughly 100 MB of RAM and under 10 % of one CPU core while the wallpaper moves.

## Widgets

Clock, calendar, reminders, music (now playing), batteries, weather, world clock, system monitor, disks, network, processes, notes,
timer, shortcuts (apps, folders and sites with their own icons), clipboard history, photos, currency rates, quote of the day and quick
controls. Cards come in S / M / L sizes, snap to a grid, can be dragged, and merge into one blob when they touch.

## Install

Download `GlassWidgets_<version>_x64-setup.exe` from the [releases page](../../releases) and run it. It installs per user (no
administrator rights) and can start Glass Widgets with Windows. The installer is not code-signed, so SmartScreen may ask you to confirm.

| Keys | Action |
| --- | --- |
| `Ctrl+Alt+W` | edit mode: gallery to add widgets, resize, remove, settings |
| `Ctrl+Alt+Q` | quit (if another program already owns this combination, close the process from Task Manager) |

The layer sits above the wallpaper and under your windows, like desktop widgets should. It is visible to screenshots and screen
recorders: the glass reads the wallpaper window itself (Windows Graphics Capture), never the screen, so it does not see itself.

## Settings

Everything lives in `%APPDATA%\macshell\config.json` (written by the edit mode; you rarely need to touch it):

| Key | Values |
| --- | --- |
| `widgetsLayout` | list of cards: `type`, `size` (`s` `m` `l`), `col`, `row`, `cfg` (per-widget settings) |
| `widgetsGlassMode` | anything but `plain` = live wallpaper behind the glass; `plain` = no capture, cheapest |
| `widgetsAppearance` | `dark` (default), `light` or `auto` (follows the brightness under each card) |
| `widgetsDim` | `auto` (default), `never`, `always` |
| `widgetsGlass` | glass amount, `0`..`1` (default `0.5`) |

Network is used only by the widgets that need it (weather, rates, quote, site icons of shortcut tiles); nothing else phones home.
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
