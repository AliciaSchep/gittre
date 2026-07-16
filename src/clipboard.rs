use std::io::{self, Write};

use base64::Engine;

/// Keep remote copies bounded. Terminals commonly cap OSC 52 payloads, and
/// emitting an unbounded review into the terminal can stall the UI.
const MAX_OSC52_BYTES: usize = 100_000;

/// Copy text using the desktop clipboard when available, with OSC 52 as the
/// terminal-native path for SSH sessions and as a fallback elsewhere.
pub fn copy(text: &str) -> Result<(), String> {
    if is_remote_session() {
        return write_osc52(text).or_else(|osc_error| {
            write_system(text).map_err(|system_error| {
                format!("OSC 52: {osc_error}; system clipboard: {system_error}")
            })
        });
    }

    write_system(text).or_else(|system_error| {
        write_osc52(text)
            .map_err(|osc_error| format!("system clipboard: {system_error}; OSC 52: {osc_error}"))
    })
}

fn is_remote_session() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

fn write_system(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|e| e.to_string())
}

fn write_osc52(text: &str) -> Result<(), String> {
    if text.len() > MAX_OSC52_BYTES {
        return Err(format!(
            "copy is {} bytes; remote limit is {MAX_OSC52_BYTES}",
            text.len()
        ));
    }

    let sequence = osc52_sequence(text, std::env::var_os("TMUX").is_some());
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| e.to_string())
}

fn osc52_sequence(text: &str, tmux: bool) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let osc = format!("\x1b]52;c;{encoded}\x07");
    if tmux {
        // tmux passthrough: its DCS prefix contributes the first of the two
        // ESC bytes required before the nested OSC sequence.
        format!("\x1bPtmux;\x1b{osc}\x1b\\")
    } else {
        osc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_osc52_sequence() {
        assert_eq!(osc52_sequence("hello", false), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn tmux_wraps_and_escapes_the_sequence() {
        assert_eq!(
            osc52_sequence("hello", true),
            "\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\"
        );
    }
}
