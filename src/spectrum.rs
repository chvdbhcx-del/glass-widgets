//! Real spectrum of what the PC plays, for the equalizer of the music card (M).
//! One worker thread captures the default render device in WASAPI shared loopback (no driver, no admin), mixes to mono, runs a 2048-point
//! Hann FFT every ~16 ms and publishes 24 log-spaced band levels (50 Hz .. 14 kHz, 0..1, auto-gain, fast attack / slow release).
//! The thread runs only while `want()` is being called (the music card draws its equalizer: visible and playing); 2 s after the last call it
//! closes the audio client and parks, so an idle card costs nothing. Audio is analysed in memory and never stored or sent anywhere.
//! `want()` returns None when capture cannot start (no device, exotic format): the card falls back to its pseudo animation.
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::*};
use std::sync::{Mutex, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};
use windows::core::Result;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

pub const BANDS: usize = 24;
const N: usize = 2048;
const F_LO: f32 = 50.0;
const F_HI: f32 = 14000.0;
/// bars stay full-scale only above this absolute level: a very quiet track is not blown up to full height
const REF_FLOOR_DB: f32 = -40.0;
/// dB between an empty bar and a full one
const RANGE_DB: f32 = 32.0;
const IDLE_AFTER_MS: u64 = 2000;

// ================================================================ DSP (pure, unit tested)

