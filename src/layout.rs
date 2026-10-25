//! Which widgets sit where: `%APPDATA%\macshell\config.json` -> `widgetsLayout` = [{id, type, size, col, row, cfg}].
//! The Tauri app writes the same file, so every write here is a read-modify-write of ONE field (`cfg` of one item), keeps all
//! other keys exactly as parsed and goes through a temp file + rename. A file that cannot be parsed is never touched.
use crate::registry;
use crate::widget::Size;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub type_id: String,
    pub size: Size,
    pub col: u32,
    pub row: u32,
    pub cfg: Value,
}

pub fn config_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("macshell").join("config.json"))
}

fn new_id(t: &str, salt: usize) -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64 ^ d.as_secs() << 20).unwrap_or(0).wrapping_add(salt as u64 * 7919);
    let mut s = String::new();
    let mut v = n;
    for _ in 0..6 {
        s.push(char::from_digit((v % 36) as u32, 36).unwrap_or('x'));
        v /= 36;
    }
    format!("{t}-{s}")
}

/// Layout from config.json (unknown types skipped; a size the widget does not support falls back to its default); the default
/// layout when `use_config` is false, the file is missing / unreadable or the array is empty. Never writes anything.
pub fn load(use_config: bool) -> Vec<Item> {
    load_path(if use_config { config_path() } else { None }.as_deref())
}

pub fn load_path(path: Option<&std::path::Path>) -> Vec<Item> {
    let read = || -> Option<Vec<Item>> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path?).ok()?).ok()?;
        let items = items_from_value(&v);
        (!items.is_empty()).then_some(items)
    };
    read().unwrap_or_else(default_layout)
}

/// The cards of a parsed config.json (`widgetsLayout`; unknown types skipped, an unsupported size falls back to the default). May be empty.
pub fn items_from_value(v: &Value) -> Vec<Item> {
    let reg = registry::global();
    let Some(arr) = v.get("widgetsLayout").and_then(|a| a.as_array()) else { return Vec::new() };
    arr.iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let t = it.get("type")?.as_str()?;
            let def = reg.get(t)?;
            let size = it.get("size").and_then(|s| s.as_str()).and_then(Size::parse).filter(|s| def.sizes.contains(s)).unwrap_or(def.default_size);
            let id = it.get("id").and_then(|s| s.as_str()).map(str::to_string).unwrap_or_else(|| new_id(t, i));
            let cfg = it.get("cfg").filter(|c| c.is_object()).cloned().unwrap_or_else(|| json!({}));
            Some(Item { id, type_id: t.to_string(), size, col: it.get("col")?.as_u64()? as u32, row: it.get("row")?.as_u64()? as u32, cfg })
        })
        .collect()
}

/// Tidy default: clock S + calendar S in row 0, then up to two more finished (non-stub) widgets as default-size cards below.
pub fn default_layout() -> Vec<Item> {
    let reg = registry::global();
    let mut order: Vec<&str> = vec!["clock", "calendar"];
    order.extend(reg.all().iter().map(|d| d.type_id).filter(|t| !matches!(*t, "clock" | "calendar") && !reg.is_stub(t)).take(2));
    let mut used: HashSet<(u32, u32)> = HashSet::new();
    let mut out = Vec::new();
    for (i, t) in order.iter().enumerate() {
        let Some(def) = reg.get(t) else { continue };
        let (cw, ch) = def.default_size.cells();
        'find: for row in 0..16 {
            for col in 0..=(2 - cw) {
                let free = (0..cw).all(|dx| (0..ch).all(|dy| !used.contains(&(col + dx, row + dy))));
                if free {
                    for dx in 0..cw {
                        for dy in 0..ch {
                            used.insert((col + dx, row + dy));
                        }
                    }
                    out.push(Item { id: new_id(t, i), type_id: t.to_string(), size: def.default_size, col, row, cfg: json!({}) });
                    break 'find;
                }
            }
        }
    }
    out
}

