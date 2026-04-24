//! Ratatui TUI for pliny — k9s-inspired single-screen layout.
//!
//! Layout:
//! ```text
//! +------------------------------------------------------------+
//! | Context (5 lines): CWD / AI state / shortcuts              |
//! +------------------------------------------------------------+
//! | AI Summary (20%)                                           |
//! +------------------------------------------------------------+
//! | Plan tree (flex)                                           |
//! +------------------------------------------------------------+
//! | Output log (25%, follows tail)                             |
//! +------------------------------------------------------------+
//! | Command bar (1 line, shown only in command mode)           |
//! +------------------------------------------------------------+
//! ```
//!
//! Modes: `Normal` (j/k nav, h/l collapse-expand, `:` enters Command,
//! `?` toggles help, q/Esc quit) and `Command` (type freeform terraform
//! args, Enter spawns `terraform <input>`, Esc cancels).

mod tree;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::io::{self, Stdout};
use std::time::Duration;

use crate::digest;
use crate::groups::{self, Group};
use crate::llm::{self, Summary};
use crate::plan::Plan;
use crate::runner::{
    RunnerEvent, RunnerHandle, WorkspaceState, read_workspaces, spawn as spawn_runner,
};

const MAX_OUTPUT_LINES: usize = 2000;

pub struct InitialState {
    pub plan: Option<Plan>,
    pub groups: Vec<Group>,
    pub summary: Summary,
    pub show_noops: bool,
    pub no_ai: bool,
    pub model: String,
}

pub fn run(init: InitialState) -> Result<()> {
    let mut terminal = enter_tui().context("failed to enter TUI mode")?;
    let result = run_app(&mut terminal, init);
    leave_tui(&mut terminal).ok();
    result
}

fn enter_tui() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));
    Ok(terminal)
}

