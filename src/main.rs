mod app;
mod git;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;

/// A lean git review TUI: read diffs, nothing else.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Path inside the repository to review (defaults to the current directory)
    #[arg(default_value = ".")]
    path: std::path::PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let repo = git2::Repository::discover(&args.path)
        .with_context(|| format!("not a git repository: {}", args.path.display()))?;
    let diff = git::diff::load_uncommitted(&repo)?;

    let mut terminal = ratatui::init();
    let result = app::App::new(diff).run(&mut terminal);
    ratatui::restore();
    result
}
