mod app;
mod git;
mod ui;

use anyhow::{Context, Result, bail};
use clap::Parser;

use git::scope::Scope;

/// A lean git review TUI: read diffs, nothing else.
///
/// With no arguments, opens a picker to choose what to review.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Review a specific commit (sha, ref, HEAD~2, …)
    rev: Option<String>,

    /// Review all uncommitted work (working tree + index vs HEAD)
    #[arg(short, long)]
    uncommitted: bool,

    /// Review staged changes only
    #[arg(short, long)]
    staged: bool,

    /// Review the current branch vs BASE (auto-detected when omitted)
    #[arg(short, long, num_args = 0..=1, default_missing_value = "", value_name = "BASE")]
    branch: Option<String>,

    /// Repository path (defaults to the current directory)
    #[arg(short = 'C', long = "repo", default_value = ".", value_name = "PATH")]
    repo: std::path::PathBuf,
}

impl Args {
    fn initial_scope(&self, repo: &git2::Repository) -> Result<Option<Scope>> {
        let flags = [
            self.uncommitted,
            self.staged,
            self.branch.is_some(),
            self.rev.is_some(),
        ];
        if flags.iter().filter(|&&f| f).count() > 1 {
            bail!("pick at most one of: REV, --uncommitted, --staged, --branch");
        }
        if self.uncommitted {
            return Ok(Some(Scope::Uncommitted));
        }
        if self.staged {
            return Ok(Some(Scope::Staged));
        }
        if let Some(base) = &self.branch {
            let base = if base.is_empty() {
                git::scope::detect_base(repo).context(
                    "no review base found: set an upstream or pass one with --branch <BASE>",
                )?
            } else {
                base.clone()
            };
            return Ok(Some(Scope::Branch { base }));
        }
        if let Some(rev) = &self.rev {
            return Ok(Some(git::scope::commit_scope(repo, rev)?));
        }
        Ok(None)
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    let repo = git2::Repository::discover(&args.repo)
        .with_context(|| format!("not a git repository: {}", args.repo.display()))?;

    // Resolve and load any CLI-selected scope before entering raw mode, so
    // errors print as plain text instead of garbling the terminal.
    let initial = match args.initial_scope(&repo)? {
        Some(scope) => {
            let diff = git::diff::load(&repo, &scope)?;
            Some((scope, diff))
        }
        None => None,
    };

    let mut terminal = ratatui::init();
    let result = app::App::new(repo, initial).run(&mut terminal);
    ratatui::restore();
    result
}