fn leave_tui(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunStatus {
    Idle,
    Running,
}

struct App {
    plan: Option<Plan>,
    groups: Vec<Group>,
    summary: Summary,
    expanded_groups: std::collections::HashSet<usize>,
    expanded_resources: std::collections::HashSet<(usize, usize)>,
    list_state: ListState,
    rows: Vec<tree::Row>,
    show_help: bool,
    show_noops: bool,
    no_ai: bool,
    model: String,
    mode: Mode,
    cmd_buf: String,
    output: Vec<String>,
    runner: Option<RunnerHandle>,
    status: RunStatus,
    last_cmd: Option<String>,
    cwd: String,
    workspaces: WorkspaceState,
}

impl App {
    fn new(init: InitialState) -> Self {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".into());
        let mut s = Self {
            plan: init.plan,
            groups: init.groups,
            summary: init.summary,
            expanded_groups: Default::default(),
            expanded_resources: Default::default(),
            list_state: ListState::default(),
            rows: Vec::new(),
            show_help: false,
            show_noops: init.show_noops,
            no_ai: init.no_ai,
            model: init.model,
            mode: Mode::Normal,
            cmd_buf: String::new(),
            output: Vec::new(),
            runner: None,
            status: RunStatus::Idle,
            last_cmd: None,
            cwd,
            workspaces: read_workspaces(),
        };
        s.rebuild_rows();
        if !s.rows.is_empty() {
            s.list_state.select(Some(0));
        }
        s
    }

    fn rebuild_rows(&mut self) {
        self.rows = tree::build_rows(&self.groups, &self.expanded_groups, &self.expanded_resources);
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len - 1);
        self.list_state.select(Some(next as usize));
    }

    fn expand(&mut self) {
        let Some(idx) = self.list_state.selected() else { return };
        let Some(row) = self.rows.get(idx).cloned() else { return };
        match row.kind {
            tree::RowKind::Group { group_idx } => {
                self.expanded_groups.insert(group_idx);
            }
            tree::RowKind::Resource { group_idx, resource_idx } => {
                self.expanded_resources.insert((group_idx, resource_idx));
            }
            tree::RowKind::Attr { .. } => {}
        }
        self.rebuild_rows();
    }

    fn collapse(&mut self) {
        let Some(idx) = self.list_state.selected() else { return };
        let Some(row) = self.rows.get(idx).cloned() else { return };
        match row.kind {
            tree::RowKind::Group { group_idx } => {
                self.expanded_groups.remove(&group_idx);
            }
            tree::RowKind::Resource { group_idx, resource_idx } => {
                self.expanded_resources.remove(&(group_idx, resource_idx));
            }
            tree::RowKind::Attr { group_idx, resource_idx, .. } => {
                self.expanded_resources.remove(&(group_idx, resource_idx));
            }
        }
        self.rebuild_rows();
        if let Some(sel) = self.list_state.selected()
            && sel >= self.rows.len()
            && !self.rows.is_empty()
        {
            self.list_state.select(Some(self.rows.len() - 1));
        }
    }

    fn push_output(&mut self, line: String) {
        self.output.push(line);
        if self.output.len() > MAX_OUTPUT_LINES {
            let excess = self.output.len() - MAX_OUTPUT_LINES;
            self.output.drain(0..excess);
        }
    }

    fn submit_command(&mut self) {
        let cmd = self.cmd_buf.trim().to_string();
        self.cmd_buf.clear();
        self.mode = Mode::Normal;
        if cmd.is_empty() {
            return;
        }

        // Alias: `:ws ...` -> `:workspace ...`
        let normalized = if let Some(rest) = cmd.strip_prefix("ws ") {
            format!("workspace {rest}")
        } else if cmd == "ws" {
            "workspace".into()
        } else {
            cmd.clone()
        };

        // `:workspace` (no args) — show picker inline, don't shell out.
        if normalized == "workspace" {
            self.show_workspace_picker();
            self.last_cmd = Some(cmd);
            return;
        }

        self.push_output(String::new());
        self.push_output(format!("──── :{normalized} ────"));
        self.runner = Some(spawn_runner(&normalized));
        self.status = RunStatus::Running;
        self.last_cmd = Some(cmd);
    }

    fn show_workspace_picker(&mut self) {
        self.workspaces = read_workspaces();
        self.push_output(String::new());
        self.push_output("──── workspaces ────".into());
        if self.workspaces.all.is_empty() {
            self.push_output(
                "(none found — run :init or check you are inside a terraform project)".into(),
            );
            return;
        }
        let all = self.workspaces.all.clone();
        let current = self.workspaces.current.clone();
        for ws in &all {
            let marker = if Some(ws) == current.as_ref() { "* " } else { "  " };
            self.push_output(format!("{marker}{ws}"));
        }
        self.push_output(
            "switch with :workspace select <name>  (or :workspace new <name>)".into(),
        );
    }

    fn drain_runner(&mut self) {
        let Some(handle) = self.runner.as_ref() else { return };
        let events = handle.drain();
        for ev in events {
            match ev {
                RunnerEvent::Line(line) => self.push_output(line),
                RunnerEvent::Done { exit_code, json_plan, original_cmd } => {
                    let code_str = exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into());
                    self.push_output(format!("── done: exit {code_str} ──"));
                    self.status = RunStatus::Idle;
                    self.runner = None;

                    // Workspace-affecting commands: refresh state.
                    if original_cmd.starts_with("workspace") || original_cmd.starts_with("init") {
                        self.workspaces = read_workspaces();
                    }

                    if let Some(json) = json_plan {
                        self.ingest_plan_json(&json);
                    }

                    // Clear plan state after successful apply.
                    if original_cmd.starts_with("apply") && exit_code == Some(0) {
                        self.plan = None;
                        self.groups.clear();
                        self.summary = Summary::Disabled("apply complete — no plan loaded");
                        self.expanded_groups.clear();
                        self.expanded_resources.clear();
                        self.rebuild_rows();
                        self.list_state.select(None);
                    }
                }
            }
        }
    }

    fn ingest_plan_json(&mut self, json: &str) {
        match Plan::parse(json) {
            Ok(plan) => {
                let groups = groups::group_plan(&plan, self.show_noops);
                let summary = if self.no_ai || std::env::var_os("OPENAI_API_KEY").is_none() {
                    Summary::Disabled(llm::disabled_reason(self.no_ai))
                } else {
                    let digest = digest::digest_with_budget(&plan, &groups);
                    self.push_output("pliny: requesting AI summary…".into());
                    llm::fetch_summary(&digest, &self.model)
                };
                self.plan = Some(plan);
                self.groups = groups;
                self.summary = summary;
                self.expanded_groups.clear();
                self.expanded_resources.clear();
                self.rebuild_rows();
                if !self.rows.is_empty() {
                    self.list_state.select(Some(0));
                }
            }
            Err(e) => self.push_output(format!("pliny: plan parse error: {e}")),
        }
    }
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, init: InitialState) -> Result<()> {
    let mut app = App::new(init);

    loop {
        terminal.draw(|f| render(f, &mut app))?;

        app.drain_runner();

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break;
        }

        match app.mode {
            Mode::Normal => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('?') => app.show_help = !app.show_help,
                KeyCode::Char(':') => {
                    app.mode = Mode::Command;
                    app.cmd_buf.clear();
                }
                KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                KeyCode::Right | KeyCode::Char('l') => app.expand(),
                KeyCode::Left | KeyCode::Char('h') => app.collapse(),
                _ => {}
            },
            Mode::Command => match key.code {
                KeyCode::Esc => {
                    app.mode = Mode::Normal;
                    app.cmd_buf.clear();
                }
                KeyCode::Enter => app.submit_command(),
                KeyCode::Backspace => {
                    app.cmd_buf.pop();
                }
                KeyCode::Char(c) => app.cmd_buf.push(c),
                _ => {}
            },
        }
    }

    Ok(())
}