struct Dsp {
    win: Vec<f32>,
    tw: Vec<(f32, f32)>,
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Dsp {
    fn new() -> Dsp {
        let t = std::f32::consts::TAU;
        Dsp {
            win: (0..N).map(|i| 0.5 - 0.5 * (t * i as f32 / N as f32).cos()).collect(),
            tw: (0..N / 2).map(|k| ((t * k as f32 / N as f32).cos(), (t * k as f32 / N as f32).sin())).collect(),
            re: vec![0.0; N],
            im: vec![0.0; N],
        }
    }

    /// In-place radix-2 FFT of `re`/`im`.
    fn fft(&mut self) {
        let (re, im) = (&mut self.re, &mut self.im);
        let mut j = 0;
        for i in 1..N {
            let mut bit = N >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j ^= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= N {
            let step = N / len;
            for i in (0..N).step_by(len) {
                for k in 0..len / 2 {
                    let (c, s) = self.tw[k * step];
                    let (a, b) = (i + k, i + k + len / 2);
                    let tr = re[b] * c + im[b] * s;
                    let ti = im[b] * c - re[b] * s;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len <<= 1;
        }
    }

    /// dB level (full-scale sine = 0 dB) of each band for the last N mono samples `x` at sample rate `sr`, with a +3 dB/octave tilt so
    /// music (about -3 dB/octave) does not droop on the right.
    fn bands_db(&mut self, x: &[f32], sr: f32) -> [f32; BANDS] {
        for i in 0..N {
            self.re[i] = x[i] * self.win[i];
            self.im[i] = 0.0;
        }
        self.fft();
        let df = sr / N as f32;
        let mut out = [0.0; BANDS];
        for (b, o) in out.iter_mut().enumerate() {
            let (lo, hi) = (band_edge(b), band_edge(b + 1));
            let mid = (lo * hi).sqrt();
            let (k0, k1) = ((lo / df).ceil() as usize, (hi / df).ceil() as usize);
            // bins whose centre lies in the band; a band narrower than a bin takes the nearest one
            let ks = if k1 > k0 { k0..k1 } else { let k = (mid / df).round() as usize; k..k + 1 };
            let mut m = 0.0f32;
            for k in ks {
                let k = k.clamp(1, N / 2 - 1);
                m = m.max((self.re[k] * self.re[k] + self.im[k] * self.im[k]).sqrt());
            }
            *o = 20.0 * (m * 4.0 / N as f32 + 1e-9).log10() + 3.0 * (mid / 1000.0).log2();
        }
        out
    }
}

/// Lower edge in Hz of band `b` (b = BANDS gives the top edge).
fn band_edge(b: usize) -> f32 {
    F_LO * (F_HI / F_LO).powf(b as f32 / BANDS as f32)
}

/// dB per band -> bar heights 0..1: the reference follows the loudest band (up at once, down 4 dB/s), bars rise fast and fall slowly.
struct Scaler {
    reff: f32,
    lv: [f32; BANDS],
}

impl Scaler {
    fn new() -> Scaler {
        Scaler { reff: REF_FLOOR_DB, lv: [0.0; BANDS] }
    }

    fn step(&mut self, db: &[f32; BANDS], dt: f32) -> [f32; BANDS] {
        let peak = db.iter().cloned().fold(-200.0, f32::max);
        self.reff = if peak > self.reff { peak } else { (self.reff - 4.0 * dt).max(peak) }.max(REF_FLOOR_DB);
        let (up, down) = (1.0 - (-dt / 0.03).exp(), 1.0 - (-dt / 0.22).exp());
        for (l, &d) in self.lv.iter_mut().zip(db) {
            let v = ((d - (self.reff - RANGE_DB)) / RANGE_DB).clamp(0.0, 1.0);
            *l += (v - *l) * if v > *l { up } else { down };
        }
        self.lv
    }
}

// ================================================================ shared state

static WANT_MS: AtomicU64 = AtomicU64::new(0);
static IDLE: AtomicBool = AtomicBool::new(false);
/// 0 starting, 1 capturing, 2 cannot capture (the card draws its pseudo animation)
static STATE: AtomicU8 = AtomicU8::new(0);
static LEVELS: Mutex<[f32; BANDS]> = Mutex::new([0.0; BANDS]);
static THREAD: OnceLock<Option<Thread>> = OnceLock::new();
static T0: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn wanted() -> bool {
    let w = WANT_MS.load(Relaxed);
    w != 0 && now_ms().saturating_sub(w) < IDLE_AFTER_MS
}

fn publish(v: [f32; BANDS]) {
    *LEVELS.lock().unwrap_or_else(|e| e.into_inner()) = v;
}

/// Called every frame the equalizer is drawn: keeps the capture alive and returns the latest band levels (0..1), or None if capture is
/// unavailable (draw the pseudo animation then).
pub fn want() -> Option<[f32; BANDS]> {
    WANT_MS.store(now_ms(), Relaxed);
    let th = THREAD.get_or_init(|| std::thread::Builder::new().name("music-spectrum".into()).spawn(worker).ok().map(|h| h.thread().clone()));
    let Some(th) = th else { return None };
    if IDLE.swap(false, AcqRel) {
        th.unpark();
    }
    if STATE.load(Relaxed) == 2 {
        return None;
    }
    Some(*LEVELS.lock().unwrap_or_else(|e| e.into_inner()))
}

// ================================================================ WASAPI loopback

enum Fmt {
    F32,
    Pcm(usize), // bytes per sample: 2, 3 or 4
}

struct Stream {
    en: IMMDeviceEnumerator,
    client: IAudioClient,
    cap: IAudioCaptureClient,
    id: String,
    ch: usize,
    fmt: Fmt,
    sr: f32,
    ring: Vec<f32>,
    pos: usize,
    added: usize,
}

unsafe fn device_id(en: &IMMDeviceEnumerator) -> Result<(IMMDevice, String)> {
    let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let p = dev.GetId()?;
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    Ok((dev, s))
}

impl Stream {
    fn open() -> Result<Stream> {
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let (dev, id) = device_id(&en)?;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            let p = client.GetMixFormat()?;
            let f = p.read_unaligned();
            let (tag, ch, bits, sr) = (f.wFormatTag, f.nChannels as usize, f.wBitsPerSample, f.nSamplesPerSec as f32);
            let sub = if tag == 0xFFFE && f.cbSize >= 22 { (p as *const WAVEFORMATEXTENSIBLE).read_unaligned().SubFormat.data1 } else { tag as u32 };
            let fmt = match (sub, bits) {
                (3, 32) => Some(Fmt::F32),
                (1, 16) => Some(Fmt::Pcm(2)),
                (1, 24) => Some(Fmt::Pcm(3)),
                (1, 32) => Some(Fmt::Pcm(4)),
                _ => None,
            };
            let init = match fmt {
                Some(_) if ch > 0 && sr > 0.0 => client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, p, None),
                _ => Err(E_FAIL.into()),
            };
            CoTaskMemFree(Some(p as *const _));
            init?;
            let cap: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Stream { en, client, cap, id, ch, fmt: fmt.unwrap_or(Fmt::F32), sr, ring: vec![0.0; N], pos: 0, added: 0 })
        }
    }

    fn push(&mut self, s: f32) {
        self.ring[self.pos] = s;
        self.pos = (self.pos + 1) % N;
        self.added += 1;
    }

    /// Everything the device has queued, mixed to mono into the ring.
    fn drain(&mut self) -> Result<()> {
        unsafe {
            while self.cap.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
                self.cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let (frames_u, ch) = (frames as usize, self.ch);
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    for _ in 0..frames_u {
                        self.push(0.0);
                    }
                } else {
                    let bps = match self.fmt { Fmt::F32 | Fmt::Pcm(4) => 4, Fmt::Pcm(b) => b };
                    let bytes = std::slice::from_raw_parts(data, frames_u * ch * bps);
                    for fr in bytes.chunks_exact(ch * bps) {
                        let mut sum = 0.0;
                        for c in fr.chunks_exact(bps) {
                            sum += match self.fmt {
                                Fmt::F32 => f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                                Fmt::Pcm(2) => i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0,
                                Fmt::Pcm(3) => (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0,
                                Fmt::Pcm(_) => i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0,
                            };
                        }
                        self.push(sum / ch as f32);
                    }
                }
                self.cap.ReleaseBuffer(frames)?;
            }
        }
        Ok(())
    }

