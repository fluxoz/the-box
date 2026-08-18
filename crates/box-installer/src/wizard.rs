//! The console setup wizard (Door 2): when a machine boots with no orders, an
//! operator standing at the screen picks a disk layout here. It's a thin
//! front-end over `box-core` — probe, then let the user cycle single/mirror/
//! pool with live validation, then an explicit ERASE gate. On confirm it writes
//! the effective orders + a disko config for the installer to act on.
//!
//! The browser wizard (later) drives the exact same core; this one just needs
//! nothing but a terminal, so it's the universal fallback.

use crate::orders;
use crate::plan;
use anyhow::{Context, Result};
use box_core::{disko, probe, LayoutKind, ResolveError, ResolveOpts, ResolvedLayout};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use std::time::Duration;

// Brass Hands industrial palette, mapped to terminal-safe truecolor.
const BRASS: Color = Color::Rgb(0xC8, 0x86, 0x2A);
const IRON: Color = Color::Rgb(0x9A, 0x9A, 0x9A);
const GOOD: Color = Color::Rgb(0x6F, 0xA8, 0x5A);
const ALERT: Color = Color::Rgb(0xC8, 0x5A, 0x3C);
const DIM: Color = Color::DarkGray;

const LAYOUTS: [LayoutKind; 3] = [LayoutKind::Single, LayoutKind::Mirror, LayoutKind::Pool];

/// The "finish in a browser" line shown in the TUI (so a person at the screen
/// can read the URL + PIN and use their phone), or `None` when not networked.
pub fn browser_hint(url: Option<&str>, pin: Option<&str>) -> Option<String> {
    let url = url?;
    Some(match pin {
        Some(p) => format!("Prefer a browser? {url}  ·  PIN {p}"),
        None => format!("Prefer a browser? {url}"),
    })
}

enum Screen {
    Choose,
    Confirm,
}

enum Outcome {
    Committed(ResolvedLayout),
    Aborted,
    /// The browser wizard committed — leave the files it wrote alone.
    CommittedElsewhere,
}

struct App {
    disks: Vec<box_core::Disk>,
    opts: ResolveOpts,
    picked: usize, // index into LAYOUTS
    screen: Screen,
    confirm_input: String,
    base: Value,
    /// Seconds until auto-cancel while no operator has responded; `None` once
    /// someone has interacted (an operator is present — stop the clock).
    idle_left: Option<u32>,
    /// If this path appears, the browser wizard committed — bow out.
    watch_commit: Option<std::path::PathBuf>,
    /// "Finish in a browser" hint (URL + PIN) shown in the title.
    browser_hint: Option<String>,
}

impl App {
    fn kind(&self) -> LayoutKind {
        LAYOUTS[self.picked]
    }

    fn resolved(&self) -> Result<ResolvedLayout, ResolveError> {
        box_core::resolve(&self.disks, self.kind(), &self.opts)
    }
}