fn render(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    let show_cmd_bar = app.mode == Mode::Command;
    let constraints: Vec<Constraint> = if show_cmd_bar {
        vec![
            Constraint::Length(8),   // context header (5 rows + borders)
            Constraint::Length(6),   // summary
            Constraint::Min(5),      // tree (flex)
            Constraint::Length(10),  // output log
            Constraint::Length(4),   // command bar
        ]
    } else {
        vec![
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Min(5),
            Constraint::Length(10),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    render_header(f, chunks[0], app);
    render_summary(f, chunks[1], app);
    render_tree(f, chunks[2], app);
    render_output(f, chunks[3], app);
    if show_cmd_bar {
        render_command_bar(f, chunks[4], app);
    }
}

fn render_header(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let ai_state = if app.no_ai {
        "disabled (--no-ai)".to_string()
    } else if std::env::var_os("OPENAI_API_KEY").is_none() {
        "disabled (no OPENAI_API_KEY)".to_string()
    } else {
        format!("{} (OPENAI_API_KEY set)", app.model)
    };
    let status = match app.status {
        RunStatus::Idle => Span::styled("idle", Style::default().fg(Color::Gray)),
        RunStatus::Running => Span::styled(
            "running…",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
    };

    let ws_line = match (&app.workspaces.current, app.workspaces.all.len()) {
        (Some(cur), n) if n > 0 => format!("{cur}  ({n} available — :ws to list)"),
        (Some(cur), _) => cur.clone(),
        (None, _) => "(none — not a terraform project?)".into(),
    };

    let left = vec![
        Line::from(vec![
            Span::styled("CWD:       ", Style::default().fg(Color::DarkGray)),
            Span::raw(&app.cwd),
        ]),
        Line::from(vec![
            Span::styled("Workspace: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                ws_line,
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("AI:        ", Style::default().fg(Color::DarkGray)),
            Span::raw(ai_state),
        ]),
        Line::from(vec![
            Span::styled("Status:    ", Style::default().fg(Color::DarkGray)),
            status,
            Span::raw("  "),
            Span::styled(
                app.last_cmd
                    .as_deref()
                    .map(|c| format!("last: :{c}"))
                    .unwrap_or_default(),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("Plan:      ", Style::default().fg(Color::DarkGray)),
            Span::raw(
                app.plan
                    .as_ref()
                    .map(|p| p.change_counts().footer())
                    .unwrap_or_else(|| "(none loaded — press : then type plan)".into()),
            ),
        ]),
        Line::from(vec![
            Span::styled("Keys:      ", Style::default().fg(Color::DarkGray)),
            key_hint(":"),
            Span::raw(" cmd  "),
            key_hint(":ws"),
            Span::raw(" workspaces  "),
            key_hint("j/k"),
            Span::raw(" nav  "),
            key_hint("h/l"),
            Span::raw(" fold  "),
            key_hint("?"),
            Span::raw(" help  "),
            key_hint("q"),
            Span::raw(" quit"),
        ]),
    ];

    let p = Paragraph::new(left)
        .block(
            Block::default()
                .title(" pliny ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn key_hint(s: &str) -> Span<'_> {
    Span::styled(
        format!("<{s}>"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )
}

fn render_summary(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let (style, title) = match &app.summary {
        Summary::Ok(_) => (Style::default().fg(Color::Green), "AI Summary"),
        Summary::Disabled(_) => (Style::default().fg(Color::Gray), "AI Summary (disabled)"),
        Summary::Error(_) => (Style::default().fg(Color::Red), "AI Summary (error)"),
    };

    let text = vec![Line::from(highlight_risky(app.summary.text()))];
    let p = Paragraph::new(text)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(style),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn render_tree(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            let line = format!("{indent}{}", row.label);
            ListItem::new(line).style(row.style())
        })
        .collect();

    let title = if app.show_help {
        " Plan (? close help) ".to_string()
    } else if app.rows.is_empty() {
        " Plan (empty — run :plan to populate) ".to_string()
    } else {
        format!(" Plan ({} rows) ", app.rows.len())
    };

    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_output(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let start = app.output.len().saturating_sub(inner_h);
    let lines: Vec<Line> = app.output[start..]
        .iter()
        .map(|s| Line::from(s.as_str()))
        .collect();
    let title = match app.status {
        RunStatus::Running => " Output (running…) ",
        RunStatus::Idle if app.output.is_empty() => " Output ",
        RunStatus::Idle => " Output ",
    };
    let p = Paragraph::new(lines).block(Block::default().title(title).borders(Borders::ALL));
    f.render_widget(p, area);
}

fn render_command_bar(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let content = Line::from(vec![
        Span::styled(
            ":",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw(&app.cmd_buf),
        Span::styled(
            "█",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::SLOW_BLINK),
        ),
    ]);
    let hint = Line::from(Span::styled(
        "  Enter runs `terraform <input>` · Esc cancel · try: plan / plan -refresh=false / apply -auto-approve",
        Style::default().fg(Color::DarkGray),
    ));
    let p = Paragraph::new(vec![content, hint]).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta)),
    );
    f.render_widget(p, area);
}

/// Render a single line, styling any `Risky:` prefix in red-bold.
fn highlight_risky(text: &str) -> Vec<Span<'_>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find("Risky:") {
        let (before, after) = rest.split_at(idx);
        if !before.is_empty() {
            spans.push(Span::raw(before));
        }
        let (risky, tail) = after.split_at("Risky:".len());
        spans.push(Span::styled(
            risky,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        rest = tail;
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest));
    }
    if spans.is_empty() {
        spans.push(Span::raw(text));
    }
    spans
}
