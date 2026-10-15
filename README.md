# tuber

A terminal YouTube Music player built with [ratatui](https://ratatui.rs). It works without an account, and can optionally use yours. It draws a braille audio visualizer and uses [ratatui-cheese](https://crates.io/crates/ratatui-cheese) (Charm-style) widgets and colors.

![tuber with search results, spectrum visualizer and waveform seekbar](docs/screenshot.png)

- **Search** uses YouTube Music's own InnerTube API, filtered to songs. It's anonymous unless you sign in.
- **Radio**: pressing enter on a song plays it and fills the queue with YouTube Music's auto-generated mix for that song.
- **Playback**: `yt-dlp` resolves the stream URL, `ffmpeg` decodes it to PCM, and `cpal` plays it.
- **Waveform seekbar**: shows the shape of the whole track (`▁▂▃▄▅▆▇█`) from a quick second ffmpeg pass. The played part takes a purple→pink gradient and the rest stays faint. The playhead is a full-height block that slides across in 1/8-column steps: a block growing in from the left (`▏▎▍▌▋▊▉█`) with the remainder shrinking out of the column behind it (`🮋🮊🮉▐🮈🮇▕`). It flashes on beats, and on each side the bars thin out toward it through two braille columns at the same height: a full one, then a dithered one (every other dot) right next to the playhead. The shrinking half needs Unicode 13 legacy-computing blocks; kitty, WezTerm, Ghostty, foot and Windows Terminal draw them natively.
- **MPRIS**: shows up in desktop now-playing widgets (Quickshell, Waybar, GNOME and KDE) with title, artist and cover art, but only once something plays. It disappears again when playback stops. Media keys and `playerctl` can control it.
- **Resume**: the queue, current track, position, volume and last search are saved locally to `~/.local/state/tuber/session.json`, with no account involved. On the next launch the session comes back paused where you left off; press space to continue.
- **Cover art**: the track's cover is drawn dimmed behind the visualizer with half blocks (`▀`, two pixels per character in 24-bit color), framed by a grey rounded border like the player panel, with the visualizer drawn on top. Video thumbnails have their letterbox bars trimmed first.
- **Visualizer** reads the exact samples sent to the speakers and draws them with braille dots on a ratatui `Canvas`. It has three modes: spectrum with peak caps, a mirrored waveform, and a triggered oscilloscope.

<details>
<summary>Fullscreen player, mirror visualizer</summary>

![tuber fullscreen player with mirrored waveform visualizer](docs/fullscreen.png)

</details>

## Requirements

`yt-dlp` and `ffmpeg` on your `PATH`, plus ALSA headers to build (`alsa-lib-devel` / `libasound2-dev`).
yt-dlp also needs a JavaScript runtime for YouTube's challenges; tuber lets it use deno, node, bun or quickjs, whichever is installed. Keep yt-dlp up to date. When YouTube changes how stream URLs are signed, updating yt-dlp is usually the fix.

## Run

```sh
cargo run --release                       # start at the search box
cargo run --release -- daft punk          # search straight away
```

## Sign in (optional)

```sh
tuber --login                     # try every browser profile, most recently used first
tuber --login chrome              # or every profile of one browser: chrome, firefox, brave, chromium, edge, vivaldi, opera
tuber --login 'chrome:Profile 1'  # or one profile
tuber --login chrome+gnomekeyring # if Chrome's cookies won't decrypt, name the keyring (gnomekeyring, kwallet, basictext)
tuber --logout
```

Sign in to [music.youtube.com](https://music.youtube.com) in your browser first. `--login` has yt-dlp copy that browser's YouTube cookies into `~/.local/state/tuber/cookies.txt`, readable only by you, and checks them by fetching your account name. While signed in:

- the header shows your account name,
- songs tuber plays for at least 10 seconds go into your YouTube Music history, like plays from the official apps,
- at launch, if your history's newest song was played on another device after tuber's last session, tuber offers it: press `c` to play it with a radio mix (from the start; YouTube doesn't sync the position within songs),
- `L` loads your liked songs,
- radio mixes and search are personalised to your account,
- yt-dlp gets the cookies too, for Premium audio quality and age-restricted tracks.

The cookies are as powerful as your browser's YouTube session, so treat the file like a password. Signing out of YouTube in the browser invalidates them; run `--login` again. If YouTube rejects them, search, radio and playback keep working anonymously. Chrome-family browsers keep the cookie key in your keyring, and yt-dlp reads GNOME Keyring through the Python `secretstorage` module (`python3-secretstorage` on Fedora and Debian), which some distro packages leave out; `--login` tells you if it's missing. The keyring may ask for permission the first time. Google no longer lets third-party apps use its device-code sign-in for YouTube Music, so borrowing the browser's session is the way that still works.

## Keys

| key | action |
| --- | --- |
| `/` | focus list + search |
| `enter` | results: play + start radio · queue: jump to track |
| `a` | add result to queue |
| `r` | start radio from selected |
| `d` | remove from queue |
| `tab` | switch results / queue |
| `f` | fullscreen the focused pane (list or player) |
| `esc` | exit fullscreen |
| `↑`/`↓` (`k`/`j`) | focus list, then move |
| `J`/`K`, `g`/`G` | page, top/bottom |
| `space` | pause |
| `n` / `p` | next / previous (restarts track if >3s in) |
| `←`/`→` (`h`/`l`) | focus player, then seek ±5s |
| `+` / `-` | volume (a circle and number show the level; the bar appears while you change it) |
| `v` | cycle visualizer mode |
| `L` | load your liked songs into results (signed in) |
| `c` | continue the song last played on another device (when offered) |
| `q` | quit |

Picking a song from the list moves focus to the player, and in fullscreen that switches the view to the player.