/// Split on ';' outside of {...}.
fn split_top(spec: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0i32, 0usize);
    for (i, ch) in spec.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            ';' if depth == 0 => {
                out.push(&spec[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&spec[start..]);
    out
}

/// "clock:s@0,0;calendar:m@1,0;weather@0,1;notes:m@0,2{\"text\":\"hi\"}" for `--cards`: size optional, optional JSON `cfg` after the position.
pub fn parse_cards(spec: &str) -> Result<Vec<Item>, String> {
    let reg = registry::global();
    let mut out = Vec::new();
    for (i, part) in split_top(spec).into_iter().map(str::trim).filter(|p| !p.is_empty()).enumerate() {
        let (part, cfg) = match part.split_once('{') {
            Some((a, b)) => (a.trim(), serde_json::from_str::<Value>(&format!("{{{b}")).map_err(|e| format!("'{part}': bad cfg JSON ({e})"))?),
            None => (part, json!({})),
        };
        let (head, pos) = part.split_once('@').ok_or_else(|| format!("'{part}': expected type[:size]@col,row"))?;
        let (t, sz) = head.split_once(':').map_or((head, None), |(a, b)| (a, Some(b)));
        let def = reg.get(t.trim()).ok_or_else(|| format!("unknown widget '{t}' (known: {})", reg.all().iter().map(|d| d.type_id).collect::<Vec<_>>().join(", ")))?;
        let size = match sz {
            Some(s) => Size::parse(s.trim()).filter(|s| def.sizes.contains(s)).ok_or_else(|| format!("'{t}' does not support size '{s}'"))?,
            None => def.default_size,
        };
        let (c, r) = pos.split_once(',').ok_or_else(|| format!("'{part}': position must be col,row"))?;
        let (col, row) = (c.trim().parse().map_err(|_| format!("bad col '{c}'"))?, r.trim().parse().map_err(|_| format!("bad row '{r}'"))?);
        out.push(Item { id: format!("{}-{i}", def.type_id), type_id: def.type_id.to_string(), size, col, row, cfg });
    }
    if out.is_empty() {
        return Err("--cards: nothing to draw".into());
    }
    Ok(out)
}

fn item_json(it: &Item) -> Value {
    json!({ "id": it.id, "type": it.type_id, "size": it.size.key(), "col": it.col, "row": it.row, "cfg": it.cfg })
}

/// Persist the `cfg` of card `id` (current value taken from `items`). Never clobbers: unparsable file -> nothing written;
/// layout array exists but no longer has this id (the Tauri app rearranged it) -> nothing written; array missing or empty ->
/// the whole current layout is written once.
pub fn persist_cfg(items: &[Item], id: &str) {
    let Some(p) = config_path() else { return };
    let mut root: Value = match std::fs::read_to_string(&p) {
        Ok(t) => match serde_json::from_str(&t) {
            Ok(v) => v,
            Err(e) => return eprintln!("mswidgets: config.json is not valid JSON ({e}); settings not saved"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return eprintln!("mswidgets: config.json unreadable ({e}); settings not saved"),
    };
    let Some(me) = items.iter().find(|i| i.id == id) else { return };
    let obj = match root.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let arr = obj.get("widgetsLayout").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if arr.is_empty() {
        obj.insert("widgetsLayout".into(), Value::Array(items.iter().map(item_json).collect()));
    } else {
        let mut arr = arr;
        match arr.iter_mut().find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id)) {
            Some(e) => e["cfg"] = me.cfg.clone(),
            None => return,
        }
        obj.insert("widgetsLayout".into(), Value::Array(arr));
    }
    let Ok(text) = serde_json::to_string_pretty(&root) else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("mswidgets: could not write config.json");
    }
}

/// UI language from config.json (`lang`: "ru" | "en"), default ru.
pub fn lang() -> crate::widget::Lang {
    let l = (|| -> Option<String> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(config_path()?).ok()?).ok()?;
        v.get("lang")?.as_str().map(str::to_string)
    })();
    if l.as_deref().is_some_and(|s| s.starts_with("en")) {
        crate::widget::Lang::En
    } else {
        crate::widget::Lang::Ru
    }
}

// ---------------------------------------------------------------- host: settings, layout write-back

/// `widgetsDim`: widgets turn monochrome while an app window is in front (auto) / always / never.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dim {
    Auto,
    Always,
    Never,
}

impl Dim {
    /// The `widgetsDim` value.
    pub fn key(self) -> &'static str {
        match self {
            Dim::Auto => "auto",
            Dim::Always => "always",
            Dim::Never => "never",
        }
    }
}

/// The `widgetsAppearance` value.
pub fn appearance_key(p: crate::motion::AppearancePref) -> &'static str {
    match p {
        crate::motion::AppearancePref::Auto => "auto",
        crate::motion::AppearancePref::Dark => "dark",
        crate::motion::AppearancePref::Light => "light",
    }
}

/// `accentMode`: where the accent colour of card titles and links comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccentMode {
    /// the fixed blue (default)
    Blue,
    /// the dominant hue of the desktop wallpaper (`accent.rs`)
    Wallpaper,
}
impl AccentMode {
    /// The `accentMode` value.
    pub fn key(self) -> &'static str {
        match self {
            AccentMode::Blue => "blue",
            AccentMode::Wallpaper => "wallpaper",
        }
    }
}

/// The host settings of config.json: `widgetsGlass` 0..1, `widgetsDim`, `widgetsGlassMode` ("plain" = no desktop capture, anything else = live glass),
/// `widgetsAppearance` ("auto" | "dark" | "light"), `checkUpdates` (bool), `accentMode` ("blue" | "wallpaper").
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Settings {
    pub glass: f32,
    pub dim: Dim,
    pub live: bool,
    /// `widgetsAppearance`: auto (from the backdrop) / dark / light glass
    pub appearance: crate::motion::AppearancePref,
    /// `checkUpdates`: one silent update check at start (off unless the user turned it on)
    pub check_updates: bool,
    pub accent: AccentMode,
}
impl Default for Settings {
    fn default() -> Self {
        Settings { glass: 0.5, dim: Dim::Auto, live: true, appearance: crate::motion::AppearancePref::Dark, check_updates: false, accent: AccentMode::Blue }
    }
}
impl Settings {
    pub fn from_value(v: &Value) -> Settings {
        let d = Settings::default();
        Settings {
            glass: v.get("widgetsGlass").and_then(|g| g.as_f64()).map_or(d.glass, |g| g.clamp(0.0, 1.0) as f32),
            dim: match v.get("widgetsDim").and_then(|g| g.as_str()) {
                Some("always") => Dim::Always,
                Some("never") => Dim::Never,
                _ => Dim::Auto,
            },
            live: v.get("widgetsGlassMode").and_then(|g| g.as_str()) != Some("plain"),
            appearance: v.get("widgetsAppearance").and_then(|g| g.as_str()).and_then(crate::motion::AppearancePref::parse).unwrap_or_default(),
            check_updates: v.get("checkUpdates").and_then(|g| g.as_bool()).unwrap_or(d.check_updates),
            accent: if v.get("accentMode").and_then(|g| g.as_str()) == Some("wallpaper") { AccentMode::Wallpaper } else { AccentMode::Blue },
        }
    }
}

