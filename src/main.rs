mod art;
mod auth;
mod envelope;
mod mpris;
mod player;
mod session;
mod ui;
mod viz;
mod yt;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_cheese::input::InputState;
use ratatui_cheese::list::ListState;
use ratatui_cheese::spinner::{SpinnerState, SpinnerType};
use ratatui_cheese::theme::Palette;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use envelope::Envelope;
use mpris::Mpris;
use souvlaki::{MediaControlEvent, SeekDirection};
use player::Player;
use viz::Visualizer;
use yt::{Rating, Track};

const FRAME: Duration = Duration::from_millis(16);
const SEEK_STEP: f64 = 5.0;
/// Local history length.
const PLAYED_MAX: usize = 200;
/// How long the launch question waits for the account's history.
const ASK_WAIT: Duration = Duration::from_secs(8);
/// How long the volume bar stays up after the last change.
const VOL_HOLD: Duration = Duration::from_secs(2);
/// How long "playing" / "paused" stay spelled out before only the icon is left.
const STATE_HOLD: Duration = Duration::from_secs(2);

enum Msg {
    Search(Result<Vec<Track>>),
    Radio { seed: String, res: Result<Vec<Track>> },
    Url { id: String, res: Result<String> },
    Media(MediaControlEvent),
    Account(Result<String>),
    Liked(Result<Vec<Track>>),
    History(Result<Vec<Track>>),
    /// History for `h`, as opposed to the launch-time check in `History`.
    Recent(Result<Vec<Track>>),
    Cover { id: String, res: Result<image::RgbImage> },
    Rating { id: String, res: Result<Rating> },
    /// A rating change went through, or failed and `prev` should come back.
    Rated { id: String, prev: Option<Rating>, res: Result<()> },
}

/// The launch question: pick up where you left off? Nothing is buffered
/// until it's answered.
pub enum Ask {
    /// Signed in: finding out whether another device played something since.
    Checking(Instant),
    /// This computer's last session: the track and position.
    Resume(Track, f64),
    /// What another device played last.
    Remote(Track),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Player,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Results,
    Queue,
}

pub struct App {
    pub palette: Palette,
    pub input: InputState,
    pub results: Vec<Track>,
    pub results_state: ListState,
    pub queue: Vec<Track>,
    pub queue_state: ListState,
    pub current: Option<usize>,
    pub tab: Tab,
    pub focus: Focus,
    /// Show only the focused pane.
    pub fullscreen: bool,
    pub player: Player,
    pub viz: Visualizer,
    pub tap: Vec<f32>,
    pub envelope: Arc<Mutex<Envelope>>,
    pub status: String,
    pub busy: usize,
    /// Signed-in account name, once YouTube confirms it.
    pub account: Option<String>,
    /// Now playing track's cover, once downloaded.
    pub cover: Option<(String, image::RgbImage)>,
    /// Track whose cover was last requested, so each is fetched once.
    cover_for: Option<String>,
    /// The launch question, while it's open.
    pub ask: Option<Ask>,
    /// The account's ratings of tracks seen so far, by video id.
    pub ratings: HashMap<String, Rating>,
    /// Track whose rating was last requested, so each is fetched once.
    rating_for: Option<String>,
    /// Show the key bindings (`?`).
    pub show_help: bool,
    /// Track to add to history once it has really played.
    report: Option<String>,
    /// Tracks played here, newest first, for `h` when not signed in.
    played: Vec<Track>,
    pub spinner: SpinnerState,
    urls: HashMap<String, String>,
    resolving: HashSet<String>,
    pending: Option<String>,
    mpris: Option<Mpris>,
    /// Restored session track to start paused at this position once its URL resolves.
    resume: Option<(String, f64)>,
    last_save: Instant,
    /// Volume as of the last frame, to notice changes from keys or MPRIS.
    vol_seen: f32,
    /// When the volume last changed; the full bar shows for `VOL_HOLD` after.
    vol_changed: Option<Instant>,
    /// Playback state word last drawn, and when it changed.
    state_seen: Option<&'static str>,
    state_changed: Instant,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
}

