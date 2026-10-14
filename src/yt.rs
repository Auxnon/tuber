//! YouTube Music access: InnerTube (the private API music.youtube.com itself
//! uses) for search, radio and the library, and yt-dlp for resolving playable
//! stream URLs. Works anonymously; with a sign-in (see `auth`) requests act as
//! that account, so radio is personalised and liked songs are available.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Command;
use std::sync::OnceLock;

use crate::auth::{self, Auth};

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

/// yt-dlp only uses deno for YouTube's JavaScript challenges unless told
/// otherwise; allow the other runtimes it supports (missing ones are skipped).
pub const JS_RUNTIMES: [&str; 8] =
    ["--js-runtimes", "deno", "--js-runtimes", "node", "--js-runtimes", "bun", "--js-runtimes", "quickjs"];

/// Max liked songs to load (YouTube pages them 100 at a time).
const LIKED_MAX: usize = 1000;

static AUTH: OnceLock<Auth> = OnceLock::new();

/// Act as this account from now on.
pub fn sign_in(a: Auth) {
    let _ = AUTH.set(a);
}

pub fn signed_in() -> bool {
    AUTH.get().is_some()
}

/// Signed in when possible; if YouTube rejects the sign-in (say it expired),
/// search and radio still work anonymously.
fn post(endpoint: &str, body: Value) -> Result<Value> {
    match request(endpoint, body.clone(), true) {
        Err(_) if signed_in() => request(endpoint, body, false),
        res => res,
    }
}

/// Only as the signed-in account.
fn post_signed(endpoint: &str, body: Value) -> Result<Value> {
    if !signed_in() {
        bail!("not signed in");
    }
    request(endpoint, body, true)
}

fn request(endpoint: &str, mut body: Value, signed: bool) -> Result<Value> {
    body["context"] = json!({
        "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20250101.01.00", "hl": "en", "gl": "US" }
    });
    let mut req = ureq::post(format!("{BASE}/{endpoint}?prettyPrint=false"))
        .header("Origin", auth::ORIGIN)
        .header("X-Origin", auth::ORIGIN)
        .header("User-Agent", UA);
    for (name, value) in AUTH.get().filter(|_| signed).map(Auth::headers).into_iter().flatten() {
        req = req.header(name, value);
    }
    let mut resp = req
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
    // Search results read "Artist • Album • 3:45" in the second column (artists
    // may span several runs). Playlists split artist and album into columns
    // and keep the duration in a fixed column.
    let meta = col(1).map(text).unwrap_or_default();
    let mut segs: Vec<&str> = meta.split(" • ").collect();
    let mut duration = segs.last().and_then(|s| parse_duration(s));
    if duration.is_some() {
        segs.pop();
    }
    let fixed = &r["fixedColumns"][0]["musicResponsiveListItemFixedColumnRenderer"]["text"];
    duration = duration.or_else(|| parse_duration(&text(fixed)));
    let album = match (segs.get(1), col(2)) {
        (Some(a), _) => a.to_string(),
        (None, Some(c)) => text(c),
        (None, None) => String::new(),
    };
    Some(Track {
        id,
        title: text(title_col),
        artist: segs.first().unwrap_or(&"").to_string(),
        album,
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

/// Every value under `key`, anywhere in `v`. Browse responses nest their
/// lists differently per page type, so search instead of hard-coding paths.
fn find<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                if k == key {
                    out.push(child);
                } else {
                    find(child, key, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| find(c, key, out)),
        _ => {}
    }
}

/// The signed-in account's liked songs, newest first.
pub fn liked() -> Result<Vec<Track>> {
    playlist("VLLM", post_signed)
}

/// Songs in a playlist page (`VL` + playlist id), following continuations.
fn playlist(browse_id: &str, post: fn(&str, Value) -> Result<Value>) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();
    let mut body = json!({ "browseId": browse_id });
    loop {
        let resp = post("browse", body)?;
        let mut items = Vec::new();
        find(&resp, "musicResponsiveListItemRenderer", &mut items);
        tracks.extend(items.into_iter().filter_map(parse_list_item));
        let mut next = Vec::new();
        find(&resp, "continuationCommand", &mut next);
        match next.first().and_then(|c| c["token"].as_str()) {
            Some(token) if tracks.len() < LIKED_MAX => body = json!({ "continuation": token }),
            _ => break,
        }
    }
    Ok(tracks)
}

/// Display name of the signed-in account.
pub fn account_name() -> Result<String> {
    let resp = post_signed("account/account_menu", json!({}))?;
    let mut names = Vec::new();
    find(&resp, "accountName", &mut names);
    match names.first().map(|n| text(n)) {
        Some(name) if !name.is_empty() => Ok(name),
        _ => bail!("YouTube didn't accept the sign-in; it may have expired (run `tuber --login` again)"),
    }
}

/// Resolve a direct audio stream URL. yt-dlp handles signature deciphering and
/// PO tokens, which change too often to be worth reimplementing.
/// Signed in, it passes the cookies too (Premium audio quality, age-restricted
/// tracks), falling back to anonymous if YouTube refuses that.
pub fn stream_url(video_id: &str) -> Result<String> {
    let cookies = auth::path().filter(|p| signed_in() && p.exists());
    match cookies.map(|c| yt_dlp(video_id, Some(&c))) {
        Some(Ok(url)) => Ok(url),
        _ => yt_dlp(video_id, None),
    }
}

fn yt_dlp(video_id: &str, cookies: Option<&std::path::Path>) -> Result<String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args(["-q", "--no-warnings", "--no-playlist", "-f", "bestaudio", "-g"]);
    cmd.args(JS_RUNTIMES);
    if let Some(c) = cookies {
        cmd.arg("--cookies").arg(c);
    }
    let out = cmd
        .arg(format!("https://music.youtube.com/watch?v={video_id}"))
        .output()
        .context("running yt-dlp (is it installed?)");
    // yt-dlp writes the jar back when it exits; make sure it stays private.
    if let Some(c) = cookies {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(c, std::fs::Permissions::from_mode(0o600));
    }
    let out = out?;
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

    /// Hits the network: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn public_playlist() {
        // YouTube Music's "Released" chart-style public playlist would rotate; any public one works.
        let id = std::env::var("TUBER_TEST_PLAYLIST").unwrap_or("VLRDCLAK5uy_kmPRjHDECIcuVwnKsx2Ng7fyNgFKWNJFs".into());
        let tracks = playlist(&id, post).unwrap();
        assert!(!tracks.is_empty());
        assert!(tracks.iter().all(|t| !t.title.is_empty() && !t.artist.is_empty()));
        eprintln!("{} tracks; first: {:?}", tracks.len(), tracks[0]);
    }
}