pub fn run(
    base_orders: Option<&str>,
    orders_out: &str,
    disko_out: &str,
    watch_commit: Option<&str>,
    browser_hint: Option<String>,
) -> Result<()> {
    let base = match base_orders {
        Some(path) => orders::load(path).with_context(|| format!("reading base orders {path}"))?,
        None => Value::Null,
    };
    let disks = probe::probe().context("probing disks for the wizard")?;

    let mut app = App {
        disks,
        opts: ResolveOpts::default(),
        picked: 0,
        screen: Screen::Choose,
        confirm_input: String::new(),
        base,
        idle_left: None,
        watch_commit: watch_commit.map(std::path::PathBuf::from),
        browser_hint,
    };

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, &mut app);
    ratatui::restore();

    match outcome? {
        Outcome::Committed(layout) => {
            let mut effective = orders::effective_orders(&app.base, &layout);
            // No install path may produce a Box without an owner. If the
            // operator brought no code, mint one and put it on the screen they
            // are standing in front of — this is the last moment they have one.
            let minted = orders::ensure_enrollment(&mut effective)?;
            std::fs::write(orders_out, serde_json::to_string_pretty(&effective)?)
                .with_context(|| format!("writing effective orders to {orders_out}"))?;
            std::fs::write(disko_out, disko::render(&layout))
                .with_context(|| format!("writing disko config to {disko_out}"))?;
            eprintln!("Storage layout confirmed:\n{}", plan::plan_summary(&layout));
            if let Some(code) = minted {
                eprintln!(
                    "\n  ┌─────────────────────────────────────────────┐\n  \
                     │  PAIRING CODE — write this down now         │\n  \
                     │                                             │\n  \
                     │      {code}                  │\n  \
                     │                                             │\n  \
                     │  You enter it once at the box's dashboard   │\n  \
                     │  to pair your first browser. It is shown    │\n  \
                     │  only here, and it never expires.           │\n  \
                     └─────────────────────────────────────────────┘\n"
                );
            }
            Ok(())
        }
        Outcome::CommittedElsewhere => {
            eprintln!("Storage was chosen in the browser wizard; continuing with that.");
            Ok(())
        }
        Outcome::Aborted => {
            eprintln!("Setup cancelled at the console — this machine is unchanged.");
            std::process::exit(10);
        }
    }
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<Outcome> {
    // Auto-cancel if nobody ever responds — a headless boot that reached the
    // wizard with no orders must neither hang nor be wiped. The clock stops the
    // moment an operator touches a key.
    const IDLE_LIMIT: u32 = 300;
    let mut seen_input = false;
    let mut idle: u32 = 0;

    loop {
        // The browser wizard beat us to it — leave its files untouched.
        if app.watch_commit.as_ref().is_some_and(|p| p.exists()) {
            return Ok(Outcome::CommittedElsewhere);
        }
        app.idle_left = (!seen_input).then(|| IDLE_LIMIT.saturating_sub(idle));
        terminal.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_secs(1))? {
            if !seen_input {
                idle += 1;
                if idle >= IDLE_LIMIT {
                    return Ok(Outcome::Aborted);
                }
            }
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        seen_input = true;

        match app.screen {
            Screen::Choose => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(Outcome::Aborted),
                KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                    app.picked = (app.picked + LAYOUTS.len() - 1) % LAYOUTS.len();
                }
                KeyCode::Right
                | KeyCode::Down
                | KeyCode::Char('l')
                | KeyCode::Char('j')
                | KeyCode::Tab => {
                    app.picked = (app.picked + 1) % LAYOUTS.len();
                }
                KeyCode::Char('1') => app.picked = 0,
                KeyCode::Char('2') => app.picked = 1,
                KeyCode::Char('3') => app.picked = 2,
                KeyCode::Enter if app.resolved().is_ok() => {
                    app.confirm_input.clear();
                    app.screen = Screen::Confirm;
                }
                _ => {}
            },
            Screen::Confirm => match key.code {
                KeyCode::Esc => app.screen = Screen::Choose,
                KeyCode::Backspace => {
                    app.confirm_input.pop();
                }
                KeyCode::Enter => {
                    if app.confirm_input.trim() == "ERASE" {
                        if let Ok(layout) = app.resolved() {
                            return Ok(Outcome::Committed(layout));
                        }
                    }
                }
                KeyCode::Char(c) if app.confirm_input.len() < 8 => {
                    app.confirm_input.push(c.to_ascii_uppercase());
                }
                _ => {}
            },
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title
            Constraint::Min(6),    // body
            Constraint::Length(3), // footer
        ])
        .split(f.area());

    title(f, chunks[0], app);
    match app.screen {
        Screen::Choose => choose_body(f, chunks[1], app),
        Screen::Confirm => confirm_body(f, chunks[1], app),
    }
    footer(f, chunks[2], app);
}

fn title(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "  THE BOX ",
            Style::default()
                .fg(Color::Black)
                .bg(BRASS)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  Set up storage",
            Style::default().fg(BRASS).add_modifier(Modifier::BOLD),
        ),
    ])];
    if let Some(hint) = &app.browser_hint {
        lines.push(Line::styled(format!("  {hint}"), Style::default().fg(IRON)));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(DIM)),
        ),
        area,
    );
}

fn choose_body(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    disk_panel(f, cols[0], app);
    choice_panel(f, cols[1], app);
}

fn disk_panel(f: &mut Frame, area: Rect, app: &App) {
    let chosen: Vec<String> = app
        .resolved()
        .map(|r| r.devices.iter().map(|d| d.name.clone()).collect())
        .unwrap_or_default();

    let mut lines = Vec::new();
    for d in &app.disks {
        let picked = chosen.iter().any(|n| n == &d.name);
        let (marker, style) = if d.removable {
            ("  ", Style::default().fg(DIM))
        } else if picked {
            (
                "‣ ",
                Style::default().fg(BRASS).add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", Style::default().fg(IRON))
        };
        let tail = if d.removable {
            "  (removable — never touched)"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(BRASS)),
            Span::styled(d.describe(), style),
            Span::styled(tail, Style::default().fg(DIM)),
        ]));
    }
    if app.disks.is_empty() {
        lines.push(Line::styled(
            "No disks detected.",
            Style::default().fg(ALERT),
        ));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel("Disks in this machine")),
        area,
    );
}