impl App {
    fn new(player: Player) -> Self {
        let (tx, rx) = channel();
        let mut input = InputState::new();
        input.set_focused(true);
        Self {
            palette: Palette::charm(),
            input,
            results: Vec::new(),
            results_state: ListState::new(0),
            queue: Vec::new(),
            queue_state: ListState::new(0),
            current: None,
            tab: Tab::Results,
            focus: Focus::List,
            fullscreen: false,
            player,
            viz: Visualizer::new(),
            tap: Vec::new(),
            envelope: Arc::default(),
            status: "no sign-in needed — type to search".into(),
            busy: 0,
            account: None,
            ask: None,
            cover: None,
            cover_for: None,
            report: None,
            played: Vec::new(),
            ratings: HashMap::new(),
            rating_for: None,
            show_help: false,
            spinner: SpinnerState::new(SpinnerType::Dot),
            urls: HashMap::new(),
            resolving: HashSet::new(),
            pending: None,
            mpris: None,
            resume: None,
            last_save: Instant::now(),
            vol_seen: f32::NAN,
            vol_changed: None,
            state_seen: None,
            state_changed: Instant::now(),
            tx,
            rx,
            quit: false,
        }
    }

    pub fn now_playing(&self) -> Option<&Track> {
        self.current.and_then(|i| self.queue.get(i))
    }

    pub fn loading(&self) -> bool {
        self.busy > 0 || self.pending.is_some() || self.player.buffering()
    }

