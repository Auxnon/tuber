//! Audio playback: ffmpeg decodes the stream URL to raw f32 PCM, a feeder thread
//! pushes it into a bounded buffer, and a cpal output callback drains it. The
//! callback also copies what it plays into a "tap" ring the visualizer reads,
//! so the visuals are the actual samples hitting the speakers.

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Mono samples kept for the visualizer.
pub const TAP_LEN: usize = 4096;
/// Seconds of decoded audio buffered ahead of playback.
const AHEAD_SECS: usize = 2;

struct Shared {
    pcm: Mutex<VecDeque<f32>>,
    tap: Mutex<VecDeque<f32>>,
    paused: AtomicBool,
    volume: AtomicU32,
    frames_played: AtomicU64,
    decoder_done: AtomicBool,
    /// Bumped on every play/seek/stop; feeder threads from older generations exit.
    generation: AtomicU64,
}

pub struct Player {
    shared: Arc<Shared>,
    _stream: cpal::Stream,
    pub rate: u32,
    channels: u16,
    url: Option<String>,
    start_secs: f64,
}

impl Player {
    pub fn new() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("no audio output device"))?;
        let cfg = device.default_output_config().context("querying output config")?;
        let (rate, channels) = (cfg.sample_rate(), cfg.channels());
        let shared = Arc::new(Shared {
            pcm: Mutex::new(VecDeque::new()),
            tap: Mutex::new(VecDeque::from(vec![0.0; TAP_LEN])),
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(0.8f32.to_bits()),
            frames_played: AtomicU64::new(0),
            decoder_done: AtomicBool::new(true),
            generation: AtomicU64::new(0),
        });
        let sh = shared.clone();
        let ch = channels as usize;
        let stream = device.build_output_stream::<f32, _, _>(
            cpal::StreamConfig { channels, sample_rate: rate, buffer_size: cpal::BufferSize::Default },
            move |out: &mut [f32], _| {
                if sh.paused.load(Relaxed) {
                    out.fill(0.0);
                    return;
                }
                let vol = f32::from_bits(sh.volume.load(Relaxed));
                let mut pcm = sh.pcm.lock().unwrap();
                let n = out.len().min(pcm.len()) / ch * ch;
                let mut tap = sh.tap.lock().unwrap();
                for frame_out in out[..n].chunks_mut(ch) {
                    let mut mono = 0.0;
                    for o in frame_out {
                        let s = pcm.pop_front().unwrap_or(0.0);
                        *o = s * vol;
                        mono += s;
                    }
                    tap.pop_front();
                    tap.push_back(mono / ch as f32);
                }
                out[n..].fill(0.0);
                sh.frames_played.fetch_add((n / ch) as u64, Relaxed);
            },
            // Printing would corrupt the TUI; underruns just play silence.
            |_| {},
            None,
        )?;
        stream.play()?;
        Ok(Self { shared, _stream: stream, rate, channels, url: None, start_secs: 0.0 })
    }

    pub fn play(&mut self, url: String, start_secs: f64) {
        let sh = &self.shared;
        let generation = sh.generation.fetch_add(1, Relaxed) + 1;
        sh.pcm.lock().unwrap().clear();
        sh.frames_played.store(0, Relaxed);
        sh.decoder_done.store(false, Relaxed);
        sh.paused.store(false, Relaxed);
        self.start_secs = start_secs;
        self.url = Some(url.clone());

        let sh = sh.clone();
        let (rate, channels) = (self.rate, self.channels as usize);
        thread::spawn(move || {
            let spawned = Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-reconnect", "1", "-reconnect_streamed", "1"])
                .args(["-reconnect_delay_max", "5", "-ss", &format!("{start_secs:.2}"), "-i", &url])
                .args(["-vn", "-f", "f32le", "-ac", &channels.to_string(), "-ar", &rate.to_string(), "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = spawned else {
                sh.decoder_done.store(true, Relaxed);
                return;
            };
            let mut stdout = child.stdout.take().unwrap();
            let max_buffered = rate as usize * channels * AHEAD_SECS;
            let mut bytes = vec![0u8; 16 * 1024];
            let mut carry = Vec::<u8>::new();
            loop {
                if sh.generation.load(Relaxed) != generation {
                    break;
                }
                if sh.pcm.lock().unwrap().len() > max_buffered {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let n = match stdout.read(&mut bytes) {
                    Ok(0) | Err(_) => {
                        if sh.generation.load(Relaxed) == generation {
                            sh.decoder_done.store(true, Relaxed);
                        }
                        break;
                    }
                    Ok(n) => n,
                };
                carry.extend_from_slice(&bytes[..n]);
                let whole = carry.len() / 4 * 4;
                let samples = carry[..whole].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b));
                if sh.generation.load(Relaxed) == generation {
                    sh.pcm.lock().unwrap().extend(samples);
                }
                carry.drain(..whole);
            }
            let _ = child.kill();
            let _ = child.wait();
        });
    }

    pub fn stop(&mut self) {
        self.shared.generation.fetch_add(1, Relaxed);
        self.shared.pcm.lock().unwrap().clear();
        self.shared.decoder_done.store(true, Relaxed);
        self.url = None;
    }

    pub fn seek_to(&mut self, secs: f64) {
        if let Some(url) = self.url.clone() {
            let paused = self.is_paused();
            self.play(url, secs.max(0.0));
            self.shared.paused.store(paused, Relaxed);
        }
    }

    pub fn position(&self) -> f64 {
        self.start_secs + self.shared.frames_played.load(Relaxed) as f64 / self.rate as f64
    }

    pub fn is_active(&self) -> bool {
        self.url.is_some()
    }

    /// Decoder hit EOF and everything buffered has been played.
    pub fn finished(&self) -> bool {
        self.url.is_some() && self.shared.decoder_done.load(Relaxed) && self.shared.pcm.lock().unwrap().is_empty()
    }

    /// Playing but nothing decoded yet (network/ffmpeg startup).
    pub fn buffering(&self) -> bool {
        self.url.is_some()
            && !self.shared.decoder_done.load(Relaxed)
            && self.shared.frames_played.load(Relaxed) == 0
            && self.shared.pcm.lock().unwrap().is_empty()
    }

    pub fn toggle_pause(&self) {
        self.shared.paused.fetch_xor(true, Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Relaxed)
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Relaxed))
    }

    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.clamp(0.0, 1.5).to_bits(), Relaxed);
    }

    /// Most recent `TAP_LEN` mono samples (pre-volume), oldest first.
    pub fn tap(&self, out: &mut Vec<f32>) {
        out.clear();
        out.extend(self.shared.tap.lock().unwrap().iter());
    }
}
