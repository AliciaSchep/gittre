mod app;
mod comments;
mod event;
mod git;
mod keymap;
mod ui;
mod watch;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use git::scope::Scope;

/// A lean git review TUI: read diffs, nothing else.
///
/// With no arguments, opens a picker to choose what to review.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Review a commit (sha, ref, HEAD~2) or a range (a..b, a...b)
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

    /// Disable auto-reload on repo changes (press r to reload manually).
    /// Useful in very large repositories.
    #[arg(long)]
    no_watch: bool,
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
            return Ok(Some(git::scope::rev_scope(repo, rev)?));
        }
        Ok(None)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Print review comments as markdown (or write with -o)
    Export {
        /// Write to a file instead of stdout
        #[arg(short, long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();

    let repo = git2::Repository::discover(&args.repo)
        .with_context(|| format!("not a git repository: {}", args.repo.display()))?;

    if let Some(Command::Export { output }) = &args.command {
        let store = comments::CommentStore::load(&repo);
        let title = repo
            .workdir()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repository".into());
        let date = app::today_string();
        let md = comments::export_markdown(&store.comments, &title, &date);
        match output {
            Some(path) => {
                std::fs::write(path, md).with_context(|| format!("writing {}", path.display()))?;
                eprintln!(
                    "exported {} comments to {}",
                    store.comments.len(),
                    path.display()
                );
            }
            None => print!("{md}"),
        }
        return Ok(());
    }

    // Resolve (and validate) any CLI-selected scope before entering raw
    // mode, so errors print as plain text; the diff itself loads in the
    // background once the TUI is up.
    let initial = args.initial_scope(&repo)?;

    let mut terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture,
        ratatui::crossterm::event::EnableBracketedPaste
    );
    // ratatui's panic hook restores the terminal but doesn't know about
    // mouse capture; chain our own so a panic doesn't leave it enabled.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::DisableMouseCapture,
            ratatui::crossterm::event::DisableBracketedPaste
        );
        prev_hook(info);
    }));
    let result = app::App::new(repo, initial, !args.no_watch).run(&mut terminal);
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture,
        ratatui::crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    result
}
