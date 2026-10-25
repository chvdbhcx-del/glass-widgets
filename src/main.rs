//! mswidgets - native (no WebView) Direct2D renderer for the macshell desktop widgets. See native/NATIVE.md.
//!   mswidgets --dump out.png --backdrop bd.png --cards "clock:s@0,0;calendar:m@1,0" [--scale 1] [--w W --h H] [--time "2026-10-24 10:08:35"]
//!   mswidgets --sheet out.png --backdrop bd.png [--only clock,calendar] [--scale 1] [--time "..."]    contact sheet: every widget x every size
//!   mswidgets --dump-ui out.png [--backdrop bd.png] --state live|edit|gallery|menu [--size WxH] [--search txt] [--group id] [--psize s|m|l] [--hover n] [--dim] [--scale S] [--cards ...]
//!                                                                     the host UI (edit mode, gallery with live previews) rendered headless
//!   mswidgets [--backdrop-fps 30] [--scale S] [--no-config] [--no-hotkeys]   live mode (Ctrl+Alt+Q quits, Ctrl+Alt+W edits; a second instance exits silently)
//!   mswidgets --bench SECONDS [--backdrop-fps N]                    live mode that prints stats and exits
//!   mswidgets --selftest-dupl                                       desktop-duplication self test (no window)
// release builds are GUI-subsystem: no console window flashes when started from the Start menu / autostart (see `attach_console`)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(clippy::too_many_arguments)]
// the widget API (widget.rs, sys.rs, tz.rs, icons.rs) is a public surface for widget files that do not exist yet: unused items are expected
#![allow(dead_code, unused_imports)]
mod backdrop;
mod calib;
mod cards;
mod gfx;
mod glass;
mod grid;
mod host;
mod icons;
mod input;
mod layout;
mod liquid;
mod motion;
mod net;
mod registry;
mod sys;
pub mod tz;
mod ui;
mod widget;
mod widgets;

use widget::{Now, Size};
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

/// widgetsGlass (0..1, default .5) from the macshell config.
pub fn glass_setting() -> f32 {
    layout::read_settings().glass
}

#[derive(Default)]
struct Args {
    dump: Option<String>,
    sheet: Option<String>,
    backdrop: Option<String>,
    cards: Option<String>,
    only: Option<Vec<String>>,
    scale: Option<f32>,
    w: Option<u32>,
    h: Option<u32>,
    bench: Option<u32>,
    fps: Option<u32>,
    time: Option<Now>,
    no_config: bool,
    selftest: bool,
    no_hotkeys: bool,
    dim: bool,
    dump_ui: Option<String>,
    state: Option<String>,
    canvas: Option<(u32, u32)>,
    search: Option<String>,
    group: Option<String>,
    psize: Option<Size>,
    hover: Option<usize>,
    appearance: Option<motion::AppearancePref>,
    glass_params: Option<String>,
    calib: Option<String>,
    calib_dir: Option<String>,
    calib_out: Option<String>,
    calib_rounds: Option<usize>,
    calib_dump: Option<String>,
    anim_strip: Option<String>,
    times: Option<Vec<f32>>,
    anim_out: bool,
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().unwrap_or_default();
        match k.as_str() {
            "--dump" => a.dump = Some(val()),
            "--sheet" => a.sheet = Some(val()),
            "--backdrop" => a.backdrop = Some(val()),
            "--cards" => a.cards = Some(val()),
            "--only" => a.only = Some(val().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()),
            "--scale" => a.scale = val().parse::<f32>().ok(),
            "--w" => a.w = val().parse().ok(),
            "--h" => a.h = val().parse().ok(),
            "--bench" => a.bench = val().parse::<u32>().ok(),
            "--backdrop-fps" => a.fps = val().parse().ok(),
            "--time" => {
                let v = val();
                a.time = Now::parse(&v);
                if a.time.is_none() {
                    eprintln!("mswidgets: --time expects \"YYYY-MM-DD HH:MM[:SS]\", got '{v}'");
                }
            }
            "--no-config" => a.no_config = true,
            "--no-hotkeys" => a.no_hotkeys = true,
            "--dim" => a.dim = true,
            "--dump-ui" => a.dump_ui = Some(val()),
            "--state" => a.state = Some(val()),
            "--size" => {
                let v = val();
                a.canvas = v.split_once(['x', 'X']).and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
                if a.canvas.is_none() {
                    eprintln!("mswidgets: --size expects WxH, got '{v}'");
                }
            }
            "--search" => a.search = Some(val()),
            "--group" => a.group = Some(val()),
            "--psize" => a.psize = Size::parse(&val()),
            "--hover" => a.hover = val().parse().ok(),
            "--appearance" => {
                let v = val();
                a.appearance = motion::AppearancePref::parse(&v);
                if a.appearance.is_none() {
                    eprintln!("mswidgets: --appearance expects auto|dark|light, got '{v}'");
                }
            }
            "--glass-params" => a.glass_params = Some(val()),
            "--anim-strip" => a.anim_strip = Some(val()),
            "--times" => a.times = Some(val().split(',').filter_map(|t| t.trim().parse().ok()).collect()),
            "--anim-out" => a.anim_out = true,
            "--calib" => a.calib = Some(val()),
            "--calib-dir" => a.calib_dir = Some(val()),
            "--calib-out" => a.calib_out = Some(val()),
            "--calib-rounds" => a.calib_rounds = val().parse().ok(),
            "--calib-dump" => a.calib_dump = Some(val()),
            "--selftest-dupl" => a.selftest = true,
            other => eprintln!("mswidgets: unknown argument {other}"),
        }
    }
    a
}

