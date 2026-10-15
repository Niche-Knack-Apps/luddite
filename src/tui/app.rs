//! Screen state, key handling and rendering for the review screen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use git2::{ObjectType, Oid, Repository};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
    DefaultTerminal, Frame,
};

use crate::push::{self, PlanRow, PushEvent, RowKind};
use crate::rewriter::{Change, CommitRecord, Report};
use crate::tui::diff::{diff_lines, DiffLine};
use crate::tui::graph::{self, GraphRow};

const TICK: Duration = Duration::from_millis(120);
const LANE_COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Red,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Graph,
    Before,
    After,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Graph => Self::Before,
            Self::Before => Self::After,
            Self::After => Self::Graph,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Graph => Self::After,
            Self::Before => Self::Graph,
            Self::After => Self::Before,
        }
    }
}

enum Mode {
    Normal,
    Help,
    PickRemote {
        items: Vec<String>,
        cursor: usize,
    },
    Contacting {
        remote: String,
    },
    Plan {
        remote: String,
        rows: Vec<PlanRow>,
        cursor: usize,
    },
    Pushing {
        remote: String,
        count: usize,
        log: Vec<String>,
        done: Option<bool>,
        rx: Receiver<PushEvent>,
    },
    Message {
        title: String,
        body: Vec<String>,
    },
}

struct Row {
    rec: usize,
    graph: GraphRow,
    deco: Vec<(String, Color)>,
}

/// What the screen did, for the caller's closing summary.
#[derive(Default)]
pub struct Outcome {
    pub pushed: bool,
    pub remote: Option<String>,
    pub refs: usize,
}

pub struct App<'a> {
    report: &'a Report,
    repo: &'a Repository,
    workdir: PathBuf,
    repo_name: String,
    rows: Vec<Row>,
    max_cells: usize,
    selected: usize,
    offset: usize,
    list_height: usize,
    focus: Focus,
    detail_scroll: u16,
    graph_area: Rect,
    detail_area: Rect,
    mode: Mode,
    remote_hint: Option<String>,
    outcome: Outcome,
    quit: bool,
}

impl<'a> App<'a> {
    pub fn new(
        repo: &'a Repository,
        report: &'a Report,
        remote_hint: Option<&str>,
    ) -> Result<Self> {
        let workdir = repo
            .workdir()
            .map(|p| p.to_path_buf())
            .context("repository has no work tree")?;
        let repo_name = workdir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into());

        // Newest first: the walk was parents-before-children, so reverse it.
        let order: Vec<usize> = (0..report.records.len()).rev().collect();
        let dag: Vec<(Oid, Vec<Oid>)> = order
            .iter()
            .map(|&i| {
                let r = &report.records[i];
                (
                    r.display_id(),
                    r.parents.iter().map(|p| report.mapped(*p)).collect(),
                )
            })
            .collect();
        let lanes = graph::layout(&dag);
        let decorations = decorations(repo);

        let mut rows = Vec::with_capacity(order.len());
        let mut max_cells = 0;
        for (k, &i) in order.iter().enumerate() {
            let g = lanes[k].clone();
            max_cells = max_cells.max(g.cells.len());
            let id = report.records[i].display_id();
            rows.push(Row {
                rec: i,
                graph: g,
                deco: decorations.get(&id).cloned().unwrap_or_default(),
            });
        }