/// Settings from the user's config.json (defaults for a missing / unreadable file).
pub fn read_settings() -> Settings {
    config_path().and_then(|p| read_root(&p).ok()).map(|v| Settings::from_value(&v)).unwrap_or_default()
}

pub fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(config_path()?).and_then(|m| m.modified()).ok()
}

/// config.json as JSON; a missing file is an empty object, anything else that cannot be parsed is an error (and is never overwritten).
fn read_root(p: &std::path::Path) -> Result<Value, String> {
    match std::fs::read_to_string(p) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{} is not valid JSON ({e})", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("{} unreadable ({e})", p.display())),
    }
}

/// Temp file + rename: the file is either the old or the new content, never half of it.
fn write_root(p: &std::path::Path, root: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not write {}: {e}", p.display())
    })
}

/// Write one top-level key (`widgetsGlass`, `widgetsGlassMode`, ...) of config.json, everything else untouched.
pub fn write_setting(key: &str, val: Value) -> Result<(), String> {
    let p = config_path().ok_or("no APPDATA")?;
    write_setting_to(&p, key, val)
}
pub fn write_setting_to(p: &std::path::Path, key: &str, val: Value) -> Result<(), String> {
    let mut root = read_root(p)?;
    root.as_object_mut().ok_or("config root is not an object")?.insert(key.to_string(), val);
    write_root(p, &root)
}

/// `widgetsLook` of a parsed config.json: the object, or `{}`.
pub fn look_from_value(v: &Value) -> Value {
    v.get("widgetsLook").filter(|l| l.is_object()).cloned().unwrap_or_else(|| json!({}))
}

/// `widgetsLook` from the user's config.json.
pub fn read_look() -> Value {
    config_path().and_then(|p| read_root(&p).ok()).map(|v| look_from_value(&v)).unwrap_or_else(|| json!({}))
}

/// The new `widgetsLook`: the entries of `old` the sliders do not own (hand-written keys survive) plus `diff`.
pub fn merge_look(old: &Value, diff: &Value) -> Value {
    let owned: Vec<&str> = crate::glass::LOOK.iter().flat_map(|d| d.names()).collect();
    let mut out = serde_json::Map::new();
    for (k, v) in old.as_object().into_iter().flatten().filter(|(k, _)| !owned.contains(&k.as_str())) {
        out.insert(k.clone(), v.clone());
    }
    for (k, v) in diff.as_object().into_iter().flatten() {
        out.insert(k.clone(), v.clone());
    }
    Value::Object(out)
}

/// Write `widgetsLook` (merged with what the file has; an empty result removes the key), everything else untouched.
pub fn write_look(diff: &Value) -> Result<(), String> {
    write_look_to(&config_path().ok_or("no APPDATA")?, diff)
}
pub fn write_look_to(p: &std::path::Path, diff: &Value) -> Result<(), String> {
    let mut root = read_root(p)?;
    let obj = root.as_object_mut().ok_or("config root is not an object")?;
    let merged = merge_look(&obj.get("widgetsLook").cloned().unwrap_or_else(|| json!({})), diff);
    if merged.as_object().is_some_and(|m| m.is_empty()) {
        if obj.remove("widgetsLook").is_none() {
            return Ok(()); // nothing there, nothing to write
        }
    } else {
        obj.insert("widgetsLook".into(), merged);
    }
    write_root(p, &root)
}

/// `widgetsPanels` of a parsed config.json: how far the gallery and the settings panel were dragged from where they open
/// (`{"gallery":[dx,dy],"settings":[dx,dy]}`, logical px; a missing or malformed entry is (0, 0)).
pub fn panels_from_value(v: &Value) -> ((f32, f32), (f32, f32)) {
    let get = |k: &str| {
        let a = v.get("widgetsPanels").and_then(|p| p.get(k)).and_then(|a| a.as_array());
        match a.map(|a| (a.first().and_then(|x| x.as_f64()), a.get(1).and_then(|x| x.as_f64()))) {
            Some((Some(x), Some(y))) if x.is_finite() && y.is_finite() => (x as f32, y as f32),
            _ => (0.0, 0.0),
        }
    };
    (get("gallery"), get("settings"))
}

/// `widgetsPanels` from the user's config.json.
pub fn read_panels() -> ((f32, f32), (f32, f32)) {
    config_path().and_then(|p| read_root(&p).ok()).map(|v| panels_from_value(&v)).unwrap_or_default()
}

/// Write `widgetsPanels` (whole pixels; only the panels that are off their home position, none = the key is removed), everything else untouched.
pub fn write_panels(gallery: (f32, f32), settings: (f32, f32)) -> Result<(), String> {
    write_panels_to(&config_path().ok_or("no APPDATA")?, gallery, settings)
}
pub fn write_panels_to(p: &std::path::Path, gallery: (f32, f32), settings: (f32, f32)) -> Result<(), String> {
    let mut root = read_root(p)?;
    let obj = root.as_object_mut().ok_or("config root is not an object")?;
    let mut m = serde_json::Map::new();
    for (k, (x, y)) in [("gallery", gallery), ("settings", settings)] {
        let (x, y) = (x.round() as i64, y.round() as i64);
        if (x, y) != (0, 0) {
            m.insert(k.into(), json!([x, y]));
        }
    }
    if m.is_empty() {
        if obj.remove("widgetsPanels").is_none() {
            return Ok(()); // nothing there, nothing to write
        }
    } else {
        obj.insert("widgetsPanels".into(), Value::Object(m));
    }
    write_root(p, &root)
}