/// Started from a terminal (no std handles of its own), a GUI-subsystem release build attaches to that console so `--bench`, `--help`-style
/// output and `eprintln!` diagnostics still show. Redirected handles (`2> log`) are kept as they are.
#[cfg(not(debug_assertions))]
unsafe fn attach_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::*;
    let has = |h: STD_HANDLE| GetStdHandle(h).map_or(false, |x| !x.is_invalid() && !x.0.is_null());
    if has(STD_OUTPUT_HANDLE) || has(STD_ERROR_HANDLE) || AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
        return;
    }
    for h in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            let _ = SetStdHandle(h, HANDLE(f.into_raw_handle()));
        }
    }
}

fn main() {
    #[cfg(not(debug_assertions))]
    unsafe {
        attach_console()
    };
    let a = parse_args();
    glass::init_params(a.glass_params.as_deref());
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    if let Some(mode) = &a.calib {
        let r = calib::run(&calib::CalibArgs { mode: mode.clone(), dir: a.calib_dir.clone().unwrap_or_else(|| "calib/ref".into()), out: a.calib_out.clone(), params: a.glass_params.clone(), rounds: a.calib_rounds.unwrap_or(6), dump: a.calib_dump.clone() });
        std::process::exit(report(r));
    }
    if a.selftest {
        std::process::exit(selftest_dupl(a.fps.unwrap_or(30)));
    }
    let code = if let Some(out) = &a.sheet {
        report(run_sheet(out, &a))
    } else if let Some(out) = &a.anim_strip {
        report(run_anim_strip(out, &a))
    } else if let Some(out) = &a.dump {
        report(run_dump(out, &a))
    } else if let Some(out) = &a.dump_ui {
        let (w, h) = a.canvas.unwrap_or((1920, 1080));
        report(ui::dump_ui(&ui::DumpArgs {
            out: out.clone(),
            backdrop: a.backdrop.clone(),
            state: a.state.clone().unwrap_or_else(|| "gallery".into()),
            w,
            h,
            scale: a.scale.unwrap_or(1.0),
            search: a.search.clone(),
            group: a.group.clone(),
            hover: a.hover,
            cards: a.cards.clone(),
            size: a.psize,
            no_config: a.no_config,
            time: a.time,
            dim: a.dim,
            appearance: a.appearance,
        }))
    } else {
        // one live instance per session (a bench run may coexist with a live one)
        if a.bench.is_none() {
            unsafe {
                let h = CreateMutexW(None, true, w!("Local\\mswidgets-native-host"));
                if h.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
                    std::process::exit(0);
                }
                std::mem::forget(h); // held until the process ends
            }
        }
        host::run(host::Opts { appearance: a.appearance, fps: a.fps.unwrap_or(30), bench: a.bench, scale: a.scale, no_config: a.no_config, time: a.time, no_hotkeys: a.no_hotkeys })
    };
    std::process::exit(code);
}

fn report(r: Result<(), String>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("mswidgets: {e}");
            1
        }
    }
}