    fn spawn<F: FnOnce() -> Msg + Send + 'static>(&self, f: F) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
    }

    fn search(&mut self, query: String) {
        self.busy += 1;
        self.status = format!("searching “{query}”");
        self.spawn(move || Msg::Search(yt::search(&query)));
    }

    fn liked(&mut self) {
        if !yt::signed_in() {
            self.status = "liked songs need a sign-in: run `tuber --login`".into();
            return;
        }
        self.busy += 1;
        self.status = "loading liked songs".into();
        self.spawn(|| Msg::Liked(yt::liked()));
    }

    /// Toggle a like or dislike on the playing track. Disliking also skips
    /// it, as the official apps do.
    fn rate(&mut self, rating: Rating) {
        if !yt::signed_in() {
            self.status = "likes need a sign-in: run `tuber --login`".into();
            return;
        }
        let Some((id, title)) = self.now_playing().map(|t| (t.id.clone(), t.title.clone())) else { return };
        let prev = self.ratings.get(&id).copied();
        let new = if prev == Some(rating) { Rating::Neutral } else { rating };
        self.ratings.insert(id.clone(), new);
        self.status = match new {
            Rating::Like => format!("liked “{title}”"),
            Rating::Dislike => format!("disliked “{title}”"),
            Rating::Neutral => format!("cleared rating of “{title}”"),
        };
        self.spawn(move || Msg::Rated { res: yt::rate(&id, new), id, prev });
        if new == Rating::Dislike {
            self.next();
        }
    }

    /// Recently played: the account's history (every device) when signed in,
    /// otherwise what was played here.
    fn recent(&mut self) {
        if yt::signed_in() {
            self.busy += 1;
            self.status = "loading history".into();
            self.spawn(|| Msg::Recent(yt::history()));
        } else {
            self.show_results("↺ recently played", self.played.clone());
        }
    }

    /// Put a list (liked songs, history) where search results go.
    fn show_results(&mut self, label: &str, tracks: Vec<Track>) {
        self.status = match tracks.len() {
            1 => "1 song".into(),
            n => format!("{n} songs"),
        };
        self.input.set_value(label.into());
        self.results_state = ListState::new(tracks.len());
        self.results = tracks;
        self.tab = Tab::Results;
        self.focus = Focus::List;
    }

    fn resolve(&mut self, id: &str) {
        if self.urls.contains_key(id) || !self.resolving.insert(id.to_string()) {
            return;
        }
        let id = id.to_string();
        self.spawn(move || Msg::Url { res: yt::stream_url(&id), id });
    }

    fn play_index(&mut self, i: usize) {
        let Some(track) = self.queue.get(i).cloned() else { return };
        if self.resume.as_ref().is_some_and(|(id, _)| *id != track.id) {
            self.resume = None;
        }
        self.current = Some(i);
        self.queue_state.select(i, self.queue.len());
        self.status = format!("{} — {}", track.title, track.artist);
        match self.urls.get(&track.id) {
            Some(url) => {
                self.pending = None;
                self.start(&track.id, url.clone());
            }
            None => {
                self.player.stop();
                self.pending = Some(track.id.clone());
                self.resolve(&track.id);
            }
        }
        // Prefetch the next stream URL so auto-advance is instant.
        if let Some(next) = self.queue.get(i + 1).map(|t| t.id.clone()) {
            self.resolve(&next);
        }
    }

    fn start(&mut self, id: &str, url: String) {
        envelope::analyse(&self.envelope, id, &url);
        self.report = Some(id.to_string());
        match self.resume.take() {
            Some((resume_id, at)) if resume_id == id => self.player.play(url, at),
            _ => self.player.play(url, 0.0),
        }
    }

    fn restore(&mut self, s: session::Session, keep_search: bool) {
        self.player.set_volume(s.volume);
        self.played = s.played;
        if keep_search {
            self.input.set_value(s.query);
            self.results_state = ListState::new(s.results.len());
            self.results = s.results;
        }
        self.queue_state = ListState::new(s.queue.len());
        self.queue = s.queue;
        if let Some(track) = s.current.and_then(|i| self.queue.get(i)).cloned() {
            let i = s.current.unwrap();
            self.current = Some(i);
            self.queue_state.select(i, self.queue.len());
            self.resume = Some((track.id.clone(), s.position));
            self.input.set_focused(false);
            self.focus = Focus::Player;
        } else if !self.results.is_empty() {
            self.input.set_focused(false);
        }
    }

    fn snapshot(&self) -> session::Session {
        let position = match (&self.resume, self.player.is_active()) {
            (Some((_, at)), _) => *at,
            (None, true) => self.player.position(),
            (None, false) => 0.0,
        };
        session::Session {
            query: self.input.value().to_string(),
            results: self.results.clone(),
            queue: self.queue.clone(),
            current: self.current,
            position,
            volume: self.player.volume(),
            played: self.played.clone(),
        }
    }

    /// The account's most recent play, if it happened somewhere else after
    /// tuber's session: tuber reports its own plays, so if the newest history
    /// entry isn't anything this session played, it's from another device
    /// (or the browser).
    fn played_elsewhere(&self, history: Vec<Track>) -> Option<Track> {
        let last = history.into_iter().next()?;
        let played_here = self.current.and_then(|c| self.queue.get(..=c)).unwrap_or_default();
        (!played_here.iter().any(|t| t.id == last.id)).then_some(last)
    }

    /// The question about this computer's last session, if there is one.
    fn ask_local(&self) -> Option<Ask> {
        let (id, at) = self.resume.as_ref()?;
        let track = self.now_playing().filter(|t| t.id == *id)?;
        Some(Ask::Resume(track.clone(), *at))
    }

    fn answer(&mut self, yes: bool) {
        match self.ask.take() {
            Some(Ask::Remote(t)) if yes => {
                self.start_radio(t);
                self.focus = Focus::Player;
            }
            // Not that one; maybe this computer's session instead.
            Some(Ask::Remote(_)) => self.ask = self.ask_local(),
            Some(Ask::Resume(..)) if yes => {
                if let Some(i) = self.current {
                    self.play_index(i);
                }
                self.focus = Focus::Player;
            }
            Some(Ask::Resume(..) | Ask::Checking(_)) => {
                self.resume = None;
                self.current = None;
                self.status = "type to search".into();
            }
            None => {}
        }
    }

    /// Replace the queue with this track followed by its YouTube Music radio.
    fn start_radio(&mut self, track: Track) {
        let seed = track.id.clone();
        self.queue = vec![track];
        self.queue_state = ListState::new(1);
        self.play_index(0);
        self.busy += 1;
        self.spawn(move || Msg::Radio { res: yt::radio(&seed), seed });
    }

    fn enqueue(&mut self, track: Track) {
        self.status = format!("queued {}", track.title);
        self.queue.push(track);
        self.queue_state = rebuilt(&self.queue_state, self.queue.len());
        let last = self.queue.len() - 1;
        if !self.player.is_active() && self.pending.is_none() {
            self.play_index(last);
        } else if self.current == Some(last - 1) {
            let id = self.queue[last].id.clone();
            self.resolve(&id);
        }
    }

    fn next(&mut self) {
        match self.current {
            Some(i) if i + 1 < self.queue.len() => self.play_index(i + 1),
            _ => {
                self.player.stop();
                self.status = "end of queue".into();
            }
        }
    }

    fn prev(&mut self) {
        match self.current {
            Some(i) if self.player.position() < 3.0 && i > 0 => self.play_index(i - 1),
            Some(_) => self.player.seek_to(0.0),
            None => {}
        }
    }

    fn remove_from_queue(&mut self, i: usize) {
        if i >= self.queue.len() {
            return;
        }
        self.queue.remove(i);
        self.queue_state = rebuilt(&self.queue_state, self.queue.len());
        match self.current {
            Some(c) if c == i => {
                self.player.stop();
                self.pending = None;
                self.current = None;
                if i < self.queue.len() {
                    self.play_index(i);
                }
            }
            Some(c) if c > i => self.current = Some(c - 1),
            _ => {}
        }
    }

    fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Search(res) => {
                self.busy -= 1;
                match res {
                    Ok(tracks) => {
                        self.status = match tracks.len() {
            1 => "1 song".into(),
            n => format!("{n} songs"),
        };
                        self.results_state = ListState::new(tracks.len());
                        self.results = tracks;
                        self.tab = Tab::Results;
                    }
                    Err(e) => self.status = format!("search failed: {e:#}"),
                }
            }
            Msg::Radio { seed, res } => {
                self.busy -= 1;
                if self.queue.first().map(|t| &t.id) != Some(&seed) {
                    return; // queue was replaced meanwhile
                }
                match res {
                    Ok(tracks) => {
                        self.queue.extend(tracks.into_iter().filter(|t| t.id != seed));
                        self.queue_state = rebuilt(&self.queue_state, self.queue.len());
                        if let Some(next) = self.current.and_then(|c| self.queue.get(c + 1)).map(|t| t.id.clone()) {
                            self.resolve(&next);
                        }
                    }
                    Err(e) => self.status = format!("radio failed: {e:#}"),
                }
            }
            Msg::Liked(res) => {
                self.busy -= 1;
                match res {
                    Ok(tracks) => self.show_results("♥ liked songs", tracks),
                    Err(e) => self.status = format!("liked songs failed: {e:#}"),
                }
            }
            Msg::Cover { id, res } => {
                if let Ok(img) = res {
                    self.cover = Some((id, img));
                }
            }
            Msg::Rating { id, res } => {
                // A rating set meanwhile with `l`/`L` wins over the fetched one.
                if let Ok(r) = res {
                    self.ratings.entry(id).or_insert(r);
                }
            }
            Msg::Rated { id, prev, res } => {
                if let Err(e) = res {
                    match prev {
                        Some(r) => self.ratings.insert(id, r),
                        None => self.ratings.remove(&id),
                    };
                    self.status = format!("rating failed: {e:#}");
                }
            }
            Msg::Recent(res) => {
                self.busy -= 1;
                match res {
                    Ok(tracks) => self.show_results("↺ recently played", tracks),
                    Err(e) => {
                        self.show_results("↺ recently played here", self.played.clone());
                        self.status = format!("account history failed, showing this computer's: {e:#}");
                    }
                }
            }
            Msg::History(res) => {
                // Only matters while the launch question is waiting on it.
                if matches!(self.ask, Some(Ask::Checking(_))) {
                    self.ask = match res {
                        Ok(history) => self.played_elsewhere(history).map(Ask::Remote).or_else(|| self.ask_local()),
                        Err(e) => {
                            self.status = format!("history failed: {e:#}");
                            self.ask_local()
                        }
                    };
                }
            }
            Msg::Account(res) => match res {
                Ok(name) => self.account = Some(name),
                Err(e) => self.status = format!("{e:#}"),
            },
            Msg::Media(event) => self.handle_media(event),
            Msg::Url { id, res } => {
                self.resolving.remove(&id);
                match res {
                    Ok(url) => {
                        self.urls.insert(id.clone(), url.clone());
                        if self.pending.as_ref() == Some(&id) {
                            self.pending = None;
                            self.start(&id, url);
                        }
                    }
                    Err(e) => {
                        self.status = format!("can't play: {e:#}");
                        if self.pending.as_ref() == Some(&id) {
                            self.pending = None;
                            self.next();
                        }
                    }
                }
            }
        }
    }

    /// Media keys, desktop widgets and `playerctl`, via MPRIS.
    fn handle_media(&mut self, event: MediaControlEvent) {
        let p = &mut self.player;
        let step = |dir| if dir == SeekDirection::Forward { 1.0 } else { -1.0 };
        match event {
            MediaControlEvent::Play if p.is_paused() => p.toggle_pause(),
            MediaControlEvent::Pause if !p.is_paused() => p.toggle_pause(),
            MediaControlEvent::Toggle => p.toggle_pause(),
            MediaControlEvent::Next => self.next(),
            MediaControlEvent::Previous => self.prev(),
            MediaControlEvent::Stop => {
                p.stop();
                self.pending = None;
            }
            MediaControlEvent::Seek(dir) => p.seek_to(p.position() + step(dir) * SEEK_STEP),
            MediaControlEvent::SeekBy(dir, by) => p.seek_to(p.position() + step(dir) * by.as_secs_f64()),
            MediaControlEvent::SetPosition(pos) => p.seek_to(pos.0.as_secs_f64()),
            MediaControlEvent::SetVolume(v) => p.set_volume(v as f32),
            MediaControlEvent::Quit => self.quit = true,
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some(ask) = &self.ask {
            let checking = matches!(ask, Ask::Checking(_));
            match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter if !checking => self.answer(true),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.answer(false),
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            return;
        }
        if self.input.focused() {
            match key.code {
                KeyCode::Enter => {
                    let q = self.input.value().trim().to_string();
                    if !q.is_empty() {
                        self.search(q);
                        self.input.set_focused(false);
                    }
                }
                KeyCode::Esc => self.input.set_focused(false),
                KeyCode::Backspace => self.input.delete_before(),
                KeyCode::Delete => self.input.delete_at(),
                KeyCode::Left => self.input.move_left(),
                KeyCode::Right => self.input.move_right(),
                KeyCode::Home => self.input.home(),
                KeyCode::End => self.input.end(),
                KeyCode::Char(c) => self.input.insert_char(c),
                _ => {}
            }
            return;
        }

        let on_list = self.focus == Focus::List;
        let list_visible = on_list || !self.fullscreen;
        let (list_len, state) = match self.tab {
            Tab::Results => (self.results.len(), &mut self.results_state),
            Tab::Queue => (self.queue.len(), &mut self.queue_state),
        };
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('f') => self.fullscreen = !self.fullscreen,
            KeyCode::Esc => self.fullscreen = false,
            KeyCode::Char('/') | KeyCode::Char('s') => {
                self.focus = Focus::List;
                self.input.set_value(String::new());
                self.input.set_focused(true);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = Focus::List;
                self.tab = if self.tab == Tab::Results { Tab::Queue } else { Tab::Results };
            }
            // Arrows first move focus to the pane they belong to, then act within it.
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Up | KeyCode::Char('k') if !on_list => {
                self.focus = Focus::List;
            }
            KeyCode::Left | KeyCode::Right if on_list => {
                self.focus = Focus::Player;
            }
            KeyCode::Down | KeyCode::Char('j') => state.select_next(list_len, false),
            KeyCode::Up | KeyCode::Char('k') => state.select_prev(list_len, false),
            KeyCode::PageDown | KeyCode::Char('J') => state.next_page(list_len),
            KeyCode::PageUp | KeyCode::Char('K') => state.prev_page(list_len),
            KeyCode::Char('g') => state.select(0, list_len),
            KeyCode::Char('G') => state.select(list_len.saturating_sub(1), list_len),
            KeyCode::Enter if list_visible => {
                let sel = state.selected();
                match self.tab {
                    Tab::Results => {
                        if let Some(t) = self.results.get(sel).cloned() {
                            self.start_radio(t);
                        }
                    }
                    Tab::Queue => self.play_index(sel),
                }
                self.focus = Focus::Player;
            }
            KeyCode::Char('a') if self.tab == Tab::Results && list_visible => {
                if let Some(t) = self.results.get(self.results_state.selected()).cloned() {
                    self.enqueue(t);
                }
            }
            KeyCode::Char('r') if list_visible => {
                let sel = state.selected();
                let list = if self.tab == Tab::Results { &self.results } else { &self.queue };
                if let Some(t) = list.get(sel).cloned() {
                    self.start_radio(t);
                }
            }
            KeyCode::Char('d') | KeyCode::Char('x') if self.tab == Tab::Queue && list_visible => {
                self.remove_from_queue(self.queue_state.selected());
            }
            KeyCode::Char(' ') => self.player.toggle_pause(),
            KeyCode::Char('n') => self.next(),
            KeyCode::Char('p') => self.prev(),
            KeyCode::Right => self.player.seek_to(self.player.position() + SEEK_STEP),
            KeyCode::Left => self.player.seek_to(self.player.position() - SEEK_STEP),
            KeyCode::Char('+') | KeyCode::Char('=') => self.player.set_volume(self.player.volume() + 0.05),
            KeyCode::Char('-') => self.player.set_volume(self.player.volume() - 0.05),
            KeyCode::Char('v') => self.viz.mode = self.viz.mode.next(),
            KeyCode::Char('l') => self.rate(Rating::Like),
            KeyCode::Char('L') => self.rate(Rating::Dislike),
            KeyCode::Char('F') => self.liked(),
            KeyCode::Char('h') => self.recent(),
            KeyCode::Char('?') => self.show_help = !self.show_help,
            _ => {}
        }
    }

    /// Volume changed within the last `VOL_HOLD`.
    pub fn adjusting_volume(&self) -> bool {
        self.vol_changed.is_some_and(|t| t.elapsed() < VOL_HOLD)
    }

    /// Whether to spell out the playback state: playing and paused only
    /// show their word for `STATE_HOLD` after changing to them.
    pub fn show_state(&mut self, word: &'static str) -> bool {
        if self.state_seen != Some(word) {
            self.state_seen = Some(word);
            self.state_changed = Instant::now();
        }
        !matches!(word, "playing" | "paused") || self.state_changed.elapsed() < STATE_HOLD
    }

    fn tick(&mut self, dt: Duration) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle_msg(msg);
        }
        if self.player.finished() {
            self.next();
        }
        if let Some(Ask::Checking(since)) = self.ask
            && since.elapsed() >= ASK_WAIT
        {
            self.ask = self.ask_local();
        }
        let vol = self.player.volume();
        if vol != self.vol_seen {
            // The first frame only records the restored volume.
            if !self.vol_seen.is_nan() {
                self.vol_changed = Some(Instant::now());
            }
            self.vol_seen = vol;
        }
        if let Some(t) = self.now_playing()
            && self.cover_for.as_ref() != Some(&t.id)
        {
            let (id, art) = (t.id.clone(), t.art.clone());
            self.cover_for = Some(id.clone());
            if let Some(url) = art {
                self.spawn(move || Msg::Cover { res: art::fetch(&url), id });
            }
        }
        if yt::signed_in()
            && let Some(t) = self.now_playing()
            && self.rating_for.as_ref() != Some(&t.id)
        {
            let id = t.id.clone();
            self.rating_for = Some(id.clone());
            self.spawn(move || Msg::Rating { res: yt::rating(&id), id });
        }
        // Report a play after 10s, so skipped tracks don't land in history.
        let p = &self.player;
        if self.report.is_some() && p.is_active() && !p.is_paused() && p.position() >= 10.0 {
            let id = self.report.take().unwrap();
            if let Some(t) = self.now_playing().filter(|t| t.id == id).cloned() {
                self.played.retain(|p| p.id != id);
                self.played.insert(0, t);
                self.played.truncate(PLAYED_MAX);
            }
            if yt::signed_in() {
                thread::spawn(move || yt::report_play(&id));
            }
        }
        self.spinner.tick(dt);
        if self.last_save.elapsed() >= Duration::from_secs(5) {
            let _ = session::save(&self.snapshot());
            self.last_save = Instant::now();
        }
        if let Some(mpris) = &mut self.mpris {
            let track = self.current.and_then(|i| self.queue.get(i));
            let p = &self.player;
            // A restored session waiting on its stream URL counts as paused, not playing.
            let paused = p.is_paused() || self.resume.is_some();
            mpris.sync(track, p.is_active() || self.pending.is_some(), paused, p.position(), p.volume());
        }
    }
}

