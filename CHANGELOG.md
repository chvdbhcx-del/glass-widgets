# Changelog

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