        let mut app = App {
            report,
            repo,
            workdir,
            repo_name,
            rows,
            max_cells,
            selected: 0,
            offset: 0,
            list_height: 1,
            focus: Focus::Graph,
            detail_scroll: 0,
            graph_area: Rect::default(),
            detail_area: Rect::default(),
            mode: Mode::Normal,
            remote_hint: remote_hint.map(str::to_string),
            outcome: Outcome::default(),
            quit: false,
        };
        // Land on the first rewritten commit so the change is on screen at once.
        if let Some(first) = app
            .rows
            .iter()
            .position(|r| app.rec(r.rec).change() != Change::Unchanged)
        {
            app.selected = first;
        }
        Ok(app)
    }

    fn rec(&self, i: usize) -> &CommitRecord {
        &self.report.records[i]
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<Outcome> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            self.tick()?;
            if self.quit {
                break;
            }
            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    _ => {}
                }
            }
        }
        Ok(std::mem::take(&mut self.outcome))
    }

    /// Work that happens between frames: contacting the remote, draining push output.
    fn tick(&mut self) -> Result<()> {
        match &mut self.mode {
            Mode::Contacting { remote } => {
                let remote = remote.clone();
                self.mode = match push::ls_remote(&self.workdir, &remote)
                    .and_then(|state| push::build_plan(self.repo, self.report, &remote, &state))
                {
                    Ok(rows) if rows.is_empty() => Mode::Message {
                        title: format!(" {remote} "),
                        body: vec!["Every branch and tag already matches the remote.".into()],
                    },
                    Ok(rows) => Mode::Plan {
                        remote,
                        rows,
                        cursor: 0,
                    },
                    Err(e) => Mode::Message {
                        title: " push ".into(),
                        body: wrap_error(&format!("{e:#}")),
                    },
                };
            }
            Mode::Pushing {
                log,
                done,
                rx,
                count,
                remote,
            } => {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        PushEvent::Line(l) => log.push(l),
                        PushEvent::Done { success, code } => {
                            *done = Some(success);
                            if success {
                                self.outcome.pushed = true;
                                self.outcome.remote = Some(remote.clone());
                                self.outcome.refs = *count;
                            } else {
                                log.push(format!(
                                    "git push exited with {}",
                                    code.map(|c| c.to_string())
                                        .unwrap_or_else(|| "signal".into())
                                ));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ── Keys ────────────────────────────────────────────────────────────────

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.mode {
            Mode::Normal => self.on_key_normal(key, ctrl),
            Mode::Help | Mode::Message { .. } => self.mode = Mode::Normal,
            Mode::Contacting { .. } => {}
            Mode::PickRemote { items, cursor } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                KeyCode::Down | KeyCode::Char('j') => *cursor = (*cursor + 1).min(items.len() - 1),
                KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
                KeyCode::Enter | KeyCode::Char('l') => {
                    let remote = items[*cursor].clone();
                    self.mode = Mode::Contacting { remote };
                }
                _ => {}
            },
            Mode::Plan {
                remote,
                rows,
                cursor,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = (*cursor + 1).min(rows.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
                KeyCode::Char(' ') => {
                    if let Some(r) = rows.get_mut(*cursor) {
                        r.selected = !r.selected;
                    }
                }
                KeyCode::Char('a') => rows.iter_mut().for_each(|r| r.selected = true),
                KeyCode::Char('n') => rows.iter_mut().for_each(|r| r.selected = false),
                KeyCode::Enter | KeyCode::Char('y') => {
                    let count = rows.iter().filter(|r| r.selected).count();
                    if count == 0 {
                        return;
                    }
                    let (tx, rx) = mpsc::channel();
                    push::spawn(self.workdir.clone(), remote.clone(), rows.clone(), tx);
                    self.mode = Mode::Pushing {
                        remote: remote.clone(),
                        count,
                        log: Vec::new(),
                        done: None,
                        rx,
                    };
                }
                _ => {}
            },
            Mode::Pushing { done, .. } => {
                if done.is_some()
                    && matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q'))
                {
                    self.mode = Mode::Normal;
                }
            }
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent, ctrl: bool) {
        let page = self.list_height.max(1);
        match (key.code, self.focus) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => self.quit = true,
            (KeyCode::Char('c'), _) if ctrl => self.quit = true,
            (KeyCode::Char('?'), _) => self.mode = Mode::Help,
            (KeyCode::Char('p'), _) => self.start_push(),
            (KeyCode::Tab, _)
            | (KeyCode::Char('l'), _)
            | (KeyCode::Right, _)
            | (KeyCode::Enter, _) => {
                self.focus = self.focus.next();
                self.detail_scroll = 0;
            }
            (KeyCode::Char('h'), _) | (KeyCode::Left, _) => {
                self.focus = self.focus.previous();
                self.detail_scroll = 0;
            }
            (KeyCode::Char('n'), _) => self.jump_rewritten(true),
            (KeyCode::Char('N'), _) => self.jump_rewritten(false),
            (KeyCode::Down, Focus::Graph) | (KeyCode::Char('j'), Focus::Graph) => {
                self.select(self.selected as isize + 1)
            }
            (KeyCode::Up, Focus::Graph) | (KeyCode::Char('k'), Focus::Graph) => {
                self.select(self.selected as isize - 1)
            }
            (KeyCode::PageDown, Focus::Graph) => {
                self.select(self.selected as isize + page as isize)
            }
            (KeyCode::PageUp, Focus::Graph) => self.select(self.selected as isize - page as isize),
            (KeyCode::Char('d'), Focus::Graph) if ctrl => {
                self.select(self.selected as isize + (page / 2) as isize)
            }
            (KeyCode::Char('u'), Focus::Graph) if ctrl => {
                self.select(self.selected as isize - (page / 2) as isize)
            }
            (KeyCode::Char('g'), Focus::Graph) | (KeyCode::Home, Focus::Graph) => self.select(0),
            (KeyCode::Char('G'), Focus::Graph) | (KeyCode::End, Focus::Graph) => {
                self.select(isize::MAX)
            }
            (KeyCode::Down, focus) | (KeyCode::Char('j'), focus) if focus != Focus::Graph => {
                self.detail_scroll = self.detail_scroll.saturating_add(1)
            }
            (KeyCode::Up, focus) | (KeyCode::Char('k'), focus) if focus != Focus::Graph => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1)
            }
            (KeyCode::PageDown, focus) if focus != Focus::Graph => {
                self.detail_scroll = self.detail_scroll.saturating_add(page as u16)
            }
            (KeyCode::PageUp, focus) if focus != Focus::Graph => {
                self.detail_scroll = self.detail_scroll.saturating_sub(page as u16)
            }
            (KeyCode::Char('d'), focus) if ctrl && focus != Focus::Graph => {
                self.detail_scroll = self.detail_scroll.saturating_add((page / 2) as u16)
            }
            (KeyCode::Char('u'), focus) if ctrl && focus != Focus::Graph => {
                self.detail_scroll = self.detail_scroll.saturating_sub((page / 2) as u16)
            }
            (KeyCode::Char('g'), focus) | (KeyCode::Home, focus) if focus != Focus::Graph => {
                self.detail_scroll = 0
            }
            (KeyCode::Char('G'), focus) | (KeyCode::End, focus) if focus != Focus::Graph => {
                self.detail_scroll = u16::MAX / 2
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }

        let in_graph = contains(self.graph_area, mouse.column, mouse.row);
        let in_detail = contains(self.detail_area, mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp if in_graph => {
                self.focus = Focus::Graph;
                self.select(self.selected as isize - 3);
            }
            MouseEventKind::ScrollDown if in_graph => {
                self.focus = Focus::Graph;
                self.select(self.selected as isize + 3);
            }
            MouseEventKind::ScrollUp if in_detail => {
                if self.focus == Focus::Graph {
                    self.focus = Focus::Before;
                }
                self.detail_scroll = self.detail_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown if in_detail => {
                if self.focus == Focus::Graph {
                    self.focus = Focus::Before;
                }
                self.detail_scroll = self.detail_scroll.saturating_add(3);
            }
            MouseEventKind::Down(MouseButton::Left) if in_graph => {
                self.focus = Focus::Graph;
                let first_row = self.graph_area.y.saturating_add(1);
                let clicked = self
                    .offset
                    .saturating_add(mouse.row.saturating_sub(first_row) as usize);
                self.select(clicked.min(self.rows.len().saturating_sub(1)) as isize);
            }
            MouseEventKind::Down(MouseButton::Left) if in_detail => {
                let midpoint = self.detail_area.x + self.detail_area.width / 2;
                self.focus = if mouse.column < midpoint {
                    Focus::Before
                } else {
                    Focus::After
                };
                self.detail_scroll = 0;
            }
            _ => {}
        }
    }

    fn select(&mut self, to: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        let to = to.clamp(0, last) as usize;
        if to != self.selected {
            self.selected = to;
            self.detail_scroll = 0;
        }
    }

    fn jump_rewritten(&mut self, forward: bool) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let hit = |i: usize| self.rec(self.rows[i].rec).change() != Change::Unchanged;
        let next = if forward {
            (self.selected + 1..n).find(|&i| hit(i))
        } else {
            (0..self.selected).rev().find(|&i| hit(i))
        };
        if let Some(i) = next {
            self.select(i as isize);
        }
    }

    fn start_push(&mut self) {
        if self.report.dry_run {
            self.mode = Mode::Message {
                title: " push ".into(),
                body: vec!["Dry run — nothing was rewritten, so there is nothing to push.".into()],
            };
            return;
        }
        if self.report.ref_updates.is_empty() {
            self.mode = Mode::Message {
                title: " push ".into(),
                body: vec!["No ref was rewritten in this run.".into()],
            };
            return;
        }
        let remotes = push::remotes(self.repo);
        match remotes.len() {
            0 => {
                self.mode = Mode::Message {
                    title: " push ".into(),
                    body: vec!["This repository has no remotes configured.".into()],
                }
            }
            1 => {
                self.mode = Mode::Contacting {
                    remote: remotes[0].clone(),
                }
            }
            _ => {
                let preferred = push::default_remote(self.repo, self.remote_hint.as_deref());
                let cursor = preferred
                    .and_then(|p| remotes.iter().position(|r| *r == p))
                    .unwrap_or(0);
                self.mode = Mode::PickRemote {
                    items: remotes,
                    cursor,
                };
            }
        }
    }

    // ── Drawing ─────────────────────────────────────────────────────────────

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(area);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(body);

        self.graph_area = left;
        self.detail_area = right;

        self.draw_header(frame, header);
        self.draw_graph(frame, left);
        self.draw_detail(frame, right);
        self.draw_footer(frame, footer);
        self.draw_modal(frame, area);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let s = &self.report.stats;
        let mut spans = vec![
            Span::styled(" luddite ", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(self.repo_name.clone(), Style::new().fg(Color::Cyan)),
            Span::raw(format!(
                "   {} commits · {} rewritten · {} cleaned · {} of {} branches · {} tags",
                s.total_commits,
                s.rewritten_commits,
                s.messages_cleaned,
                s.branches_updated,
                s.branches_scanned,
                s.tags_updated
            )),
        ];
        if s.commits_signed > 0 {
            spans.push(Span::raw(format!(" · {} re-signed", s.commits_signed)));
        }
        if self.report.dry_run {
            spans.push(Span::styled("   [dry run]", Style::new().fg(Color::Yellow)));
        }
        if self.outcome.pushed {
            spans.push(Span::styled(
                format!("   pushed {} refs ✓", self.outcome.refs),
                Style::new().fg(Color::Green),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let text = match self.mode {
            Mode::Normal => " j/k ↑↓ move/scroll   h/l ←/→ Graph·Before·After   wheel pane-scroll   n/N rewritten   p push   ? help   q quit",
            Mode::Plan { .. } => " j/k move   space toggle   a all   n none   Enter push   Esc cancel",
            Mode::Pushing { .. } => " pushing… (atomic: every ref updates or none does)",
            Mode::PickRemote { .. } => " j/k choose remote   Enter select   Esc cancel",
            _ => " any key to close",
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::new().fg(Color::DarkGray)),
            area,
        );
    }

    fn draw_graph(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Graph;
        let block = Block::bordered()
            .title(" Commits ")
            .border_style(border(focused));
        let inner = block.inner(area);
        let height = inner.height as usize;
        let width = inner.width as usize;
        self.list_height = height.max(1);

        if self.selected < self.offset {
            self.offset = self.selected;
        } else if height > 0 && self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }

        let graph_w = self.max_cells.min(width / 3).max(1);
        let mut lines = Vec::with_capacity(height);
        for (k, row) in self.rows.iter().enumerate().skip(self.offset).take(height) {
            let rec = self.rec(row.rec);
            let change = rec.change();
            let (marker, style) = match change {
                Change::Cleaned => (
                    "~",
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Change::Cascade => (
                    "·",
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM),
                ),
                Change::Unchanged => (" ", Style::new()),
            };
            let mut spans: Vec<Span> = Vec::new();
            let mut used = 0;
            for &(ch, lane) in row.graph.cells.iter().take(graph_w) {
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::new().fg(lane_color(lane)),
                ));
                used += 1;
            }
            if row.graph.cells.len() > graph_w {
                spans.pop();
                spans.push(Span::styled("…", Style::new().fg(Color::DarkGray)));
            }
            if used < graph_w {
                spans.push(Span::raw(" ".repeat(graph_w - used)));
            }
            spans.push(Span::styled(format!(" {marker} "), style));
            let sha = short(rec.display_id());
            spans.push(Span::styled(sha, style));
            let mut rest = width.saturating_sub(graph_w + 3 + 7 + 1);
            let mut deco_text = String::new();
            let mut deco_spans: Vec<Span> = Vec::new();
            if !row.deco.is_empty() {
                deco_spans.push(Span::raw(" ("));
                for (i, (d, c)) in row.deco.iter().enumerate() {
                    if i > 0 {
                        deco_spans.push(Span::raw(", "));
                        deco_text.push_str(", ");
                    }
                    deco_spans.push(Span::styled(
                        d.clone(),
                        Style::new().fg(*c).add_modifier(Modifier::BOLD),
                    ));
                    deco_text.push_str(d);
                }
                deco_spans.push(Span::raw(")"));
                rest = rest.saturating_sub(deco_text.chars().count() + 3);
            }
            let subject = truncate(&rec.summary, rest);
            let subj_len = subject.chars().count();
            spans.push(Span::styled(format!(" {subject}"), style));
            spans.extend(deco_spans);
            let filled = graph_w
                + 3
                + 7
                + 1
                + subj_len
                + if deco_text.is_empty() {
                    0
                } else {
                    deco_text.chars().count() + 3
                };
            if filled < width {
                spans.push(Span::raw(" ".repeat(width - filled)));
            }
            let mut line = Line::from(spans);
            if k == self.selected {
                line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect) {
        let row = self.rows.get(self.selected);
        let changed = row
            .map(|row| self.rec(row.rec).change() != Change::Unchanged)
            .unwrap_or(false);
        let tab_style = |tab: Focus, color: Color| {
            let mut style = if changed {
                Style::new().fg(color)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            if self.focus == tab {
                style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            }
            style
        };
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(" BEFORE ", tab_style(Focus::Before, Color::Red)),
            Span::styled(" → ", Style::new().fg(Color::DarkGray)),
            Span::styled(" AFTER ", tab_style(Focus::After, Color::Green)),
            Span::raw(" "),
        ]);
        let block = Block::bordered()
            .title(title)
            .border_style(border(self.focus != Focus::Graph));
        // With graph focus, leave the original visible as the natural starting
        // point for the Graph → Before → After → Graph focus cycle.
        let side = if self.focus == Focus::After {
            Focus::After
        } else {
            Focus::Before
        };
        let lines = match row {
            Some(row) => self.detail_lines(row, side),
            None => vec![Line::raw("no commits")],
        };
        let inner = block.inner(area);
        // Count rendered rows, not source lines: identities and messages often
        // wrap in this narrow pane and must remain reachable by wheel/key scroll.
        let width = inner.width.max(1) as usize;
        let rendered_lines: usize = lines
            .iter()
            .map(|line| line.width().max(1).div_ceil(width))
            .sum();
        let max_scroll = rendered_lines.saturating_sub(inner.height as usize) as u16;
        let scroll = self.detail_scroll.min(max_scroll);
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        frame.render_widget(paragraph.block(block).scroll((scroll, 0)), area);
    }

    fn detail_lines(&self, row: &Row, side: Focus) -> Vec<Line<'static>> {
        let rec = self.rec(row.rec);
        let after = side == Focus::After;
        let changed = rec.change() != Change::Unchanged;
        let side_color = if changed {
            if after {
                Color::Green
            } else {
                Color::Red
            }
        } else {
            Color::Gray
        };
        let key = |k: &str| Span::styled(format!("{k:<10}"), Style::new().fg(Color::DarkGray));
        let mut out: Vec<Line<'static>> = Vec::new();

        let view = if after { "AFTER" } else { "BEFORE" };
        out.push(Line::styled(
            format!(" {view} — use ←/→ or h/l to change view "),
            Style::new().fg(side_color).add_modifier(Modifier::BOLD),
        ));
        out.push(Line::raw(""));

        let displayed_id = if after {
            rec.new.unwrap_or(rec.old)
        } else {
            rec.old
        };
        let mut id_spans = vec![
            key("Commit"),
            Span::styled(short(displayed_id), Style::new().fg(side_color)),
        ];
        if after && self.report.dry_run && changed {
            id_spans.push(Span::styled(
                "  (preview; id assigned during rewrite)",
                Style::new().fg(Color::Yellow),
            ));
        }
        out.push(Line::from(id_spans));

        let (status, color) = match rec.change() {
            Change::Cleaned => {
                let mut why = Vec::new();
                if rec.msg_changed {
                    why.push("message cleaned");
                }
                if rec.identity_changed {
                    why.push("identity replaced");
                }
                if !rec.gitlinks.is_empty() {
                    why.push("gitlinks remapped");
                }
                if after {
                    (format!("REWRITTEN — {}", why.join(", ")), Color::Green)
                } else {
                    (format!("ORIGINAL — {}", why.join(", ")), Color::Red)
                }
            }
            Change::Cascade if after => ("REWRITTEN — parent changed".to_string(), Color::Green),
            Change::Cascade => ("ORIGINAL — parent was rewritten".to_string(), Color::Red),
            Change::Unchanged => ("unchanged".to_string(), Color::DarkGray),
        };
        out.push(Line::from(vec![
            key("Status"),
            Span::styled(status, Style::new().fg(color)),
        ]));

        let identity = |label: &'static str, old: &str, new: &str| {
            let value = if after { new } else { old };
            let color = if old == new { Color::Gray } else { side_color };
            Line::from(vec![
                key(label),
                Span::styled(value.to_string(), Style::new().fg(color)),
            ])
        };
        out.push(identity("Author", &rec.old_author, &rec.new_author));
        out.push(identity(
            "Committer",
            &rec.old_committer,
            &rec.new_committer,
        ));

        let signed = if after {
            rec.signed_after
        } else {
            rec.signed_before
        };
        let signature_changed = rec.signed_before != rec.signed_after || (changed && signed);
        let mut signature = if signed { "yes" } else { "no" }.to_string();
        if after && rec.signed_before && !rec.signed_after {
            signature.push_str("  (dropped; run with --sign to re-sign)");
        } else if after && rec.signed_before && rec.signed_after && changed {
            signature.push_str("  (re-signed)");
        }
        out.push(Line::from(vec![
            key("Signed"),
            Span::styled(
                signature,
                Style::new().fg(if signature_changed {
                    side_color
                } else {
                    Color::DarkGray
                }),
            ),
        ]));

        let parents: Vec<String> = rec
            .parents
            .iter()
            .map(|p| {
                if after {
                    short(self.report.mapped(*p))
                } else {
                    short(*p)
                }
            })
            .collect();
        out.push(Line::from(vec![
            key("Parents"),
            Span::styled(
                if parents.is_empty() {
                    "(root)".to_string()
                } else {
                    parents.join(" ")
                },
                Style::new().fg(if rec.parents_changed {
                    side_color
                } else {
                    Color::Gray
                }),
            ),
        ]));

        if !row.deco.is_empty() {
            let mut spans = vec![key("Refs")];
            for (i, (d, c)) in row.deco.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(", "));
                }
                spans.push(Span::styled(d.clone(), Style::new().fg(*c)));
            }
            out.push(Line::from(spans));
        }
        for g in &rec.gitlinks {
            out.push(Line::from(vec![
                key("Gitlink"),
                Span::raw(format!("{}: ", g.path)),
                Span::styled(
                    short(if after { g.new } else { g.old }),
                    Style::new().fg(side_color),
                ),
            ]));
        }

        out.push(Line::raw(""));
        out.push(Line::styled(
            if rec.msg_changed {
                if after {
                    "── message after (additions highlighted) ──"
                } else {
                    "── message before (removals highlighted) ──"
                }
            } else {
                "── message (unchanged) ──"
            },
            Style::new().fg(if rec.msg_changed {
                side_color
            } else {
                Color::DarkGray
            }),
        ));
        if rec.msg_changed {
            for d in diff_lines(&rec.old_message, &rec.new_message) {
                match (after, d) {
                    (_, DiffLine::Same(l)) => out.push(Line::raw(format!("  {l}"))),
                    (false, DiffLine::Del(l)) => {
                        out.push(Line::styled(format!("- {l}"), Style::new().fg(Color::Red)))
                    }
                    (true, DiffLine::Add(l)) => out.push(Line::styled(
                        format!("+ {l}"),
                        Style::new().fg(Color::Green),
                    )),
                    _ => {}
                }
            }
        } else {
            let message = if after {
                &rec.new_message
            } else {
                &rec.old_message
            };
            for l in message.lines() {
                out.push(Line::raw(format!("  {l}")));
            }
        }
        out
    }

    fn draw_modal(&self, frame: &mut Frame, area: Rect) {
        match &self.mode {
            Mode::Normal => {}
            Mode::Help => {
                let body: Vec<Line> = HELP.iter().map(|l| Line::raw(*l)).collect();
                modal(frame, area, " keys ", body, 60, 60);
            }
            Mode::Message { title, body } => {
                let lines: Vec<Line> = body.iter().map(|l| Line::raw(l.clone())).collect();
                modal(frame, area, title, lines, 60, 40);
            }
            Mode::Contacting { remote } => {
                modal(
                    frame,
                    area,
                    " push ",
                    vec![Line::raw(format!("Contacting {remote} (git ls-remote)…"))],
                    50,
                    20,
                );
            }
            Mode::PickRemote { items, cursor } => {
                let lines: Vec<Line> = items
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let l = Line::raw(format!("  {r}"));
                        if i == *cursor {
                            l.style(Style::new().add_modifier(Modifier::REVERSED))
                        } else {
                            l
                        }
                    })
                    .collect();
                modal(frame, area, " push to which remote? ", lines, 50, 40);
            }
            Mode::Plan {
                remote,
                rows,
                cursor,
            } => {
                let selected = rows.iter().filter(|r| r.selected).count();
                let rect = centered(area, 90, 80);
                let inner_h = rect.height.saturating_sub(4) as usize; // borders + header + footer
                let offset = cursor
                    .saturating_sub(inner_h.saturating_sub(1) / 2)
                    .min(rows.len().saturating_sub(inner_h));
                let mut lines: Vec<Line> = vec![Line::styled(
                    format!(" {selected} of {} refs selected — only refs that differ from the remote are listed", rows.len()),
                    Style::new().fg(Color::DarkGray),
                )];
                for (i, r) in rows
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(inner_h.saturating_sub(2))
                {
                    let mark = if r.selected { "[x]" } else { "[ ]" };
                    let remote_v = r.remote.map(short).unwrap_or_else(|| "(absent)".into());
                    let kind_style = match r.kind {
                        RowKind::Rewritten => Style::new().fg(Color::Yellow),
                        RowKind::Diverged => Style::new().fg(Color::Red),
                        RowKind::New => Style::new().fg(Color::Cyan),
                    };
                    let note = if r.note.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", r.note)
                    };
                    let l = Line::from(vec![
                        Span::raw(format!(" {mark} ")),
                        Span::raw(format!("{:<44}", truncate(&r.dst, 44))),
                        Span::styled(remote_v, Style::new().fg(Color::Red)),
                        Span::raw(" → "),
                        Span::styled(short(r.src), Style::new().fg(Color::Green)),
                        Span::raw("  "),
                        Span::styled(format!("{}{note}", r.kind.label()), kind_style),
                    ]);
                    lines.push(if i == *cursor {
                        l.style(Style::new().add_modifier(Modifier::REVERSED))
                    } else {
                        l
                    });
                }
                lines.push(Line::styled(
                    " Enter pushes the selected refs atomically, each with --force-with-lease=<ref>:<remote value shown>",
                    Style::new().fg(Color::DarkGray),
                ));
                modal_rect(frame, rect, &format!(" push to {remote} "), lines);
            }
            Mode::Pushing {
                remote,
                log,
                done,
                count,
                ..
            } => {
                let rect = centered(area, 80, 70);
                let inner_h = rect.height.saturating_sub(3) as usize;
                let title = match done {
                    None => format!(" pushing {count} refs to {remote}… "),
                    Some(true) => format!(" pushed {count} refs to {remote} ✓ "),
                    Some(false) => format!(" push to {remote} FAILED — nothing was updated "),
                };
                let mut lines: Vec<Line> = log
                    .iter()
                    .rev()
                    .take(inner_h.saturating_sub(1))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(|l| Line::raw(format!(" {l}")))
                    .collect();
                if done.is_some() {
                    lines.push(Line::styled(
                        " Enter to close",
                        Style::new().fg(Color::DarkGray),
                    ));
                }
                modal_rect(frame, rect, &title, lines);
            }
        }
    }
}