/// Headless render of `specs` over the backdrop picture (or the plain base) on a w x h canvas, saved as PNG.
/// Always preview mode: sample data, no I/O, overlays painted once at the fixed time.
fn render_png(out: &str, bd: Option<&str>, specs: &[cards::CardSpec], labels: Vec<(String, f32, f32)>, w: u32, h: u32, s: f32, time: Option<Now>, appearance: Option<motion::AppearancePref>) -> Result<(), String> {
    use backdrop::Backdrop;
    let e = |what: &'static str| move |e: windows::core::Error| format!("{what}: {e}");
    let gfx = gfx::Gfx::new(None).map_err(e("device"))?;
    let backdrop: Box<dyn Backdrop> = match bd.map(|p| backdrop::FileBackdrop::new(&gfx, p, w, h)) {
        Some(Ok(b)) => Box::new(b),
        other => {
            if let Some(Err(err)) = other {
                eprintln!("mswidgets: backdrop image: {err}; using the plain base");
            }
            Box::new(backdrop::PlainBackdrop::new(&gfx, w, h).map_err(e("backdrop"))?)
        }
    };
    let env = cards::SceneEnv { preview: true, hwnd: 0, lang: layout::lang(), utc_off: sys::local_utc_offset_min() };
    let mut scene = cards::Scene::new(&gfx, w as i32, h as i32, s, glass_setting(), backdrop.bitmap(), specs, &env).map_err(e("scene"))?;
    scene.labels = labels;
    scene.set_appearance_pref(appearance.unwrap_or_else(|| layout::read_settings().appearance));
    let now = time.unwrap_or_else(Now::sample);
    for i in 0..scene.len() {
        scene.tick_card(&gfx, i, &now);
    }
    let all = if scene.len() >= 64 { u64::MAX } else { (1u64 << scene.len()) - 1 };
    scene.measure_lum(&gfx, all, false).map_err(e("luminance"))?;
    scene.apply_debug(&gfx, &now);
    for i in 0..scene.len() {
        scene.render_card(&gfx, i, &now).map_err(e("render"))?;
    }
    let full = windows::Win32::Foundation::RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    scene.composite(&gfx, full, Some(backdrop.bitmap()), &now).map_err(e("composite"))?;
    let mut px = vec![0u8; (w * h * 4) as usize];
    scene
        .read_rect(&gfx, full, |y, row| {
            let o = y as usize * w as usize * 4;
            px[o..o + row.len()].copy_from_slice(row);
        })
        .map_err(e("readback"))?;
    gfx.save_png(out, w, h, &px).map_err(e("save"))?;
    eprintln!("saved {out} ({w}x{h}, {} cards)", scene.len());
    Ok(())
}

/// `--anim-strip out.png --cards "calendar:s@0,0" [--times 0,40,80,...] [--anim-out]`: the FIRST card of `--cards` once per time step (ms into
/// its materialisation, or dematerialisation with `--anim-out`), side by side: a filmstrip of the animation, rendered headless.
fn run_anim_strip(out: &str, a: &Args) -> Result<(), String> {
    let items = layout::parse_cards(a.cards.as_deref().unwrap_or("calendar:s@0,0"))?;
    let it = &items[0];
    let times = a.times.clone().unwrap_or_else(|| if a.anim_out { vec![0.0, 30.0, 60.0, 100.0, 140.0, 180.0, 220.0, 250.0] } else { vec![0.0, 40.0, 80.0, 120.0, 160.0, 220.0, 300.0, 450.0] });
    let key = if a.anim_out { "_mout" } else { "_mat" };
    let spec: Vec<String> = times.iter().enumerate().map(|(i, t)| format!("{}:{}@{i},0{{\"{key}\":{t}}}", it.type_id, it.size.key())).collect();
    run_dump_cards(out, a, Some(spec.join(";")))
}

/// `--dump`: the cards of `--cards` (or the config layout) at their grid cells, composited like a desktop screenshot.
fn run_dump(out: &str, a: &Args) -> Result<(), String> {
    run_dump_cards(out, a, a.cards.clone())
}

fn run_dump_cards(out: &str, a: &Args, cards: Option<String>) -> Result<(), String> {
    let s = a.scale.unwrap_or(1.0);
    let items = match &cards {
        Some(c) => layout::parse_cards(c)?,
        None => layout::load(!a.no_config),
    };
    let specs = cards::specs(&items, s);
    let right = specs.iter().map(|c| c.x + c.w).max().unwrap_or(0);
    let bottom = specs.iter().map(|c| c.y + c.h).max().unwrap_or(0);
    let w = a.w.unwrap_or((right as f32 + cards::ORIGIN.0 * s).round().max(64.0) as u32);
    let h = a.h.unwrap_or((bottom as f32 + 30.0 * s).round().max(64.0) as u32);
    render_png(out, a.backdrop.as_deref(), &specs, Vec::new(), w, h, s, a.time, a.appearance)
}

