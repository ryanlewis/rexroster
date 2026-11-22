//! The roster TUI: one `rex events` stream feeds the model; rows are redrawn
//! in place (ratatui only writes the cells that changed).

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
    Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use serde_json::{Value, json};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{self, Border, Key, Options, View};
use crate::events::{self, Event};
use crate::launch;
use crate::lock::Lock;
use crate::model::{Model, Needs, Row, short_age};
use crate::pin::{self, Decision};
use crate::rex::{self, Rex};
use crate::size;

/// Where a shown worker floats: centred, most of the tab.
const SHOWN_BOUNDS: [f64; 4] = [0.1, 0.08, 0.8, 0.84];
/// Two clicks on the same worker within this time are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Resizes allowed for one row count before giving up, so the layer can't bounce.
const MAX_FITS: u8 = 4;
/// A terminal resize this long after our own counts as the user's, and gets fresh attempts.
const OWN_RESIZE_WINDOW: Duration = Duration::from_secs(2);

/// What [`App::measure`] finds: the new height and new `(x, width)` when
/// they need to change.
type Measured = (Option<f64>, Option<(f64, f64)>);
/// The reset control, shown while the roster has been dragged.
const RESET: &str = "↺ reset";
/// The reset control where there is little room.
const RESET_SHORT: &str = "↺";

/// Key hints, shown on the last line while the roster has focus.
pub const FOOTER: &str = "⏎ show · v view · o options · l lead · q quit";
/// The widest idle time `short_age` gives under 100 days, such as `59m`.
const AGE_WIDTH: usize = 3;
/// The key hints where the full ones don't fit.
pub const FOOTER_SHORT: &str = "⏎ v o l r q";
/// Hints on the options panel's last line.
pub const PANEL_HINTS: &str = "⏎ change · esc save and close";
/// Lines the options panel takes: a heading, one per option, the hints.
pub const PANEL_LINES: usize = Key::ALL.len() + 2;
/// Expanded view: the most lines a message wraps to.
pub const MSG_LINES: usize = 3;
/// Expanded view: continuation lines start under the label.
const INDENT: usize = 4;
/// The label column's width follows the longest label, within these bounds.
pub const LABEL_MIN: usize = 6;
pub const LABEL_MAX: usize = 20;
/// The roster's block label once it is leaving, so `open` and `spawn` no
/// longer take it for a live roster while its block closes.
const LEAVING_LABEL: &str = "roster (closing)";

/// What wakes the loop: an event from Rex (`None` when the stream ends) or
/// terminal input. Both arrive on one channel, so either wakes it at once.
enum Msg {
    Rex(Option<Event>),
    Term(TermEvent),
}

impl From<Option<Event>> for Msg {
    fn from(e: Option<Event>) -> Self {
        Msg::Rex(e)
    }
}

/// Forward terminal input to the loop's channel.
fn spawn_input(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(Msg::Term(ev)).is_err() {
                return;
            }
        }
    });
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

struct App<'a> {
    rex: &'a dyn Rex,
    session: String,
    model: Model,
    selected: Option<String>,
    /// Where the rows were drawn, how many lines were scrolled off the top,
    /// and which worker each drawn line belongs to.
    list_area: Rect,
    offset: usize,
    line_owner: Vec<usize>,
    /// The pane's width when last drawn; the expanded view wraps to it.
    width: u16,
    /// Whether the pane has focus; the key hints show only then.
    focused: bool,
    /// The worker last clicked, and when, to spot a double-click.
    last_click: Option<(String, Instant)>,
    /// The last Rex error. It goes on the next key press, and on the next
    /// refresh or action that works.
    error: Option<String>,
    /// A problem loading or saving the config file. It goes on the next key
    /// press only, so an unrelated refresh can't hide it.
    config_error: Option<String>,
    /// A revert that failed, or giving up on reverts. Like `config_error`, it
    /// goes on the next key press only.
    pin_error: Option<String>,
    fit: Fit,
    row_scale: size::RowScale,
    /// The `session.view` this pass fetched, so a fit in the same pass
    /// needn't fetch it again. Dropped before the loop waits.
    view: Option<Value>,
    /// When the spinner started; its frame is a function of the time since.
    spin_start: Instant,
    /// Display options in effect, and the file the options panel saves to.
    opts: Options,
    config_path: Option<PathBuf>,
    /// The options panel, while it is open, and where its options were drawn.
    panel: Option<Panel>,
    panel_area: Rect,
    /// The lead's pinned session label, and the guard that decides reverts.
    pin: PathBuf,
    guard: pin::Guard,
    /// The open lock, held from deciding to exit until the roster has left.
    leaving: Option<Lock>,
    follow: Follow,
    /// Where the reset control was drawn; empty while it isn't shown.
    reset_area: Rect,
}

/// Following the lead's pane: the layer's top right corner goes to the top
/// right of the lead's pane whenever the layout changes, until the user
/// moves that corner.
#[derive(Default)]
struct Follow {
    /// The user moved the layer, so it stays where they put it.
    dragged: bool,
    /// The bounds the roster last gave the layer, or found it at first, to
    /// tell its own moves from the user's.
    set: Option<[f64; 4]>,
    /// One column's and one row's share of the window, once measured, so a
    /// corner off by less than a cell isn't taken for a drag.
    cell: [f64; 2],
    /// The user set the layer's width, so it is never narrowed.
    user_width: bool,
}

/// What `rexroster` starts the TUI with.
pub struct Settings {
    pub opts: Options,
    pub path: Option<PathBuf>,
    /// A problem loading the config file, shown on the footer line.
    pub error: Option<String>,
    /// The lead's pin file.
    pub pin: PathBuf,
}

/// The options panel: which option the cursor is on, and the options as they
/// were when it opened, so closing saves only what changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    pub cursor: usize,
    pub start: Options,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelAction {
    Stay,
    Close,
}

impl Panel {
    pub fn new(opts: Options) -> Self {
        Self {
            cursor: 0,
            start: opts,
        }
    }

    /// Handle a key: move, cycle the option under the cursor, or close.
    pub fn key(&mut self, code: KeyCode, opts: &mut Options) -> PanelAction {
        let last = Key::ALL.len() - 1;
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => {
                opts.cycle(Key::ALL[self.cursor], 1);
            }
            KeyCode::Left => opts.cycle(Key::ALL[self.cursor], -1),
            KeyCode::Esc | KeyCode::Char('o') => return PanelAction::Close,
            _ => {}
        }
        PanelAction::Stay
    }

    /// A click on option `index` moves the cursor there and cycles it.
    pub fn click(&mut self, index: usize, opts: &mut Options) {
        if let Some(key) = Key::ALL.get(index) {
            self.cursor = index;
            opts.cycle(*key, 1);
        }
    }

    /// The options that differ from when the panel opened.
    pub fn changed(&self, opts: &Options) -> Vec<Key> {
        Key::ALL
            .into_iter()
            .filter(|k| opts.value(*k) != self.start.value(*k))
            .collect()
    }
}

#[derive(Default)]
struct Fit {
    dirty: bool,
    want: u16,
    attempts: u8,
    last_resize: Option<Instant>,
    /// Measuring failed (say no client has drawn the pane yet): retries
    /// used so far, and when to try again.
    retries: u32,
    retry_at: Option<Instant>,
    /// A fresh view to place the layer by: the layout may have changed.
    place: bool,
    /// The columns the roster's terminal should have; 0 until noted.
    want_cols: u16,
    /// A fit waiting for the lead to show again: another pane was zoomed
    /// over it, so its rows couldn't be measured.
    hidden: bool,
}

pub fn run(lead: String, session: String, settings: Settings) -> Result<()> {
    let cli = rex::Cli::new();
    let me = std::env::var("REX_BLOCK").ok().filter(|s| !s.is_empty());
    let (tx, rx) = mpsc::channel();
    let input_tx = tx.clone();
    // Subscribe before the first refresh so nothing falls between them.
    let mut child = events::spawn(&cli, &session, tx)?;
    let mut app = App {
        rex: &cli,
        session,
        model: Model::new(lead, me, now()),
        selected: None,
        list_area: Rect::default(),
        offset: 0,
        line_owner: Vec::new(),
        width: 0,
        focused: false,
        last_click: None,
        error: None,
        config_error: settings.error,
        pin_error: None,
        fit: Fit::default(),
        row_scale: size::RowScale::default(),
        view: None,
        spin_start: Instant::now(),
        opts: settings.opts,
        config_path: settings.path,
        panel: None,
        panel_area: Rect::default(),
        pin: settings.pin,
        guard: pin::Guard::default(),
        leaving: None,
        follow: Follow::default(),
        reset_area: Rect::default(),
    };
    let result = app
        .model
        .refresh(app.rex, &app.session, now())
        .and_then(|()| {
            let mut terminal = ratatui::init();
            install_panic_hook();
            let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableFocusChange);
            spawn_input(input_tx);
            // A worker minimized before the roster started gets no view event.
            app.sync(Needs {
                view: true,
                blocks: false,
            });
            let result = app.run(&mut terminal, &rx);
            // Nothing reverts once the roster is gone, and the next spawn
            // pins afresh, so leave (and drop the pin) however the roster ends.
            app.leave();
            let _ = restore_input(&mut std::io::stdout());
            ratatui::restore();
            result
        });
    // Stop the event stream however the roster ends.
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Turn off the mouse capture and focus reports that `run` turns on.
fn restore_input(w: &mut impl Write) -> std::io::Result<()> {
    execute!(w, DisableFocusChange, DisableMouseCapture)
}

/// On a panic, also turn off mouse capture and focus reports, then run the
/// hook already set: ratatui's, which leaves raw mode and the alternate
/// screen and then prints the panic.
fn install_panic_hook() {
    let next = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_input(&mut std::io::stdout());
        next(info);
    }));
}

