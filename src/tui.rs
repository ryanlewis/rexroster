//! The roster TUI: one `rex events` stream feeds the model; rows are redrawn
//! in place (ratatui only writes the cells that changed).

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
use serde_json::json;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{self, Border, Key, Options, View};
use crate::events::{self, Event};
use crate::model::{Model, Needs, Row, short_age};
use crate::rex::{self, Rex};
use crate::size;

/// Where a shown worker floats: centred, most of the tab.
const SHOWN_BOUNDS: [f64; 4] = [0.1, 0.08, 0.8, 0.84];
/// Two clicks on the same row within this time are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Resizes allowed for one row count before giving up, so the layer can't bounce.
const MAX_FITS: u8 = 4;
/// A terminal resize this long after our own counts as the user's, and gets fresh attempts.
const OWN_RESIZE_WINDOW: Duration = Duration::from_secs(2);

/// Key hints, shown on the last line while the roster has focus.
pub const FOOTER: &str = "⏎ show · v view · o options · l lead · q quit";
/// Hints on the options panel's last line.
pub const PANEL_HINTS: &str = "⏎ change · esc save and close";
/// Lines the options panel takes: a heading, one per option, the hints.
pub const PANEL_LINES: usize = Key::ALL.len() + 2;
/// Expanded view: the most lines a message wraps to.
pub const MSG_LINES: usize = 3;
/// Expanded view: continuation lines start under the label.
const INDENT: usize = 3;
/// The label column's width follows the longest label, within these bounds.
pub const LABEL_MIN: usize = 6;
pub const LABEL_MAX: usize = 20;

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
    /// Several lines per worker instead of one.
    expanded: bool,
    /// Whether the pane has focus; the key hints show only then.
    focused: bool,
    last_click: Option<(usize, Instant)>,
    error: Option<String>,
    fit: Fit,
    /// When the spinner started; its frame is a function of the time since.
    spin_start: Instant,
    /// Display options in effect, and the file the options panel saves to.
    opts: Options,
    config_path: Option<PathBuf>,
    /// The options panel, while it is open, and where its options were drawn.
    panel: Option<Panel>,
    panel_area: Rect,
}

/// What `rexroster` starts the TUI with.
pub struct Settings {
    pub opts: Options,
    pub path: Option<PathBuf>,
    /// A problem loading the config file, shown on the footer line.
    pub error: Option<String>,
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
        expanded: settings.opts.view == View::Expanded,
        focused: false,
        last_click: None,
        error: settings.error,
        fit: Fit::default(),
        spin_start: Instant::now(),
        opts: settings.opts,
        config_path: settings.path,
        panel: None,
        panel_area: Rect::default(),
    };
    let result = app
        .model
        .refresh(app.rex, &app.session, now())
        .and_then(|()| {
            let mut terminal = ratatui::init();
            let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableFocusChange);
            spawn_input(input_tx);
            // A worker minimized before the roster started gets no view event.
            app.hide_minimized();
            let result = app.run(&mut terminal, &rx);
            let _ = execute!(std::io::stdout(), DisableFocusChange, DisableMouseCapture);
            ratatui::restore();
            result
        });
    // Stop the event stream however the roster ends.
    let _ = child.kill();
    let _ = child.wait();
    result
}