/// `--sheet`: every registered widget at every supported size, labelled `id:size`, flowing in rows of ~1700 px.
fn run_sheet(out: &str, a: &Args) -> Result<(), String> {
    let s = a.scale.unwrap_or(1.0);
    let reg = registry::global();
    let (width, margin, gap, label) = (1700.0 * s, 24.0 * s, 24.0 * s, 24.0 * s);
    let (mut x, mut y, mut row_h) = (margin, margin + label, 0.0f32);
    let (mut specs, mut labels) = (Vec::new(), Vec::new());
    // grouped by size class (S band, M band, L band) so rows stay compact; each band starts on a fresh row
    for size in Size::ALL {
        if x > margin {
            x = margin;
            y += row_h + gap + label;
            row_h = 0.0;
        }
        for def in reg.all() {
            if !def.sizes.contains(&size) || a.only.as_ref().is_some_and(|o| !o.iter().any(|id| id == def.type_id)) {
                continue;
            }
            let (lw, lh) = size.logical();
            let (pw, ph) = ((lw * s).round(), (lh * s).round());
            if x + pw > width - margin && x > margin {
                x = margin;
                y += row_h + gap + label;
                row_h = 0.0;
            }
            let id = format!("{}:{}", def.type_id, size.key());
            labels.push((id.clone(), x, y - label + 2.0 * s));
            specs.push(cards::CardSpec { id, type_id: def.type_id.to_string(), size, cfg: serde_json::json!({}), x: x.round() as i32, y: y.round() as i32, w: pw as i32, h: ph as i32 });
            x += pw + gap;
            row_h = row_h.max(ph);
        }
    }
    if specs.is_empty() {
        return Err(format!("--sheet: no widget matches --only (known: {})", reg.all().iter().map(|d| d.type_id).collect::<Vec<_>>().join(", ")));
    }
    let used_w = specs.iter().map(|c| c.x + c.w).max().unwrap_or(0) as f32 + margin;
    render_png(out, a.backdrop.as_deref(), &specs, labels, used_w.round() as u32, (y + row_h + margin).round() as u32, s, a.time, a.appearance)
}

/// No window, nothing drawn on the desktop: duplicate the primary output for ~2 s on the owning adapter, GPU-copy the card
/// regions, run the 8x8 luminance probe on them and print the counters. Proves the capture path without live mode.
fn selftest_dupl(fps: u32) -> i32 {
    use windows::Win32::Foundation::RECT;
    let Some(out) = backdrop::find_primary_output() else {
        eprintln!("selftest: no primary output found");
        return 1;
    };
    let r = (|| -> windows::core::Result<()> {
        let gfx = gfx::Gfx::new(Some(&out.adapter))?;
        let win = RECT { left: 0, top: 0, right: 420, bottom: 250 };
        let items = layout::parse_cards("clock:s@0,0;calendar:s@1,0").unwrap_or_default();
        let specs = cards::specs(&items, 1.0);
        let rects: Vec<RECT> = specs.iter().map(|c| c.rect()).collect();
        let d = backdrop::DuplBackdrop::new(&gfx, &out, win, &rects, 48, fps)?;
        let env = cards::SceneEnv { preview: true, hwnd: 0, lang: widget::Lang::Ru, utc_off: 0 };
        let mut scene = cards::Scene::new(&gfx, 420, 250, 1.0, 0.5, backdrop::Backdrop::bitmap(&d), &specs, &env)?;
        backdrop::Backdrop::start(&d);
        std::thread::sleep(std::time::Duration::from_millis(2000));
        let mask = backdrop::Backdrop::take_dirty(&d);
        scene.measure_lum(&gfx, 3, false)?;
        let (acq, copied) = backdrop::Backdrop::stats(&d);
        println!("adapter={} warp={} output={}x{}", out.adapter_name, gfx.warp, out.rect.right - out.rect.left, out.rect.bottom - out.rect.top);
        println!("duplication_frames={acq} frames_with_gpu_copy={copied} dirty_mask={mask:#b} lum0={:.3} lum1={:.3}", scene.lum(0), scene.lum(1));
        Ok(())
    })();
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("selftest failed: {e}");
            1
        }
    }
}