impl App<'_> {
    fn run(&mut self, terminal: &mut DefaultTerminal, rx: &Receiver<Msg>) -> Result<()> {
        loop {
            let rows = self.model.rows(now());
            if self.model.should_exit(now()) && self.confirm_exit() {
                return Ok(());
            }
            let lines = if self.panel.is_some() {
                PANEL_LINES
            } else {
                total_lines(&rows, self.width, self.expanded())
            };
            self.note_rows(lines);
            self.note_columns(self.wanted_columns(&rows));
            self.fit_if_due();
            terminal.draw(|f| self.draw(f, &rows))?;

            // Sleep until a message arrives, the tick is due or a fit is to
            // be retried, then take everything that is waiting.
            let wait = self.fit.retry_at.map_or(tick(&rows), |t| {
                tick(&rows).min(t.saturating_duration_since(Instant::now()))
            });
            let first = match rx.recv_timeout(wait) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => bail!("rex events stopped"),
            };
            let mut needs = Needs::default();
            for msg in first.into_iter().chain(rx.try_iter()) {
                match msg {
                    Msg::Rex(Some(Event::SessionLabel(label))) => self.on_label(&label),
                    Msg::Rex(Some(event)) => needs = needs.or(self.model.apply(event, now())),
                    Msg::Rex(None) => bail!("rex events stopped"),
                    Msg::Term(ev) => {
                        if self.on_terminal_event(&ev, &rows) {
                            return Ok(());
                        }
                    }
                }
            }
            self.sync(needs);
        }
    }

    /// Check again under the open lock, against a fresh listing, that the
    /// roster is done: a worker spawned since the last event keeps it. When
    /// it is, the lock stays held until [`App::leave`], so no spawn runs in
    /// between. Without the lock it exits anyway, as it would have before.
    fn confirm_exit(&mut self) -> bool {
        let lock = Lock::acquire(&launch::open_lock(&self.pin));
        let res = self.model.refresh(self.rex, &self.session, now());
        let exit = res.is_ok() && self.model.should_exit(now());
        self.report(res);
        if exit {
            self.leaving = lock.ok();
        }
        exit
    }

    /// Leave for good, under the open lock: give up the roster's label, so a
    /// spawn after this finds no roster, pins afresh and opens a new one,
    /// then drop the pin.
    fn leave(&mut self) {
        let _lock = self
            .leaving
            .take()
            .or_else(|| Lock::acquire(&launch::open_lock(&self.pin)).ok());
        if let Some(me) = &self.model.me {
            let _ = self.rex.call(
                "session.set_block_label",
                json!({ "session_id": self.session, "block_id": me, "label": LEAVING_LABEL }),
            );
        }
        pin::release(&self.pin);
    }

    /// Put the pinned label back if a worker renamed the session.
    fn on_label(&mut self, label: &str) {
        let pinned = pin::read(&self.pin);
        match self
            .guard
            .decide(label, pinned.as_deref(), self.model.has_workers(), now())
        {
            Decision::Keep => {}
            Decision::Revert(to) => {
                if let Err(err) = pin::rename(self.rex, &self.session, &to) {
                    self.guard.forget_last();
                    self.pin_error = Some(format!("{err:#}"));
                }
            }
            Decision::GiveUp => {
                self.pin_error = Some(format!(
                    "renamed {} times in {}s; leaving it as {label}. rexroster pin keeps a new name",
                    pin::MAX_REVERTS,
                    pin::WINDOW_SECS
                ));
            }
        }
    }

    fn expanded(&self) -> bool {
        self.opts.view == View::Expanded
    }

    /// Fetch what the events need: the view, to hide minimized workers (kept
    /// for a fit in this pass), and the blocks. Shows the first error, or
    /// clears the last one when everything worked.
    fn sync(&mut self, needs: Needs) {
        if !needs.view && !needs.blocks {
            return;
        }
        let mut res = Ok(());
        if needs.view {
            res = self
                .rex
                .call("session.view", json!({ "session_id": self.session }))
                .and_then(|view| {
                    let hidden = self.hide_minimized(&view);
                    self.view = Some(view);
                    self.fit.place = true;
                    hidden
                });
        }
        if needs.blocks {
            res = res.and(self.model.refresh(self.rex, &self.session, now()));
        }
        self.report(res);
    }

    /// Show an action's error, or clear the last one when it worked.
    fn report<T>(&mut self, res: Result<T>) {
        self.error = res.err().map(|err| format!("{err:#}"));
    }

    /// The error on the footer line, if any.
    fn shown_error(&self) -> Option<&str> {
        self.error
            .as_deref()
            .or(self.config_error.as_deref())
            .or(self.pin_error.as_deref())
    }

    /// The last line shows key hints while focused, and an error whenever
    /// there is one. The options panel has its own hints line instead.
    fn footer_shown(&self) -> bool {
        self.panel.is_none() && (self.focused || self.shown_error().is_some())
    }

    /// Note the rows the pane needs: the workers' lines, plus the footer while focused.
    fn note_rows(&mut self, lines: usize) {
        let want = size::wanted_rows(lines, self.footer_shown(), self.opts.border);
        if want != self.fit.want {
            self.fit = Fit {
                dirty: true,
                want,
                last_resize: self.fit.last_resize,
                place: self.fit.place,
                want_cols: self.fit.want_cols,
                ..Fit::default()
            };
        }
    }

    /// Note the columns the pane needs, on the terms of [`Self::note_rows`].
    fn note_columns(&mut self, cols: u16) {
        if cols != self.fit.want_cols {
            self.fit = Fit {
                dirty: true,
                want: self.fit.want,
                last_resize: self.fit.last_resize,
                place: self.fit.place,
                want_cols: cols,
                ..Fit::default()
            };
        }
    }

    /// The columns the pane needs. Compact: the widest row and a column to
    /// spare, at least `COMPACT_MIN_COLUMNS`. Rows are measured with the
    /// idle time at its widest, so the width holds as times tick over and
    /// workers start and stop working. Expanded: `MIN_COLUMNS`, for
    /// the messages. The options panel, while open, needs `PANEL_COLUMNS`.
    fn wanted_columns(&self, rows: &[Row]) -> u16 {
        let chrome = 2 * u16::from(self.opts.border == Border::Rounded);
        let view = if self.expanded() {
            size::MIN_COLUMNS
        } else {
            let cols = Columns {
                age: AGE_WIDTH,
                ..Columns::for_rows(rows)
            };
            let widest = rows
                .iter()
                .map(|r| row_line(r, false, cols, Duration::ZERO).width())
                .max()
                .unwrap_or(0);
            let widest = u16::try_from(widest).unwrap_or(u16::MAX);
            (widest.saturating_add(1 + chrome)).max(size::COMPACT_MIN_COLUMNS)
        };
        if self.panel.is_some() {
            view.max(size::PANEL_COLUMNS + chrome)
        } else {
            view
        }
    }

    /// Fit the layer if something asked for it or a retry is due. Drops this
    /// pass's view either way, so the next pass fetches a fresh one.
    fn fit_if_due(&mut self) {
        if self.fit.retry_at.is_some_and(|t| t <= Instant::now()) {
            self.fit.retry_at = None;
            self.fit.dirty = true;
        }
        if self.fit.dirty || self.fit.place {
            self.fit_layer();
        }
        self.view = None;
    }

    /// Resize the layer towards the wanted rows, at most `MAX_FITS` times per
    /// row count. Until the pane has been drawn it can't be measured, so it
    /// tries again later, backing off, for up to `size::RETRY_TOTAL`. In the
    /// same call, move it to the lead's pane while it follows the lead.
    fn fit_layer(&mut self) {
        let mut sizing = std::mem::take(&mut self.fit.dirty) && self.fit.attempts < MAX_FITS;
        let placing = std::mem::take(&mut self.fit.place);
        if !sizing && !placing {
            return;
        }
        let Some(me) = self.model.me.clone() else {
            return;
        };
        let view = match self.view.take() {
            Some(view) => Some(view),
            None => self
                .rex
                .call("session.view", json!({ "session_id": self.session }))
                .ok(),
        };
        let found = view
            .as_ref()
            .and_then(|v| Some((v, rex::floating_layer_of(v, &me)?)));
        let Some((view, (layer, bounds))) = found else {
            if sizing {
                self.retry_fit();
            }
            return;
        };
        // While another pane is zoomed over the lead there is nothing to
        // measure by; the fit waits for a view that shows the lead again.
        if rex::zoomed_over(view, &self.model.lead) {
            self.fit.hidden |= sizing;
            sizing = false;
        } else if std::mem::take(&mut self.fit.hidden) {
            sizing = self.fit.attempts < MAX_FITS;
        }
        // A width other than the roster last set is the user's.
        if let Some(set) = self.follow.set
            && size::off_by_cells(set[2], bounds[2], self.follow.cell[0])
        {
            self.follow.user_width = true;
        }
        let mut target = bounds;
        let mut sized = false;
        if sizing {
            match self.measure(&me, view, bounds) {
                Some((height, width)) => {
                    self.fit.retries = 0;
                    self.fit.retry_at = None;
                    sized = height.is_some() || width.is_some();
                    if let Some((x, w)) = width {
                        (target[0], target[2]) = (x, w);
                    }
                    if let Some(h) = height {
                        target[3] = h;
                    }
                }
                None => self.retry_fit(),
            }
        }
        self.place(view, &me, bounds, &mut target);
        if !sized && target == bounds {
            return;
        }
        if sized {
            self.fit.attempts += 1;
            // Only a new size resizes the terminal; a move alone doesn't.
            self.fit.last_resize = Some(Instant::now());
        }
        let res = self.rex.call(
            "session.resize_layer",
            json!({
                "session_id": self.session,
                "layer_id": layer,
                "bounds": target,
            }),
        );
        match res {
            Ok(_) => self.follow.set = Some(target),
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    /// Measuring failed: try again later, while retries are left.
    fn retry_fit(&mut self) {
        if let Some(delay) = size::retry_delay(self.fit.retries) {
            self.fit.retries += 1;
            self.fit.retry_at = Some(Instant::now() + delay);
        }
    }

    /// Move `target`'s top right corner to the lead pane's, unless the user
    /// has moved the layer since the roster last did, or the lead isn't a
    /// tiled pane in the roster's window. A corner somewhere the roster didn't
    /// put it means the user moved the layer: from then on it stays.
    fn place(&mut self, view: &Value, me: &str, bounds: [f64; 4], target: &mut [f64; 4]) {
        // Until the roster moves it, where it is now is where it was put.
        let set = *self.follow.set.get_or_insert(bounds);
        if size::corner_moved(set, bounds, self.follow.cell) {
            self.follow.dragged = true;
        }
        if self.follow.dragged {
            return;
        }
        let Some((window, area)) = rex::tiled_area_of(view, &self.model.lead) else {
            return;
        };
        if rex::window_of(view, me).as_deref() != Some(window.as_str()) {
            return;
        }
        let (x, y) = size::corner_place(area, target[2], target[3]);
        if (x - target[0]).abs() > 1e-6 || (y - target[1]).abs() > 1e-6 {
            (target[0], target[1]) = (x, y);
        }
    }

    /// Follow the lead's pane again, moving back to it now.
    fn reset_follow(&mut self) {
        self.follow = Follow {
            cell: self.follow.cell,
            ..Follow::default()
        };
        self.fit.place = true;
        // A width the user set is let go too, so the width fits again.
        self.fit.dirty = true;
    }

    /// If the layer doesn't fit, the height and the left edge and width it
    /// should have. `None` when the panes can't be measured yet.
    fn measure(&mut self, me: &str, view: &Value, bounds: [f64; 4]) -> Option<Measured> {
        let lead = &self.model.lead;
        // Zoomed, the lead fills more than its layout cell says.
        let shown = rex::tiled_area_of(view, lead).map(|(_, area)| area);
        let lead_h = match shown {
            Some(area) => area[3],
            None => rex::rect_height_of(view, lead)?,
        };
        let lead_size = self.rex.block_call(&self.session, lead, "size").ok()?;
        let my_size = self.rex.block_call(&self.session, me, "size").ok()?;
        let lead_rows = rex::drawn_rows(&lead_size)?;
        let mut rows = rex::drawn_rows(&my_size)?;
        // A layer squeezed below a row still reports one; count only full rows.
        if let (Some(mine), Some(full)) = (
            rex::cell_height_px(&my_size),
            rex::cell_height_px(&lead_size),
        ) {
            rows = size::full_rows(rows, mine, full);
        }
        let per_row = self
            .row_scale
            .observe(bounds[3], rows, lead_h / f64::from(lead_rows));
        let new_h = size::new_height(self.fit.want, rows, per_row, bounds[1], bounds[3]);
        // Older Rex servers may not report columns; then only the height fits.
        let per_col = (|| {
            let lead_w = match shown {
                Some(area) => area[2],
                None => rex::rect_width_of(view, lead)?,
            };
            Some(lead_w / f64::from(rex::drawn_columns(&lead_size)?))
        })();
        self.follow.cell = [per_col.unwrap_or(0.0), per_row];
        let want = self.fit.want_cols;
        let narrow = !self.follow.user_width;
        let new_w = per_col.filter(|_| want > 0).and_then(|per_col| {
            let cols = rex::drawn_columns(&my_size)?;
            size::new_width(cols, want, per_col, bounds[0], bounds[2], narrow)
        });
        Some((new_h, new_w))
    }

    fn selected_index(&self, rows: &[Row]) -> Option<usize> {
        let sel = self.selected.as_deref();
        rows.iter()
            .position(|r| Some(r.block.as_str()) == sel)
            .or((!rows.is_empty()).then_some(0))
    }

    fn on_terminal_event(&mut self, ev: &TermEvent, rows: &[Row]) -> bool {
        match ev {
            TermEvent::Key(k) if k.kind == KeyEventKind::Press => {
                self.error = None;
                self.config_error = None;
                self.pin_error = None;
                let ctrl_c =
                    k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
                if ctrl_c || (k.code == KeyCode::Char('q') && self.panel.is_none()) {
                    return true;
                }
                if let Some(panel) = &mut self.panel {
                    let action = panel.key(k.code, &mut self.opts);
                    if action == PanelAction::Close {
                        self.close_panel();
                    }
                    return false;
                }
                match k.code {
                    KeyCode::Char('q') => return true,
                    _ if ctrl_c => return true,
                    KeyCode::Char('j') | KeyCode::Down => self.step(rows, 1),
                    KeyCode::Char('k') | KeyCode::Up => self.step(rows, -1),
                    KeyCode::Enter => {
                        if let Some(i) = self.selected_index(rows) {
                            self.toggle(&rows[i].block);
                        }
                    }
                    KeyCode::Char('l') => self.focus_lead(),
                    KeyCode::Char('r') => self.reset_follow(),
                    KeyCode::Char('v') => {
                        self.opts.view = match self.opts.view {
                            View::Compact => View::Expanded,
                            View::Expanded => View::Compact,
                        };
                    }
                    KeyCode::Char('o') => self.panel = Some(Panel::new(self.opts)),
                    _ => {}
                }
            }
            TermEvent::Mouse(m)
                if m.kind == MouseEventKind::Down(MouseButton::Left)
                    && self.reset_area.contains((m.column, m.row).into()) =>
            {
                self.reset_follow();
            }
            TermEvent::Mouse(m)
                if m.kind == MouseEventKind::Down(MouseButton::Left) && self.panel.is_some() =>
            {
                let a = self.panel_area;
                let inside = m.column >= a.x && m.column < a.right() && m.row >= a.y;
                if inside && let Some(panel) = &mut self.panel {
                    panel.click(usize::from(m.row - a.y), &mut self.opts);
                }
            }
            TermEvent::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.row_at(m.column, m.row).filter(|&i| i < rows.len()) {
                    let block = rows[i].block.clone();
                    self.selected = Some(block.clone());
                    // By block, not row: the rows may have moved in between.
                    let double = self
                        .last_click
                        .as_ref()
                        .is_some_and(|(b, t)| *b == block && t.elapsed() <= DOUBLE_CLICK);
                    if double {
                        self.last_click = None;
                        self.toggle(&block);
                    } else {
                        self.last_click = Some((block, Instant::now()));
                    }
                }
            }
            TermEvent::FocusGained => self.focused = true,
            TermEvent::FocusLost => self.focused = false,
            TermEvent::Resize(..) => {
                self.fit.dirty = true;
                let ours = self
                    .fit
                    .last_resize
                    .is_some_and(|t| t.elapsed() < OWN_RESIZE_WINDOW);
                if !ours {
                    self.fit.attempts = 0;
                }
                // A new client may not have drawn the pane yet.
                self.fit.retries = 0;
            }
            _ => {}
        }
        false
    }

    /// Close the options panel and save what changed in it.
    fn close_panel(&mut self) {
        let Some(panel) = self.panel.take() else {
            return;
        };
        let keys = panel.changed(&self.opts);
        if keys.is_empty() {
            return;
        }
        let res = match &self.config_path {
            Some(path) => config::save(path, &self.opts, &keys),
            None => Err(anyhow::anyhow!(
                "no config directory: set HOME or XDG_CONFIG_HOME"
            )),
        };
        self.config_error = res.err().map(|err| format!("{err:#}"));
    }

    fn step(&mut self, rows: &[Row], by: isize) {
        if rows.is_empty() {
            return;
        }
        let i = self.selected_index(rows).unwrap_or(0);
        let j = i.saturating_add_signed(by).min(rows.len() - 1);
        self.selected = Some(rows[j].block.clone());
    }

    /// The worker drawn at a screen position, on any of its lines.
    fn row_at(&self, column: u16, row: u16) -> Option<usize> {
        let a = self.list_area;
        if column < a.x || column >= a.right() || row < a.y || row >= a.bottom() {
            return None;
        }
        self.line_owner
            .get(usize::from(row - a.y) + self.offset)
            .copied()
    }

    /// Show a detached worker as a centred float beside the lead, or hide a shown one.
    fn toggle(&mut self, block: &str) {
        let s = &self.session;
        let res = if self.model.is_shown(block) {
            self.rex.call(
                "session.detach_block",
                json!({ "session_id": s, "block_id": block }),
            )
        } else {
            self.rex
                .call(
                    "session.move_block",
                    json!({ "session_id": s, "block_id": block, "anchor_block_id": self.model.lead }),
                )
                .and_then(|_| {
                    self.rex.call(
                        "session.float_block",
                        json!({ "session_id": s, "block_id": block, "bounds": SHOWN_BOUNDS }),
                    )
                })
                .and_then(|_| {
                    self.rex.call(
                        "session.focus_block",
                        json!({ "session_id": s, "block_id": block }),
                    )
                })
        };
        self.report(res);
    }

    /// Treat Rex's minimize as hide: detach any of our workers whose layer
    /// is minimized in `view`. The detach changes the view again, and that
    /// pass finds nothing left to do. Returns the first error.
    fn hide_minimized(&self, view: &Value) -> Result<()> {
        let mut res = Ok(());
        for block in self.model.to_detach(view) {
            let detached = self.rex.call(
                "session.detach_block",
                json!({ "session_id": self.session, "block_id": block }),
            );
            res = res.and(detached.map(drop));
        }
        res
    }

    fn focus_lead(&mut self) {
        let res = self.rex.call(
            "session.focus_block",
            json!({ "session_id": self.session, "block_id": self.model.lead }),
        );
        self.report(res);
    }

    fn draw(&mut self, f: &mut ratatui::Frame, rows: &[Row]) {
        let area = f.area();
        let chrome = 2 * u16::from(self.opts.border == Border::Rounded);
        let footer = self.footer_line(usize::from(area.width.saturating_sub(chrome)));
        // While dragged, the top right shows a control to follow the lead again.
        let reset =
            self.follow
                .dragged
                .then_some(if area.width >= 40 { RESET } else { RESET_SHORT });
        let gray = Style::new().fg(Color::Gray);
        self.reset_area = Rect::default();
        let inner = match self.opts.border {
            Border::None => area,
            Border::Rounded => {
                let title = if self.panel.is_some() {
                    " options "
                } else {
                    " workers "
                };
                let mut block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(Color::DarkGray))
                    .title(Line::styled(title, gray));
                if self.footer_shown() {
                    block = block.title_bottom(footer.clone());
                }
                if let Some(reset) = reset {
                    let w = u16::try_from(reset.width()).unwrap_or(0) + 2;
                    block = block.title(Line::styled(format!(" {reset} "), gray).right_aligned());
                    self.reset_area = right_end(area, w, 1);
                }
                let inner = block.inner(area);
                f.render_widget(block, area);
                inner
            }
        };
        self.width = inner.width;
        // With no border the control sits on the first line, so that line
        // is cut short to leave it room, with a space before it.
        let reset = reset.filter(|_| self.opts.border == Border::None);
        let mut reserve = 0;
        if let Some(reset) = reset {
            let w = u16::try_from(reset.width()).unwrap_or(0);
            self.reset_area = right_end(area, w, 0);
            reserve = self.reset_area.width + u16::from(self.reset_area.width > 0);
        }
        if self.panel.is_some() {
            self.draw_panel(f, inner);
        } else {
            let footer =
                (self.opts.border == Border::None && self.footer_shown()).then_some(footer);
            self.draw_list(f, inner, rows, footer, reserve);
        }
        if let Some(reset) = reset {
            f.render_widget(Line::styled(reset, gray), self.reset_area);
        }
    }

    /// The key hints, or the error when there is one, cut to `width`.
    fn footer_line(&self, width: usize) -> Line<'static> {
        match self.shown_error() {
            Some(err) => Line::styled(cut(err, width), Style::new().fg(Color::Red)),
            None => {
                let hints = if FOOTER.width() <= width {
                    FOOTER
                } else {
                    FOOTER_SHORT
                };
                Line::styled(cut(hints, width), Style::new().add_modifier(Modifier::DIM))
            }
        }
    }

    fn draw_panel(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let Some(panel) = &self.panel else { return };
        let width = usize::from(area.width);
        let mut lines = vec![Line::styled(
            cut("Options", width),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        for (i, key) in Key::ALL.into_iter().enumerate() {
            let selected = i == panel.cursor;
            let text = format!(
                "{}{:<7} {}",
                if selected { "›" } else { " " },
                key.name(),
                self.opts.value(key)
            );
            let style = if selected {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            lines.push(Line::styled(cut(&text, width), style));
        }
        lines.push(match self.shown_error() {
            Some(err) => Line::styled(cut(err, width), Style::new().fg(Color::Red)),
            None => Line::styled(
                cut(PANEL_HINTS, width),
                Style::new().add_modifier(Modifier::DIM),
            ),
        });
        // Clicks land on the option lines, under the heading.
        self.panel_area = Rect {
            y: area.y.saturating_add(1),
            height: area
                .height
                .saturating_sub(1)
                .min(u16::try_from(Key::ALL.len()).unwrap_or(0)),
            ..area
        };
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_list(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        rows: &[Row],
        footer: Option<Line>,
        reserve: u16,
    ) {
        let footer_rows = u16::from(footer.is_some() && area.height > 0);
        let list = Rect {
            height: area.height - footer_rows,
            ..area
        };
        self.list_area = list;
        let sel = self.selected_index(rows);
        // The selection only matters to keys and clicks, so mark it only
        // while the roster has focus. It still decides the scroll below.
        let marked = sel.filter(|_| self.focused);
        let elapsed = self.spin_start.elapsed();
        let laid = layout(rows, marked, area.width, elapsed, self.expanded());
        let visible = usize::from(list.height);
        // Keep the selected worker on screen when the list is taller than the
        // pane; one taller than the pane shows from its header down.
        let start = sel
            .and_then(|i| laid.iter().position(|(o, _)| *o == i))
            .unwrap_or(0);
        let end = sel.map_or(0, |i| {
            laid.iter().rposition(|(o, _)| *o == i).map_or(0, |l| l + 1)
        });
        self.offset = end.saturating_sub(visible).min(start);
        self.line_owner = laid.iter().map(|(o, _)| *o).collect();
        // A row's first line is where its name and idle time are.
        let first_row = laid
            .get(self.offset)
            .map(|(o, _)| *o)
            .filter(|&o| self.offset == 0 || laid[self.offset - 1].0 != o);
        let mut lines: Vec<Line> = laid
            .into_iter()
            .skip(self.offset)
            .take(visible)
            .map(|(_, l)| l)
            .collect();
        // The first line gives up its right `reserve` columns. A row line
        // gives them up from its label, so the idle time stays whole.
        if reserve > 0 && !lines.is_empty() {
            let first = Rect {
                width: list.width.saturating_sub(reserve),
                height: 1,
                ..list
            };
            if let Some(i) = first_row {
                let over = lines[0].width().saturating_sub(usize::from(first.width));
                let mut cols = Columns::for_rows(rows);
                cols.label = cols.label.saturating_sub(over);
                lines[0] = row_line(&rows[i], marked == Some(i), cols, elapsed);
            }
            f.render_widget(Paragraph::new(lines.remove(0)), first);
            let rest = Rect {
                y: list.y + 1,
                height: list.height - 1,
                ..list
            };
            f.render_widget(Paragraph::new(lines), rest);
        } else {
            f.render_widget(Paragraph::new(lines), list);
        }
        if let Some(line) = footer.filter(|_| footer_rows == 1) {
            let at = Rect {
                y: area.bottom() - 1,
                height: 1,
                ..area
            };
            f.render_widget(Paragraph::new(line), at);
        }
    }
}

/// The last `w` columns of `area`'s top line, `gap` columns in from its
/// right edge. Empty when they don't fit.
fn right_end(area: Rect, w: u16, gap: u16) -> Rect {
    if area.height == 0 || w + gap > area.width {
        return Rect::default();
    }
    Rect {
        x: area.right() - gap - w,
        y: area.y,
        width: w,
        height: 1,
    }
}

/// Column widths shared by every row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Columns {
    pub label: usize,
    pub age: usize,
}

impl Columns {
    /// The label column fits the longest label, between `LABEL_MIN` and
    /// `LABEL_MAX`; the age column fits the longest idle time.
    pub fn for_rows(rows: &[Row]) -> Self {
        let longest = rows.iter().map(|r| r.label.width()).max().unwrap_or(0);
        let age = rows
            .iter()
            .filter_map(|r| idle_age(r).map(|a| short_age(a).len()))
            .max()
            .unwrap_or(0);
        Self {
            label: longest.clamp(LABEL_MIN, LABEL_MAX),
            age,
        }
    }
}

/// How long a worker has sat in its state, or `None` while it is working:
/// the roster only counts the time a worker spends not working.
fn idle_age(r: &Row) -> Option<u64> {
    r.age.filter(|_| r.state != "working")
}

/// `s` cut to `width` display columns, ending in `…` when anything was cut.
pub fn cut(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_owned();
    }
    let Some(room) = width.checked_sub(1) else {
        return String::new();
    };
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        used += w;
        out.push(c);
    }
    // Don't leave a space before the ellipsis.
    let mut out = out.trim_end().to_owned();
    out.push('…');
    out
}

