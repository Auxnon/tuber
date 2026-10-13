//! Whole-track loudness envelope for the waveform seekbar. A second, low-rate
//! ffmpeg pass over the stream (about 1–2s for a typical song) produces RMS
//! buckets, which fill in progressively as they're decoded.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

const RATE: usize = 4000;
pub const BUCKETS_PER_SEC: usize = 10;
const BUCKET: usize = RATE / BUCKETS_PER_SEC;

#[derive(Default)]
pub struct Envelope {
    pub id: String,
    /// RMS per 1/BUCKETS_PER_SEC seconds.
    pub buckets: Vec<f32>,
    pub done: bool,
}

impl Envelope {
    pub fn secs(&self) -> f64 {
        self.buckets.len() as f64 / BUCKETS_PER_SEC as f64
    }

    /// Mean RMS over [t0, t1) seconds, or `None` if not analysed yet.
    /// Once analysis is done, ranges past the end (metadata durations run a
    /// little long) clamp to the last bucket.
    pub fn mean(&self, t0: f64, t1: f64) -> Option<f32> {
        let len = self.buckets.len();
        let mut b0 = (t0 * BUCKETS_PER_SEC as f64) as usize;
        let mut b1 = ((t1 * BUCKETS_PER_SEC as f64).ceil() as usize).max(b0 + 1);
        if b1 > len {
            if !self.done || len == 0 {
                return None;
            }
            b1 = len;
            b0 = b0.min(len - 1);
        }
        Some(self.buckets[b0..b1].iter().sum::<f32>() / (b1 - b0) as f32)
    }
}

/// Start analysing `url` into `env` unless it's already working on `id`.
/// A newer call replaces the id, which makes older threads stop.
pub fn analyse(env: &Arc<Mutex<Envelope>>, id: &str, url: &str) {
    {
        let mut e = env.lock().unwrap();
        if e.id == id {
            return;
        }
        *e = Envelope { id: id.to_string(), ..Default::default() };
    }
    let (env, id, url) = (env.clone(), id.to_string(), url.to_string());
    thread::spawn(move || {
        let Ok(mut child) = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i", &url, "-vn", "-ac", "1", "-ar", &RATE.to_string(), "-f", "f32le", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let mut stdout = child.stdout.take().unwrap();
        let mut raw = vec![0u8; BUCKET * 4];
        loop {
            if stdout.read_exact(&mut raw).is_err() {
                break; // EOF (a trailing partial bucket is dropped)
            }
            let sum_sq: f32 = raw.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b).powi(2)).sum();
            let rms = (sum_sq / BUCKET as f32).sqrt();
            let mut e = env.lock().unwrap();
            if e.id != id {
                break;
            }
            e.buckets.push(rms);
        }
        let _ = child.kill();
        let _ = child.wait();
        let mut e = env.lock().unwrap();
        if e.id == id {
            e.done = true;
        }
    });
}
