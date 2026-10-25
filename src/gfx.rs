//! D3D11 + Direct2D 1.1 + DirectWrite + WIC plumbing shared by every other module.
//! All drawing is GPU side; the only CPU readbacks are the final dirty rectangle of the window
//! (to hand it to UpdateLayeredWindow) and a 8x8 luminance probe (at most 2 Hz).
use std::mem::ManuallyDrop;
use windows::core::{Interface, Result, HSTRING};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HMODULE, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;

pub struct Gfx {
    pub d3d: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub d2d: ID2D1Factory1,
    pub dc: ID2D1DeviceContext,
    pub dw: IDWriteFactory,
    pub wic: IWICImagingFactory,
    pub warp: bool,
    _dev: ID2D1Device,
}

// ---------------------------------------------------------------- small value helpers

pub fn col(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}
pub fn rf(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}
pub fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}
pub fn mat(s: f32, tx: f32, ty: f32) -> Matrix3x2 {
    Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: tx, M32: ty }
}
pub fn rr(x: f32, y: f32, w: f32, h: f32, r: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: rf(x, y, x + w, y + h), radiusX: r, radiusY: r }
}
fn raw<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

// effect property setters (index = the D2D1_*_PROP_* value)
pub fn fx_f32(e: &ID2D1Effect, i: u32, v: f32) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_FLOAT, raw(&v)) }
}
pub fn fx_u32(e: &ID2D1Effect, i: u32, v: u32) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_ENUM, raw(&v)) }
}
pub fn fx_bool(e: &ID2D1Effect, i: u32, v: bool) -> Result<()> {
    let b: i32 = v as i32;
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_BOOL, raw(&b)) }
}
pub fn fx_vec2(e: &ID2D1Effect, i: u32, v: [f32; 2]) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_VECTOR2, raw(&v)) }
}
pub fn fx_vec4(e: &ID2D1Effect, i: u32, v: [f32; 4]) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_VECTOR4, raw(&v)) }
}
pub fn fx_mat5x4(e: &ID2D1Effect, i: u32, v: [f32; 20]) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_MATRIX_5X4, raw(&v)) }
}

pub fn fx_mat3x2(e: &ID2D1Effect, i: u32, v: Matrix3x2) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_MATRIX_3X2, raw(&v)) }
}

pub fn bmp_props(opts: D2D1_BITMAP_OPTIONS, alpha: D2D1_ALPHA_MODE) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: alpha },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: opts,
        colorContext: ManuallyDrop::new(None),
    }
}

/// Unhinted symmetric grayscale text with mild stem darkening: light text on glass keeps the weight of macOS font smoothing instead of the
/// thin default look (the layered window has no ClearType anyway). `MSW_TEXT_GAMMA` / `MSW_TEXT_CONTRAST` override the defaults for tuning.
fn set_text_params(dw: &IDWriteFactory, dc: &ID2D1DeviceContext) {
    let knob = |name: &str, default: f32| std::env::var(name).ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(default);
    let (gamma, contrast) = (knob("MSW_TEXT_GAMMA", 2.2), knob("MSW_TEXT_CONTRAST", 0.6));
    unsafe {
        if let Ok(f3) = dw.cast::<IDWriteFactory3>() {
            // `MSW_TEXT_MODE`: natural (default: hinted vertically, crisp x-height / baselines, even digits at 100 %) | sym (unhinted, softer,
            // better at high DPI) | gdi (fully hinted: breaks kerning, kept for comparison)
            let (mode, fit) = match std::env::var("MSW_TEXT_MODE").unwrap_or_default().as_str() {
                "sym" => (DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC, DWRITE_GRID_FIT_MODE_DISABLED),
                "gdi" => (DWRITE_RENDERING_MODE1_GDI_NATURAL, DWRITE_GRID_FIT_MODE_ENABLED),
                "natural_nofit" => (DWRITE_RENDERING_MODE1_NATURAL, DWRITE_GRID_FIT_MODE_DISABLED),
                _ => (DWRITE_RENDERING_MODE1_NATURAL, DWRITE_GRID_FIT_MODE_ENABLED),
            };
            if let Ok(p) = f3.CreateCustomRenderingParams(gamma, contrast, contrast, 0.0, DWRITE_PIXEL_GEOMETRY_FLAT, mode, fit) {
                dc.SetTextRenderingParams(&p);
            }
        }
    }
}