// ---------------------------------------------------------------- layout profiles
//
// config.json: `profiles` = { name: { "items": [<widgetsLayout entries>], "look": {<widgetsLook>} } } and `activeProfile` = name. The live layout stays in
// `widgetsLayout` / `widgetsLook` as always (the host and the Tauri app read those); a profile is a named snapshot of both. Everything below works on the
// parsed JSON only (unit-tested without a file); `profile_op` is the one place that touches config.json.

/// The saved profile names (sorted: the JSON object is a sorted map).
pub fn profile_names(root: &Value) -> Vec<String> {
    root.get("profiles").and_then(|p| p.as_object()).map(|p| p.keys().cloned().collect()).unwrap_or_default()
}

/// `activeProfile` when a profile of that name still exists.
pub fn active_profile(root: &Value) -> Option<String> {
    let a = root.get("activeProfile")?.as_str()?;
    root.get("profiles")?.get(a).is_some().then(|| a.to_string())
}

/// The current layout + look as one profile entry.
fn snapshot(root: &Value) -> Value {
    json!({ "items": root.get("widgetsLayout").filter(|v| v.is_array()).cloned().unwrap_or_else(|| json!([])), "look": look_from_value(root) })
}

fn put_profile(root: &mut Value, name: &str, entry: Value) -> Result<(), String> {
    let obj = root.as_object_mut().ok_or("config root is not an object")?;
    match obj.entry("profiles").or_insert_with(|| json!({})) {
        Value::Object(m) => m.insert(name.to_string(), entry),
        _ => return Err("`profiles` in config.json is not an object".into()),
    };
    Ok(())
}

/// "Save current layout as...": the desk (`widgetsLayout` + `widgetsLook`) becomes profile `name` (replacing a profile of that name) and the active one.
pub fn profile_save(root: &mut Value, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty profile name".into());
    }
    let snap = snapshot(root);
    put_profile(root, name, snap)?;
    root["activeProfile"] = json!(name);
    Ok(())
}

/// Switch to profile `name`: the current desk is first written back into the profile that was active (nothing is lost; with no active profile it is
/// kept as a new profile called `stash_name`), then the profile's items and look become `widgetsLayout` / `widgetsLook`. Other keys are untouched.
pub fn profile_switch(root: &mut Value, name: &str, stash_name: &str) -> Result<(), String> {
    let entry = root.get("profiles").and_then(|p| p.get(name)).cloned().ok_or_else(|| format!("no profile '{name}'"))?;
    let active = active_profile(root);
    if active.as_deref() == Some(name) {
        return Ok(());
    }
    let snap = snapshot(root);
    match active {
        Some(a) => put_profile(root, &a, snap)?,
        None => {
            // an unnamed desk with something on it is kept, not overwritten
            if snap["items"].as_array().is_some_and(|a| !a.is_empty()) {
                let mut n = stash_name.to_string();
                let mut i = 1;
                while root["profiles"].get(&n).is_some() {
                    i += 1;
                    n = format!("{stash_name} {i}");
                }
                put_profile(root, &n, snap)?;
            }
        }
    }
    let obj = root.as_object_mut().ok_or("config root is not an object")?;
    obj.insert("widgetsLayout".into(), entry.get("items").filter(|v| v.is_array()).cloned().unwrap_or_else(|| json!([])));
    match entry.get("look").filter(|l| l.as_object().is_some_and(|m| !m.is_empty())) {
        Some(l) => obj.insert("widgetsLook".into(), l.clone()),
        None => obj.remove("widgetsLook"),
    };
    obj.insert("activeProfile".into(), json!(name));
    Ok(())
}

/// "Delete current layout": the active profile is forgotten; the desk keeps what it shows. Returns the deleted name.
pub fn profile_delete_active(root: &mut Value) -> Result<String, String> {
    let a = active_profile(root).ok_or("no active profile")?;
    let obj = root.as_object_mut().ok_or("config root is not an object")?;
    if let Some(Value::Object(m)) = obj.get_mut("profiles") {
        m.remove(&a);
        if m.is_empty() {
            obj.remove("profiles");
        }
    }
    obj.remove("activeProfile");
    Ok(a)
}

/// Read config.json, apply `f`, write it back (temp file + rename). An unparsable file is refused and never touched; nothing is written when `f` fails.
pub fn profile_op(f: impl FnOnce(&mut Value) -> Result<(), String>) -> Result<(), String> {
    profile_op_at(&config_path().ok_or("no APPDATA")?, f)
}
pub fn profile_op_at(p: &std::path::Path, f: impl FnOnce(&mut Value) -> Result<(), String>) -> Result<(), String> {
    let mut root = read_root(p)?;
    f(&mut root)?;
    write_root(p, &root)
}

/// config.json as parsed JSON for read-only use (`{}` when missing / unparsable).
pub fn read_config() -> Value {
    config_path().and_then(|p| read_root(&p).ok()).unwrap_or_else(|| json!({}))
}

