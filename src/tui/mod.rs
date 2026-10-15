//! The post-run review screen: a lazygit-style commit graph with every
//! rewritten commit highlighted, a before/after pane for the selected commit,
//! and a confirm-and-push step.

pub mod app;
pub mod diff;
pub mod graph;

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use git2::Repository;
use std::io::stdout;

use crate::rewriter::Report;
pub use app::Outcome;

/// Take over the terminal until the user quits; always restores it.
pub fn run(repo: &Repository, report: &Report, remote_hint: Option<&str>) -> Result<Outcome> {
    let mut terminal = ratatui::init();
    let mouse_enabled = execute!(stdout(), EnableMouseCapture).is_ok();
    let result =
        app::App::new(repo, report, remote_hint).and_then(|mut app| app.run(&mut terminal));
    if mouse_enabled {
        let _ = execute!(stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    result
}
