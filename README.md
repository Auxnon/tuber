# tuber

A terminal YouTube Music player built with [ratatui](https://ratatui.rs). It needs no account or sign-in. It draws a braille audio visualizer and uses [ratatui-cheese](https://crates.io/crates/ratatui-cheese) (Charm-style) widgets and colors.

- **Search** uses YouTube Music's own InnerTube API, filtered to songs and called anonymously.
- **Radio**: pressing enter on a song plays it and fills the queue with YouTube Music's auto-generated mix for that song.
- **Playback**: `yt-dlp` resolves the stream URL, `ffmpeg` decodes it to PCM, and `cpal` plays it.
- **Visualizer** reads the exact samples sent to the speakers and draws them with braille dots on a ratatui `Canvas`. It has three modes: spectrum with peak caps, a mirrored waveform, and a triggered oscilloscope.

## Requirements

`yt-dlp` and `ffmpeg` on your `PATH`, plus ALSA headers to build (`alsa-lib-devel` / `libasound2-dev`).
Keep yt-dlp up to date. When YouTube changes how stream URLs are signed, updating yt-dlp is usually the fix.

## Run

```sh
cargo run --release                       # start at the search box
cargo run --release -- daft punk          # search straight away
```

## Keys

| key | action |
| --- | --- |
| `/` | search |
| `enter` | results: play + start radio · queue: jump to track |
| `a` | add result to queue |
| `r` | start radio from selected |
| `d` | remove from queue |
| `tab` | switch results / queue |
| `j`/`k`, `J`/`K`, `g`/`G` | move, page, top/bottom |
| `space` | pause |
| `n` / `p` | next / previous (restarts track if >3s in) |
| `←`/`→` (`h`/`l`) | seek ±5s |
| `+` / `-` | volume |
| `v` | cycle visualizer mode |
| `q` | quit |