/// Persist the whole layout after the user moved / resized / added / removed a card. Per-item keys the host does not know are kept,
/// entries of widget types this renderer does not know are kept untouched, an unparsable file is never overwritten.
pub fn save_layout(items: &[Item]) -> Result<(), String> {
    let p = config_path().ok_or("no APPDATA")?;
    save_layout_to(&p, items)
}
pub fn save_layout_to(p: &std::path::Path, items: &[Item]) -> Result<(), String> {
    let mut root = read_root(p)?;
    let obj = root.as_object_mut().ok_or("config root is not an object")?;
    let old: Vec<Value> = obj.get("widgetsLayout").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let reg = registry::global();
    let id_of = |v: &Value| v.get("id").and_then(|s| s.as_str()).map(str::to_string);
    let mut out: Vec<Value> = items
        .iter()
        .map(|it| {
            let mut e = old.iter().find(|o| id_of(o).as_deref() == Some(it.id.as_str())).filter(|o| o.is_object()).cloned().unwrap_or_else(|| json!({}));
            e["id"] = json!(it.id);
            e["type"] = json!(it.type_id);
            e["size"] = json!(it.size.key());
            e["col"] = json!(it.col);
            e["row"] = json!(it.row);
            e["cfg"] = it.cfg.clone();
            e
        })
        .collect();
    for o in &old {
        let unknown = o.get("type").and_then(|t| t.as_str()).is_none_or(|t| reg.get(t).is_none());
        let have = id_of(o).is_some_and(|id| items.iter().any(|it| it.id == id));
        if unknown && !have {
            out.push(o.clone());
        }
    }
    obj.insert("widgetsLayout".into(), Value::Array(out));
    write_root(p, &root)
}

fn base36(mut v: u128) -> String {
    let mut s = Vec::new();
    loop {
        s.push(char::from_digit((v % 36) as u32, 36).unwrap_or('0'));
        v /= 36;
        if v == 0 {
            break;
        }
    }
    s.iter().rev().collect()
}