/// `s` cut to `width` and padded with spaces to exactly `width` columns.
fn pad(s: &str, width: usize) -> String {
    let s = cut(s, width);
    let fill = width.saturating_sub(s.width());
    format!("{s}{}", " ".repeat(fill))
}

/// One row: `›` when selected, the glyph, `▸` when shown, a space, the
/// label, then the idle time right-aligned. No message: the expanded view
/// puts that on the lines below.
pub fn row_line(r: &Row, selected: bool, cols: Columns, elapsed: Duration) -> Line<'static> {
    let (g, style) = glyph(&r.state, elapsed);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let strong = if selected {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let mut spans = vec![
        Span::styled(if selected { "›" } else { " " }, strong),
        Span::styled(g, style),
        Span::raw(if r.shown { "▸ " } else { "  " }),
        Span::styled(pad(&r.label, cols.label), strong),
    ];
    if cols.age > 0 {
        let age = idle_age(r).map(short_age).unwrap_or_default();
        spans.push(Span::styled(format!(" {age:>w$}", w = cols.age), dim));
    }
    Line::from(spans)
}

/// Word-wrap `text` to `width` columns, in at most `max` lines. When it
/// needs more, the last line ends in `…`.
pub fn wrap(text: &str, width: usize, max: usize) -> Vec<String> {
    if width == 0 || max == 0 {
        return Vec::new();
    }
    // A message that already ends in a word of its own, `…`, would wrap onto a
    // line by itself. Take it off and put it back on the last line after.
    let body = text.trim_end();
    let (body, open) = match body.strip_suffix('…') {
        Some(b) if b.ends_with(char::is_whitespace) && !b.trim().is_empty() => (b, true),
        _ => (text, false),
    };
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in body.split_whitespace() {
        let mut word = word.to_owned();
        loop {
            let need = if line.is_empty() {
                word.width()
            } else {
                line.width() + 1 + word.width()
            };
            if need <= width {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            // A word longer than the line: break it.
            let (head, tail) = split_at_width(&word, width);
            lines.push(head);
            word = tail;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.len() > max {
        // Greedy wrapping means the remaining lines can't share one line,
        // so joining them always overflows and `cut` adds the ellipsis.
        let rest = lines.split_off(max - 1).join(" ");
        lines.push(cut(&rest, width));
    }
    if open && let Some(last) = lines.last_mut() {
        // Ends the last text line, replacing its final character if it is full.
        if !last.ends_with('…') {
            *last = cut(&format!("{last}…"), width);
        }
    }
    lines
}

fn split_at_width(s: &str, width: usize) -> (String, String) {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > width && i > 0 {
            return (s[..i].to_owned(), s[i..].to_owned());
        }
        used += w;
    }
    (s.to_owned(), String::new())
}

/// The task line in the expanded view, when there is a task that isn't
/// already the message.
fn task_line(r: &Row) -> Option<&str> {
    r.task.as_deref().filter(|t| *t != r.msg)
}

/// How many lines a worker takes at `width`.
pub fn worker_height(r: &Row, width: u16, expanded: bool) -> usize {
    if !expanded {
        return 1;
    }
    let room = usize::from(width).saturating_sub(INDENT);
    1 + wrap(&r.msg, room, MSG_LINES).len() + usize::from(task_line(r).is_some())
}

/// The lines all workers take at `width`.
pub fn total_lines(rows: &[Row], width: u16, expanded: bool) -> usize {
    rows.iter().map(|r| worker_height(r, width, expanded)).sum()
}

/// Every drawn line, with the index of the worker it belongs to.
pub fn layout(
    rows: &[Row],
    sel: Option<usize>,
    width: u16,
    elapsed: Duration,
    expanded: bool,
) -> Vec<(usize, Line<'static>)> {
    let cols = Columns::for_rows(rows);
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let selected = sel == Some(i);
        out.push((i, row_line(r, selected, cols, elapsed)));
        if !expanded {
            continue;
        }
        let bold = if selected {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let indent = " ".repeat(INDENT);
        let room = usize::from(width).saturating_sub(INDENT);
        for text in wrap(&r.msg, room, MSG_LINES) {
            out.push((i, Line::styled(format!("{indent}{text}"), bold)));
        }
        if let Some(task) = task_line(r) {
            let dim = bold.add_modifier(Modifier::DIM);
            out.push((i, Line::styled(format!("{indent}{}", cut(task, room)), dim)));
        }
    }
    out
}

/// The `working` glyph: braille dots turning clockwise, one frame per `SPIN_TICK`.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPIN_TICK: Duration = Duration::from_millis(100);
/// How often an idle roster wakes, to keep the time-in-state column current.
pub const IDLE_TICK: Duration = Duration::from_secs(1);

/// The spinner frame for a time since the spinner started.
pub fn spinner_frame(elapsed: Duration) -> &'static str {
    let step = elapsed.as_millis() / SPIN_TICK.as_millis();
    // `% len` keeps the index small, so the conversion can't fail.
    let i = usize::try_from(step % SPINNER.len() as u128).unwrap_or(0);
    SPINNER[i]
}

/// How long the loop waits for input before redrawing: the spinner's tick
/// while any worker is working, otherwise the slow idle tick.
pub fn tick(rows: &[Row]) -> Duration {
    if rows.iter().any(|r| r.state == "working") {
        SPIN_TICK
    } else {
        IDLE_TICK
    }
}

/// A state's glyph and colour. `elapsed` drives the `working` spinner.
pub fn glyph(state: &str, elapsed: Duration) -> (&'static str, Style) {
    match state {
        "working" => (spinner_frame(elapsed), Style::new().fg(Color::Yellow)),
        "blocked" => ("◐", Style::new().fg(Color::Red)),
        "done" => ("✓", Style::new().fg(Color::Green)),
        "error" => ("✗", Style::new().fg(Color::Red)),
        "idle" => ("○", Style::new().add_modifier(Modifier::DIM)),
        _ => ("·", Style::new().add_modifier(Modifier::DIM)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Row;
    use crate::rex::fake::Fake;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::Path;

    fn row(block: &str, label: &str, state: &str, shown: bool) -> Row {
        Row {
            block: block.into(),
            label: label.into(),
            shown,
            state: state.into(),
            age: Some(75),
            msg: "Running tests".into(),
            task: None,
        }
    }

    fn app(rex: &dyn Rex) -> App<'_> {
        App {
            rex,
            session: "s".into(),
            model: Model::new("block:lead".into(), Some("block:me".into()), 0),
            selected: None,
            list_area: Rect::default(),
            offset: 0,
            line_owner: Vec::new(),
            width: 0,
            focused: false,
            last_click: None,
            error: None,
            config_error: None,
            pin_error: None,
            fit: Fit::default(),
            row_scale: size::RowScale::default(),
            view: None,
            spin_start: Instant::now(),
            opts: Options::default(),
            config_path: None,
            panel: None,
            panel_area: Rect::default(),
            pin: crate::lock::temp_dir("tui-pin").join("pin"),
            guard: pin::Guard::default(),
            leaving: None,
            follow: Follow::default(),
            reset_area: Rect::default(),
        }
    }

    #[test]
    fn puts_the_pinned_label_back_only_while_a_worker_is_live() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let dir = crate::lock::temp_dir("tui-on-label");
        a.pin = dir.join("pin");
        pin::write(&a.pin, "lead name").unwrap();
        // No workers yet: a rename sticks.
        a.on_label("worker-1");
        assert!(rex.params("session.set_label").is_empty());
        let meta = crate::model::Record {
            app: "rexlead".into(),
            state: "idle".into(),
            msg: Some(r#"{"lead":"block:lead"}"#.into()),
            updated_at: None,
        };
        a.model.apply(
            Event::Status {
                block: "block:w".into(),
                id: "meta".into(),
                record: meta,
            },
            0,
        );
        a.on_label("worker-1");
        assert_eq!(
            rex.params("session.set_label"),
            [json!({"session_id": "s", "label": "lead name"})]
        );
        // Our own rename comes back as the pinned label: nothing more.
        a.on_label("lead name");
        assert_eq!(rex.params("session.set_label").len(), 1);
    }

    fn live_worker(a: &mut App) {
        let meta = crate::model::Record {
            app: "rexlead".into(),
            state: "idle".into(),
            msg: Some(r#"{"lead":"block:lead"}"#.into()),
            updated_at: None,
        };
        a.model.apply(
            Event::Status {
                block: "block:w".into(),
                id: "meta".into(),
                record: meta,
            },
            0,
        );
    }

    #[test]
    fn the_give_up_notice_survives_a_refresh_in_the_same_pass() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let dir = crate::lock::temp_dir("tui-give-up");
        a.pin = dir.join("pin");
        pin::write(&a.pin, "lead name").unwrap();
        live_worker(&mut a);
        for _ in 0..=pin::MAX_REVERTS {
            a.on_label("someone");
        }
        a.sync(Needs {
            blocks: true,
            view: false,
        });
        assert!(
            a.shown_error()
                .is_some_and(|e| e.contains("leaving it as someone"))
        );
    }

    #[test]
    fn failed_reverts_do_not_count_towards_giving_up() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let dir = crate::lock::temp_dir("tui-failed-revert");
        a.pin = dir.join("pin");
        pin::write(&a.pin, "lead name").unwrap();
        live_worker(&mut a);
        rex.fail("session.set_label");
        for _ in 0..pin::MAX_REVERTS {
            a.on_label("worker");
        }
        rex.heal();
        a.on_label("worker");
        assert_eq!(rex.params("session.set_label").len(), pin::MAX_REVERTS + 1);
    }

    /// A server whose blocks and session label are shared state, so a
    /// roster and a spawn can run against it at once. `session.new_block`
    /// can be held at a gate, to stop a spawn part way.
    #[derive(Default)]
    struct Server {
        blocks: std::sync::Mutex<Vec<(String, String)>>,
        layers: std::sync::Mutex<usize>,
        /// Told when a spawn reaches `new_block`, which then waits for `gate`.
        reached: std::sync::Mutex<Option<mpsc::Sender<()>>>,
        gate: std::sync::Mutex<Option<Receiver<()>>>,
        /// The open lock, which must be held when the roster relabels itself.
        lock: std::sync::Mutex<Option<PathBuf>>,
    }

    impl Server {
        fn label_of(&self, id: &str) -> Option<String> {
            let blocks = self.blocks.lock().unwrap();
            blocks.iter().find(|(b, _)| b == id).map(|(_, l)| l.clone())
        }
    }

    impl Rex for Server {
        fn call(&self, method: &str, params: Value) -> Result<Value> {
            Ok(match method {
                "session.list_blocks" => {
                    let blocks = self.blocks.lock().unwrap();
                    let list: Vec<Value> = blocks
                        .iter()
                        .map(|(id, l)| json!({"block_id": id, "label": l, "placement": "detached"}))
                        .collect();
                    json!({ "blocks": list })
                }
                "session.view" => json!({"label": "lead name", "windows": [
                    {"window_id": "window:w", "layers": [
                        {"layer_id": "layer:t", "kind": "tiled", "blocks": [{"block_id": "block:lead"}]}
                    ]}
                ]}),
                "session.new_block" => {
                    if let Some(tx) = self.reached.lock().unwrap().take() {
                        tx.send(()).unwrap();
                    }
                    if let Some(rx) = self.gate.lock().unwrap().take() {
                        rx.recv().unwrap();
                    }
                    let label = params["label"].as_str().unwrap().to_owned();
                    self.blocks.lock().unwrap().push(("block:w2".into(), label));
                    json!({"block_id": "block:w2"})
                }
                "session.new_layer" => {
                    *self.layers.lock().unwrap() += 1;
                    let label = params["layout"]["block"]["label"].as_str().unwrap();
                    self.blocks
                        .lock()
                        .unwrap()
                        .push(("block:r2".into(), label.into()));
                    json!({"layer_id": "layer:r2"})
                }
                "session.set_block_label" => {
                    if let Some(lock) = &*self.lock.lock().unwrap() {
                        assert!(
                            Lock::try_acquire(lock).unwrap().is_none(),
                            "relabelled unlocked"
                        );
                    }
                    let id = params["block_id"].as_str().unwrap();
                    for (b, l) in self.blocks.lock().unwrap().iter_mut() {
                        if b == id {
                            *l = params["label"].as_str().unwrap().to_owned();
                        }
                    }
                    Value::Null
                }
                other => panic!("unexpected {other}"),
            })
        }

        fn block_call(&self, _: &str, _: &str, _: &str) -> Result<Value> {
            Ok(json!({"records": []}))
        }
    }

    /// A roster whose only worker has just closed, with the lead's name
    /// pinned: it has decided to exit.
    fn roster_whose_last_worker_closed<'a>(server: &'a Server, dir: &Path) -> App<'a> {
        server
            .blocks
            .lock()
            .unwrap()
            .push(("block:me".into(), launch::roster_label("block:lead")));
        let mut a = app(server);
        a.pin = pin::path(dir, "s", "block:lead");
        pin::write(&a.pin, "lead name").unwrap();
        a.model.refresh(server, "s", now()).unwrap();
        live_worker(&mut a);
        a.model.apply(Event::BlockClosed("block:w".into()), now());
        assert!(a.model.should_exit(now()));
        a
    }

    fn spawn_worker(server: &Server, dir: &Path) -> launch::Spawned {
        let spec = launch::Spawn {
            lead: "block:lead",
            name: "w2",
            task: None,
            cwd: None,
            cmd: &["true".into()],
        };
        launch::spawn(server, "s", &spec, Path::new("/bin/rr"), None, dir).unwrap()
    }

    #[test]
    fn a_worker_spawned_as_the_roster_decides_to_exit_keeps_it() {
        // The spawn has seen the roster and its pin, so it won't pin or open
        // one, and is about to start the worker when the roster decides its
        // last worker is gone. No `block_created` event reaches the roster.
        let dir = crate::lock::temp_dir("tui-exit-race");
        let server = Server::default();
        let mut a = roster_whose_last_worker_closed(&server, &dir);
        let (reached, at_new_block) = mpsc::channel();
        let (open_gate, gate) = mpsc::channel();
        *server.reached.lock().unwrap() = Some(reached);
        *server.gate.lock().unwrap() = Some(gate);
        let exit = std::thread::scope(|s| {
            let spawned = s.spawn(|| spawn_worker(&server, &dir));
            at_new_block.recv().unwrap();
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                open_gate.send(()).unwrap();
            });
            let exit = a.confirm_exit();
            if exit {
                a.leave();
            }
            let out = spawned.join().unwrap();
            assert!(out.pin_error.is_none() && out.open_error.is_none());
            exit
        });
        // The roster waited for the spawn, saw the new block and stayed,
        // keeping the pin and its label for the worker.
        assert!(!exit);
        assert_eq!(pin::read(&a.pin).as_deref(), Some("lead name"));
        assert_eq!(
            server.label_of("block:me"),
            Some(launch::roster_label("block:lead"))
        );
        assert_eq!(*server.layers.lock().unwrap(), 0);
    }

    #[test]
    fn a_worker_spawned_just_after_the_roster_leaves_gets_a_new_one_and_a_pin() {
        // The roster's block is still listed while it closes, but under its
        // old label it would pass for a live roster.
        let dir = crate::lock::temp_dir("tui-exit-race-after");
        let server = Server::default();
        let mut a = roster_whose_last_worker_closed(&server, &dir);
        assert!(a.confirm_exit());
        // No spawn can slip in between deciding to exit and leaving.
        let lock = launch::open_lock(&a.pin);
        *server.lock.lock().unwrap() = Some(lock.clone());
        assert!(Lock::try_acquire(&lock).unwrap().is_none());
        a.leave();
        assert!(Lock::try_acquire(&lock).unwrap().is_some());
        assert_eq!(pin::read(&a.pin), None);
        assert_eq!(server.label_of("block:me").as_deref(), Some(LEAVING_LABEL));
        let out = spawn_worker(&server, &dir);
        assert_eq!(out.json["roster_layer_id"], "layer:r2");
        assert_eq!(pin::read(&a.pin).as_deref(), Some("lead name"));
    }

    fn render(app: &mut App, rows: &[Row]) -> String {
        render_at(app, rows, 52, 5)
    }

    fn render_at(app: &mut App, rows: &[Row], width: u16, height: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| app.draw(f, rows)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn worker(label: &str, state: &str, shown: bool, age: Option<u64>, msg: &str) -> Row {
        Row {
            block: format!("block:{label}"),
            label: label.into(),
            shown,
            state: state.into(),
            age,
            msg: msg.into(),
            task: None,
        }
    }

    #[test]
    fn draws_compact_rows_with_no_frame() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [
            worker("alpha", "working", true, Some(75), "Running tests"),
            worker("beta", "done", false, Some(5), "PR raised"),
            worker(
                "gamma-long",
                "blocked",
                false,
                Some(4000),
                "Allow git push to the remote?",
            ),
        ];
        // Rendered within the first spinner tick, so frame 0. Unfocused: no
        // footer. No messages, and no time for the working worker.
        assert_eq!(
            render_at(&mut a, &rows, 40, 4),
            [
                " ⠋▸ alpha                               ",
                " ✓  beta       5s                       ",
                " ◐  gamma-long 1h                       ",
                "                                        ",
            ]
            .join("\n")
        );
    }

    #[test]
    fn label_width_follows_the_longest_label_within_bounds() {
        let r = |l: &str| worker(l, "idle", false, None, "");
        assert_eq!(
            Columns::for_rows(&[r("ab")]),
            Columns {
                label: LABEL_MIN,
                age: 0
            }
        );
        assert_eq!(Columns::for_rows(&[r("ab"), r("a-twelve-chr")]).label, 12);
        assert_eq!(Columns::for_rows(&[r(&"x".repeat(40))]).label, LABEL_MAX);
        let aged = [
            worker("a", "idle", false, Some(5), ""),
            worker("b", "idle", false, Some(700), ""),
        ];
        assert_eq!(Columns::for_rows(&aged).age, 3);
        // A working worker's time doesn't count towards the column.
        let busy = [
            worker("a", "idle", false, Some(5), ""),
            worker("b", "working", false, Some(700), ""),
        ];
        assert_eq!(Columns::for_rows(&busy).age, 2);
    }

    #[test]
    fn cuts_with_an_ellipsis() {
        assert_eq!(cut("hello", 5), "hello");
        assert_eq!(cut("hello", 4), "hel…");
        assert_eq!(cut("hello", 1), "…");
        assert_eq!(cut("hello", 0), "");
        // Wide characters count as two columns.
        assert_eq!(cut("日本語", 4), "日…");
        assert_eq!(cut("to the remote", 8), "to the…");
    }

    #[test]
    fn long_labels_are_cut_at_the_cap() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [worker(
            "a-very-long-worker-label-indeed",
            "idle",
            false,
            None,
            "hi",
        )];
        let line = render_at(&mut a, &rows, 30, 1);
        assert_eq!(line, " ○  a-very-long-worker-…      ");
    }

    #[test]
    fn footer_shows_only_while_focused() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [worker("alpha", "idle", false, Some(5), "hi")];
        assert!(!render_at(&mut a, &rows, 40, 2).contains("l lead"));
        a.on_terminal_event(&TermEvent::FocusGained, &rows);
        let screen = render_at(&mut a, &rows, 40, 2);
        assert_eq!(
            screen.lines().nth(1),
            Some("⏎ v o l r q                             ")
        );
        // Where the full hints fit, they show.
        let screen = render_at(&mut a, &rows, 50, 2);
        assert_eq!(screen.lines().nth(1).map(str::trim_end), Some(FOOTER));
        a.on_terminal_event(&TermEvent::FocusLost, &rows);
        assert!(!render_at(&mut a, &rows, 40, 2).contains("l lead"));
    }

    #[test]
    fn chevron_shows_only_while_focused() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [worker("alpha", "idle", false, Some(5), "hi")];
        assert!(render_at(&mut a, &rows, 30, 1).starts_with(" ○  alpha"));
        a.on_terminal_event(&TermEvent::FocusGained, &rows);
        let screen = render_at(&mut a, &rows, 30, 2);
        assert!(screen.starts_with("›○  alpha"), "{screen}");
        a.on_terminal_event(&TermEvent::FocusLost, &rows);
        assert!(render_at(&mut a, &rows, 30, 1).starts_with(" ○  alpha"));
    }

    #[test]
    fn scrolls_to_keep_the_selection_visible() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows: Vec<Row> = ["a1", "a2", "a3"]
            .iter()
            .map(|l| worker(l, "idle", false, None, ""))
            .collect();
        a.selected = Some("block:a3".into());
        let screen = render_at(&mut a, &rows, 12, 2);
        assert!(
            screen.lines().nth(1).unwrap().starts_with(" ○  a3"),
            "{screen}"
        );
        assert_eq!(a.offset, 1);
    }

    #[test]
    fn a_focus_change_resizes_once_for_the_footer() {
        let rex = Fake::default();
        // The pane has 3 rows, for 3 workers: it fits until the footer shows.
        sized(&rex, 3);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        a.fit_layer();
        assert!(rex.params("session.resize_layer").is_empty());

        a.on_terminal_event(&TermEvent::FocusGained, &[]);
        a.note_rows(3);
        assert!(a.fit.dirty);
        a.fit_layer();
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        // The pane now has the 4 rows it wants: no further resize.
        sized(&rex, 4);
        a.on_terminal_event(&TermEvent::Resize(40, 4), &[]);
        a.note_rows(3);
        a.fit_layer();
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        // Another loop pass with nothing new does nothing.
        a.note_rows(3);
        assert!(!a.fit.dirty);
    }

    #[test]
    fn keys_move_and_toggle() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [
            row("block:1", "a", "idle", false),
            row("block:2", "b", "idle", false),
        ];
        let key = |c| TermEvent::Key(KeyCode::Char(c).into());
        a.on_terminal_event(&key('j'), &rows);
        assert_eq!(a.selected.as_deref(), Some("block:2"));
        a.on_terminal_event(&key('j'), &rows);
        assert_eq!(a.selected.as_deref(), Some("block:2"));
        a.on_terminal_event(&TermEvent::Key(KeyCode::Up.into()), &rows);
        assert_eq!(a.selected.as_deref(), Some("block:1"));

        a.on_terminal_event(&TermEvent::Key(KeyCode::Enter.into()), &rows);
        assert_eq!(
            rex.methods(),
            [
                "session.move_block",
                "session.float_block",
                "session.focus_block"
            ]
        );
        assert_eq!(
            rex.params("session.move_block")[0]["anchor_block_id"],
            "block:lead"
        );

        a.on_terminal_event(&key('l'), &rows);
        assert_eq!(
            rex.params("session.focus_block")[1]["block_id"],
            "block:lead"
        );
        assert!(a.on_terminal_event(&key('q'), &rows));
    }

    #[test]
    fn enter_hides_a_shown_worker() {
        let rex = Fake::default();
        rex.on(
            "session.list_blocks",
            json!({"blocks": [{"block_id": "block:1", "label": "a", "placement": "placed"}]}),
        );
        let mut a = app(&rex);
        a.model.refresh(&rex, "s", 0).unwrap();
        rex.calls.borrow_mut().clear();
        a.toggle("block:1");
        assert_eq!(rex.methods(), ["session.detach_block"]);
    }

    #[test]
    fn double_click_toggles_the_row() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [
            row("block:1", "a", "idle", false),
            row("block:2", "b", "idle", false),
        ];
        render(&mut a, &rows);
        let click = TermEvent::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        a.on_terminal_event(&click, &rows);
        assert_eq!(a.selected.as_deref(), Some("block:2"));
        assert!(rex.methods().is_empty());
        a.on_terminal_event(&click, &rows);
        assert_eq!(rex.params("session.move_block")[0]["block_id"], "block:2");
    }

    fn sized(rex: &Fake, roster_rows: u16) {
        rex.on(
            "session.view",
            serde_json::from_str(include_str!("../testdata/view.json")).unwrap(),
        );
        rex.on(
            "block:lead size",
            json!({"rows": 25, "desired": [{"rows": 25}]}),
        );
        rex.on(
            "block:roster size",
            json!({"rows": roster_rows, "desired": [{"rows": roster_rows}]}),
        );
    }

    #[test]
    fn fits_the_layer_to_the_rows() {
        let rex = Fake::default();
        sized(&rex, 1);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        a.fit_layer();
        let p = &rex.params("session.resize_layer")[0];
        assert_eq!(p["layer_id"], "layer:float");
        // Lead: 0.5 of the window over 25 rows, so 0.02 a row; 2 rows missing.
        let h = p["bounds"][3].as_f64().unwrap();
        assert!((h - 0.16).abs() < 1e-9, "{h}");
    }

    #[test]
    fn fits_to_a_zoomed_lead() {
        let rex = Fake::default();
        sized(&rex, 1);
        let mut v: Value = serde_json::from_str(include_str!("../testdata/view.json")).unwrap();
        v["windows"][0]["zoomed_block_id"] = json!("block:lead");
        rex.on("session.view", v);
        // Zoomed, the lead's 50 rows fill the window: 0.02 a row.
        rex.on(
            "block:lead size",
            json!({"rows": 50, "desired": [{"rows": 50}]}),
        );
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        a.fit_layer();
        let h = rex.params("session.resize_layer")[0]["bounds"][3]
            .as_f64()
            .unwrap();
        assert!((h - 0.16).abs() < 1e-9, "{h}");
    }

    #[test]
    fn widens_to_a_zoomed_lead() {
        let rex = Fake::default();
        sized(&rex, 3);
        // The lead's cell is half the window, 100 columns wide; zoomed, it
        // draws 200 columns over the whole window: 0.005 a column.
        let mut v: Value = serde_json::from_str(include_str!("../testdata/view.json")).unwrap();
        v["windows"][0]["layers"][0]["layout"]["children"][0]["rect"] = json!([0, 0, 0.5, 1]);
        v["windows"][0]["zoomed_block_id"] = json!("block:lead");
        rex.on("session.view", v);
        rex.on(
            "block:lead size",
            json!({"rows": 50, "columns": 200, "desired": [{"rows": 50}]}),
        );
        rex.on(
            "block:roster size",
            json!({"rows": 3, "columns": 20, "desired": [{"rows": 3}]}),
        );
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        a.note_columns(size::MIN_COLUMNS);
        a.fit_layer();
        assert!(
            (a.follow.cell[0] - 0.005).abs() < 1e-9,
            "{:?}",
            a.follow.cell
        );
        let p = &rex.params("session.resize_layer")[0];
        let missing = f64::from(size::MIN_COLUMNS - 20);
        let w = p["bounds"][2].as_f64().unwrap();
        assert!((w - (0.27 + missing * 0.005)).abs() < 1e-9, "{w}");
    }

    #[test]
    fn does_not_fit_while_another_pane_is_zoomed_over_the_lead() {
        let rex = Fake::default();
        sized(&rex, 1);
        let mut v: Value = serde_json::from_str(include_str!("../testdata/view.json")).unwrap();
        v["windows"][0]["zoomed_block_id"] = json!("block:other");
        rex.on("session.view", v);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        a.fit_layer();
        assert!(rex.params("session.resize_layer").is_empty());
        // The fit waits for the lead to show again, however long that takes.
        a.fit.retry_at = None;
        rex.on(
            "session.view",
            serde_json::from_str(include_str!("../testdata/view.json")).unwrap(),
        );
        view_pass(&mut a);
        let h = resized(&rex, 0)[3];
        assert!((h - 0.16).abs() < 1e-9, "{h}");
    }

    #[test]
    fn widens_a_narrow_layer_and_keeps_its_height() {
        let rex = Fake::default();
        sized(&rex, 3);
        // Lead: the full window width over 100 columns, so 0.01 a column.
        rex.on(
            "block:lead size",
            json!({"rows": 25, "columns": 100, "desired": [{"rows": 25}]}),
        );
        rex.on(
            "block:roster size",
            json!({"rows": 3, "columns": 20, "desired": [{"rows": 3}]}),
        );
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        a.note_columns(size::MIN_COLUMNS);
        a.fit_layer();
        let p = &rex.params("session.resize_layer")[0];
        let b: Vec<f64> = p["bounds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        // 30 columns missing: 0.27 + 0.30 wide, right edge still at 0.99.
        assert!((b[2] - 0.57).abs() < 1e-9, "{b:?}");
        assert!((b[0] + b[2] - 0.99).abs() < 1e-9, "{b:?}");
        assert_eq!(b[1], 0.02);
        assert!((b[3] - 0.12).abs() < 1e-9, "{b:?}");
    }

    #[test]
    fn grows_a_layer_squeezed_below_a_row() {
        // Rex reports one row, but at 15px of a 34px cell it is half hidden.
        let rex = Fake::default();
        sized(&rex, 1);
        rex.on(
            "block:lead size",
            json!({"rows": 25, "cell_height_px": 34, "desired": [{"rows": 25}]}),
        );
        rex.on(
            "block:roster size",
            json!({"rows": 1, "cell_height_px": 15, "desired": [{"rows": 1}]}),
        );
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(1);
        a.fit_layer();
        let p = &rex.params("session.resize_layer")[0];
        // A row is 0.02 of the window: one row missing.
        let h = p["bounds"][3].as_f64().unwrap();
        assert!((h - 0.14).abs() < 1e-9, "{h}");
    }

    #[test]
    fn does_not_resize_when_it_fits_or_before_the_pane_is_drawn() {
        let rex = Fake::default();
        sized(&rex, 3);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        a.fit_layer();
        assert!(rex.params("session.resize_layer").is_empty());

        rex.on(
            "block:roster size",
            json!({"rows": 24, "columns": 80, "desired": []}),
        );
        a.fit.dirty = true;
        a.fit_layer();
        assert!(rex.params("session.resize_layer").is_empty());
    }

    #[test]
    fn stops_resizing_after_max_fits() {
        let rex = Fake::default();
        // The rows never change, as if every resize lands on the same height.
        sized(&rex, 1);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        for _ in 0..10 {
            a.fit.dirty = true;
            a.fit_layer();
        }
        assert_eq!(
            rex.params("session.resize_layer").len(),
            usize::from(MAX_FITS)
        );
        // Our own resize echoing back as a terminal resize grants no new attempts.
        a.on_terminal_event(&TermEvent::Resize(50, 5), &[]);
        a.fit_layer();
        assert_eq!(
            rex.params("session.resize_layer").len(),
            usize::from(MAX_FITS)
        );
    }

    /// A view with the lead tiled at `lead` and the roster floating at `roster`.
    fn laid_out(lead: [f64; 4], roster: [f64; 4]) -> Value {
        json!({"windows": [{"window_id": "window:1", "layers": [
            {"layer_id": "layer:tiled", "kind": "tiled", "bounds": [0, 0, 1, 1],
             "layout": {"rect": [0, 0, 1, 1], "children": [
                {"rect": lead, "block_id": "block:lead"},
                {"rect": [0.5, 0, 0.5, 1], "block_id": "block:other"}]},
             "blocks": [{"block_id": "block:lead"}, {"block_id": "block:other"}]},
            {"layer_id": "layer:float", "kind": "floating", "bounds": roster,
             "layout": {"rect": roster, "block_id": "block:roster"},
             "blocks": [{"block_id": "block:roster"}]}
        ]}]})
    }

    const START: [f64; 4] = [0.72, 0.02, 0.27, 0.12];
    const FULL: [f64; 4] = [0.0, 0.0, 1.0, 1.0];
    const LEFT_HALF: [f64; 4] = [0.0, 0.0, 0.5, 1.0];

    /// A roster whose lead fills the window, and the roster in its corner.
    fn following(rex: &Fake) -> App<'_> {
        rex.on("session.view", laid_out(FULL, START));
        let mut a = app(rex);
        a.model.me = Some("block:roster".into());
        view_pass(&mut a);
        assert!(rex.params("session.resize_layer").is_empty());
        a
    }

    /// What a `session_view_changed` event leads to: a fresh view, then a fit.
    fn view_pass(a: &mut App) {
        a.sync(Needs {
            view: true,
            blocks: false,
        });
        a.fit_if_due();
    }

    /// The bounds of the `n`th `session.resize_layer` call.
    fn resized(rex: &Fake, n: usize) -> [f64; 4] {
        let p = &rex.params("session.resize_layer")[n];
        let b: Vec<f64> = p["bounds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        [b[0], b[1], b[2], b[3]]
    }

    fn close(a: [f64; 4], b: [f64; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn follows_the_lead_when_it_splits() {
        let rex = Fake::default();
        let mut a = following(&rex);
        // The lead splits: it now has the left half of the window.
        rex.on("session.view", laid_out(LEFT_HALF, START));
        view_pass(&mut a);
        let moved = resized(&rex, 0);
        assert!(close(moved, [0.22, 0.02, 0.27, 0.12]), "{moved:?}");
        assert_eq!(
            rex.params("session.resize_layer")[0]["layer_id"],
            "layer:float"
        );
        // Its own move coming back in the view isn't taken for a drag.
        rex.on("session.view", laid_out(LEFT_HALF, moved));
        view_pass(&mut a);
        assert!(!a.follow.dragged);
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        // The other pane closes: back to the window's corner.
        rex.on("session.view", laid_out(FULL, moved));
        view_pass(&mut a);
        assert!(close(resized(&rex, 1), START), "{:?}", resized(&rex, 1));
    }

    #[test]
    fn stays_where_it_was_dragged() {
        let rex = Fake::default();
        let mut a = following(&rex);
        let dropped = [0.3, 0.4, 0.27, 0.12];
        rex.on("session.view", laid_out(FULL, dropped));
        view_pass(&mut a);
        assert!(a.follow.dragged);
        rex.on("session.view", laid_out(LEFT_HALF, dropped));
        view_pass(&mut a);
        assert!(rex.params("session.resize_layer").is_empty());
    }

    #[test]
    fn a_resize_that_keeps_the_corner_is_not_a_drag() {
        let rex = Fake::default();
        let mut a = following(&rex);
        // Pulled out from the bottom left corner: the top right stays.
        rex.on("session.view", laid_out(FULL, [0.5, 0.02, 0.49, 0.3]));
        view_pass(&mut a);
        assert!(!a.follow.dragged);
        rex.on("session.view", laid_out(LEFT_HALF, [0.5, 0.02, 0.49, 0.3]));
        view_pass(&mut a);
        let moved = resized(&rex, 0);
        assert!(close(moved, [0.0, 0.02, 0.49, 0.3]), "{moved:?}");
    }

    #[test]
    fn does_not_move_unless_the_lead_is_tiled_in_its_window() {
        let rex = Fake::default();
        let mut a = following(&rex);
        // The lead floats.
        let mut v = laid_out(LEFT_HALF, START);
        let tiled = &mut v["windows"][0]["layers"][0];
        tiled["layout"]["children"][0]["block_id"] = json!("block:x");
        tiled["blocks"][0]["block_id"] = json!("block:x");
        v["windows"][0]["layers"]
            .as_array_mut()
            .unwrap()
            .push(json!(
                {"layer_id": "layer:lead", "kind": "floating", "bounds": [0.1, 0.1, 0.3, 0.3],
                 "layout": {"rect": [0.1, 0.1, 0.3, 0.3], "block_id": "block:lead"},
                 "blocks": [{"block_id": "block:lead"}]}
            ));
        rex.on("session.view", v);
        view_pass(&mut a);
        // The lead is tiled, but in another window.
        let mut v = laid_out(LEFT_HALF, START);
        let float = v["windows"][0]["layers"][1].take();
        v["windows"][0]["layers"].as_array_mut().unwrap().pop();
        v["windows"]
            .as_array_mut()
            .unwrap()
            .push(json!({"window_id": "window:2", "layers": [float]}));
        rex.on("session.view", v);
        view_pass(&mut a);
        // Another pane is zoomed over the lead.
        let mut v = laid_out(LEFT_HALF, START);
        v["windows"][0]["zoomed_block_id"] = json!("block:other");
        rex.on("session.view", v);
        view_pass(&mut a);
        assert!(rex.params("session.resize_layer").is_empty());
        assert!(!a.follow.dragged);
    }

    #[test]
    fn rounding_by_rex_is_not_a_drag() {
        let rex = Fake::default();
        let mut a = following(&rex);
        // Measured: 100 columns and 50 rows over the window.
        rex.on(
            "block:lead size",
            json!({"rows": 50, "columns": 100, "desired": [{"rows": 50}]}),
        );
        rex.on(
            "block:roster size",
            json!({"rows": 4, "columns": 30, "desired": [{"rows": 4}]}),
        );
        a.note_rows(4);
        a.fit_if_due();
        assert!(a.follow.cell[0] > 0.0 && a.follow.cell[1] > 0.0);
        let [x, y, w, h] = a.follow.set.unwrap();
        // Rex snaps the corner by a third of a column and of a row.
        rex.on(
            "session.view",
            laid_out(FULL, [x + 0.0033, y + 0.0067, w, h]),
        );
        view_pass(&mut a);
        assert!(!a.follow.dragged);
        // A drag of one column is still a drag.
        rex.on("session.view", laid_out(FULL, [x - 0.01, y, w, h]));
        view_pass(&mut a);
        assert!(a.follow.dragged);
    }

    /// The lead fills a window of 100 columns and 50 rows; the roster is at
    /// `bounds`, `cols` columns wide, and already has the rows it wants.
    fn columns_at(rex: &Fake, a: &App, bounds: [f64; 4], cols: u16) {
        rex.on("session.view", laid_out(FULL, bounds));
        rex.on(
            "block:lead size",
            json!({"rows": 50, "columns": 100, "desired": [{"rows": 50}]}),
        );
        let rows = a.fit.want;
        rex.on(
            "block:roster size",
            json!({"rows": rows, "columns": cols, "desired": [{"rows": rows}]}),
        );
    }

    /// One pass of the run loop: note what the rows need, then fit.
    fn width_pass(a: &mut App, rows: &[Row]) {
        a.note_rows(total_lines(rows, a.width, a.expanded()));
        a.note_columns(a.wanted_columns(rows));
        a.sync(Needs {
            view: true,
            blocks: false,
        });
        a.fit_if_due();
    }

    /// The roster's terminal now has `cols` columns at the last bounds it set.
    fn landed(rex: &Fake, a: &App, cols: u16) -> [f64; 4] {
        let n = rex.params("session.resize_layer").len();
        let b = resized(rex, n - 1);
        columns_at(rex, a, b, cols);
        b
    }

    const RIGHT: f64 = 0.72 + 0.27;

    #[test]
    fn wanted_columns_follow_the_view_and_the_panel() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let short = [row("block:1", "a", "idle", false)];
        let long = [row("block:1", "a-twenty-column-name", "idle", false)];
        // Compact: the widest row (glyph, marker, label, time) and a column.
        assert_eq!(a.wanted_columns(&short), size::COMPACT_MIN_COLUMNS);
        assert_eq!(a.wanted_columns(&long), 4 + 20 + 4 + 1);
        a.opts.border = Border::Rounded;
        assert_eq!(a.wanted_columns(&long), 4 + 20 + 4 + 1 + 2);
        a.panel = Some(Panel {
            cursor: 0,
            start: a.opts,
        });
        assert_eq!(a.wanted_columns(&short), size::PANEL_COLUMNS + 2);
        a.panel = None;
        a.opts.view = View::Expanded;
        assert_eq!(a.wanted_columns(&short), size::MIN_COLUMNS);
    }

    #[test]
    fn the_compact_width_holds_as_idle_times_change() {
        let rex = Fake::default();
        let a = app(&rex);
        let at = |state: &str, age| {
            let mut r = row("block:1", "a-twenty-column-name", state, false);
            r.age = Some(age);
            a.wanted_columns(&[r])
        };
        let w = at("idle", 9);
        assert_eq!(at("idle", 10), w);
        assert_eq!(at("idle", 600), w);
        assert_eq!(at("working", 10), w);
    }

    #[test]
    fn compact_narrows_to_its_rows_and_v_refits_both_ways() {
        let rex = Fake::default();
        // Opened 50 columns wide.
        rex.on(
            "session.view",
            laid_out(FULL, [RIGHT - 0.5, 0.02, 0.5, 0.12]),
        );
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        view_pass(&mut a);
        let rows = [row("block:1", "a-twenty-column-name", "idle", false)];
        a.note_rows(total_lines(&rows, a.width, a.expanded()));
        columns_at(&rex, &a, [RIGHT - 0.5, 0.02, 0.5, 0.12], 50);
        width_pass(&mut a, &rows);
        // 28 columns at 0.01 each, with the right edge where it was.
        let b = landed(&rex, &a, 29);
        assert!(close(b, [RIGHT - 0.29, 0.02, 0.29, 0.12]), "{b:?}");
        width_pass(&mut a, &rows);
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        assert!(!a.follow.dragged && !a.follow.user_width);
        // v: the expanded view widens to 50 columns.
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('v').into()), &rows);
        width_pass(&mut a, &rows);
        let b = landed(&rex, &a, 50);
        assert!(
            (b[2] - 0.5).abs() < 1e-9 && (b[0] + b[2] - RIGHT).abs() < 1e-9,
            "{b:?}"
        );
        // And back.
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('v').into()), &rows);
        width_pass(&mut a, &rows);
        let b = landed(&rex, &a, 29);
        assert!(close(b, [RIGHT - 0.29, 0.02, 0.29, 0.12]), "{b:?}");
        assert!(!a.follow.dragged && !a.follow.user_width);
    }

    #[test]
    fn a_roster_widened_by_hand_is_not_narrowed_until_r() {
        let rex = Fake::default();
        let mut a = following(&rex);
        let rows = [row("block:1", "a-twenty-column-name", "idle", false)];
        a.note_rows(total_lines(&rows, a.width, a.expanded()));
        columns_at(&rex, &a, START, 27);
        width_pass(&mut a, &rows);
        let b = landed(&rex, &a, 29);
        // The user drags the left edge out to 60 columns.
        columns_at(&rex, &a, [RIGHT - 0.6, b[1], 0.6, b[3]], 60);
        width_pass(&mut a, &rows);
        assert!(a.follow.user_width && !a.follow.dragged);
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        // A row count change refits the height, not the width.
        a.fit.dirty = true;
        a.fit_if_due();
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        // r lets the width go.
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('r').into()), &rows);
        a.fit_if_due();
        let b = resized(&rex, 1);
        assert!(close(b, [RIGHT - 0.29, 0.02, 0.29, 0.12]), "{b:?}");
    }

    #[test]
    fn r_puts_a_dragged_roster_back_and_it_follows_again() {
        let rex = Fake::default();
        let mut a = following(&rex);
        let dropped = [0.3, 0.4, 0.27, 0.12];
        rex.on("session.view", laid_out(FULL, dropped));
        view_pass(&mut a);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('r').into()), &[]);
        a.fit_if_due();
        assert!(close(resized(&rex, 0), START), "{:?}", resized(&rex, 0));
        assert!(!a.follow.dragged);
        rex.on("session.view", laid_out(FULL, START));
        view_pass(&mut a);
        rex.on("session.view", laid_out(LEFT_HALF, START));
        view_pass(&mut a);
        let moved = resized(&rex, 1);
        assert!(close(moved, [0.22, 0.02, 0.27, 0.12]), "{moved:?}");
    }

    #[test]
    fn a_move_alone_leaves_the_next_terminal_resize_to_the_user() {
        let rex = Fake::default();
        let mut a = following(&rex);
        a.fit.attempts = MAX_FITS;
        rex.on("session.view", laid_out(LEFT_HALF, START));
        view_pass(&mut a);
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        // Moving doesn't resize the terminal, so this one is the user's.
        a.on_terminal_event(&TermEvent::Resize(60, 5), &[]);
        assert_eq!(a.fit.attempts, 0);
    }

    #[test]
    fn r_does_nothing_while_following() {
        let rex = Fake::default();
        let mut a = following(&rex);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('r').into()), &[]);
        a.fit_if_due();
        assert!(rex.params("session.resize_layer").is_empty());
    }

    #[test]
    fn the_reset_control_shows_while_dragged_and_a_click_puts_it_back() {
        for border in [Border::None, Border::Rounded] {
            let rex = Fake::default();
            let mut a = following(&rex);
            a.opts.border = border;
            let rows = [row("block:1", "a", "idle", false)];
            let screen = render(&mut a, &rows);
            assert!(!screen.contains('↺'), "{screen}");
            rex.on("session.view", laid_out(FULL, [0.3, 0.4, 0.27, 0.12]));
            view_pass(&mut a);
            let screen = render(&mut a, &rows);
            let top = screen.lines().next().unwrap();
            let reset_at = if border == Border::None { 45 } else { 43 };
            assert_eq!(
                top.chars()
                    .skip(reset_at)
                    .collect::<String>()
                    .trim_end_matches(['╮', ' ']),
                "↺ reset",
                "{screen}"
            );
            let click = TermEvent::Mouse(ratatui::crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 47,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            a.on_terminal_event(&click, &rows);
            // The click doesn't select or show a worker.
            assert_eq!(a.selected, None);
            assert!(a.last_click.is_none());
            a.fit_if_due();
            assert!(close(resized(&rex, 0), START), "{:?}", resized(&rex, 0));
            let screen = render(&mut a, &rows);
            assert!(!screen.contains('↺'), "{screen}");
        }
    }

    #[test]
    fn the_reset_control_never_covers_row_text() {
        let long = "a-very-long-worker-label-that-runs-on";
        let rows = [row("block:1", long, "idle", false)];
        for width in [20, 26] {
            let rex = Fake::default();
            let mut a = following(&rex);
            let before = render_at(&mut a, &rows, width, 5);
            rex.on("session.view", laid_out(FULL, [0.3, 0.4, 0.27, 0.12]));
            view_pass(&mut a);
            let after = render_at(&mut a, &rows, width, 5);
            let reset = if width >= 40 { RESET } else { RESET_SHORT };
            // The label gives up the room, cut with `…`; the glyph and the
            // idle time stay whole, with a space before the control.
            let top = after.lines().next().unwrap();
            let label = usize::from(width) - reset.width() - 1 - " ○   1m".width();
            let cut_label = cut(long, label);
            assert!(cut_label.ends_with('…'), "{cut_label}");
            assert_eq!(top, format!(" ○  {cut_label} 1m {reset}"), "{after}");
            // The lines below, and how many there are, don't change.
            assert_eq!(
                after.lines().skip(1).collect::<Vec<_>>(),
                before.lines().skip(1).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn with_a_border_the_reset_control_leaves_the_first_row_whole() {
        let rows = [row("block:1", "a-very-long-worker-label", "idle", false)];
        let rex = Fake::default();
        let mut a = following(&rex);
        a.opts.border = Border::Rounded;
        let before = render_at(&mut a, &rows, 26, 5);
        rex.on("session.view", laid_out(FULL, [0.3, 0.4, 0.27, 0.12]));
        view_pass(&mut a);
        let after = render_at(&mut a, &rows, 26, 5);
        assert!(after.contains('↺'), "{after}");
        assert_eq!(after.lines().nth(1), before.lines().nth(1), "{after}");
    }

    #[test]
    fn spinner_frame_follows_elapsed_time() {
        let ms = Duration::from_millis;
        assert_eq!(spinner_frame(ms(0)), "⠋");
        assert_eq!(spinner_frame(ms(99)), "⠋");
        assert_eq!(spinner_frame(ms(100)), "⠙");
        assert_eq!(spinner_frame(ms(950)), "⠏");
        // Wraps after the last frame.
        assert_eq!(spinner_frame(ms(1000)), "⠋");
        assert_eq!(spinner_frame(ms(1_234_567)), SPINNER[5]);
        // Only `working` spins.
        assert_eq!(glyph("working", ms(300)).0, "⠸");
        assert_eq!(glyph("blocked", ms(300)).0, "◐");
        assert_eq!(glyph("blocked", ms(700)).0, "◐");
    }

    #[test]
    fn ticks_fast_only_while_a_worker_is_working() {
        assert_eq!(tick(&[]), IDLE_TICK);
        let idle = [
            row("block:1", "a", "idle", false),
            row("block:2", "b", "done", false),
        ];
        assert_eq!(tick(&idle), IDLE_TICK);
        let busy = [
            row("block:1", "a", "idle", false),
            row("block:2", "b", "working", false),
        ];
        assert_eq!(tick(&busy), SPIN_TICK);
    }

    #[test]
    fn redrawing_a_spinner_frame_changes_only_that_cell() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [
            row("block:1", "alpha", "working", false),
            row("block:2", "beta", "done", false),
        ];
        let mut t = Terminal::new(TestBackend::new(52, 4)).unwrap();
        t.draw(|f| a.draw(f, &rows)).unwrap();
        let before = t.backend().buffer().clone();
        a.spin_start -= SPIN_TICK;
        t.draw(|f| a.draw(f, &rows)).unwrap();
        let after = t.backend().buffer().clone();
        let changed: Vec<_> = before
            .diff(&after)
            .into_iter()
            .map(|(x, y, _)| (x, y))
            .collect();
        // ratatui writes only the diff, so the frame is one cell: no flicker.
        assert_eq!(changed, [(1, 0)]);
    }

    #[test]
    fn wraps_words_and_caps_with_an_ellipsis() {
        assert_eq!(wrap("one two three", 7, 3), ["one two", "three"]);
        assert_eq!(wrap("", 10, 3), Vec::<String>::new());
        // A word longer than the line is broken.
        assert_eq!(wrap("abcdefghij", 4, 3), ["abcd", "efgh", "ij"]);
        // Too long for three lines: the third ends in an ellipsis.
        assert_eq!(
            wrap("aa bb cc dd ee ff gg", 5, 3),
            ["aa bb", "cc dd", "ee f…"]
        );
        assert_eq!(wrap("text", 0, 3), Vec::<String>::new());
    }

    #[test]
    fn ellipsis_never_sits_alone_on_a_line() {
        let path = "/home/u/dev/smd/.worktrees/feat-max-width;";
        let w = path.width();
        let cut_path = "/home/u/dev/smd/.worktrees/feat-max-width…";
        // The message ends in an ellipsis word after a long unbreakable word:
        // the last line is full, so its final character gives way.
        let msg = format!("Running cd {path} …");
        assert_eq!(wrap(&msg, w, 3), ["Running cd", cut_path]);
        // Room on the last line: the ellipsis is appended.
        assert_eq!(
            wrap(&msg, w + 5, 3),
            ["Running cd".to_owned(), format!("{path}…")]
        );
        // Our own cut, with the last kept line full of one long word.
        let msg = format!("Running cd {path} sed -n 1,40p");
        assert_eq!(wrap(&msg, w, 2), ["Running cd", cut_path]);
        assert_eq!(wrap("abcd efgh …", 4, 2), ["abcd", "efg…"]);
        // A message that is only an ellipsis stays as it is.
        assert_eq!(wrap("…", 4, 3), ["…"]);
        // Compact view: `cut` puts the ellipsis on the text.
        assert_eq!(cut("abcdef …", 6), "abcde…");
        assert_eq!(cut("abc …", 5), "abc …");
    }

    #[test]
    fn ellipsis_word_over_the_line_cap_is_not_added_twice() {
        assert_eq!(wrap("aa bb cc dd …", 5, 2), ["aa bb", "cc d…"]);
    }

    fn tasked(label: &str, msg: &str, task: Option<&str>) -> Row {
        Row {
            task: task.map(Into::into),
            ..worker(label, "blocked", false, Some(5), msg)
        }
    }

    #[test]
    fn worker_height_in_each_view() {
        let short = tasked("a", "Allow push?", Some("fix login"));
        let long = tasked("b", &"word ".repeat(40), Some("bump deps"));
        let same = tasked("c", "fix login", Some("fix login"));
        let none = tasked("d", "", None);
        for r in [&short, &long, &same, &none] {
            assert_eq!(worker_height(r, 30, false), 1);
        }
        // Header, one msg line, the task.
        assert_eq!(worker_height(&short, 30, true), 3);
        // Header, msg capped at three lines, the task.
        assert_eq!(worker_height(&long, 30, true), 1 + MSG_LINES + 1);
        // The task is left out when it is the message.
        assert_eq!(worker_height(&same, 30, true), 2);
        assert_eq!(worker_height(&none, 30, true), 1);

        let rows = [short, long, same, none];
        assert_eq!(total_lines(&rows, 30, false), 4);
        assert_eq!(total_lines(&rows, 30, true), 3 + 5 + 2 + 1);
        assert_eq!(
            size::wanted_rows(total_lines(&rows, 30, true), true, Border::None),
            12
        );
    }

    #[test]
    fn draws_the_expanded_view() {
        let rex = Fake::default();
        let mut a = app(&rex);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('v').into()), &[]);
        assert!(a.expanded());
        let rows = [
            tasked(
                "alpha",
                "Allow git push to the remote branch now?",
                Some("fix login"),
            ),
            tasked("beta", "PR raised", None),
        ];
        assert_eq!(
            render_at(&mut a, &rows, 25, 6),
            [
                " ◐  alpha  5s            ",
                "    Allow git push to the",
                "    remote branch now?   ",
                "    fix login            ",
                " ◐  beta   5s            ",
                "    PR raised            ",
            ]
            .join("\n")
        );
    }

    #[test]
    fn clicks_on_any_line_pick_the_worker() {
        let rex = Fake::default();
        let mut a = app(&rex);
        a.opts.view = View::Expanded;
        let rows = [
            tasked("alpha", "one", Some("task a")),
            tasked("beta", "two", None),
        ];
        render_at(&mut a, &rows, 24, 6);
        assert_eq!(a.line_owner, [0, 0, 0, 1, 1]);
        assert_eq!(a.row_at(3, 2), Some(0));
        assert_eq!(a.row_at(3, 3), Some(1));
        assert_eq!(a.row_at(3, 4), Some(1));
        assert_eq!(a.row_at(3, 5), None);
        // j moves by worker, not by line.
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('j').into()), &rows);
        assert_eq!(a.selected.as_deref(), Some("block:beta"));
    }

    #[test]
    fn toggling_the_view_or_a_rewrap_resizes_once() {
        let rex = Fake::default();
        sized(&rex, 2);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.width = 24;
        let mut rows = vec![
            tasked("alpha", "short", None),
            tasked("beta", "short", None),
        ];
        a.note_rows(total_lines(&rows, a.width, a.expanded()));
        a.fit_layer();
        assert!(rex.params("session.resize_layer").is_empty());

        a.opts.view = View::Expanded;
        a.note_rows(total_lines(&rows, a.width, a.expanded()));
        a.fit_layer();
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        sized(&rex, 4);
        a.note_rows(total_lines(&rows, a.width, a.expanded()));
        assert!(!a.fit.dirty);

        // A longer message wraps to two lines: one more resize.
        rows[0].msg = "a message that needs two lines".into();
        a.note_rows(total_lines(&rows, a.width, a.expanded()));
        assert!(a.fit.dirty);
        a.fit_layer();
        assert_eq!(rex.params("session.resize_layer").len(), 2);
    }

    #[test]
    fn errors_show_even_without_focus() {
        // A background refresh fails while a worker has focus.
        let rex = Fake::default();
        let mut a = app(&rex);
        a.error = Some("calling session.view: boom".into());
        let rows = [worker("alpha", "idle", false, Some(5), "hi")];
        let screen = render_at(&mut a, &rows, 40, 2);
        assert!(screen.contains("boom"), "{screen}");
        // And the height makes room for it.
        a.note_rows(1);
        assert_eq!(a.fit.want, 2);
    }

    #[test]
    fn a_tall_selected_worker_keeps_its_header_on_screen() {
        // A 5-line worker in a 3-line pane.
        let rex = Fake::default();
        let mut a = app(&rex);
        a.opts.view = View::Expanded;
        let rows = [
            tasked("alpha", "one", None),
            tasked("beta", &"word ".repeat(40), Some("task")),
        ];
        a.selected = Some("block:beta".into());
        let screen = render_at(&mut a, &rows, 24, 3);
        assert!(
            screen.lines().next().unwrap().starts_with(" ◐  beta"),
            "{screen}"
        );
    }

    #[test]
    fn panel_is_a_state_machine() {
        let mut opts = Options::default();
        let mut p = Panel::new(opts);
        assert_eq!(p.key(KeyCode::Enter, &mut opts), PanelAction::Stay);
        assert_eq!(opts.border, Border::Rounded);
        p.key(KeyCode::Char('j'), &mut opts);
        assert_eq!(p.cursor, 1);
        p.key(KeyCode::Down, &mut opts);
        p.key(KeyCode::Down, &mut opts);
        p.key(KeyCode::Down, &mut opts);
        assert_eq!(p.cursor, 3, "stops at the last option");
        p.key(KeyCode::Up, &mut opts);
        p.key(KeyCode::Up, &mut opts);
        p.key(KeyCode::Char(' '), &mut opts);
        assert_eq!(opts.view, View::Expanded);
        p.key(KeyCode::Left, &mut opts);
        assert_eq!(opts.view, View::Compact);
        // Back where it started: only the border changed.
        assert_eq!(p.changed(&opts), [Key::Border]);
        p.key(KeyCode::Up, &mut opts);
        p.key(KeyCode::Right, &mut opts);
        assert!(p.changed(&opts).is_empty());
        p.click(1, &mut opts);
        assert_eq!((p.cursor, opts.view), (1, View::Expanded));
        p.click(9, &mut opts);
        assert_eq!(
            opts.view,
            View::Expanded,
            "a click past the options does nothing"
        );
        assert_eq!(p.key(KeyCode::Esc, &mut opts), PanelAction::Close);
        assert_eq!(p.key(KeyCode::Char('o'), &mut opts), PanelAction::Close);
    }

    #[test]
    fn draws_the_options_panel() {
        let rex = Fake::default();
        let mut a = app(&rex);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('o').into()), &[]);
        assert!(a.panel.is_some());
        assert_eq!(
            render_at(&mut a, &[], 32, 6),
            [
                "Options                         ",
                "›border  none                   ",
                " view    compact                ",
                " watch   true                   ",
                " notify  none                   ",
                "⏎ change · esc save and close   ",
            ]
            .join("\n")
        );
        // q doesn't quit from the panel; Esc closes it.
        assert!(!a.on_terminal_event(&TermEvent::Key(KeyCode::Char('q').into()), &[]));
        a.on_terminal_event(&TermEvent::Key(KeyCode::Esc.into()), &[]);
        assert!(a.panel.is_none());
    }

    #[test]
    fn draws_a_rounded_border_in_both_views() {
        let rex = Fake::default();
        let mut a = app(&rex);
        a.opts.border = Border::Rounded;
        a.focused = true;
        let rows = [tasked("alpha", "Running tests", Some("fix login"))];
        assert_eq!(
            render_at(&mut a, &rows, 30, 3),
            [
                "╭ workers ───────────────────╮",
                "│›◐  alpha  5s               │",
                "╰⏎ v o l r q─────────────────╯",
            ]
            .join("\n")
        );
        a.opts.view = View::Expanded;
        assert_eq!(
            render_at(&mut a, &rows, 30, 5),
            [
                "╭ workers ───────────────────╮",
                "│›◐  alpha  5s               │",
                "│    Running tests           │",
                "│    fix login               │",
                "╰⏎ v o l r q─────────────────╯",
            ]
            .join("\n")
        );
    }

    #[test]
    fn sizes_for_the_panel_and_both_borders() {
        let n = Border::None;
        let r = Border::Rounded;
        // The panel: heading, four options, hints; no separate footer.
        assert_eq!(size::wanted_rows(PANEL_LINES, false, n), 6);
        assert_eq!(size::wanted_rows(PANEL_LINES, false, r), 8);
        let rex = Fake::default();
        let mut a = app(&rex);
        a.focused = true;
        a.panel = Some(Panel::new(a.opts));
        assert!(!a.footer_shown());
        a.note_rows(PANEL_LINES);
        assert_eq!(a.fit.want, 6);
    }

    #[test]
    fn a_border_change_in_the_panel_resizes_once_and_saves() {
        let rex = Fake::default();
        sized(&rex, 6);
        let dir = std::env::temp_dir().join(format!("rexroster-tui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.config_path = Some(dir.join("config.toml"));
        let key = |c| TermEvent::Key(KeyCode::Char(c).into());
        a.on_terminal_event(&key('o'), &[]);
        a.note_rows(PANEL_LINES);
        a.fit_layer();
        assert!(
            rex.params("session.resize_layer").is_empty(),
            "6 rows fit the panel"
        );
        a.on_terminal_event(&TermEvent::Key(KeyCode::Enter.into()), &[]);
        assert_eq!(a.opts.border, Border::Rounded);
        a.note_rows(PANEL_LINES);
        a.fit_layer();
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        sized(&rex, 8);
        a.note_rows(PANEL_LINES);
        assert!(!a.fit.dirty, "no second resize once it fits");

        a.on_terminal_event(&key('o'), &[]);
        assert!(a.panel.is_none());
        assert_eq!(a.shown_error(), None);
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert_eq!(text, "border = \"rounded\"\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_view_option_sets_the_current_view_too() {
        let rex = Fake::default();
        let mut a = app(&rex);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('o').into()), &[]);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Down.into()), &[]);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Enter.into()), &[]);
        assert!(a.expanded());
        // With nowhere to save, closing reports it.
        a.on_terminal_event(&TermEvent::Key(KeyCode::Esc.into()), &[]);
        assert!(a.shown_error().unwrap().contains("no config directory"));
    }

    #[test]
    fn cycling_the_view_away_and_back_keeps_the_current_view() {
        // Compact -> expanded -> compact in the panel.
        let rex = Fake::default();
        let mut a = app(&rex);
        let key = |c| TermEvent::Key(c);
        a.on_terminal_event(&key(KeyCode::Char('o').into()), &[]);
        a.on_terminal_event(&key(KeyCode::Down.into()), &[]);
        a.on_terminal_event(&key(KeyCode::Enter.into()), &[]);
        assert!(a.expanded());
        a.on_terminal_event(&key(KeyCode::Left.into()), &[]);
        assert_eq!(a.opts.view, View::Compact);
        assert!(!a.expanded());
    }

    #[test]
    fn the_borderless_footer_uses_the_full_width() {
        // The footer must not be cut two columns short.
        let rex = Fake::default();
        let mut a = app(&rex);
        a.focused = true;
        let screen = render_at(&mut a, &[], 45, 1);
        assert_eq!(screen, FOOTER);
    }

    #[test]
    fn an_error_clears_on_the_next_refresh_or_action_that_works() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [worker("alpha", "idle", false, Some(5), "hi")];
        let blocks = Needs {
            blocks: true,
            view: false,
        };
        rex.fail("session.list_blocks");
        a.sync(blocks);
        assert!(render_at(&mut a, &rows, 40, 2).contains("boom"));
        // A pass with nothing to fetch keeps it.
        a.sync(Needs::default());
        assert!(a.error.is_some());
        rex.heal();
        a.sync(blocks);
        assert_eq!(a.error, None);
        assert!(!render_at(&mut a, &rows, 40, 2).contains("boom"));
        a.note_rows(1);
        assert_eq!(a.fit.want, 1, "the footer row goes too");

        // One failure in a pass is kept even when the rest of it works.
        rex.fail("session.view");
        a.sync(Needs {
            blocks: true,
            view: true,
        });
        assert!(a.error.as_deref().unwrap().contains("session.view"));
        // A failed action shows; the same action working clears it.
        rex.heal();
        rex.fail("session.focus_block");
        a.focus_lead();
        assert!(a.error.is_some());
        rex.heal();
        a.focus_lead();
        assert_eq!(a.error, None);

        // A config file problem stays until a key press.
        a.config_error = Some("bad config".into());
        a.sync(blocks);
        assert!(render_at(&mut a, &rows, 40, 2).contains("bad config"));
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('j').into()), &rows);
        assert_eq!(a.shown_error(), None);
    }

    #[test]
    fn retries_the_fit_until_the_pane_is_drawn() {
        let rex = Fake::default();
        sized(&rex, 1);
        let undrawn = json!({"rows": 24, "columns": 80, "desired": []});
        rex.on("block:roster size", undrawn.clone());
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        a.fit_if_due();
        assert!(rex.params("session.resize_layer").is_empty());
        assert!(a.fit.retry_at.is_some(), "a retry is scheduled");
        // Not due yet: nothing happens.
        a.fit_if_due();
        assert_eq!(a.fit.retries, 1);

        // A client draws the pane, and no Resize ever arrives.
        rex.on(
            "block:roster size",
            json!({"rows": 1, "desired": [{"rows": 1}]}),
        );
        a.fit.retry_at = Some(Instant::now());
        a.fit_if_due();
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        assert_eq!((a.fit.retries, a.fit.retry_at), (0, None));

        // A pane that is never drawn: the retries stop within the budget.
        rex.on("block:roster size", undrawn);
        a.note_rows(5);
        let mut fits = 0;
        while a.fit.dirty || a.fit.retry_at.is_some() {
            a.fit.retry_at = a.fit.retry_at.map(|_| Instant::now());
            a.fit_if_due();
            fits += 1;
            assert!(fits < 100, "never stops");
        }
        let budget = (0..).map_while(size::retry_delay).count();
        assert_eq!(fits, budget + 1);
    }

    #[test]
    fn the_shown_marker_is_apart_from_the_label_at_every_width() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [
            worker("beta-deps", "working", true, Some(5), "hi"),
            worker("gamma", "idle", false, Some(5), "hi"),
        ];
        for width in 4..40 {
            let screen = render_at(&mut a, &rows, width, 2);
            let shown = screen.lines().next().unwrap();
            let hidden = screen.lines().nth(1).unwrap();
            let after = |l: &str| l.chars().nth(3);
            assert_eq!(shown.chars().nth(2), Some('▸'), "{width}: {shown}");
            assert_eq!(after(shown), Some(' '), "{width}: {shown}");
            assert_eq!(after(hidden), Some(' '), "{width}: {hidden}");
            if width > 4 {
                // Both labels start in the same column.
                assert_eq!(shown.find('b').map(|i| shown[..i].chars().count()), Some(4));
                assert_eq!(
                    hidden.find('g').map(|i| hidden[..i].chars().count()),
                    Some(4)
                );
            }
        }
    }

    #[test]
    fn v_and_the_panel_share_one_view() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let key = |c| TermEvent::Key(KeyCode::Char(c).into());
        a.on_terminal_event(&key('v'), &[]);
        assert_eq!(a.opts.view, View::Expanded);
        // The panel shows the view in use, and closing it unchanged saves
        // nothing (with nowhere to save, a save would be an error).
        a.on_terminal_event(&key('o'), &[]);
        assert!(render_at(&mut a, &[], 32, 4).contains("view    expanded"));
        a.on_terminal_event(&TermEvent::Key(KeyCode::Esc.into()), &[]);
        assert_eq!(a.shown_error(), None);
        assert!(a.expanded());
        a.on_terminal_event(&key('v'), &[]);
        assert!(!a.expanded());
    }

    #[test]
    fn a_view_change_fetches_the_view_once_per_pass() {
        let rex = Fake::default();
        sized(&rex, 1);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        // A resize echo and a view event in the same pass.
        a.on_terminal_event(&TermEvent::Resize(40, 1), &[]);
        a.sync(Needs {
            blocks: true,
            view: true,
        });
        a.fit_if_due();
        assert_eq!(rex.params("session.view").len(), 1);
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        // The view isn't kept past the pass.
        assert!(a.view.is_none());
        a.fit.dirty = true;
        a.fit_if_due();
        assert_eq!(rex.params("session.view").len(), 2);
    }

    #[test]
    fn a_double_click_follows_the_worker_not_the_row() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let one = row("block:1", "a", "idle", false);
        let two = row("block:2", "b", "idle", false);
        let click = TermEvent::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        let rows = [one.clone(), two.clone()];
        render(&mut a, &rows);
        a.on_terminal_event(&click, &rows);
        // The rows swap places before the second click on the same line.
        let swapped = [two, one];
        render(&mut a, &swapped);
        a.on_terminal_event(&click, &swapped);
        assert!(rex.methods().is_empty(), "{:?}", rex.methods());
        assert_eq!(a.selected.as_deref(), Some("block:1"));
        // A second click on block:1 is the double-click.
        a.on_terminal_event(&click, &swapped);
        assert_eq!(rex.params("session.move_block")[0]["block_id"], "block:1");
    }

    #[test]
    fn restoring_input_turns_off_mouse_capture_and_focus_reports() {
        // The panic hook and the normal exit both use this.
        let mut out = Vec::new();
        restore_input(&mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("\x1b[?1004l"), "focus reports: {out:?}");
        assert!(out.contains("\x1b[?1000l"), "mouse capture: {out:?}");
    }

    #[test]
    fn a_failed_save_survives_the_refresh_that_closing_the_panel_causes() {
        // The panel closes, the layer resizes, and the view event's refresh
        // must not wipe the save error before it is drawn.
        let rex = Fake::default();
        let mut a = app(&rex);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Char('o').into()), &[]);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Enter.into()), &[]);
        a.on_terminal_event(&TermEvent::Key(KeyCode::Esc.into()), &[]);
        a.sync(Needs {
            blocks: true,
            view: true,
        });
        let screen = render_at(&mut a, &[], 45, 1);
        assert!(screen.contains("no config directory"), "{screen}");
    }

    #[test]
    fn a_resize_after_the_retries_ran_out_gets_new_ones() {
        // No client for 5 s, then one attaches and its first
        // Resize lands before the pane reports a drawn size.
        let rex = Fake::default();
        sized(&rex, 1);
        rex.on(
            "block:roster size",
            json!({"rows": 24, "columns": 80, "desired": []}),
        );
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_rows(3);
        while a.fit.dirty || a.fit.retry_at.is_some() {
            a.fit.retry_at = a.fit.retry_at.map(|_| Instant::now());
            a.fit_if_due();
        }
        a.on_terminal_event(&TermEvent::Resize(48, 5), &[]);
        a.fit_if_due();
        assert!(a.fit.retry_at.is_some(), "the resize's fit is retried");
    }
}