/// ListState only tracks a selection, so carry it over when the list length changes.
fn rebuilt(old: &ListState, len: usize) -> ListState {
    let mut s = ListState::new(len);
    s.select(old.selected().min(len.saturating_sub(1)), len);
    s
}

const USAGE: &str = "\
usage: tuber [search terms...]
       tuber --login [browser]   sign in with a browser's YouTube session
       tuber --logout            forget the sign-in
       tuber --history           show the account's recent plays, and what launch would offer

Signing in is optional. It copies the YouTube cookies of a browser that's
signed in to music.youtube.com (chrome, firefox, brave, chromium, edge,
vivaldi, opera; every profile is tried by default) into ~/.local/state/tuber/cookies.txt.";

/// The account's newest plays as tuber sees them, and the launch decision.
fn print_history() -> Result<()> {
    yt::sign_in(auth::Auth::load().context("not signed in: run `tuber --login`")?);
    println!("account: {}", yt::account_name()?);
    let history = yt::history()?;
    println!("history ({} entries), newest first:", history.len());
    for t in history.iter().take(8) {
        println!("  {}  {} — {}", t.id, t.title, t.artist);
    }
    let s = session::load().unwrap_or_default();
    let here: Vec<&Track> = s.current.and_then(|c| s.queue.get(..=c)).unwrap_or_default().iter().collect();
    match history.first() {
        None => println!("launch would offer: nothing from other devices (history is empty)"),
        Some(t) if here.iter().any(|h| h.id == t.id) => {
            println!("launch would offer: this computer's session ({} was played here)", t.title)
        }
        Some(t) => println!("launch would offer: {} — {} from another device", t.title, t.artist),
    }
    Ok(())
}

