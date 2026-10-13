//! Local session persistence: queue, current track, position, volume and last
//! search. Lives in `$XDG_STATE_HOME/tuber/session.json` (default
//! `~/.local/state/tuber/`) and needs no account.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::yt::Track;

#[derive(Serialize, Deserialize, Default, PartialEq)]
pub struct Session {
    pub query: String,
    pub results: Vec<Track>,
    pub queue: Vec<Track>,
    pub current: Option<usize>,
    /// Seconds into the current track.
    pub position: f64,
    pub volume: f32,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("tuber/session.json"))
}

pub fn load() -> Option<Session> {
    serde_json::from_slice(&fs::read(path()?).ok()?).ok()
}

pub fn save(session: &Session) -> Result<()> {
    let path = path().context("no state directory")?;
    fs::create_dir_all(path.parent().unwrap())?;
    // Write-then-rename so a crash mid-write can't leave a truncated file.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(session)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}
