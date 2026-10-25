# Changelog

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