const HELP: [&str; 12] = [
    "  j / k, ↑ / ↓        move in graph or scroll the active detail view",
    "  PgUp / PgDn, ^u ^d  page",
    "  g / G, Home / End   first / last commit",
    "  n / N               next / previous rewritten commit",
    "  h / l, ← / →, Tab   cycle Graph → Before → After → Graph",
    "  p                   push: pick a remote, review every ref that differs, confirm",
    "  ? / any key         close this help",
    "  q / Esc / ^c        quit",
    "",
    "  ~  rewritten (message, identity or gitlinks cleaned)",
    "  ·  rewritten only because a parent changed",
    "  mouse wheel scrolls the pane under it; click a commit or detail side",
];

// ── helpers ────────────────────────────────────────────────────────────────

fn short(oid: Oid) -> String {
    oid.to_string()[..7].to_string()
}

fn lane_color(lane: usize) -> Color {
    LANE_COLORS[lane % LANE_COLORS.len()]
}

fn border(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn wrap_error(e: &str) -> Vec<String> {
    e.split(": ").map(str::to_string).collect()
}

fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (area.width * pct_w / 100).max(20).min(area.width);
    let h = (area.height * pct_h / 100).max(5).min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn modal(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line>, pct_w: u16, pct_h: u16) {
    let rect = centered(area, pct_w, pct_h);
    modal_rect(frame, rect, title, lines);
}

fn modal_rect(frame: &mut Frame, rect: Rect, title: &str, lines: Vec<Line>) {
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(title.to_string())
        .border_style(Style::new().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

/// commit id → names to show next to it (branch, remote/branch, tag: x, stash).
fn decorations(repo: &Repository) -> HashMap<Oid, Vec<(String, Color)>> {
    let mut out: HashMap<Oid, Vec<(String, Color)>> = HashMap::new();
    let head_branch = repo
        .head()
        .ok()
        .filter(|h| h.is_branch())
        .and_then(|h| h.shorthand().map(str::to_string));
    if let Ok(refs) = repo.references() {
        for r in refs.flatten() {
            let name = match r.name() {
                Some(n) => n.to_string(),
                None => continue,
            };
            if name.starts_with("refs/backup/")
                || name.starts_with("refs/original/")
                || name.starts_with("refs/notes/")
            {
                continue;
            }
            let commit = match r.peel(ObjectType::Commit) {
                Ok(o) => o.id(),
                Err(_) => continue,
            };
            let (label, color) = if let Some(b) = name.strip_prefix("refs/heads/") {
                let is_head = head_branch.as_deref() == Some(b);
                (
                    if is_head {
                        format!("HEAD → {b}")
                    } else {
                        b.to_string()
                    },
                    Color::Green,
                )
            } else if let Some(t) = name.strip_prefix("refs/tags/") {
                (format!("tag: {t}"), Color::Yellow)
            } else if let Some(rm) = name.strip_prefix("refs/remotes/") {
                if rm.ends_with("/HEAD") {
                    continue;
                }
                (rm.to_string(), Color::Red)
            } else if name == "refs/stash" {
                ("stash".to_string(), Color::Magenta)
            } else {
                (name.clone(), Color::DarkGray)
            };
            out.entry(commit).or_default().push((label, color));
        }
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| {
            a.0.starts_with("tag: ")
                .cmp(&b.0.starts_with("tag: "))
                .then(a.0.cmp(&b.0))
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_cycles_through_both_comparison_views() {
        assert!(Focus::Graph.next() == Focus::Before);
        assert!(Focus::Before.next() == Focus::After);
        assert!(Focus::After.next() == Focus::Graph);
        assert!(Focus::Graph.previous() == Focus::After);
        assert!(Focus::After.previous() == Focus::Before);
        assert!(Focus::Before.previous() == Focus::Graph);
    }

    #[test]
    fn hit_testing_excludes_the_bottom_and_right_edges() {
        let area = Rect::new(10, 20, 5, 4);
        assert!(contains(area, 10, 20));
        assert!(contains(area, 14, 23));
        assert!(!contains(area, 15, 23));
        assert!(!contains(area, 14, 24));
    }
}
