//! The roster TUI: one `rex events` stream feeds the model; rows are redrawn
//! in place (ratatui only writes the cells that changed).

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Row as TableRow, Table, TableState};
use serde_json::json;

use crate::events::{self, Event};
use crate::model::{Model, Row, short_age};
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

pub const FOOTER: &str = " ⏎ show/hide · l lead · q quit ";

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
    table: TableState,
    table_area: Rect,
    last_click: Option<(usize, Instant)>,
    error: Option<String>,
    fit: Fit,
    /// When the spinner started; its frame is a function of the time since.
    spin_start: Instant,
}

#[derive(Default)]
struct Fit {
    dirty: bool,
    want: u16,
    attempts: u8,
    last_resize: Option<Instant>,
}

pub fn run(lead: String, session: String) -> Result<()> {
    let cli = rex::Cli::new();
    let me = std::env::var("REX_BLOCK").ok().filter(|s| !s.is_empty());
    let (tx, rx) = mpsc::channel();
    // Subscribe before the first refresh so nothing falls between them.
    let mut child = events::spawn(&cli, &session, tx)?;
    let mut app = App {
        rex: &cli,
        session,
        model: Model::new(lead, me, now()),
        selected: None,
        table: TableState::default(),
        table_area: Rect::default(),
        last_click: None,
        error: None,
        fit: Fit::default(),
        spin_start: Instant::now(),
    };
    let result = app
        .model
        .refresh(app.rex, &app.session, now())
        .and_then(|()| {
            let mut terminal = ratatui::init();
            let _ = execute!(std::io::stdout(), EnableMouseCapture);
            let result = app.run(&mut terminal, &rx);
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
            ratatui::restore();
            result
        });
    // Stop the event stream however the roster ends.
    let _ = child.kill();
    let _ = child.wait();
    result
}

impl App<'_> {
    fn run(&mut self, terminal: &mut DefaultTerminal, rx: &Receiver<Option<Event>>) -> Result<()> {
        loop {
            let mut needs_blocks = false;
            loop {
                match rx.try_recv() {
                    Ok(Some(event)) => needs_blocks |= self.model.apply(event, now()).blocks,
                    Ok(None) | Err(TryRecvError::Disconnected) => bail!("rex events stopped"),
                    Err(TryRecvError::Empty) => break,
                }
            }
            if needs_blocks && let Err(err) = self.model.refresh(self.rex, &self.session, now()) {
                self.error = Some(format!("{err:#}"));
            }

            let rows = self.model.rows(now());
            if self.model.should_exit(now()) {
                return Ok(());
            }
            self.note_count(rows.len());
            if self.fit.dirty {
                self.fit_layer();
            }
            terminal.draw(|f| self.draw(f, &rows))?;

            if event::poll(tick(&rows))? {
                let quit = self.on_terminal_event(&event::read()?, &rows);
                if quit {
                    return Ok(());
                }
            }
        }
    }

    fn note_count(&mut self, count: usize) {
        let want = size::wanted_rows(count);
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
                    _ => {}
                }
            }
            TermEvent::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.row_at(m.column, m.row, rows.len()) {
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

    fn step(&mut self, rows: &[Row], by: isize) {
        if rows.is_empty() {
            return;
        }
        let i = self.selected_index(rows).unwrap_or(0);
        let j = i.saturating_add_signed(by).min(rows.len() - 1);
        self.selected = Some(rows[j].block.clone());
    }

    fn row_at(&self, column: u16, row: u16, count: usize) -> Option<usize> {
        let a = self.table_area;
        if column < a.x || column >= a.right() || row < a.y || row >= a.bottom() {
            return None;
        }
        let i = usize::from(row - a.y) + self.table.offset();
        (i < count).then_some(i)
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
        let footer = match &self.error {
            Some(err) => Line::styled(format!(" {err} "), Style::new().fg(Color::Red)),
            None => Line::styled(FOOTER, Style::new().fg(Color::DarkGray)),
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Line::styled(" workers ", Style::new().fg(Color::Gray)))
            .title_bottom(footer);
        let area = f.area();
        self.table_area = block.inner(area);
        self.table.select(self.selected_index(rows));
        let elapsed = self.spin_start.elapsed();
        let table = Table::new(rows.iter().map(|r| table_row(r, elapsed)), WIDTHS)
            .block(block)
            .column_spacing(1)
            .highlight_symbol("›")
            .row_highlight_style(Style::new().add_modifier(Modifier::BOLD));
        f.render_stateful_widget(table, area, &mut self.table);
    }
}

const WIDTHS: [Constraint; 4] = [
    Constraint::Length(2),
    Constraint::Max(20),
    Constraint::Length(3),
    Constraint::Fill(1),
];

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

fn table_row(r: &Row, elapsed: Duration) -> TableRow<'_> {
    let (g, style) = glyph(&r.state, elapsed);
    let dim = Style::new().add_modifier(Modifier::DIM);
    TableRow::new([
        Cell::from(Line::from(vec![
            Span::styled(g, style),
            Span::raw(if r.shown { "▸" } else { " " }),
        ])),
        Cell::from(r.label.as_str()),
        Cell::from(Span::styled(r.age.map(short_age).unwrap_or_default(), dim)),
        Cell::from(r.msg.as_str()),
    ])
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
        }
    }

    fn app(rex: &Fake) -> App<'_> {
        App {
            rex,
            session: "s".into(),
            model: Model::new("block:lead".into(), Some("block:me".into()), 0),
            selected: None,
            table: TableState::default(),
            table_area: Rect::default(),
            last_click: None,
            error: None,
            fit: Fit::default(),
            spin_start: Instant::now(),
        }
    }

    fn render(app: &mut App, rows: &[Row]) -> String {
        let mut t = Terminal::new(TestBackend::new(52, 5)).unwrap();
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

    #[test]
    fn draws_rows_with_glyph_marker_age_and_msg() {
        let rex = Fake::default();
        let mut a = app(&rex);
        let rows = [
            row("block:1", "alpha", "working", true),
            row("block:2", "beta", "done", false),
            row("block:3", "gamma", "blocked", false),
        ];
        let screen = render(&mut a, &rows);
        let lines: Vec<&str> = screen.lines().collect();
        assert!(lines[0].contains("workers"));
        // Rendered within the first spinner tick, so frame 0.
        assert!(lines[1].contains("›⠋▸ alpha"), "{}", lines[1]);
        assert!(lines[1].contains(" 1m  Running tests"), "{}", lines[1]);
        assert!(!lines[1].contains("working"), "{}", lines[1]);
        assert!(lines[2].contains(" ✓  beta"), "{}", lines[2]);
        assert!(lines[3].contains(" ◐  gamma"), "{}", lines[3]);
        assert!(lines[4].contains("l lead"), "{}", lines[4]);
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
            row: 2,
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
        sized(&rex, 3);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_count(3);
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
        sized(&rex, 5);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_count(3);
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
        sized(&rex, 3);
        let mut a = app(&rex);
        a.model.me = Some("block:roster".into());
        a.note_count(3);
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
        assert_eq!(changed, [(2, 1)]);
    }
}
