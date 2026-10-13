//! Audio visualizer drawn with braille dots on a ratatui `Canvas`. Each terminal
//! cell is a 2x4 dot grid, so a W x H area gives 2W x 4H "pixels".

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::symbols::Marker;
use ratatui::widgets::Widget;
use ratatui::widgets::canvas::{Canvas, Context, Line};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

const FFT_LEN: usize = 2048;
const SCOPE_LEN: usize = 1024;
const F_LO: f32 = 30.0;
const F_HI: f32 = 16_000.0;
const DB_FLOOR: f32 = -65.0;
const DB_RANGE: f32 = 60.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Spectrum,
    Mirror,
    Scope,
}

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Mode::Spectrum => Mode::Mirror,
            Mode::Mirror => Mode::Scope,
            Mode::Scope => Mode::Spectrum,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Spectrum => "spectrum",
            Mode::Mirror => "mirror",
            Mode::Scope => "scope",
        }
    }
}

pub struct Visualizer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    buf: Vec<Complex<f32>>,
    mags: Vec<f32>,
    bands: Vec<f32>,
    peaks: Vec<f32>,
    scope: Vec<f32>,
    pub mode: Mode,
    /// Smoothed loudness, 0..1 (fast attack, slow release).
    pub level: f32,
    /// Jumps to 1 on a transient, decays each frame — for beat flashes.
    pub flash: f32,
    loud_avg: f32,
}

impl Visualizer {
    pub fn new() -> Self {
        let window = (0..FFT_LEN)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_LEN as f32).cos())
            .collect();
        Self {
            fft: FftPlanner::new().plan_fft_forward(FFT_LEN),
            window,
            buf: vec![Complex::default(); FFT_LEN],
            mags: vec![0.0; FFT_LEN / 2],
            bands: Vec::new(),
            peaks: Vec::new(),
            scope: Vec::new(),
            mode: Mode::Spectrum,
            level: 0.0,
            flash: 0.0,
            loud_avg: 0.0,
        }
    }

    /// Analyse the latest samples. `n_bands` should match the render width in cells.
    /// When `live` is false (paused/stopped) everything falls to zero.
    pub fn update(&mut self, tap: &[f32], rate: u32, n_bands: usize, live: bool) {
        if self.bands.len() != n_bands {
            self.bands = vec![0.0; n_bands];
            self.peaks = vec![0.0; n_bands];
        }
        let tail = &tap[tap.len().saturating_sub(FFT_LEN)..];
        for (i, c) in self.buf.iter_mut().enumerate() {
            let s = if live { tail.get(i).copied().unwrap_or(0.0) } else { 0.0 };
            *c = Complex::new(s * self.window[i], 0.0);
        }
        self.fft.process(&mut self.buf);
        // Hann window has coherent gain 0.5, so a full-scale sine peaks at ~N/4.
        for (m, c) in self.mags.iter_mut().zip(&self.buf) {
            *m = c.norm() * 4.0 / FFT_LEN as f32;
        }

        let f_hi = F_HI.min(rate as f32 / 2.0);
        let bin_hz = rate as f32 / FFT_LEN as f32;
        for k in 0..n_bands {
            let f0 = F_LO * (f_hi / F_LO).powf(k as f32 / n_bands as f32);
            let f1 = F_LO * (f_hi / F_LO).powf((k + 1) as f32 / n_bands as f32);
            let b0 = ((f0 / bin_hz) as usize).min(self.mags.len() - 1);
            let b1 = ((f1 / bin_hz) as usize).clamp(b0 + 1, self.mags.len());
            let amp = self.mags[b0..b1].iter().copied().fold(0.0, f32::max);
            // +3 dB/octave tilt so music (roughly pink) reads flat instead of bass-heavy.
            let fc = (f0 * f1).sqrt();
            let db = 20.0 * (amp + 1e-9).log10() + 3.0 * (fc / 1000.0).log2();
            let v = ((db - DB_FLOOR) / DB_RANGE).clamp(0.0, 1.0);

            let s = &mut self.bands[k];
            *s = if v > *s { v * 0.7 + *s * 0.3 } else { (*s - 0.03).max(v) };
            let p = &mut self.peaks[k];
            *p = if *s > *p { *s } else { (*p - 0.006).max(0.0) };
        }

        // Loudness + crude beat detection over the most recent ~20ms.
        let recent = &tap[tap.len().saturating_sub(1024)..];
        let rms = if live && !recent.is_empty() {
            (recent.iter().map(|s| s * s).sum::<f32>() / recent.len() as f32).sqrt()
        } else {
            0.0
        };
        let target = (rms * 3.0).min(1.0);
        self.level = if target > self.level { target * 0.6 + self.level * 0.4 } else { self.level * 0.9 + target * 0.1 };
        if rms > 0.05 && rms > self.loud_avg * 1.35 && self.flash < 0.4 {
            self.flash = 1.0;
        } else {
            self.flash *= 0.86;
        }
        self.loud_avg = self.loud_avg * 0.95 + rms * 0.05;

        self.scope.clear();
        if live {
            let tail = &tap[tap.len().saturating_sub(SCOPE_LEN * 2)..];
            // Trigger on a rising zero crossing so the trace holds still.
            let start = (1..tail.len().saturating_sub(SCOPE_LEN))
                .find(|&i| tail[i - 1] <= 0.0 && tail[i] > 0.0)
                .unwrap_or(0);
            self.scope.extend_from_slice(&tail[start..(start + SCOPE_LEN).min(tail.len())]);
        }
    }
}

