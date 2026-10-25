# Changelog

## [Unreleased]

### Added
- Tray icon with a menu (settings, edit widgets, hide / show, start with Windows, quit); it is added again after Explorer restarts.
- Settings panel (`Ctrl+Alt+G`, tray, or the edit-mode strip) with 11 live sliders for the look of the widgets, appearance / dimming / glass-source switches, an autostart switch and "Reset look". Slider values are stored in `widgetsLook`.

## [0.1.0] - 2026-10-25

First public release.

- 19 widgets, Liquid-Glass renderer on Direct2D, drag / edit mode / gallery, light and dark appearance, merge of touching cards.
- The glass reads the wallpaper through Windows Graphics Capture, so the widgets are visible to screenshots and recorders.
- Shortcut tiles for sites show the site's own icon.
- Per-user installer (NSIS) with an optional "start with Windows" entry.
