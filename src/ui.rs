use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
use ratatui_cheese::help::{Binding, Help, HelpStyles};
use ratatui_cheese::input::Input;
use ratatui_cheese::list::{List, ListItem, ListItemContext};
use ratatui_cheese::spinner::Spinner;

use crate::viz::{VizWidget, lerp};
use crate::yt::Track;
use crate::{App, Focus, Tab};

/// Two-line track row in the Bubbles list style: a coloured bar marks the selection.
struct Row<'a> {
    track: &'a Track,
    playing: bool,
    paused: bool,
}

impl ListItem for Row<'_> {
    fn height(&self) -> u16 {
        2
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &ListItemContext) {
        let p = &ctx.palette;
        let (bar, title, desc) = if ctx.selected {
            (Style::new().fg(p.primary), Style::new().fg(p.primary), Style::new().fg(p.secondary))
        } else {
            (Style::new(), Style::new().fg(p.foreground), Style::new().fg(p.muted))
        };
        let mark = match (self.playing, self.paused) {
            (true, false) => "♪ ",
            (true, true) => "‖ ",
            _ => "",
        };
        let t = self.track;
        let dur = t.duration.map(fmt_time).unwrap_or_default();
        let meta = if t.album.is_empty() { t.artist.clone() } else { format!("{} · {}", t.artist, t.album) };
        let text_w = area.width.saturating_sub(2) as usize;
        for (dy, line) in [
            Line::from(vec![Span::styled(mark, Style::new().fg(p.success)), Span::styled(t.title.as_str(), title)]),
            Line::from(Span::styled(truncate(&meta, text_w.saturating_sub(dur.len() + 1)), desc)),
        ]
        .into_iter()
        .enumerate()
        {
            let y = area.y + dy as u16;
            if ctx.selected {
                buf.set_string(area.x, y, "│", bar);
            }
            let inner = Rect::new(area.x + 2, y, area.width.saturating_sub(2), 1);
            line.render(inner, buf);
        }
        if !dur.is_empty() && area.width as usize > dur.len() + 4 {
            let x = area.right() - dur.len() as u16;
            buf.set_string(x, area.y + 1, &dur, Style::new().fg(p.faint));
        }
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let p = app.palette.clone();
    let area = f.area().inner(Margin::new(2, 1));
    let show_list = !app.fullscreen || app.focus == Focus::List;
    let show_player = !app.fullscreen || app.focus == Focus::Player;
    let [header, _, search, _, body, _, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(if show_list { 2 } else { 0 }),
        Constraint::Length(if show_list { 1 } else { 0 }),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // Header: badge, spinner, status.
    let [badge, spin, status] =
        Layout::horizontal([Constraint::Length(9), Constraint::Length(2), Constraint::Fill(1)]).areas(header);
    Paragraph::new(" ♫ tuber ").style(Style::new().fg(p.on_highlight).bg(p.secondary).bold()).render(badge, f.buffer_mut());
    let status_area = Rect { x: spin.x + 1, ..spin };
    if app.loading() {
        f.render_stateful_widget(Spinner::default().style(Style::new().fg(p.primary)), status_area, &mut app.spinner);
    }
    Paragraph::new(app.status.as_str()).fg(p.muted).render(status, f.buffer_mut());

    if show_list {
        let input = Input::new("Search YouTube Music").placeholder("artist, song, album…").prompt("› ").palette(&p);
        f.render_stateful_widget(input, search, &mut app.input);
    }

    if show_list && show_player {
        let [left, _, right] =
            Layout::horizontal([Constraint::Percentage(42), Constraint::Length(2), Constraint::Fill(1)]).areas(body);
        draw_lists(f, app, left);
        draw_player(f, app, right);
    } else if show_list {
        draw_lists(f, app, body);
    } else {
        draw_player(f, app, body);
    }

    let bindings = if app.input.focused() {
        vec![Binding::new("enter", "search"), Binding::new("esc", "cancel")]
    } else {
        let mut b = vec![Binding::new("/", "search")];
        if show_list {
            b.push(Binding::new("enter", if app.tab == Tab::Results { "play + radio" } else { "play" }));
            if app.tab == Tab::Results {
                b.push(Binding::new("a", "enqueue"));
            } else {
                b.push(Binding::new("d", "remove"));
            }
        }
        if app.fullscreen {
            b.push(Binding::new("f/esc", "exit fullscreen"));
        } else {
            b.push(Binding::new("f", "fullscreen"));
        }
        b.extend([
            Binding::new("space", "pause"),
            Binding::new("n/p", "next/prev"),
            if app.focus == Focus::List { Binding::new("←/→", "player") } else { Binding::new("←/→", "seek") },
            if app.focus == Focus::List { Binding::new("↑/↓", "move") } else { Binding::new("↑/↓", "list") },
            Binding::new("+/-", "volume"),
            Binding::new("v", "visualizer"),
            Binding::new("tab", "results/queue"),
            Binding::new("q", "quit"),
        ]);
        b
    };
    Help::default().bindings(bindings).styles(HelpStyles::from_palette(&p)).render(help, f.buffer_mut());
}

fn draw_lists(f: &mut Frame, app: &mut App, area: Rect) {
    // Unfocused: same layout, accents dimmed.
    let mut p = app.palette.clone();
    if app.focus != Focus::List {
        p.primary = p.muted;
        p.secondary = p.faint;
    }
    let p = &p;
    let [tabs, _, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    let tab = |label: String, active: bool| {
        if active {
            Span::styled(label, Style::new().fg(p.on_highlight).bg(p.primary).bold())
        } else {
            Span::styled(label, Style::new().fg(p.muted))
        }
    };
    Line::from(vec![
        tab(format!(" Results {} ", app.results.len()), app.tab == Tab::Results),
        Span::raw("  "),
        tab(format!(" Queue {} ", app.queue.len()), app.tab == Tab::Queue),
    ])
    .render(tabs, f.buffer_mut());

    let paused = app.player.is_paused();
    let playing_id = app.now_playing().map(|t| t.id.clone());
    let (tracks, state, is_queue) = match app.tab {
        Tab::Results => (&app.results, &mut app.results_state, false),
        Tab::Queue => (&app.queue, &mut app.queue_state, true),
    };
    if tracks.is_empty() {
        let hint = if is_queue { "Queue is empty — enter on a result starts a radio" } else { "Press / to search" };
        Paragraph::new(hint).fg(p.faint).render(list_area.inner(Margin::new(2, 0)), f.buffer_mut());
        return;
    }
    let rows: Vec<Row> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| Row {
            track: t,
            playing: if is_queue { app.current == Some(i) } else { playing_id.as_deref() == Some(&t.id) },
            paused,
        })
        .collect();
    f.render_stateful_widget(List::new(&rows).palette(p.clone()).selection_indicator(""), list_area, state);
}

fn draw_player(f: &mut Frame, app: &mut App, area: Rect) {
    let p = app.palette.clone();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if app.focus == Focus::Player { p.primary } else { p.border }))
        .title(Line::from(Span::styled(
            " now playing ",
            Style::new().fg(if app.focus == Focus::Player { p.primary } else { p.muted }),
        )))
        .title_bottom(
            Line::from(Span::styled(format!(" {} · v ", app.viz.mode.label()), Style::new().fg(p.faint))).right_aligned(),
        );
    let inner = block.inner(area).inner(Margin::new(2, 1));
    block.render(area, f.buffer_mut());

    let [title, meta, _, viz_area, _, progress, info] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let track = app.now_playing().cloned();
    match &track {
        Some(t) => {
            Paragraph::new(t.title.as_str()).style(Style::new().fg(p.foreground).bold()).render(title, f.buffer_mut());
            let m = if t.album.is_empty() { t.artist.clone() } else { format!("{} · {}", t.artist, t.album) };
            Paragraph::new(m).fg(p.secondary).render(meta, f.buffer_mut());
        }
        None => Paragraph::new("nothing playing").fg(p.faint).render(title, f.buffer_mut()),
    }

    app.player.tap(&mut app.tap);
    let live = app.player.is_active() && !app.player.is_paused();
    app.viz.update(&app.tap, app.player.rate, viz_area.width as usize, live);
    VizWidget { viz: &app.viz, from: p.secondary, to: p.primary, peak: Color::Rgb(0xFF, 0xD1, 0xFC) }
        .render(viz_area, f.buffer_mut());

    seekbar(app, track.as_ref(), progress, f.buffer_mut());

    let state = if !app.player.is_active() {
        "■ stopped"
    } else if app.player.is_paused() {
        "‖ paused"
    } else if app.player.buffering() {
        "… buffering"
    } else {
        "▶ playing"
    };
    let vol = (app.player.volume() * 100.0).round() as u32;
    let vol_bar: String = (0..10).map(|i| if i * 10 < vol { '▮' } else { '▯' }).collect();
    Line::from(vec![
        Span::styled(state, Style::new().fg(p.success)),
        Span::styled("   vol ", Style::new().fg(p.faint)),
        Span::styled(vol_bar, Style::new().fg(p.secondary)),
        Span::styled(format!(" {vol}%"), Style::new().fg(p.muted)),
    ])
    .render(info, f.buffer_mut());
}

const BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Playhead: a vertical line that widens into a full block across its cell.
const HEAD: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// Waveform seekbar: `1:23 ▂▃▅▇▆▅▃●▄▃▂▂▃▄▆▇▆▅ 7:10`.
/// Column heights come from the whole-track loudness envelope. The played part
/// takes the visualizer's purple→pink gradient, and the unplayed part stays dim.
/// The playhead grows ▏→█ as playback crosses its cell (8 steps per column)
/// and flashes on beats.
fn seekbar(app: &App, track: Option<&Track>, area: Rect, buf: &mut Buffer) {
    let p = &app.palette;
    let env = app.envelope.lock().unwrap();
    let env_ok = track.is_some_and(|t| t.id == env.id);
    let active = app.player.is_active();
    let pos = if active { app.player.position() } else { 0.0 };
    let total = track
        .and_then(|t| t.duration)
        .map(|d| d as f64)
        .or_else(|| (env_ok && env.done).then(|| env.secs()))
        .unwrap_or(0.0);

    let (l, r) = (fmt_time(pos as u64), if total > 0.0 { fmt_time(total as u64) } else { "--:--".into() });
    let w = (area.width as usize).saturating_sub(l.len() + r.len() + 2);
    if w == 0 {
        return;
    }
    buf.set_string(area.x, area.y, &l, Style::new().fg(p.muted));
    buf.set_string(area.right() - r.len() as u16, area.y, &r, Style::new().fg(p.muted));

    let x0 = area.x + l.len() as u16 + 1;
    let progress = if total > 0.0 { (pos / total).clamp(0.0, 1.0) * w as f64 } else { 0.0 };
    let head = (progress as usize).min(w - 1);
    let sub = progress.fract() as f32;
    let col_secs = total / w as f64;
    // Stretch this track's own quietest..loudest column range over the 8 block
    // heights; normalising to the absolute peak flattens mastered music.
    let means: Vec<Option<f32>> = (0..w)
        .map(|c| (env_ok && total > 0.0).then(|| env.mean(c as f64 * col_secs, (c + 1) as f64 * col_secs)).flatten())
        .collect();
    let (lo, hi) = means.iter().flatten().fold((f32::MAX, 0.0f32), |(lo, hi), &m| (lo.min(m), hi.max(m)));
    let span = (hi - lo).max(hi * 0.05).max(1e-6);
    for (c, mean) in means.iter().enumerate() {
        let x = x0 + c as u16;
        if active && c == head {
            let head_glyph = HEAD[((sub * 8.0) as usize).min(7)];
            let color = lerp(p.primary, Color::Rgb(0xFF, 0xF0, 0xFF), app.viz.flash * 0.85);
            buf.set_string(x, area.y, head_glyph, Style::new().fg(color).bold());
            continue;
        }
        let glyph = mean.map_or("·", |m| BLOCKS[(((m - lo) / span * 7.0).round() as usize).min(7)]);
        let played = lerp(p.secondary, p.primary, c as f32 / (w - 1).max(1) as f32);
        let color = match c {
            _ if !active => p.faint,
            c if c < head => played,
            _ => p.faint,
        };
        buf.set_string(x, area.y, glyph, Style::new().fg(color));
    }
}

fn fmt_time(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}
