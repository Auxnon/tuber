mod player;
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
use std::thread;
use std::time::{Duration, Instant};

use player::Player;
use viz::Visualizer;
use yt::Track;

const FRAME: Duration = Duration::from_millis(16);
const SEEK_STEP: f64 = 5.0;

enum Msg {
    Search(Result<Vec<Track>>),
    Radio { seed: String, res: Result<Vec<Track>> },
    Url { id: String, res: Result<String> },
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
    pub player: Player,
    pub viz: Visualizer,
    pub tap: Vec<f32>,
    pub status: String,
    pub busy: usize,
    pub spinner: SpinnerState,
    urls: HashMap<String, String>,
    resolving: HashSet<String>,
    pending: Option<String>,
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
            player,
            viz: Visualizer::new(),
            tap: Vec::new(),
            status: "no sign-in needed — type to search".into(),
            busy: 0,
            spinner: SpinnerState::new(SpinnerType::Dot),
            urls: HashMap::new(),
            resolving: HashSet::new(),
            pending: None,
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

    fn resolve(&mut self, id: &str) {
        if self.urls.contains_key(id) || !self.resolving.insert(id.to_string()) {
            return;
        }
        let id = id.to_string();
        self.spawn(move || Msg::Url { res: yt::stream_url(&id), id });
    }

    fn play_index(&mut self, i: usize) {
        let Some(track) = self.queue.get(i).cloned() else { return };
        self.current = Some(i);
        self.queue_state.select(i, self.queue.len());
        self.status = format!("{} — {}", track.title, track.artist);
        match self.urls.get(&track.id) {
            Some(url) => {
                self.player.play(url.clone(), 0.0);
                self.pending = None;
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
            Msg::Url { id, res } => {
                self.resolving.remove(&id);
                match res {
                    Ok(url) => {
                        self.urls.insert(id.clone(), url.clone());
                        if self.pending.as_ref() == Some(&id) {
                            self.pending = None;
                            self.player.play(url, 0.0);
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

        let (list_len, state) = match self.tab {
            Tab::Results => (self.results.len(), &mut self.results_state),
            Tab::Queue => (self.queue.len(), &mut self.queue_state),
        };
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('/') | KeyCode::Char('s') => {
                self.input.set_value(String::new());
                self.input.set_focused(true);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.tab = if self.tab == Tab::Results { Tab::Queue } else { Tab::Results };
            }
            KeyCode::Down | KeyCode::Char('j') => state.select_next(list_len, false),
            KeyCode::Up | KeyCode::Char('k') => state.select_prev(list_len, false),
            KeyCode::PageDown | KeyCode::Char('J') => state.next_page(list_len),
            KeyCode::PageUp | KeyCode::Char('K') => state.prev_page(list_len),
            KeyCode::Char('g') => state.select(0, list_len),
            KeyCode::Char('G') => state.select(list_len.saturating_sub(1), list_len),
            KeyCode::Enter => {
                let sel = state.selected();
                match self.tab {
                    Tab::Results => {
                        if let Some(t) = self.results.get(sel).cloned() {
                            self.start_radio(t);
                        }
                    }
                    Tab::Queue => self.play_index(sel),
                }
            }
            KeyCode::Char('a') if self.tab == Tab::Results => {
                if let Some(t) = self.results.get(self.results_state.selected()).cloned() {
                    self.enqueue(t);
                }
            }
            KeyCode::Char('r') => {
                let sel = state.selected();
                let list = if self.tab == Tab::Results { &self.results } else { &self.queue };
                if let Some(t) = list.get(sel).cloned() {
                    self.start_radio(t);
                }
            }
            KeyCode::Char('d') | KeyCode::Char('x') if self.tab == Tab::Queue => {
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
        self.spinner.tick(dt);
    }
}

/// ListState only tracks a selection, so carry it over when the list length changes.
fn rebuilt(old: &ListState, len: usize) -> ListState {
    let mut s = ListState::new(len);
    s.select(old.selected().min(len.saturating_sub(1)), len);
    s
}

fn main() -> Result<()> {
    // Open the audio device before taking over the terminal so failures print cleanly.
    let player = Player::new()?;
    let mut app = App::new(player);
    let query: Vec<String> = std::env::args().skip(1).collect();
    if !query.is_empty() {
        let q = query.join(" ");
        app.input.set_value(q.clone());
        app.input.set_focused(false);
        app.search(q);
    }
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
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