fn choice_panel(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .split(area);

    // The three layout options.
    let mut opt_lines = Vec::new();
    for (i, k) in LAYOUTS.iter().enumerate() {
        let selected = i == app.picked;
        let (bullet, style) = if selected {
            ("●", Style::default().fg(BRASS).add_modifier(Modifier::BOLD))
        } else {
            ("○", Style::default().fg(IRON))
        };
        opt_lines.push(Line::from(vec![
            Span::styled(format!(" {bullet} {}. ", i + 1), style),
            Span::styled(k.label(), style),
            Span::styled(format!("  — {}", blurb(*k)), Style::default().fg(DIM)),
        ]));
    }
    f.render_widget(
        Paragraph::new(opt_lines).block(panel("Layout  (←/→ to change)")),
        rows[0],
    );

    // Live verdict for the current choice.
    let verdict = match app.resolved() {
        Ok(layout) => verdict_ok(&layout),
        Err(e) => vec![
            Line::styled(
                "Not possible on this machine:",
                Style::default().fg(ALERT).add_modifier(Modifier::BOLD),
            ),
            Line::styled(format!("{e}"), Style::default().fg(ALERT)),
        ],
    };
    f.render_widget(
        Paragraph::new(verdict)
            .wrap(Wrap { trim: true })
            .block(panel("What this does")),
        rows[1],
    );
}

fn verdict_ok(layout: &ResolvedLayout) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("Usable capacity: ", Style::default().fg(IRON)),
        Span::styled(
            format!("~{} GB", layout.usable_gb()),
            Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
        ),
    ])];
    lines.push(Line::styled(
        "Will erase and use:",
        Style::default().fg(IRON),
    ));
    for d in &layout.devices {
        lines.push(Line::styled(
            format!("   • {}", d.describe()),
            Style::default().fg(Color::White),
        ));
    }
    for w in &layout.warnings {
        lines.push(Line::styled(
            format!("   ! {w}"),
            Style::default().fg(ALERT),
        ));
    }
    lines
}

fn confirm_body(f: &mut Frame, area: Rect, app: &App) {
    let layout = match app.resolved() {
        Ok(l) => l,
        Err(_) => {
            f.render_widget(
                Paragraph::new("nothing to confirm").block(panel("Confirm")),
                area,
            );
            return;
        }
    };

    let mut lines = vec![
        Line::styled(
            "This ERASES the disks below and installs Box OS. All data on them is lost.",
            Style::default().fg(ALERT).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
    ];
    lines.extend(verdict_ok(&layout));
    lines.push(Line::raw(""));

    let typed = &app.confirm_input;
    let ready = typed.trim() == "ERASE";
    let field_style = if ready {
        Style::default().fg(GOOD).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(BRASS).add_modifier(Modifier::BOLD)
    };
    lines.push(Line::from(vec![
        Span::styled("Type ", Style::default().fg(IRON)),
        Span::styled(
            "ERASE",
            Style::default().fg(ALERT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to proceed:  ", Style::default().fg(IRON)),
        Span::styled(format!("[{typed}_]"), field_style),
    ]));
    if ready {
        lines.push(Line::styled(
            "  press Enter to install",
            Style::default().fg(GOOD),
        ));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel("Confirm — point of no return")),
        area,
    );
}

fn footer(f: &mut Frame, area: Rect, app: &App) {
    let keys = match app.screen {
        Screen::Choose => "←/→ choose layout   Enter continue   q quit",
        Screen::Confirm => "type ERASE   Enter install   Esc back",
    };
    let mut spans = vec![Span::styled(format!("  {keys}"), Style::default().fg(IRON))];
    if let Some(n) = app.idle_left {
        spans.push(Span::styled(
            format!("     · no input — auto-cancel in {n}s"),
            Style::default().fg(if n <= 30 { ALERT } else { DIM }),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(DIM)),
        ),
        area,
    );
}

fn panel(t: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {t} "),
            Style::default().fg(BRASS).add_modifier(Modifier::BOLD),
        ))
}

fn blurb(k: LayoutKind) -> &'static str {
    match k {
        LayoutKind::Single => "one disk",
        LayoutKind::Mirror => "survives one disk failing",
        LayoutKind::Pool => "one big volume, no safety net",
    }
}