/// `<type>-<base36 millis since epoch>`, made unique among `items`.
pub fn new_id_time(t: &str, items: &[Item]) -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let mut id = format!("{t}-{}", base36(ms));
    let mut n = 0u32;
    while items.iter().any(|i| i.id == id) {
        n += 1;
        id = format!("{t}-{}{}", base36(ms), base36(n as u128));
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    /// persist_cfg keeps every unknown key / other item, replaces only the cfg of one id, and leaves no temp file.
    #[test]
    fn persist_cfg_is_surgical() {
        let dir = std::env::temp_dir().join(format!("mswidgets-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("macshell")).unwrap();
        std::env::set_var("APPDATA", &dir);
        let cfg = dir.join("macshell").join("config.json");
        std::fs::write(&cfg, r#"{"mode":"widgets","extra":{"a":[1,2,3]},"widgetsLayout":[{"cfg":{},"col":0,"id":"clock-1","row":0,"size":"s","type":"clock","unknown":7},{"cfg":{"k":1},"col":1,"id":"calendar-2","row":0,"size":"s","type":"calendar"}]}"#).unwrap();
        let mut items = vec![
            Item { id: "clock-1".into(), type_id: "clock".into(), size: Size::S, col: 0, row: 0, cfg: json!({"zones": []}) },
            Item { id: "calendar-2".into(), type_id: "calendar".into(), size: Size::S, col: 1, row: 0, cfg: json!({"k": 1}) },
        ];
        persist_cfg(&items, "clock-1");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["mode"], "widgets");
        assert_eq!(v["extra"]["a"][2], 3);
        assert_eq!(v["widgetsLayout"][0]["cfg"]["zones"], json!([]));
        assert_eq!(v["widgetsLayout"][0]["unknown"], 7);
        assert_eq!(v["widgetsLayout"][1]["cfg"]["k"], 1);
        assert!(!dir.join("macshell").join("config.json.tmp").exists());
        // an id the file no longer has: nothing is written
        items[0].id = "gone".into();
        let before = std::fs::read_to_string(&cfg).unwrap();
        persist_cfg(&items, "gone");
        assert_eq!(before, std::fs::read_to_string(&cfg).unwrap());
        // unparsable file: untouched
        std::fs::write(&cfg, "{ not json").unwrap();
        persist_cfg(&items, "gone");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "{ not json");
        // empty layout: the whole current layout is written once
        std::fs::write(&cfg, r#"{"mode":"widgets","widgetsLayout":[]}"#).unwrap();
        items[0].id = "clock-1".into();
        persist_cfg(&items, "clock-1");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["widgetsLayout"].as_array().unwrap().len(), 2);
        assert_eq!(v["mode"], "widgets");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn item(id: &str, t: &str, size: Size, col: u32, row: u32) -> Item {
        Item { id: id.into(), type_id: t.into(), size, col, row, cfg: json!({}) }
    }

    /// The layout round trip: moved / added / removed items are written, foreign keys and foreign widget types survive,
    /// an unparsable file is refused, a missing file is created. Uses explicit paths (no APPDATA, no races with the other test).
    #[test]
    fn layout_round_trip_keeps_foreign_data() {
        let dir = std::env::temp_dir().join(format!("mswidgets-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config.json");
        std::fs::write(
            &f,
            r#"{"mode":"widgets","widgetsGlass":0.7,"widgetsLayout":[{"id":"clock-1","type":"clock","size":"s","col":0,"row":0,"cfg":{},"unknown":7},{"id":"zzz-9","type":"no_such_widget","size":"m","col":3,"row":1,"cfg":{"a":1}},{"id":"gone-1","type":"calendar","size":"s","col":4,"row":0,"cfg":{}}]}"#,
        )
        .unwrap();
        let items = vec![item("clock-1", "clock", Size::M, 2, 1), item("weather-abc", "weather", Size::S, 0, 0)];
        save_layout_to(&f, &items).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let arr = v["widgetsLayout"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!((arr[0]["id"].as_str(), arr[0]["size"].as_str(), arr[0]["col"].as_u64(), arr[0]["row"].as_u64()), (Some("clock-1"), Some("m"), Some(2), Some(1)));
        assert_eq!(arr[0]["unknown"], 7);
        assert_eq!(arr[1]["id"], "weather-abc");
        assert_eq!(arr[2]["id"], "zzz-9"); // foreign widget type kept
        assert!(arr.iter().all(|e| e["id"] != "gone-1")); // a known type that the user removed is gone
        assert_eq!(v["mode"], "widgets");
        assert_eq!(v["widgetsGlass"], 0.7);
        assert!(!f.with_extension("json.tmp").exists());
        let back = load_path(Some(&f));
        assert_eq!(back.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["clock-1", "weather-abc"]);
        assert_eq!((back[0].col, back[0].row), (2, 1));
        assert_eq!(Settings::from_value(&v).glass, 0.7);
        // single setting write keeps the layout
        write_setting_to(&f, "widgetsGlassMode", json!("plain")).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert!(!Settings::from_value(&v).live);
        assert_eq!(v["widgetsLayout"].as_array().unwrap().len(), 3);
        // unparsable: refused, untouched
        std::fs::write(&f, "{ nope").unwrap();
        assert!(save_layout_to(&f, &items).is_err());
        assert!(write_setting_to(&f, "widgetsGlass", json!(0.1)).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{ nope");
        assert_eq!(load_path(Some(&f)).len(), default_layout().len()); // falls back to the default layout
        // missing: created
        let g = dir.join("sub").join("config.json");
        save_layout_to(&g, &items).unwrap();
        assert_eq!(load_path(Some(&g)).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_defaults_and_ids() {
        assert_eq!(Settings::from_value(&json!({})), Settings::default());
        let s = Settings::from_value(&json!({"widgetsGlass": 7, "widgetsDim": "always", "widgetsGlassMode": "plain"}));
        assert_eq!((s.glass, s.dim, s.live), (1.0, Dim::Always, false));
        let items = vec![item("clock-1", "clock", Size::S, 0, 0)];
        let a = new_id_time("clock", &items);
        assert!(a.starts_with("clock-") && a.len() > 8);
        let taken = vec![item(&a, "clock", Size::S, 0, 0)];
        assert_ne!(new_id_time("clock", &taken), a);
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }

    /// `widgetsLook` write-back: only the slider-owned keys change, hand-written keys, other settings and the layout survive;
    /// an empty diff removes the object; an unparsable file is refused.
    #[test]
    fn look_write_back_merges() {
        let dir = std::env::temp_dir().join(format!("mswidgets-look-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config.json");
        std::fs::write(&f, r#"{"mode":"widgets","widgetsGlass":0.7,"widgetsLayout":[{"id":"clock-1","type":"clock","size":"s","col":0,"row":0,"cfg":{}}],"widgetsLook":{"spec_len":0.3,"blur":9.0}}"#).unwrap();
        write_look_to(&f, &json!({"blur": 6.5, "chroma_r": 0.08, "chroma_b": 0.12})).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["widgetsLook"], json!({"spec_len": 0.3, "blur": 6.5, "chroma_r": 0.08, "chroma_b": 0.12}));
        assert_eq!((v["mode"].as_str(), v["widgetsGlass"].as_f64(), v["widgetsLayout"].as_array().map(|a| a.len())), (Some("widgets"), Some(0.7), Some(1)));
        assert_eq!(look_from_value(&v)["blur"], 6.5);
        // sliders back at the base: the owned keys go, the hand-written one stays
        write_look_to(&f, &json!({})).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["widgetsLook"], json!({"spec_len": 0.3}));
        // nothing left: the key is removed
        std::fs::write(&f, r#"{"widgetsLook":{"blur":9}}"#).unwrap();
        write_look_to(&f, &json!({})).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert!(v.get("widgetsLook").is_none());
        // a file without the key and an empty diff is not rewritten
        std::fs::write(&f, "{\"a\":  1}").unwrap();
        write_look_to(&f, &json!({})).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{\"a\":  1}");
        // a missing file is created, an unparsable one is refused
        let g = dir.join("new").join("config.json");
        write_look_to(&g, &json!({"sat": 1.2})).unwrap();
        assert_eq!(look_from_value(&serde_json::from_str(&std::fs::read_to_string(&g).unwrap()).unwrap())["sat"], 1.2);
        std::fs::write(&f, "{ nope").unwrap();
        assert!(write_look_to(&f, &json!({"sat": 1.2})).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{ nope");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `widgetsPanels`: only non-zero offsets are stored, an all-zero state removes the key, everything else in the file survives.
    #[test]
    fn panel_offsets_round_trip() {
        let dir = std::env::temp_dir().join(format!("mswidgets-panels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config.json");
        std::fs::write(&f, r#"{"widgetsGlass":0.7,"widgetsLook":{"blur":9.0}}"#).unwrap();
        write_panels_to(&f, (12.4, -30.6), (0.0, 0.0)).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["widgetsPanels"], json!({"gallery": [12, -31]}));
        assert_eq!((v["widgetsGlass"].as_f64(), v["widgetsLook"]["blur"].as_f64()), (Some(0.7), Some(9.0)));
        assert_eq!(panels_from_value(&v), ((12.0, -31.0), (0.0, 0.0)));
        write_panels_to(&f, (12.0, -31.0), (-5.0, 0.0)).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(panels_from_value(&v), ((12.0, -31.0), (-5.0, 0.0)));
        write_panels_to(&f, (0.0, 0.0), (0.2, -0.3)).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert!(v.get("widgetsPanels").is_none() && v["widgetsGlass"] == 0.7);
        // no key and nothing to store: the file is not rewritten; garbage entries read as zero; an unparsable file is refused
        std::fs::write(&f, "{\"a\":  1}").unwrap();
        write_panels_to(&f, (0.0, 0.0), (0.0, 0.0)).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{\"a\":  1}");
        assert_eq!(panels_from_value(&json!({"widgetsPanels": {"gallery": [1], "settings": "x"}})), ((0.0, 0.0), (0.0, 0.0)));
        std::fs::write(&f, "{ nope").unwrap();
        assert!(write_panels_to(&f, (1.0, 1.0), (0.0, 0.0)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn two_profile_config() -> Value {
        json!({
            "mode": "widgets",
            "widgetsLayout": [{"id": "clock-1", "type": "clock", "size": "s", "col": 0, "row": 0, "cfg": {}}],
            "widgetsLook": {"blur": 9.0},
            "other": {"keep": [1, 2]}
        })
    }

    /// save -> the first profile is created from the desk and becomes active; the live keys are untouched.
    #[test]
    fn profile_save_creates_and_activates() {
        let mut v = two_profile_config();
        assert_eq!((profile_names(&v), active_profile(&v)), (vec![], None));
        profile_save(&mut v, "  Work ").unwrap();
        assert_eq!(profile_names(&v), ["Work"]);
        assert_eq!(active_profile(&v).as_deref(), Some("Work"));
        assert_eq!(v["profiles"]["Work"]["items"][0]["id"], "clock-1");
        assert_eq!(v["profiles"]["Work"]["look"], json!({"blur": 9.0}));
        assert_eq!(v["widgetsLayout"].as_array().unwrap().len(), 1);
        assert_eq!(v["widgetsLook"], json!({"blur": 9.0}));
        assert_eq!(v["other"], json!({"keep": [1, 2]}));
        // an empty name is refused and changes nothing
        let before = v.clone();
        assert!(profile_save(&mut v, "   ").is_err());
        assert_eq!(v, before);
        // a desk without any layout / look saves empty containers
        let mut e = json!({});
        profile_save(&mut e, "Empty").unwrap();
        assert_eq!(e["profiles"]["Empty"], json!({"items": [], "look": {}}));
        // a broken `profiles` is not overwritten
        let mut bad = json!({"profiles": 5});
        assert!(profile_save(&mut bad, "x").is_err());
    }

    /// switch: the desk is written back into the active profile first, then the chosen one is loaded (look removed when empty).
    #[test]
    fn profile_switch_saves_back_then_loads() {
        let mut v = two_profile_config();
        profile_save(&mut v, "Work").unwrap();
        // a second profile: one card more, no look; then back to the desk of "Work" with an edit
        v["widgetsLayout"].as_array_mut().unwrap().push(json!({"id": "notes-2", "type": "notes", "size": "m", "col": 1, "row": 0, "cfg": {}}));
        v.as_object_mut().unwrap().remove("widgetsLook");
        profile_save(&mut v, "Home").unwrap();
        assert_eq!((active_profile(&v).as_deref(), profile_names(&v)), (Some("Home"), vec!["Home".to_string(), "Work".to_string()]));
        // edit the desk while "Home" is active, then switch: the edit lands in "Home", "Work" is loaded
        v["widgetsLayout"][0]["col"] = json!(1);
        profile_switch(&mut v, "Work", "Unsaved").unwrap();
        assert_eq!(v["profiles"]["Home"]["items"][0]["col"], 1);
        assert_eq!(v["profiles"]["Home"]["items"].as_array().unwrap().len(), 2);
        assert!(v["profiles"]["Home"]["look"].as_object().unwrap().is_empty());
        assert_eq!(v["widgetsLayout"].as_array().unwrap().len(), 1);
        assert_eq!(v["widgetsLook"], json!({"blur": 9.0}));
        assert_eq!(active_profile(&v).as_deref(), Some("Work"));
        // and back: "Home" has no look, so the key goes away
        profile_switch(&mut v, "Home", "Unsaved").unwrap();
        assert!(v.get("widgetsLook").is_none());
        assert_eq!(v["widgetsLayout"].as_array().unwrap().len(), 2);
        assert_eq!(v["profiles"]["Work"]["look"], json!({"blur": 9.0}));
        // foreign keys survive all of it; switching to the active or to an unknown profile changes nothing
        assert_eq!(v["other"], json!({"keep": [1, 2]}));
        let before = v.clone();
        profile_switch(&mut v, "Home", "Unsaved").unwrap();
        assert!(profile_switch(&mut v, "Nope", "Unsaved").is_err());
        assert_eq!(v, before);
    }

    /// With no active profile (just deleted) a switch keeps the desk as a stash profile instead of overwriting it; an empty desk is not stashed.
    #[test]
    fn profile_switch_without_active_keeps_the_desk() {
        let mut v = two_profile_config();
        profile_save(&mut v, "Work").unwrap();
        v["widgetsLayout"][0]["row"] = json!(3);
        profile_save(&mut v, "Home").unwrap();
        assert_eq!(profile_delete_active(&mut v).unwrap(), "Home");
        assert_eq!(profile_names(&v), ["Work"]);
        assert!(v.get("activeProfile").is_none());
        assert_eq!(v["widgetsLayout"][0]["row"], 3); // the desk keeps what it showed
        profile_switch(&mut v, "Work", "Unsaved").unwrap();
        assert_eq!(v["profiles"]["Unsaved"]["items"][0]["row"], 3);
        assert_eq!(v["widgetsLayout"][0]["row"], 0);
        // again with an existing stash name: numbered
        profile_delete_active(&mut v).unwrap();
        v["widgetsLayout"][0]["row"] = json!(5);
        profile_save(&mut v, "Zed").unwrap();
        profile_delete_active(&mut v).unwrap();
        profile_switch(&mut v, "Unsaved", "Unsaved").unwrap();
        assert_eq!(v["profiles"]["Unsaved 2"]["items"][0]["row"], 5);
        // an empty desk is not stashed
        let mut e = json!({"profiles": {"A": {"items": [], "look": {}}}, "widgetsLayout": []});
        profile_switch(&mut e, "A", "Unsaved").unwrap();
        assert_eq!(profile_names(&e), ["A"]);
    }

    /// delete: only the active profile goes, the desk and the other profiles stay; the last one takes `profiles` with it.
    #[test]
    fn profile_delete_active_only() {
        let mut v = two_profile_config();
        assert!(profile_delete_active(&mut v).is_err());
        profile_save(&mut v, "A").unwrap();
        profile_save(&mut v, "B").unwrap();
        assert_eq!(profile_delete_active(&mut v).unwrap(), "B");
        assert_eq!(profile_names(&v), ["A"]);
        assert!(active_profile(&v).is_none());
        assert_eq!(v["widgetsLayout"].as_array().unwrap().len(), 1);
        // a dangling activeProfile (the profile was removed by hand) counts as none
        v["activeProfile"] = json!("A");
        assert_eq!(profile_delete_active(&mut v).unwrap(), "A");
        assert!(v.get("profiles").is_none() && v.get("activeProfile").is_none());
        assert_eq!(v["widgetsLook"], json!({"blur": 9.0}));
        v["activeProfile"] = json!("ghost");
        assert!(profile_delete_active(&mut v).is_err());
    }

    /// The file round trip: a failed transformation writes nothing, an unparsable file is refused, a profile switch yields a layout `items_from_value` reads.
    #[test]
    fn profile_op_on_a_file() {
        let dir = std::env::temp_dir().join(format!("mswidgets-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config.json");
        std::fs::write(&f, r#"{"mode":"widgets","widgetsLayout":[{"id":"clock-1","type":"clock","size":"s","col":0,"row":0,"cfg":{}}]}"#).unwrap();
        profile_op_at(&f, |r| profile_save(r, "One")).unwrap();
        let before = std::fs::read_to_string(&f).unwrap();
        assert!(profile_op_at(&f, |r| profile_switch(r, "Nope", "U")).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), before);
        profile_op_at(&f, |r| {
            r["widgetsLayout"] = json!([{"id": "calendar-9", "type": "calendar", "size": "s", "col": 1, "row": 1, "cfg": {}}]);
            profile_save(r, "Two")
        })
        .unwrap();
        profile_op_at(&f, |r| profile_switch(r, "One", "U")).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let items = items_from_value(&v);
        assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["clock-1"]);
        assert_eq!(v["mode"], "widgets");
        assert!(!f.with_extension("json.tmp").exists());
        std::fs::write(&f, "{ nope").unwrap();
        assert!(profile_op_at(&f, |r| profile_save(r, "x")).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{ nope");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_updates_is_opt_in() {
        assert!(!Settings::from_value(&json!({})).check_updates);
        assert!(!Settings::from_value(&json!({"checkUpdates": "yes"})).check_updates);
        assert!(Settings::from_value(&json!({"checkUpdates": true})).check_updates);
    }

    #[test]
    fn accent_mode_setting() {
        assert_eq!(Settings::from_value(&json!({})).accent, AccentMode::Blue);
        assert_eq!(Settings::from_value(&json!({"accentMode": "wallpaper"})).accent, AccentMode::Wallpaper);
        assert_eq!(Settings::from_value(&json!({"accentMode": "nonsense"})).accent, AccentMode::Blue);
        assert_eq!(AccentMode::Wallpaper.key(), "wallpaper");
    }

    #[test]
    fn cards_arg_parses_sizes_and_cfg() {
        let ok = parse_cards("clock:s@0,0;calendar:m@1,0;weather:l@0,1{\"city\":\"Oslo\"}").unwrap();
        assert_eq!(ok.len(), 3);
        assert_eq!(ok[2].cfg["city"], "Oslo");
        assert!(parse_cards("worldclock:s@0,0").is_err()); // S not supported
        assert!(parse_cards("nope@0,0").is_err());
    }
}
