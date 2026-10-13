//! Anonymous YouTube Music access: InnerTube (the private API music.youtube.com
//! itself uses) for search + radio, and yt-dlp for resolving playable stream URLs.
//! No account, cookies, or API key required.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Command;

const BASE: &str = "https://music.youtube.com/youtubei/v1";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
/// Search filter for the "Songs" tab (same value ytmusicapi uses).
const SONGS_FILTER: &str = "EgWKAQIIAWoQEAMQBBAJEAoQBRAREBAQFQ%3D%3D";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<u64>,
    /// Cover art URL (544px), for MPRIS.
    #[serde(default)]
    pub art: Option<String>,
}

fn post(endpoint: &str, mut body: Value) -> Result<Value> {
    body["context"] = json!({
        "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20250101.01.00", "hl": "en", "gl": "US" }
    });
    let mut resp = ureq::post(format!("{BASE}/{endpoint}?prettyPrint=false"))
        .header("Origin", "https://music.youtube.com")
        .header("User-Agent", UA)
        .send_json(&body)
        .with_context(|| format!("innertube {endpoint}"))?;
    Ok(resp.body_mut().read_json()?)
}

fn text(v: &Value) -> String {
    v["runs"]
        .as_array()
        .map(|runs| runs.iter().filter_map(|r| r["text"].as_str()).collect())
        .unwrap_or_default()
}

/// Largest thumbnail, upsized: googleusercontent URLs take the size as a suffix.
fn art_url(thumbnails: &Value) -> Option<String> {
    let url = thumbnails.as_array()?.last()?["url"].as_str()?;
    Some(match url.rfind('=') {
        Some(i) if url.contains("googleusercontent.com") => format!("{}=w544-h544-l90-rj", &url[..i]),
        _ => url.to_string(),
    })
}

fn parse_duration(s: &str) -> Option<u64> {
    let mut secs = 0u64;
    for part in s.trim().split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    s.contains(':').then_some(secs)
}

pub fn search(query: &str) -> Result<Vec<Track>> {
    let resp = post("search", json!({ "query": query, "params": SONGS_FILTER }))?;
    let sections = &resp["contents"]["tabbedSearchResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
        ["sectionListRenderer"]["contents"];
    let mut tracks = Vec::new();
    for section in sections.as_array().into_iter().flatten() {
        for item in section["musicShelfRenderer"]["contents"].as_array().into_iter().flatten() {
            if let Some(t) = parse_list_item(&item["musicResponsiveListItemRenderer"]) {
                tracks.push(t);
            }
        }
    }
    Ok(tracks)
}

fn parse_list_item(r: &Value) -> Option<Track> {
    let cols = r["flexColumns"].as_array()?;
    let col = |i: usize| cols.get(i).map(|c| &c["musicResponsiveListItemFlexColumnRenderer"]["text"]);
    let title_col = col(0)?;
    let id = title_col["runs"][0]["navigationEndpoint"]["watchEndpoint"]["videoId"]
        .as_str()
        .or_else(|| r["playlistItemData"]["videoId"].as_str())?
        .to_string();
    // Second column reads "Artist • Album • 3:45" (artists may span several runs).
    let meta = col(1).map(text).unwrap_or_default();
    let mut segs: Vec<&str> = meta.split(" • ").collect();
    let duration = segs.last().and_then(|s| parse_duration(s));
    if duration.is_some() {
        segs.pop();
    }
    Some(Track {
        id,
        title: text(title_col),
        artist: segs.first().unwrap_or(&"").to_string(),
        album: segs.get(1).unwrap_or(&"").to_string(),
        duration,
        art: art_url(&r["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]),
    })
}

/// YouTube Music's auto-generated radio for a track ("RDAMVM" mix); the first
/// entry is the seed track itself.
pub fn radio(video_id: &str) -> Result<Vec<Track>> {
    let resp = post(
        "next",
        json!({ "videoId": video_id, "playlistId": format!("RDAMVM{video_id}"), "isAudioOnly": true }),
    )?;
    let items = &resp["contents"]["singleColumnMusicWatchNextResultsRenderer"]["tabbedRenderer"]
        ["watchNextTabbedResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["musicQueueRenderer"]
        ["content"]["playlistPanelRenderer"]["contents"];
    let mut tracks = Vec::new();
    for item in items.as_array().into_iter().flatten() {
        let r = match &item["playlistPanelVideoRenderer"] {
            Value::Null => &item["playlistPanelVideoWrapperRenderer"]["primaryRenderer"]["playlistPanelVideoRenderer"],
            r => r,
        };
        let Some(id) = r["videoId"].as_str() else { continue };
        let byline = text(&r["longBylineText"]);
        let segs: Vec<&str> = byline.split(" • ").collect();
        tracks.push(Track {
            id: id.to_string(),
            title: text(&r["title"]),
            artist: text(&r["shortBylineText"]),
            album: segs.get(1).unwrap_or(&"").to_string(),
            duration: parse_duration(&text(&r["lengthText"])),
            art: art_url(&r["thumbnail"]["thumbnails"]),
        });
    }
    Ok(tracks)
}

/// Resolve a direct audio stream URL. yt-dlp handles signature deciphering and
/// PO tokens, which change too often to be worth reimplementing.
pub fn stream_url(video_id: &str) -> Result<String> {
    let out = Command::new("yt-dlp")
        .args(["-q", "--no-warnings", "--no-playlist", "-f", "bestaudio", "-g"])
        .arg(format!("https://music.youtube.com/watch?v={video_id}"))
        .output()
        .context("running yt-dlp (is it installed?)")?;
    if !out.status.success() {
        bail!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let url = String::from_utf8(out.stdout)?.lines().next().unwrap_or("").to_string();
    if url.is_empty() {
        bail!("yt-dlp returned no URL");
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("7:10"), Some(430));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("Homework"), None);
        assert_eq!(parse_duration("1999"), None);
    }
}
