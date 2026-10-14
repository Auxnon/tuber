//! Optional sign-in, borrowed from a browser that's already signed in to
//! YouTube Music. yt-dlp exports that browser's cookies (it knows how to
//! decrypt every major browser's store), we keep only the YouTube and Google
//! ones in a private file, and InnerTube requests are signed with them the way
//! music.youtube.com signs its own (`SAPISIDHASH`). Google's device-code OAuth
//! no longer works for third-party YouTube Music clients, so this is the
//! method that still works without a developer project.

use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session;

pub const ORIGIN: &str = "https://music.youtube.com";

/// yt-dlp browser names and where each keeps its profile, in detection order.
const BROWSERS: [(&str, &str); 9] = [
    ("chrome", ".config/google-chrome"),
    ("firefox", ".mozilla/firefox"),
    ("firefox", "snap/firefox/common/.mozilla/firefox"),
    ("brave", ".config/BraveSoftware/Brave-Browser"),
    ("chromium", ".config/chromium"),
    ("edge", ".config/microsoft-edge"),
    ("vivaldi", ".config/vivaldi"),
    ("opera", ".config/opera"),
    ("chromium", "snap/chromium/common/chromium"),
];

pub struct Auth {
    cookie: String,
    sapisid: String,
}

/// The cookie jar, next to the session file. Readable by you only.
pub fn path() -> Option<PathBuf> {
    session::state_dir().map(|d| d.join("cookies.txt"))
}

impl Auth {
    pub fn load() -> Option<Self> {
        parse(&fs::read_to_string(path()?).ok()?)
    }

    /// Headers that make an InnerTube request act as the signed-in account.
    pub fn headers(&self) -> [(&'static str, String); 3] {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let hash = sha1_smol::Sha1::from(format!("{ts} {} {ORIGIN}", self.sapisid)).digest().to_string();
        [
            ("Cookie", self.cookie.clone()),
            ("Authorization", format!("SAPISIDHASH {ts}_{hash}")),
            ("X-Goog-AuthUser", "0".into()),
        ]
    }
}

/// Netscape cookie jar → `Cookie` header for youtube.com, if it holds a sign-in.
fn parse(jar: &str) -> Option<Auth> {
    let mut cookie = Vec::new();
    let mut sapisid = None;
    for line in jar.lines() {
        // HttpOnly cookies are written as comments with this prefix.
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        let f: Vec<&str> = line.split('\t').collect();
        if line.starts_with('#') || f.len() != 7 || !f[0].ends_with("youtube.com") {
            continue;
        }
        let (name, value) = (f[5], f[6]);
        if name == "SAPISID" || (name == "__Secure-3PAPISID" && sapisid.is_none()) {
            sapisid = Some(value.to_string());
        }
        cookie.push(format!("{name}={value}"));
    }
    Some(Auth { sapisid: sapisid?, cookie: cookie.join("; ") })
}

/// First installed browser, in `BROWSERS` order.
fn detect() -> Option<&'static str> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    BROWSERS.iter().find(|(_, dir)| home.join(dir).is_dir()).map(|(name, _)| *name)
}

/// Create (or truncate) a file only you can read.
fn private(path: &Path) -> Result<fs::File> {
    Ok(OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?)
}

/// Copy the sign-in from `browser` (or the first one found). Returns the
/// browser used. Accepts anything yt-dlp's `--cookies-from-browser` does,
/// e.g. `chrome:Profile 1` or `firefox:default-release`.
pub fn login(browser: Option<String>) -> Result<(String, Auth)> {
    let browser = match browser {
        Some(b) => b,
        None => detect().context("no supported browser found; name one, e.g. `tuber --login firefox`")?.into(),
    };
    let path = path().context("no state directory")?;
    fs::create_dir_all(path.parent().unwrap())?;

    // yt-dlp dumps the browser's whole jar into --cookies when it exits, so
    // point it at a scratch file and keep only what tuber needs.
    let dump = path.with_extension("dump");
    // yt-dlp loads this file before dumping into it, and rejects an empty one.
    private(&dump)?.write_all(b"# Netscape HTTP Cookie File\n")?;
    let out = Command::new("yt-dlp")
        .args(["-q", "--no-warnings", "--skip-download", "--no-playlist", "--cookies-from-browser", &browser])
        .arg("--cookies")
        .arg(&dump)
        // Any video will do; yt-dlp needs one to run. This is YouTube's first upload.
        .arg("https://www.youtube.com/watch?v=jNQXAC9IVRw")
        .output()
        .context("running yt-dlp (is it installed?)");
    let jar = fs::read_to_string(&dump).unwrap_or_default();
    let _ = fs::remove_file(&dump);
    let out = out?;

    let kept: String = jar
        .lines()
        .filter(|l| {
            let domain = l.strip_prefix("#HttpOnly_").unwrap_or(l).split('\t').next().unwrap_or("");
            l.starts_with("# Netscape") || domain.ends_with("youtube.com") || domain.ends_with("google.com")
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let Some(auth) = parse(&kept) else {
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            // Last message, minus progress lines redrawn with \r.
            let err = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
            let err = err.rsplit('\r').next().unwrap_or("").trim_start_matches("yt-dlp: ");
            bail!("yt-dlp couldn't read {browser}'s cookies: {err}");
        }
        bail!("{browser} isn't signed in to YouTube; sign in at {ORIGIN} there, then run this again");
    };
    let tmp = path.with_extension("txt.tmp");
    private(&tmp)?.write_all(kept.as_bytes())?;
    fs::rename(tmp, &path)?;
    Ok((browser, auth))
}

/// Forget the sign-in. Returns whether there was one.
pub fn logout() -> Result<bool> {
    match path().map(fs::remove_file) {
        Some(Ok(())) => Ok(true),
        Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Some(Err(e)) => Err(e.into()),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jar() {
        let jar = "# Netscape HTTP Cookie File\n\
            .youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tf6=40000000\n\
            #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n\
            .youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\txyz/A\n\
            .google.com\tTRUE\t/\tTRUE\t0\tSAPISID\tnot-this\n";
        let auth = parse(jar).unwrap();
        assert_eq!(auth.cookie, "PREF=f6=40000000; SID=abc; SAPISID=xyz/A");
        assert_eq!(auth.sapisid, "xyz/A");
        assert!(auth.headers()[1].1.starts_with("SAPISIDHASH "));
        assert!(parse(".youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tx\n").is_none());
    }
}
