use ratatui::prelude::*;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Files above this many bytes skip highlighting (pager path).
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;

/// Shared syntect state. Highlighting needs a truecolor terminal; when
/// COLORTERM doesn't advertise one, `enabled` is false and callers fall back
/// to the plain diff colors.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
    enabled: bool,
}

impl Highlighter {
    pub fn new() -> Self {
        let enabled = std::env::var("COLORTERM")
            .map(|v| v.contains("truecolor") || v.contains("24bit"))
            .unwrap_or(false);
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_else(|| themes.themes.values().next().cloned().unwrap_or_default());
        Highlighter {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme,
            enabled,
        }
    }

    fn syntax_for(&self, path: &str) -> Option<&SyntaxReference> {
        if !self.enabled {
            return None;
        }
        let ext = std::path::Path::new(path).extension()?.to_str()?;
        self.syntaxes.find_syntax_by_extension(ext)
    }

    /// Independent per-line highlighting (delta-style) for diff lines.
    /// Approximate — multi-line constructs lose state — but cheap and stable.
    pub fn line_spans(&self, path: &str, content: &str) -> Option<Vec<Span<'static>>> {
        let syntax = self.syntax_for(path)?;
        let mut hl = HighlightLines::new(syntax, &self.theme);
        let line = format!("{content}\n");
        let regions = hl.highlight_line(&line, &self.syntaxes).ok()?;
        Some(regions_to_spans(&regions))
    }

    /// Stateful whole-file highlighting for the full-file pager.
    pub fn file_lines(&self, path: &str, content: &str) -> Option<Vec<Vec<Span<'static>>>> {
        if content.len() > MAX_HIGHLIGHT_BYTES {
            return None;
        }
        let syntax = self.syntax_for(path)?;
        let mut hl = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::new();
        for line in content.lines() {
            let line = format!("{line}\n");
            let regions = hl.highlight_line(&line, &self.syntaxes).ok()?;
            out.push(regions_to_spans(&regions));
        }
        Some(out)
    }
}

fn regions_to_spans(regions: &[(syntect::highlighting::Style, &str)]) -> Vec<Span<'static>> {
    regions
        .iter()
        .map(|(style, text)| {
            let fg = style.foreground;
            let mut s = Style::new().fg(Color::Rgb(fg.r, fg.g, fg.b));
            if style
                .font_style
                .contains(syntect::highlighting::FontStyle::BOLD)
            {
                s = s.bold();
            }
            if style
                .font_style
                .contains(syntect::highlighting::FontStyle::ITALIC)
            {
                s = s.italic();
            }
            Span::styled(text.trim_end_matches('\n').to_string(), s)
        })
        .collect()
}