/// `--login`/`--logout`/`--history`/`--help`; `Some` means handled, so exit.
fn command(args: &[String]) -> Option<Result<()>> {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => println!("{USAGE}"),
        Some("--logout") => match auth::logout() {
            Ok(true) => println!("signed out"),
            Ok(false) => println!("wasn't signed in"),
            Err(e) => return Some(Err(e)),
        },
        Some("--history") => return Some(print_history()),
        Some("--login") => {
            return Some((|| {
                let (profile, name) = auth::login(args.get(1).cloned(), yt::account_name_as)?;
                println!("signed in as {name} (from {profile})");
                Ok(())
            })());
        }
        _ => return None,
    }
    Some(Ok(()))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(res) = command(&args) {
        return res;
    }
    // Open the audio device before taking over the terminal so failures print cleanly.
    let player = Player::new()?;
    let mut app = App::new(player);
    let tx = app.tx.clone();
    app.mpris = Mpris::new(move |e| {
        let _ = tx.send(Msg::Media(e));
    });
    if let Some(a) = auth::Auth::load() {
        yt::sign_in(a);
        app.status = "signed in — type to search, F for liked songs".into();
        app.spawn(|| Msg::Account(yt::account_name()));
        app.spawn(|| Msg::History(yt::history()));
    }
    let query = args;
    if let Some(s) = session::load() {
        app.restore(s, query.is_empty());
    }
    // Signed in, first find out whether another device played since.
    app.ask = if yt::signed_in() { Some(Ask::Checking(Instant::now())) } else { app.ask_local() };
    if !query.is_empty() {
        let q = query.join(" ");
        app.input.set_value(q.clone());
        app.input.set_focused(false);
        app.search(q);
    }
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    let _ = session::save(&app.snapshot());
    res
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut last = Instant::now();
    while !app.quit {
        let now = Instant::now();
        app.tick(now - last);
        last = now;
        terminal.draw(|f| ui::draw(f, app))?;
        let deadline = now + FRAME;
        while event::poll(deadline.saturating_duration_since(Instant::now()))? {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                app.handle_key(key);
            }
        }
    }
    Ok(())
}