impl App<'_> {
    fn run(&mut self, terminal: &mut DefaultTerminal, rx: &Receiver<Msg>) -> Result<()> {
        loop {
            let rows = self.model.rows(now());
            if self.model.should_exit(now()) {
                return Ok(());
            }
            let lines = if self.panel.is_some() {
                PANEL_LINES
            } else {
                total_lines(&rows, self.width, self.expanded)
            };
            self.note_rows(lines);
            if self.fit.dirty {
                self.fit_layer();
            }
            terminal.draw(|f| self.draw(f, &rows))?;

            // Sleep until a message arrives or the tick is due, then take
            // everything that is waiting.
            let first = match rx.recv_timeout(tick(&rows)) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => bail!("rex events stopped"),
            };
            let mut needs = Needs::default();
            for msg in first.into_iter().chain(rx.try_iter()) {
                match msg {
                    Msg::Rex(Some(event)) => needs = needs.or(self.model.apply(event, now())),
                    Msg::Rex(None) => bail!("rex events stopped"),
                    Msg::Term(ev) => {
                        if self.on_terminal_event(&ev, &rows) {
                            return Ok(());
                        }
                    }
                }
            }
            if needs.view {
                self.hide_minimized();
            }
            if needs.blocks
                && let Err(err) = self.model.refresh(self.rex, &self.session, now())
            {
                self.error = Some(format!("{err:#}"));
            }
        }
    }

    /// The last line shows key hints while focused, and an error whenever
    /// there is one. The options panel has its own hints line instead.
    fn footer_shown(&self) -> bool {
        self.panel.is_none() && (self.focused || self.error.is_some())
    }

    /// Note the rows the pane needs: the workers' lines, plus the footer while focused.
    fn note_rows(&mut self, lines: usize) {
        let want = size::wanted_rows(lines, self.footer_shown(), self.opts.border);
        if want != self.fit.want {
            self.fit = Fit {
                dirty: true,
                want,
                attempts: 0,
                last_resize: self.fit.last_resize,
            };
        }
    }

    /// Resize the layer towards the wanted rows. Does nothing until the pane
    /// has been drawn, and at most `MAX_FITS` times per row count.
    fn fit_layer(&mut self) {
        self.fit.dirty = false;
        if self.fit.attempts >= MAX_FITS {
            return;
        }
        let Some(me) = self.model.me.clone() else {
            return;
        };
        let Some((layer, bounds, height)) = self.measure(&me) else {
            return;
        };
        let [x, y, w, _] = bounds;
        let Some(new_h) = height else { return };
        self.fit.attempts += 1;
        self.fit.last_resize = Some(Instant::now());
        let res = self.rex.call(
            "session.resize_layer",
            json!({
                "session_id": self.session,
                "layer_id": layer,
                "bounds": [x, y, w, new_h],
            }),
        );
        if let Err(err) = res {
            self.error = Some(format!("{err:#}"));
        }
    }

    /// The roster's layer, its bounds and, if it doesn't fit, the height it should have.
    fn measure(&self, me: &str) -> Option<(String, [f64; 4], Option<f64>)> {
        let view = self
            .rex
            .call("session.view", json!({ "session_id": self.session }))
            .ok()?;
        let (layer, bounds) = rex::floating_layer_of(&view, me)?;
        let lead = &self.model.lead;
        let lead_h = rex::rect_height_of(&view, lead)?;
        let lead_rows = rex::drawn_rows(&self.rex.block_call(&self.session, lead, "size").ok()?)?;
        let rows = rex::drawn_rows(&self.rex.block_call(&self.session, me, "size").ok()?)?;
        let per_row = lead_h / f64::from(lead_rows);
        let new_h = size::new_height(self.fit.want, rows, per_row, bounds[1], bounds[3]);
        Some((layer, bounds, new_h))
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
                let ctrl_c =
                    k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
                if ctrl_c || (k.code == KeyCode::Char('q') && self.panel.is_none()) {
                    return true;
                }
                if let Some(panel) = &mut self.panel {
                    let view = self.opts.view;
                    let action = panel.key(k.code, &mut self.opts);
                    self.apply_view_option(view);
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
                    KeyCode::Char('v') => self.expanded = !self.expanded,
                    KeyCode::Char('o') => self.panel = Some(Panel::new(self.opts)),
                    _ => {}
                }
            }
            TermEvent::Mouse(m)
                if m.kind == MouseEventKind::Down(MouseButton::Left) && self.panel.is_some() =>
            {
                let a = self.panel_area;
                let inside = m.column >= a.x && m.column < a.right() && m.row >= a.y;
                if inside && let Some(panel) = &mut self.panel {
                    let view = self.opts.view;
                    panel.click(usize::from(m.row - a.y), &mut self.opts);
                    self.apply_view_option(view);
                }
            }
            TermEvent::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.row_at(m.column, m.row).filter(|&i| i < rows.len()) {
                    self.selected = Some(rows[i].block.clone());
                    let double = self
                        .last_click
                        .is_some_and(|(j, t)| j == i && t.elapsed() <= DOUBLE_CLICK);
                    if double {
                        self.last_click = None;
                        self.toggle(&rows[i].block);
                    } else {
                        self.last_click = Some((i, Instant::now()));
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
            }
            _ => {}
        }
        false
    }

    /// The panel's view option is the starting view; changing it (from
    /// `before`) also switches the current view, so the change shows when
    /// the panel closes.
    fn apply_view_option(&mut self, before: View) {
        if self.opts.view != before {
            self.expanded = self.opts.view == View::Expanded;
        }
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
        if let Err(err) = res {
            self.error = Some(format!("{err:#}"));
        }
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
        if let Err(err) = res {
            self.error = Some(format!("{err:#}"));
        }
    }

    /// Treat Rex's minimize as hide: detach any of our workers whose layer
    /// was minimized. The detach changes the view again, and that pass finds
    /// nothing left to do.
    fn hide_minimized(&mut self) {
        let view = match self
            .rex
            .call("session.view", json!({ "session_id": self.session }))
        {
            Ok(view) => view,
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                return;
            }
        };
        for block in self.model.to_detach(&view) {
            let res = self.rex.call(
                "session.detach_block",
                json!({ "session_id": self.session, "block_id": block }),
            );
            if let Err(err) = res {
                self.error = Some(format!("{err:#}"));
            }
        }
    }

    fn focus_lead(&mut self) {
        let res = self.rex.call(
            "session.focus_block",
            json!({ "session_id": self.session, "block_id": self.model.lead }),
        );
        if let Err(err) = res {
            self.error = Some(format!("{err:#}"));
        }
    }

    fn draw(&mut self, f: &mut ratatui::Frame, rows: &[Row]) {
        let area = f.area();
        let chrome = 2 * u16::from(self.opts.border == Border::Rounded);
        let footer = self.footer_line(usize::from(area.width.saturating_sub(chrome)));
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
                    .title(Line::styled(title, Style::new().fg(Color::Gray)));
                if self.footer_shown() {
                    block = block.title_bottom(footer.clone());
                }
                let inner = block.inner(area);
                f.render_widget(block, area);
                inner
            }
        };
        self.width = inner.width;
        if self.panel.is_some() {
            self.draw_panel(f, inner);
        } else {
            let footer =
                (self.opts.border == Border::None && self.footer_shown()).then_some(footer);
            self.draw_list(f, inner, rows, footer);
        }
    }

    /// The key hints, or the error when there is one, cut to `width`.
    fn footer_line(&self, width: usize) -> Line<'static> {
        match &self.error {
            Some(err) => Line::styled(cut(err, width), Style::new().fg(Color::Red)),
            None => Line::styled(cut(FOOTER, width), Style::new().add_modifier(Modifier::DIM)),
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
        lines.push(match &self.error {
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
    ) {
        let footer_rows = u16::from(footer.is_some() && area.height > 0);
        let list = Rect {
            height: area.height - footer_rows,
            ..area
        };
        self.list_area = list;
        let sel = self.selected_index(rows);
        let laid = layout(
            rows,
            sel,
            area.width,
            self.spin_start.elapsed(),
            self.expanded,
        );
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
        let lines: Vec<Line> = laid
            .into_iter()
            .skip(self.offset)
            .take(visible)
            .map(|(_, l)| l)
            .collect();
        f.render_widget(Paragraph::new(lines), list);
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

/// Column widths shared by every row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Columns {
    pub label: usize,
    pub age: usize,
}

impl Columns {
    /// The label column fits the longest label, between `LABEL_MIN` and
    /// `LABEL_MAX`; the age column fits the longest age.
    pub fn for_rows(rows: &[Row]) -> Self {
        let longest = rows.iter().map(|r| r.label.width()).max().unwrap_or(0);
        let age = rows
            .iter()
            .filter_map(|r| r.age.map(|a| short_age(a).len()))
            .max()
            .unwrap_or(0);
        Self {
            label: longest.clamp(LABEL_MIN, LABEL_MAX),
            age,
        }
    }
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

/// One row: `›` when selected, the glyph, `▸` when shown, the label, the age
/// right-aligned, then the message filling the rest of `width`.
pub fn row_line(
    r: &Row,
    selected: bool,
    cols: Columns,
    width: u16,
    elapsed: Duration,
) -> Line<'static> {
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
        Span::raw(if r.shown { "▸" } else { " " }),
        Span::styled(pad(&r.label, cols.label), strong),
    ];
    let mut used = 3 + cols.label;
    if cols.age > 0 {
        let age = r.age.map(short_age).unwrap_or_default();
        spans.push(Span::styled(format!(" {age:>w$}", w = cols.age), dim));
        used += 1 + cols.age;
    }
    let room = usize::from(width).saturating_sub(used + 1);
    if room > 0 && !r.msg.is_empty() {
        spans.push(Span::raw(format!(" {}", cut(&r.msg, room))));
    }
    Line::from(spans)
}

/// Word-wrap `text` to `width` columns, in at most `max` lines. When it
/// needs more, the last line ends in `…`.
pub fn wrap(text: &str, width: usize, max: usize) -> Vec<String> {
    if width == 0 || max == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
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
        if !expanded {
            out.push((i, row_line(r, selected, cols, width, elapsed)));
            continue;
        }
        let bold = if selected {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        // Line 1: markers, label and age, with no message.
        let header = Row {
            msg: String::new(),
            ..r.clone()
        };
        out.push((i, row_line(&header, selected, cols, width, elapsed)));
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

    fn app(rex: &Fake) -> App<'_> {
        App {
            rex,
            session: "s".into(),
            model: Model::new("block:lead".into(), Some("block:me".into()), 0),
            selected: None,
            list_area: Rect::default(),
            offset: 0,
            line_owner: Vec::new(),
            width: 0,
            expanded: false,
            focused: false,
            last_click: None,
            error: None,
            fit: Fit::default(),
            spin_start: Instant::now(),
            opts: Options::default(),
            config_path: None,
            panel: None,
            panel_area: Rect::default(),
        }
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
        // Rendered within the first spinner tick, so frame 0. Unfocused: no footer.
        assert_eq!(
            render_at(&mut a, &rows, 40, 4),
            [
                "›⠋▸alpha      1m Running tests          ",
                " ✓ beta       5s PR raised              ",
                " ◐ gamma-long 1h Allow git push to the… ",
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
        assert_eq!(line, "›○ a-very-long-worker-… hi    ");
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
            Some("⏎ show · v view · o options · l lead ·… ")
        );
        a.on_terminal_event(&TermEvent::FocusLost, &rows);
        assert!(!render_at(&mut a, &rows, 40, 2).contains("l lead"));
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
            screen.lines().nth(1).unwrap().starts_with("›○ a3"),
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
        assert!(a.expanded);
        let rows = [
            tasked(
                "alpha",
                "Allow git push to the remote branch now?",
                Some("fix login"),
            ),
            tasked("beta", "PR raised", None),
        ];
        assert_eq!(
            render_at(&mut a, &rows, 24, 6),
            [
                "›◐ alpha  5s            ",
                "   Allow git push to the",
                "   remote branch now?   ",
                "   fix login            ",
                " ◐ beta   5s            ",
                "   PR raised            ",
            ]
            .join("\n")
        );
    }

    #[test]
    fn clicks_on_any_line_pick_the_worker() {
        let rex = Fake::default();
        let mut a = app(&rex);
        a.expanded = true;
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
        a.note_rows(total_lines(&rows, a.width, a.expanded));
        a.fit_layer();
        assert!(rex.params("session.resize_layer").is_empty());

        a.expanded = true;
        a.note_rows(total_lines(&rows, a.width, a.expanded));
        a.fit_layer();
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        sized(&rex, 4);
        a.note_rows(total_lines(&rows, a.width, a.expanded));
        assert!(!a.fit.dirty);

        // A longer message wraps to two lines: one more resize.
        rows[0].msg = "a message that needs two lines".into();
        a.note_rows(total_lines(&rows, a.width, a.expanded));
        assert!(a.fit.dirty);
        a.fit_layer();
        assert_eq!(rex.params("session.resize_layer").len(), 2);
    }

    #[test]
    fn errors_show_even_without_focus() {
        // Review finding: a background refresh fails while a worker has focus.
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
        // Review finding: a 5-line worker in a 3-line pane.
        let rex = Fake::default();
        let mut a = app(&rex);
        a.expanded = true;
        let rows = [
            tasked("alpha", "one", None),
            tasked("beta", &"word ".repeat(40), Some("task")),
        ];
        a.selected = Some("block:beta".into());
        let screen = render_at(&mut a, &rows, 24, 3);
        assert!(
            screen.lines().next().unwrap().starts_with("›◐ beta"),
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
        assert_eq!(p.cursor, 1, "stops at the last option");
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
            render_at(&mut a, &[], 32, 4),
            [
                "Options                         ",
                "›border  none                   ",
                " view    compact                ",
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
                "│›◐ alpha  5s Running tests  │",
                "╰⏎ show · v view · o options…╯",
            ]
            .join("\n")
        );
        a.expanded = true;
        assert_eq!(
            render_at(&mut a, &rows, 30, 5),
            [
                "╭ workers ───────────────────╮",
                "│›◐ alpha  5s                │",
                "│   Running tests            │",
                "│   fix login                │",
                "╰⏎ show · v view · o options…╯",
            ]
            .join("\n")
        );
    }

    #[test]
    fn sizes_for_the_panel_and_both_borders() {
        let n = Border::None;
        let r = Border::Rounded;
        // The panel: heading, two options, hints; no separate footer.
        assert_eq!(size::wanted_rows(PANEL_LINES, false, n), 4);
        assert_eq!(size::wanted_rows(PANEL_LINES, false, r), 6);
        let rex = Fake::default();
        let mut a = app(&rex);
        a.focused = true;
        a.panel = Some(Panel::new(a.opts));
        assert!(!a.footer_shown());
        a.note_rows(PANEL_LINES);
        assert_eq!(a.fit.want, 4);
    }

    #[test]
    fn a_border_change_in_the_panel_resizes_once_and_saves() {
        let rex = Fake::default();
        sized(&rex, 4);
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
            "4 rows fit the panel"
        );
        a.on_terminal_event(&TermEvent::Key(KeyCode::Enter.into()), &[]);
        assert_eq!(a.opts.border, Border::Rounded);
        a.note_rows(PANEL_LINES);
        a.fit_layer();
        assert_eq!(rex.params("session.resize_layer").len(), 1);
        sized(&rex, 6);
        a.note_rows(PANEL_LINES);
        assert!(!a.fit.dirty, "no second resize once it fits");

        a.on_terminal_event(&key('o'), &[]);
        assert!(a.panel.is_none());
        assert_eq!(a.error, None);
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
        assert!(a.expanded);
        // With nowhere to save, closing reports it.
        a.on_terminal_event(&TermEvent::Key(KeyCode::Esc.into()), &[]);
        assert!(a.error.as_deref().unwrap().contains("no config directory"));
    }

    #[test]
    fn cycling_the_view_away_and_back_keeps_the_current_view() {
        // Review finding: compact -> expanded -> compact in the panel.
        let rex = Fake::default();
        let mut a = app(&rex);
        let key = |c| TermEvent::Key(c);
        a.on_terminal_event(&key(KeyCode::Char('o').into()), &[]);
        a.on_terminal_event(&key(KeyCode::Down.into()), &[]);
        a.on_terminal_event(&key(KeyCode::Enter.into()), &[]);
        assert!(a.expanded);
        a.on_terminal_event(&key(KeyCode::Left.into()), &[]);
        assert_eq!(a.opts.view, View::Compact);
        assert!(!a.expanded);
    }

    #[test]
    fn the_borderless_footer_uses_the_full_width() {
        // Review finding: it was cut two columns short.
        let rex = Fake::default();
        let mut a = app(&rex);
        a.focused = true;
        let screen = render_at(&mut a, &[], 45, 1);
        assert_eq!(screen, FOOTER);
    }
}
