mod art;
mod auth;
mod envelope;
mod mpris;
mod player;
mod session;
mod ui;
mod viz;
mod yt;

use anyhow::Result;
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
use yt::Track;

const FRAME: Duration = Duration::from_millis(16);
const SEEK_STEP: f64 = 5.0;

enum Msg {
    Search(Result<Vec<Track>>),
    Radio { seed: String, res: Result<Vec<Track>> },
    Url { id: String, res: Result<String> },
    Media(MediaControlEvent),
    Account(Result<String>),
    Liked(Result<Vec<Track>>),
    History(Result<Vec<Track>>),
    Cover { id: String, res: Result<image::RgbImage> },
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
    /// Last song played on another device, offered at launch (`c`).
    pub offer: Option<Track>,
    /// Track to add to the account's history once it has really played.
    report: Option<String>,
    pub spinner: SpinnerState,
    urls: HashMap<String, String>,
    resolving: HashSet<String>,
    pending: Option<String>,
    mpris: Option<Mpris>,
    /// Restored session track to start paused at this position once its URL resolves.
    resume: Option<(String, f64)>,
    last_save: Instant,
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
            offer: None,
            cover: None,
            cover_for: None,
            report: None,
            spinner: SpinnerState::new(SpinnerType::Dot),
            urls: HashMap::new(),
            resolving: HashSet::new(),
            pending: None,
            mpris: None,
            resume: None,
            last_save: Instant::now(),
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
        self.offer = None;
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
        self.report = yt::signed_in().then(|| id.to_string());
        match self.resume.take() {
            Some((resume_id, at)) if resume_id == id => {
                self.player.play(url, at);
                self.player.set_paused(true);
            }
            _ => self.player.play(url, 0.0),
        }
    }

    fn restore(&mut self, s: session::Session, keep_search: bool) {
        self.player.set_volume(s.volume);
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
            self.pending = Some(track.id.clone());
            self.resolve(&track.id);
            self.status = format!("resumed {} — space to play", track.title);
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
        }
    }

    /// Offer the account's most recent play if it happened somewhere else
    /// after tuber's session: tuber reports its own plays, so if the newest
    /// history entry isn't anything this session played, it's from another
    /// device (or the browser).
    fn offer_from(&mut self, history: Vec<Track>) {
        let Some(last) = history.into_iter().next() else { return };
        let played_here = self.current.and_then(|c| self.queue.get(..=c)).unwrap_or_default();
        if played_here.iter().any(|t| t.id == last.id) {
            return;
        }
        self.status = format!("last played elsewhere: {} — {} · c to continue", last.title, last.artist);
        self.offer = Some(last);
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
                        self.status = format!("{} songs", tracks.len());
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
                    Ok(tracks) => {
                        self.status = format!("{} liked songs", tracks.len());
                        self.input.set_value("♥ liked songs".into());
                        self.results_state = ListState::new(tracks.len());
                        self.results = tracks;
                        self.tab = Tab::Results;
                        self.focus = Focus::List;
                    }
                    Err(e) => self.status = format!("liked songs failed: {e:#}"),
                }
            }
            Msg::Cover { id, res } => {
                if let Ok(img) = res {
                    self.cover = Some((id, img));
                }
            }
            Msg::History(res) => match res {
                Ok(history) => self.offer_from(history),
                Err(e) => self.status = format!("history failed: {e:#}"),
            },
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
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') if on_list => {
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
            KeyCode::Right | KeyCode::Char('l') => self.player.seek_to(self.player.position() + SEEK_STEP),
            KeyCode::Left | KeyCode::Char('h') => self.player.seek_to(self.player.position() - SEEK_STEP),
            KeyCode::Char('+') | KeyCode::Char('=') => self.player.set_volume(self.player.volume() + 0.05),
            KeyCode::Char('-') => self.player.set_volume(self.player.volume() - 0.05),
            KeyCode::Char('v') => self.viz.mode = self.viz.mode.next(),
            KeyCode::Char('L') => self.liked(),
            KeyCode::Char('c') => {
                if let Some(t) = self.offer.take() {
                    self.start_radio(t);
                    self.focus = Focus::Player;
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self, dt: Duration) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle_msg(msg);
        }
        if self.player.finished() {
            self.next();
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
        // Report a play after 10s, so skipped tracks don't land in history.
        let p = &self.player;
        if self.report.is_some() && p.is_active() && !p.is_paused() && p.position() >= 10.0 {
            let id = self.report.take().unwrap();
            thread::spawn(move || yt::report_play(&id));
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

Signing in is optional. It copies the YouTube cookies of a browser that's
signed in to music.youtube.com (chrome, firefox, brave, chromium, edge,
vivaldi, opera; every profile is tried by default) into ~/.local/state/tuber/cookies.txt.";

/// `--login`/`--logout`/`--help`; `Some` means handled, so exit.
fn command(args: &[String]) -> Option<Result<()>> {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => println!("{USAGE}"),
        Some("--logout") => match auth::logout() {
            Ok(true) => println!("signed out"),
            Ok(false) => println!("wasn't signed in"),
            Err(e) => return Some(Err(e)),
        },
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
        app.status = "signed in — type to search, L for liked songs".into();
        app.spawn(|| Msg::Account(yt::account_name()));
        app.spawn(|| Msg::History(yt::history()));
    }
    let query = args;
    if let Some(s) = session::load() {
        app.restore(s, query.is_empty());
    }
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
