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
const BROWSERS: [(&str, &str); 10] = [
    ("chrome", ".config/google-chrome"),
    ("firefox", ".mozilla/firefox"),
    ("firefox", ".config/mozilla/firefox"),
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
    /// `__Secure-1PAPISID` / `__Secure-3PAPISID`, which browsers also sign with.
    sapisid_1p: Option<String>,
    sapisid_3p: Option<String>,
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
    /// The Authorization value matches what music.youtube.com itself sends:
    /// one timestamped SHA-1 per APISID cookie.
    pub fn headers(&self) -> [(&'static str, String); 3] {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let hash = |id: &str| sha1_smol::Sha1::from(format!("{ts} {id} {ORIGIN}")).digest().to_string();
        let mut authorization = format!("SAPISIDHASH {ts}_{}", hash(&self.sapisid));
        for (scheme, id) in [("SAPISID1PHASH", &self.sapisid_1p), ("SAPISID3PHASH", &self.sapisid_3p)] {
            if let Some(id) = id {
                authorization += &format!(" {scheme} {ts}_{}", hash(id));
            }
        }
        [
            ("Cookie", self.cookie.clone()),
            ("Authorization", authorization),
            ("X-Goog-AuthUser", "0".into()),
        ]
    }
}

/// Netscape cookie jar → the `Cookie` header a browser would send to
/// music.youtube.com, if it holds a sign-in.
fn parse(jar: &str) -> Option<Auth> {
    let mut cookies: Vec<(String, String)> = Vec::new();
    for line in jar.lines() {
        // HttpOnly cookies are written as comments with this prefix.
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        let f: Vec<&str> = line.split('\t').collect();
        // Only cookies music.youtube.com gets: not www./accounts./m.youtube.com ones.
        if line.starts_with('#') || f.len() != 7 || !matches!(f[0], ".youtube.com" | "youtube.com" | "music.youtube.com")
        {
            continue;
        }
        let (name, value) = (f[5].to_string(), f[6].to_string());
        // One value per name; a more specific domain wins, as in a browser.
        match cookies.iter_mut().find(|(n, _)| *n == name) {
            Some(c) if f[0] == "music.youtube.com" => c.1 = value,
            Some(_) => {}
            None => cookies.push((name, value)),
        }
    }
    let get = |name: &str| cookies.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    let sapisid_3p = get("__Secure-3PAPISID");
    Some(Auth {
        sapisid: get("SAPISID").or_else(|| sapisid_3p.clone())?,
        sapisid_1p: get("__Secure-1PAPISID"),
        sapisid_3p,
        cookie: cookies.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; "),
    })
}

/// Profiles to try for a `--cookies-from-browser` spec, as specs. A spec
/// that names a profile is tried as is. Otherwise every profile of that
/// browser (every installed browser, with no spec) that has a cookie store,
/// most recently used first, since yt-dlp alone only reads one of them.
fn candidates(spec: Option<&str>) -> Vec<String> {
    if let Some(s) = spec.filter(|s| s.contains(':')) {
        return vec![s.to_string()];
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
    let mut found: Vec<(SystemTime, String)> = Vec::new();
    for (name, dir) in BROWSERS {
        // `chrome+gnomekeyring` keeps its keyring choice.
        let base = match spec {
            Some(s) if s.split('+').next() != Some(name) => continue,
            Some(s) => s,
            None => name,
        };
        let Ok(entries) = fs::read_dir(home.join(dir)) else { continue };
        for profile in entries.flatten() {
            let store = ["Cookies", "Network/Cookies", "cookies.sqlite"]
                .iter()
                .find_map(|f| fs::metadata(profile.path().join(f)).ok());
            if let Some(meta) = store {
                let used = meta.modified().unwrap_or(UNIX_EPOCH);
                found.push((used, format!("{base}:{}", profile.file_name().to_string_lossy())));
            }
        }
    }
    found.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
    let mut specs: Vec<String> = found.into_iter().map(|(_, s)| s).collect();
    if specs.is_empty()
        && let Some(s) = spec
    {
        specs.push(s.to_string()); // unknown layout; let yt-dlp pick
    }
    specs
}

/// A private copy of the cookie jar for one yt-dlp run, deleted on drop.
/// yt-dlp rewrites its --cookies file on exit, and runs overlap (stream
/// prefetch, play reports), so none of them gets the real jar.
pub struct JarCopy(PathBuf);

impl JarCopy {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for JarCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn jar_copy() -> Option<JarCopy> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let src = path()?;
    let jar = fs::read(&src).ok()?;
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let copy = JarCopy(src.with_extension(format!("{}-{n}.tmp", std::process::id())));
    private(copy.path()).ok()?.write_all(&jar).ok()?;
    Some(copy)
}

/// Create (or truncate) a file only you can read.
fn private(path: &Path) -> Result<fs::File> {
    Ok(OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?)
}

/// Ask yt-dlp for one browser profile's cookies. Returns the YouTube and
/// Google ones, plus yt-dlp's warnings (decryption and keyring problems
/// show up there rather than as errors).
fn export(spec: &str, dump: &Path) -> Result<(String, Vec<String>)> {
    // yt-dlp loads this file before dumping the browser's jar into it on
    // exit, and rejects an empty one.
    private(dump)?.write_all(b"# Netscape HTTP Cookie File\n")?;
    let out = Command::new("yt-dlp")
        .args(["-q", "--skip-download", "--no-playlist", "--cookies-from-browser", spec])
        .args(crate::yt::JS_RUNTIMES)
        .arg("--cookies")
        .arg(dump)
        // Any video will do; yt-dlp needs one to run. This is YouTube's first upload.
        .arg("https://www.youtube.com/watch?v=jNQXAC9IVRw")
        .output()
        .context("running yt-dlp (is it installed?)");
    let jar = fs::read_to_string(dump).unwrap_or_default();
    let _ = fs::remove_file(dump);
    let out = out?;
    let kept = jar
        .lines()
        .filter(|l| {
            let domain = l.strip_prefix("#HttpOnly_").unwrap_or(l).split('\t').next().unwrap_or("");
            l.starts_with("# Netscape") || domain.ends_with("youtube.com") || domain.ends_with("google.com")
        })
        .map(|l| format!("{l}\n"))
        .collect();
    // Progress lines are redrawn with \r; keep the last state of each.
    let mut notes: Vec<String> = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter_map(|l| l.rsplit('\r').next())
        .filter(|l| l.starts_with("ERROR") || l.contains("error:") || l.contains("cookie") || l.contains("keyring"))
        .map(|l| l.trim_start_matches("yt-dlp: ").to_string())
        .collect();
    notes.dedup();
    Ok((kept, notes))
}

/// Copy the sign-in from a browser. `spec` is anything yt-dlp's
/// `--cookies-from-browser` takes (`chrome`, `chrome:Profile 1`,
/// `chrome+gnomekeyring`, `firefox`); without one, every installed browser's
/// profiles are tried. Returns the profile used.
/// Keyrings yt-dlp can read Chrome's cookie key from. It guesses which one
/// from the desktop, and on desktops it doesn't know (Hyprland, sway, …)
/// it guesses none, so v11-encrypted cookies fail to decrypt.
const KEYRINGS: [&str; 3] = ["gnomekeyring", "kwallet6", "kwallet5"];

/// `chrome:Profile 1` → `chrome+gnomekeyring:Profile 1`.
fn with_keyring(spec: &str, keyring: &str) -> String {
    match spec.split_once(':') {
        Some((browser, profile)) => format!("{browser}+{keyring}:{profile}"),
        None => format!("{spec}+{keyring}"),
    }
}

pub fn login(spec: Option<String>, verify: impl Fn(&Auth) -> Result<String>) -> Result<(String, String)> {
    let path = path().context("no state directory")?;
    fs::create_dir_all(path.parent().unwrap())?;
    let dump = path.with_extension("dump");

    let specs = candidates(spec.as_deref());
    if specs.is_empty() {
        bail!("no supported browser profile found; name one, e.g. `tuber --login firefox`");
    }
    let mut report = Vec::new();
    for s in &specs {
        let (mut kept, mut notes) = export(s, &dump)?;
        let mut used = s.clone();
        // Undecryptable cookies: retry the profile with each keyring.
        if parse(&kept).is_none() && !s.contains('+') && notes.iter().any(|n| n.contains("cannot decrypt")) {
            notes.retain(|n| !n.contains("cannot decrypt"));
            for keyring in KEYRINGS {
                let attempt = with_keyring(s, keyring);
                let (k, n) = export(&attempt, &dump)?;
                if parse(&k).is_some() {
                    (kept, used) = (k, attempt);
                    break;
                }
                notes.extend(n.into_iter().map(|n| format!("with {keyring}: {n}")));
            }
        }
        // Cookies can outlive the session they belong to, so ask YouTube.
        match parse(&kept).map(|auth| verify(&auth)) {
            Some(Ok(name)) => {
                let tmp = path.with_extension("txt.tmp");
                private(&tmp)?.write_all(kept.as_bytes())?;
                fs::rename(tmp, &path)?;
                return Ok((used, name));
            }
            Some(Err(e)) => notes.push(format!("{e:#}")),
            None => {}
        }
        notes.dedup();
        // yt-dlp reads GNOME Keyring / KWallet through optional extras; name
        // the missing one rather than echoing every failed attempt.
        if notes.iter().any(|n| n.contains("secretstorage")) {
            notes = vec![
                "its cookies are locked by GNOME Keyring, which yt-dlp can't open without the Python \
                 `secretstorage` module: install python3-secretstorage (Fedora, Debian, Ubuntu) or \
                 python-secretstorage (Arch), or `pip install secretstorage` where yt-dlp's Python can see it"
                    .into(),
            ];
        }
        let why = if notes.is_empty() { "not signed in to YouTube".into() } else { notes.join("\n    ") };
        report.push(format!("  {s}: {why}"));
    }
    bail!(
        "no browser profile had a YouTube sign-in. Sign in at {ORIGIN}, or name the profile, \
         e.g. `tuber --login 'chrome:Profile 1'`.\n{}",
        report.join("\n")
    )
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
        let jar = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\ta\n\
            www.youtube.com\tFALSE\t/\tTRUE\t0\tSAPISID\tstale\n\
            .youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-3PAPISID\tc\n";
        let auth = parse(jar).unwrap();
        assert_eq!(auth.cookie, "SAPISID=a; __Secure-3PAPISID=c");
        let header = &auth.headers()[1].1;
        assert!(header.contains(" SAPISID3PHASH ") && !header.contains("SAPISID1PHASH"), "{header}");
        assert!(parse(".youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tx\n").is_none());
    }

    #[test]
    fn keyring_specs() {
        assert_eq!(with_keyring("chrome:Profile 1", "gnomekeyring"), "chrome+gnomekeyring:Profile 1");
        assert_eq!(with_keyring("brave", "kwallet6"), "brave+kwallet6");
    }
}