/// Render the visualizer with a bottom→top gradient between `from` and `to`.
pub struct VizWidget<'a> {
    pub viz: &'a Visualizer,
    pub from: Color,
    pub to: Color,
    pub peak: Color,
}

fn rgb(c: Color) -> (f32, f32, f32) {
    match c {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        // xterm greyscale ramp
        Color::Indexed(n @ 232..=255) => {
            let v = 8.0 + 10.0 * (n - 232) as f32;
            (v, v, v)
        }
        _ => (200.0, 200.0, 200.0),
    }
}

pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    let (a, b, t) = (rgb(a), rgb(b), t.clamp(0.0, 1.0));
    let mix = |x: f32, y: f32| (x + (y - x) * t).round() as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

impl Widget for VizWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 2 || area.height < 1 {
            return;
        }
        let (w, h) = (area.width as f64 * 2.0, area.height as f64 * 4.0);
        let rows = area.height as usize;
        let viz = self.viz;
        Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, w - 1.0])
            .y_bounds([0.0, h - 1.0])
            .paint(|ctx| match viz.mode {
                Mode::Spectrum => {
                    for (k, (&s, &p)) in viz.bands.iter().zip(&viz.peaks).enumerate() {
                        let x = k as f64 * 2.0;
                        let top = s as f64 * (h - 1.0);
                        self.column(ctx, x, 0.0, top, rows, |r| r as f32 / rows as f32);
                        if p > 0.02 {
                            ctx.draw(&Line::new(x, p as f64 * (h - 1.0), x, p as f64 * (h - 1.0), self.peak));
                        }
                    }
                }
                Mode::Mirror => {
                    let mid = (h - 1.0) / 2.0;
                    for (k, &s) in viz.bands.iter().enumerate() {
                        let x = k as f64 * 2.0;
                        let half = s as f64 * mid;
                        let mid_row = rows as f32 / 2.0;
                        let t = |r: usize| ((r as f32 + 0.5 - mid_row).abs() / mid_row).min(1.0);
                        self.column(ctx, x, mid - half, mid + half, rows, t);
                    }
                }
                Mode::Scope => {
                    let n = viz.scope.len();
                    if n < 2 {
                        ctx.draw(&Line::new(0.0, (h - 1.0) / 2.0, w - 1.0, (h - 1.0) / 2.0, self.from));
                        return;
                    }
                    let mid = (h - 1.0) / 2.0;
                    let pt = |i: usize| {
                        let x = i as f64 / (n - 1) as f64 * (w - 1.0);
                        let s = (viz.scope[i] as f64 * 1.6).clamp(-1.0, 1.0);
                        (x, mid + s * mid, s.abs() as f32)
                    };
                    let step = (n as f64 / w).max(1.0);
                    let mut i = 0.0;
                    let mut prev = pt(0);
                    while (i + step) < n as f64 {
                        i += step;
                        let cur = pt(i as usize);
                        ctx.draw(&Line::new(prev.0, prev.1, cur.0, cur.1, lerp(self.from, self.to, cur.2 * 1.5)));
                        prev = cur;
                    }
                }
            })
            .render(area, buf);
    }
}

impl VizWidget<'_> {
    /// One bar from `y0` to `y1` (dot units), split per cell row so each row
    /// gets its own gradient colour (`t(row)` → 0..1).
    fn column(&self, ctx: &mut Context, x: f64, y0: f64, y1: f64, rows: usize, t: impl Fn(usize) -> f32) {
        if y1 - y0 < 0.5 {
            return;
        }
        for r in 0..rows {
            let (lo, hi) = (r as f64 * 4.0, r as f64 * 4.0 + 3.0);
            let (a, b) = (y0.max(lo), y1.min(hi));
            if a <= b {
                ctx.draw(&Line::new(x, a, x, b, lerp(self.from, self.to, t(r))));
            }
        }
    }
}