    /// Capture until the card stops asking, the default device changes (Ok) or the device fails (Err).
    fn run(&mut self, dsp: &mut Dsp) -> Result<()> {
        let mut sc = Scaler::new();
        let (mut last, mut dev_check) = (Instant::now(), Instant::now());
        let mut x = vec![0.0f32; N];
        while wanted() {
            self.drain()?;
            let dt = last.elapsed().as_secs_f32();
            if dt >= 0.014 {
                last = Instant::now();
                // a render device with nothing to play delivers no packets at all: that is silence, not "keep the last picture"
                for _ in 0..if self.added == 0 { ((dt * self.sr) as usize).min(N) } else { 0 } {
                    self.push(0.0);
                }
                self.added = 0;
                let (a, b) = self.ring.split_at(self.pos); // oldest .. newest
                x[..b.len()].copy_from_slice(b);
                x[b.len()..].copy_from_slice(a);
                publish(sc.step(&dsp.bands_db(&x, self.sr), dt));
            }
            if dev_check.elapsed() > Duration::from_secs(2) {
                dev_check = Instant::now();
                if unsafe { device_id(&self.en) }.map_or(true, |(_, id)| id != self.id) {
                    return Ok(()); // another default device: reopen on it
                }
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

fn worker() {
    // MTA for the thread's whole life (it never exits before the process does, so there is no CoUninitialize to pair)
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut dsp = Dsp::new();
    loop {
        if !wanted() {
            publish([0.0; BANDS]);
            IDLE.store(true, SeqCst);
            if !wanted() {
                std::thread::park(); // want() unparks; a spurious wake just loops
            }
            continue;
        }
        IDLE.store(false, SeqCst);
        match Stream::open() {
            Ok(mut s) => {
                STATE.store(1, Relaxed);
                let _ = s.run(&mut dsp);
                std::thread::sleep(Duration::from_millis(300));
            }
            Err(_) => {
                STATE.store(2, Relaxed);
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, amp: f32, sr: f32) -> Vec<f32> {
        (0..N).map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / sr).sin()).collect()
    }

    fn band_of(hz: f32) -> usize {
        (0..BANDS).find(|&b| hz < band_edge(b + 1)).unwrap()
    }

    fn argmax(v: &[f32; BANDS]) -> usize {
        (0..BANDS).fold(0, |m, i| if v[i] > v[m] { i } else { m })
    }

    #[test]
    fn sine_lands_in_its_band() {
        let mut d = Dsp::new();
        for hz in [120.0, 440.0, 1000.0, 3200.0, 9000.0] {
            for sr in [44100.0, 48000.0] {
                let db = d.bands_db(&sine(hz, 0.5, sr), sr);
                let (got, want) = (argmax(&db), band_of(hz));
                assert!(got.abs_diff(want) <= 1, "{hz} Hz @ {sr}: band {got}, expected {want}");
            }
        }
    }

    #[test]
    fn silence_is_empty() {
        let mut d = Dsp::new();
        let db = d.bands_db(&vec![0.0; N], 48000.0);
        let mut s = Scaler::new();
        let mut v = [1.0; BANDS];
        for _ in 0..200 {
            v = s.step(&db, 0.016);
        }
        assert!(v.iter().all(|&x| x < 0.001), "{v:?}");
    }

    #[test]
    fn auto_gain_is_bounded() {
        let mut s = Scaler::new();
        // loud then quiet: heights stay inside 0..1, the reference never drops under the floor
        for amp_db in [-3.0f32, -3.0, -30.0, -60.0, -90.0] {
            for _ in 0..400 {
                let v = s.step(&[amp_db; BANDS], 0.016);
                assert!(v.iter().all(|&x| (0.0..=1.0).contains(&x)));
                assert!(s.reff >= REF_FLOOR_DB);
            }
        }
        // a very quiet signal is not stretched to full height
        let v = s.step(&[-80.0; BANDS], 0.016);
        assert!(v[0] < 0.01);
        // a normal level fills the bars
        let mut s = Scaler::new();
        let mut v = [0.0; BANDS];
        for _ in 0..100 {
            v = s.step(&[-6.0; BANDS], 0.016);
        }
        assert!(v[0] > 0.95);
    }

    /// Manual: `cargo test live_loopback -- --ignored --nocapture` while something plays on this PC.
    #[test]
    #[ignore]
    fn live_loopback() {
        let t0 = Instant::now();
        let mut best = 0.0f32;
        while t0.elapsed() < Duration::from_secs(5) {
            if let Some(v) = want() {
                best = best.max(v.iter().cloned().fold(0.0, f32::max));
                if (t0.elapsed().as_millis() / 250) % 4 == 0 {
                    println!("{}", v.iter().map(|&x| ['_', '.', ':', '|', '#'][(x * 4.0).round() as usize]).collect::<String>());
                }
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        println!("state={} peak={best:.2}", STATE.load(Relaxed));
        assert!(STATE.load(Relaxed) == 1);
    }
}