impl Gfx {
    /// Device on `adapter` (the GPU that owns the monitor we duplicate) or the default hardware adapter;
    /// falls back to WARP (software) when no hardware device can be created.
    pub fn new(adapter: Option<&IDXGIAdapter1>) -> Result<Gfx> {
        unsafe {
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
            let mut dev: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            let mut warp = false;
            let hw = match adapter {
                Some(a) => {
                    let a: IDXGIAdapter = a.cast()?;
                    D3D11CreateDevice(&a, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx))
                }
                None => D3D11CreateDevice(None::<&IDXGIAdapter>, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)),
            };
            if hw.is_err() || dev.is_none() {
                eprintln!("mswidgets: hardware D3D11 device failed ({:?}), using WARP", hw.err());
                warp = true;
                dev = None;
                ctx = None;
                D3D11CreateDevice(None::<&IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx))?;
            }
            let d3d = dev.ok_or_else(windows::core::Error::empty)?;
            let ctx = ctx.ok_or_else(windows::core::Error::empty)?;
            // the capture thread copies into the backdrop texture while the UI thread draws with D2D
            if let Ok(mt) = d3d.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let dxgi: IDXGIDevice = d3d.cast()?;
            let opts = D2D1_FACTORY_OPTIONS { debugLevel: D2D1_DEBUG_LEVEL_NONE };
            let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, Some(&opts))?;
            let dev2d = d2d.CreateDevice(&dxgi)?;
            let dc = dev2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            set_text_params(&dw, &dc);
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            Ok(Gfx { d3d, ctx, d2d, dc, dw, wic, warp, _dev: dev2d })
        }
    }

    // ------------------------------------------------------------ bitmaps

    /// GPU render target, premultiplied BGRA.
    pub fn target(&self, w: u32, h: u32) -> Result<ID2D1Bitmap1> {
        let p = bmp_props(D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED);
        unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, None, 0, &p) }
    }
    /// CPU-readable staging bitmap (copy a target into it with CopyFromBitmap, then Map).
    pub fn cpu_read(&self, w: u32, h: u32) -> Result<ID2D1Bitmap1> {
        let p = bmp_props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_ALPHA_MODE_PREMULTIPLIED);
        unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, None, 0, &p) }
    }
    /// Immutable bitmap from premultiplied BGRA pixels.
    pub fn from_bgra(&self, w: u32, h: u32, px: &[u8]) -> Result<ID2D1Bitmap1> {
        let p = bmp_props(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_PREMULTIPLIED);
        unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(px.as_ptr() as *const _), w * 4, &p) }
    }

    /// Copy `r` of `src` (a target bitmap) through `cpu` and call `f(y, row_bytes)` for each row.
    pub fn read_rect(&self, src: &ID2D1Bitmap1, cpu: &ID2D1Bitmap1, r: RECT, mut f: impl FnMut(i32, &[u8])) -> Result<()> {
        unsafe {
            let dst = D2D_POINT_2U { x: r.left as u32, y: r.top as u32 };
            let sr = D2D_RECT_U { left: r.left as u32, top: r.top as u32, right: r.right as u32, bottom: r.bottom as u32 };
            cpu.CopyFromBitmap(Some(&dst), src, Some(&sr))?;
            let m = cpu.Map(D2D1_MAP_OPTIONS_READ)?;
            let n = ((r.right - r.left) * 4) as usize;
            for y in r.top..r.bottom {
                let row = std::slice::from_raw_parts(m.bits.add(y as usize * m.pitch as usize + r.left as usize * 4), n);
                f(y, row);
            }
            cpu.Unmap()
        }
    }

    /// Record drawing into a command list (the device context target is restored to none afterwards).
    pub fn record(&self, f: impl FnOnce(&ID2D1DeviceContext) -> Result<()>) -> Result<ID2D1CommandList> {
        unsafe {
            let cl = self.dc.CreateCommandList()?;
            self.dc.SetTarget(&cl);
            self.dc.BeginDraw();
            self.dc.SetTransform(&mat(1.0, 0.0, 0.0));
            let r = f(&self.dc);
            let e = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None::<&ID2D1Image>);
            r?;
            e?;
            cl.Close()?;
            Ok(cl)
        }
    }

    /// Linear gradient of white with the given (position, alpha) stops.
    pub fn white_gradient(&self, p0: (f32, f32), p1: (f32, f32), stops: &[(f32, f32)]) -> Result<ID2D1LinearGradientBrush> {
        unsafe {
            let st: Vec<D2D1_GRADIENT_STOP> = stops.iter().map(|&(p, a)| D2D1_GRADIENT_STOP { position: p, color: col(1.0, 1.0, 1.0, a) }).collect();
            let coll = self.dc.CreateGradientStopCollection(&st, D2D1_COLOR_SPACE_SRGB, D2D1_COLOR_SPACE_SRGB, D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_EXTEND_MODE_CLAMP, D2D1_COLOR_INTERPOLATION_MODE_STRAIGHT)?;
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(p0.0, p0.1), endPoint: pt(p1.0, p1.1) };
            self.dc.CreateLinearGradientBrush(&props, None, &coll)
        }
    }

    // ------------------------------------------------------------ WIC png

    /// Image -> premultiplied BGRA exactly w x h: scaled (Fant) to COVER the box, the overflow cropped around the centre
    /// (a backdrop of another aspect ratio is never stretched).
    pub fn load_png(&self, path: &str, w: u32, h: u32) -> Result<Vec<u8>> {
        unsafe {
            let dec = self.wic.CreateDecoderFromFilename(&HSTRING::from(path), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
            let fr = dec.GetFrame(0)?;
            let (mut iw, mut ih) = (0u32, 0u32);
            fr.GetSize(&mut iw, &mut ih)?;
            let k = (w as f32 / iw.max(1) as f32).max(h as f32 / ih.max(1) as f32);
            let (sw, sh) = (((iw as f32 * k).ceil() as u32).max(w), ((ih as f32 * k).ceil() as u32).max(h));
            let sc = self.wic.CreateBitmapScaler()?;
            sc.Initialize(&fr, sw, sh, WICBitmapInterpolationModeFant)?;
            let cl = self.wic.CreateBitmapClipper()?;
            cl.Initialize(&sc, &WICRect { X: ((sw - w) / 2) as i32, Y: ((sh - h) / 2) as i32, Width: w as i32, Height: h as i32 })?;
            let cv = self.wic.CreateFormatConverter()?;
            cv.Initialize(&cl, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
            let mut buf = vec![0u8; (w * h * 4) as usize];
            cv.CopyPixels(std::ptr::null(), w * 4, &mut buf)?;
            Ok(buf)
        }
    }

    /// Straight BGRA pixels -> PNG file.
    pub fn save_png(&self, path: &str, w: u32, h: u32, bgra: &[u8]) -> Result<()> {
        unsafe {
            let stream = self.wic.CreateStream()?;
            stream.InitializeFromFilename(&HSTRING::from(path), GENERIC_WRITE.0)?;
            let enc = self.wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut fr: Option<IWICBitmapFrameEncode> = None;
            enc.CreateNewFrame(&mut fr, std::ptr::null_mut())?;
            let fr = fr.ok_or_else(windows::core::Error::empty)?;
            fr.Initialize(None)?;
            fr.SetSize(w, h)?;
            let mut fmt = GUID_WICPixelFormat32bppBGRA;
            fr.SetPixelFormat(&mut fmt)?;
            fr.WritePixels(h, w * 4, bgra)?;
            fr.Commit()?;
            enc.Commit()
        }
    }
}
