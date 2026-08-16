//! Console rendering for startup diagnostics.
//!
//! Error values carry plain text (the same strings serve HTTP dev-page
//! bodies and log files); color is applied here, at the terminal boundary
//! only — stderr must be a TTY and `NO_COLOR` unset. The layout mirrors
//! `anyhow`'s report (`Error: ...` + `Caused by:` chain) so a piped or
//! non-TTY run prints byte-identical output to the previous behavior.

use std::io::IsTerminal as _;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";
const BLUE: &str = "\x1b[34m";

/// Prints an error report to stderr, colorized when it is a terminal.
pub(crate) fn report(err: &anyhow::Error) {
    let colored = std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let mut out = String::new();
    push_block(&mut out, &format!("Error: {err}"), "", colored);
    let mut causes = err.chain().skip(1).peekable();
    if causes.peek().is_some() {
        out.push('\n');
        if colored {
            out.push_str(&format!("{BOLD}Caused by:{RESET}"));
        } else {
            out.push_str("Caused by:");
        }
        out.push('\n');
        for cause in causes {
            push_block(&mut out, &cause.to_string(), "    ", colored);
        }
    }
    eprint!("{out}");
}

/// Renders one (possibly multi-line) message, painting each line by shape.
fn push_block(out: &mut String, text: &str, indent: &str, colored: bool) {
    for line in text.lines() {
        out.push_str(indent);
        if colored {
            out.push_str(&paint_line(line));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
}

/// Applies the rustc-like palette to one diagnostic line: red for the error
/// headline and caret, blue for the gutter, cyan for the location, dim for
/// tracebacks, and a minimal Lua highlight inside source lines.
fn paint_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    // `Error: ...` / `error: ...` headlines (`script error:` is how
    // `Error::Script` diagnostics display).
    for prefix in ["Error:", "script error:", "error:"] {
        if let Some(message) = trimmed.strip_prefix(prefix) {
            return format!("{indent}{BOLD}{RED}{prefix}{RESET}{BOLD}{message}{RESET}");
        }
    }
    // `  --> path:line`
    if let Some(location) = trimmed.strip_prefix("--> ") {
        return format!("{indent}{BOLD}{BLUE}-->{RESET} {CYAN}{location}{RESET}");
    }
    // Caret marker: `   | ^^^^^`
    if let Some((gutter, rest)) = line.split_once('|')
        && gutter.trim().is_empty()
        && !rest.trim().is_empty()
        && rest.trim().chars().all(|c| c == '^')
    {
        return format!("{gutter}{BOLD}{BLUE}|{RESET}{BOLD}{RED}{rest}{RESET}");
    }
    // Gutter lines: `  15 | code` and the bare `     |` spacer.
    if let Some((gutter, code)) = line.split_once('|')
        && gutter.trim().chars().all(|c| c.is_ascii_digit())
    {
        return format!("{BOLD}{BLUE}{gutter}|{RESET}{}", paint_lua(code));
    }
    // Tracebacks and their tab-indented frames.
    if trimmed.starts_with("stack traceback:") || line.starts_with('\t') {
        return format!("{DIM}{line}{RESET}");
    }
    if trimmed == "Caused by:" {
        return format!("{indent}{BOLD}Caused by:{RESET}");
    }
    line.to_string()
}

const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// A single-line Lua highlight: comments dimmed, strings green, keywords
/// magenta. Line-local by design — a snippet is a few lines around one
/// error, so multi-line string/comment state is not worth carrying.
fn paint_lua(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + 16);
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &code[i..];
        // A comment runs to the end of the line.
        if rest.starts_with("--") {
            out.push_str(DIM);
            out.push_str(GREEN);
            out.push_str(rest);
            out.push_str(RESET);
            break;
        }
        // A quoted string (line-local; an unterminated one just stays green
        // to the end of the line).
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i] as char;
            let mut end = i + 1;
            while end < bytes.len() {
                if bytes[end] == b'\\' {
                    end += 2;
                    continue;
                }
                if bytes[end] as char == quote {
                    end += 1;
                    break;
                }
                end += 1;
            }
            let end = end.min(bytes.len());
            out.push_str(GREEN);
            out.push_str(&code[i..end]);
            out.push_str(RESET);
            i = end;
            continue;
        }
        // A word: keyword or identifier.
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let mut end = i + 1;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let word = &code[i..end];
            if LUA_KEYWORDS.contains(&word) {
                out.push_str(MAGENTA);
                out.push_str(word);
                out.push_str(RESET);
            } else {
                out.push_str(word);
            }
            i = end;
            continue;
        }
        out.push(code[i..].chars().next().expect("char"));
        i += code[i..].chars().next().expect("char").len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_arrow_gutter_and_caret_are_painted() {
        assert!(paint_line("Error: script error").contains(RED));
        assert!(paint_line("  --> app.lua:15").contains(CYAN));
        assert!(paint_line("  15 |     local x = 1").starts_with(BOLD));
        assert!(paint_line("     |            ^^^^^").contains(RED));
        assert!(paint_line("\tapp.lua:14: in function").starts_with(DIM));
    }

    #[test]
    fn lua_highlight_covers_keywords_strings_and_comments() {
        let painted = paint_lua("local s = \"text\" -- note");
        assert!(painted.contains(&format!("{MAGENTA}local{RESET}")));
        assert!(painted.contains(&format!("{GREEN}\"text\"{RESET}")));
        assert!(painted.contains(&format!("{DIM}{GREEN}-- note{RESET}")));
        // Identifiers that merely contain a keyword stay unpainted.
        let painted = paint_lua("ending = localize()");
        assert!(!painted.contains(MAGENTA), "got: {painted}");
    }
}
