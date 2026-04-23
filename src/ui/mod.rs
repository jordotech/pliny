//! Ratatui TUI for pliny.
//!
//! Layout:
//! ```text
//! +----------------------------------------+
//! | Summary (30%): AI output or status     |
//! +----------------------------------------+
//! | Tree (70%): grouped plan changes       |
//! |                                        |
//! |                                        |
//! +----------------------------------------+
//! ```
//!
//! Tree model: a flat list of rows with indent levels. Each row is either a
//! `Group` header (`+ aws_dynamodb_table (3)`), a `Resource` entry
//! (`+ aws_dynamodb_table.users`), or an `Attr` leaf
//! (`billing_mode: null -> PAY_PER_REQUEST`). Expansion state lives in
//! [`AppState`] as two sets of indices; rows are rebuilt on every state change.

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
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::io::{self, Stdout};

use crate::groups::Group;
use crate::llm::Summary;
use crate::plan::Plan;

pub fn run(plan: Plan, groups: Vec<Group>, summary: Summary) -> Result<()> {
    let mut terminal = enter_tui().context("failed to enter TUI mode")?;
    let result = run_app(&mut terminal, plan, groups, summary);
    // ALWAYS restore terminal, even if the event loop panicked.
    leave_tui(&mut terminal).ok();
    result
}

fn enter_tui() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    // Install a panic hook that leaves the terminal before printing the panic,
    // so the user doesn't end up with a corrupted shell.
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

struct AppState {
    plan: Plan,
    groups: Vec<Group>,
    summary: Summary,
    expanded_groups: std::collections::HashSet<usize>,
    expanded_resources: std::collections::HashSet<(usize, usize)>,
    list_state: ListState,
    show_help: bool,
    /// Cached flat rows rebuilt every frame from expansion state.
    rows: Vec<tree::Row>,
}

impl AppState {
    fn new(plan: Plan, groups: Vec<Group>, summary: Summary) -> Self {
        let mut s = Self {
            plan,
            groups,
            summary,
            expanded_groups: Default::default(),
            expanded_resources: Default::default(),
            list_state: ListState::default(),
            show_help: false,
            rows: Vec::new(),
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
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        let Some(row) = self.rows.get(idx).cloned() else {
            return;
        };
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
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        let Some(row) = self.rows.get(idx).cloned() else {
            return;
        };
        match row.kind {
            tree::RowKind::Group { group_idx } => {
                self.expanded_groups.remove(&group_idx);
            }
            tree::RowKind::Resource { group_idx, resource_idx } => {
                self.expanded_resources.remove(&(group_idx, resource_idx));
            }
            tree::RowKind::Attr { group_idx, resource_idx, .. } => {
                // Collapse parent resource if we're on an attr.
                self.expanded_resources.remove(&(group_idx, resource_idx));
            }
        }
        self.rebuild_rows();
        // Keep cursor in range.
        if let Some(sel) = self.list_state.selected()
            && sel >= self.rows.len()
            && !self.rows.is_empty()
        {
            self.list_state.select(Some(self.rows.len() - 1));
        }
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    plan: Plan,
    groups: Vec<Group>,
    summary: Summary,
) -> Result<()> {
    let mut state = AppState::new(plan, groups, summary);

    loop {
        terminal.draw(|f| render(f, &mut state))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Ctrl-C quits too.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('?') => state.show_help = !state.show_help,
            KeyCode::Down | KeyCode::Char('j') => state.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => state.move_cursor(-1),
            KeyCode::Right | KeyCode::Char('l') => state.expand(),
            KeyCode::Left | KeyCode::Char('h') => state.collapse(),
            _ => {}
        }
    }

    Ok(())
}

fn render(f: &mut ratatui::Frame, state: &mut AppState) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    render_summary(f, chunks[0], state);
    render_tree(f, chunks[1], state);
}

fn render_summary(f: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &AppState) {
    let (style, title) = match &state.summary {
        Summary::Ok(_) => (Style::default().fg(Color::Green), "AI Summary"),
        Summary::Disabled(_) => (Style::default().fg(Color::Gray), "AI Summary (disabled)"),
        Summary::Error(_) => (Style::default().fg(Color::Red), "AI Summary (error)"),
    };

    let total = state.plan.total_changes();
    let stats = format!(
        "changes: {total}   groups: {g}",
        g = state.groups.len()
    );
    let text: Vec<Line> = vec![
        Line::from(Span::styled(stats, Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(highlight_risky(state.summary.text())),
    ];

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

fn render_tree(f: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &mut AppState) {
    let items: Vec<ListItem> = state
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            let line = format!("{indent}{}", row.label);
            ListItem::new(line).style(row.style())
        })
        .collect();

    let title = if state.show_help {
        "Plan (? close help)"
    } else {
        "Plan (↑/↓ nav, → expand, ← collapse, ? help, q quit)"
    };

    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");

    f.render_stateful_widget(list, area, &mut state.list_state);
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
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
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
