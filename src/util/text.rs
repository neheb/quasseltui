//! Sanitizing untrusted text for the terminal.
//!
//! Every string from the core (nicks, topics, message bodies, error
//! strings) is attacker-controllable, and IRC has a long history of
//! smuggling terminal escapes. Control characters are replaced with a
//! visible `\xNN` form rather than dropped, so an operator can see what a
//! misbehaving peer sent; the escaped form is printable, which makes the
//! sanitizer idempotent.

/// C0 controls, DEL, and C1 controls (the 8-bit CSI range).
fn is_unsafe(c: char) -> bool {
    matches!(c as u32, 0x00..=0x1f | 0x7f..=0x9f)
}

/// Escape control characters as `\xNN`.
pub fn sanitize_terminal(text: &str) -> String {
    if !text.chars().any(is_unsafe) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if is_unsafe(c) {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Remove mIRC formatting codes: bold `\x02`, color `\x03` (with optional
/// `fg[,bg]` digit arguments), reset `\x0f`, monospace `\x11`, reverse
/// `\x16`, italic `\x1d`, strikethrough `\x1e`, underline `\x1f`.
///
/// Run this before [`sanitize_terminal`] on message bodies: the codes are
/// routine in real traffic and would otherwise show up as `\x02` junk.
/// Anything else (ESC included) is left for the sanitizer to escape.
pub fn strip_mirc_formatting(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let digits_at = |start: usize| -> usize {
        let mut n = 0;
        while n < 2 && chars.get(start + n).is_some_and(char::is_ascii_digit) {
            n += 1;
        }
        n
    };
    while i < chars.len() {
        match chars[i] {
            '\x03' => {
                i += 1;
                let fg = digits_at(i);
                if fg > 0 {
                    i += fg;
                    if chars.get(i) == Some(&',') {
                        let bg = digits_at(i + 1);
                        if bg > 0 {
                            i += 1 + bg;
                        }
                    }
                }
            }
            '\x02' | '\x0f' | '\x11' | '\x16' | '\x1d' | '\x1e' | '\x1f' => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Sanitize and cap length, for untrusted text in notices and banners.
/// Sanitizing can quadruple the length, so a runaway reason is truncated
/// with an explicit marker.
pub fn sanitize_and_truncate(text: &str, max_chars: usize) -> String {
    let cleaned = sanitize_terminal(text);
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let mut out: String = cleaned.chars().take(max_chars).collect();
    out.push_str("...[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_text_passes_through() {
        for text in [
            "hello world",
            "résumé",
            "#python-dev",
            "",
            "[bold red]spoof[/]",
        ] {
            assert_eq!(sanitize_terminal(text), text);
        }
    }

    #[test]
    fn controls_are_escaped() {
        let cleaned = sanitize_terminal("\x1b[31mRED");
        assert!(!cleaned.contains('\x1b'));
        assert!(cleaned.contains("\\x1b"));
        let cleaned = sanitize_terminal("a\x07b\x08c\x0dd\x0ae\x09f");
        for raw in ['\x07', '\x08', '\x0d', '\x0a', '\x09'] {
            assert!(!cleaned.contains(raw));
        }
        assert_eq!(sanitize_terminal("a\x00b\x7fc"), "a\\x00b\\x7fc");
        let cleaned = sanitize_terminal("a\u{9b}b\u{9c}c");
        assert_eq!(cleaned, "a\\x9bb\\x9cc");
    }

    #[test]
    fn sanitizer_is_idempotent() {
        let once = sanitize_terminal("\x1b[0m");
        assert_eq!(sanitize_terminal(&once), once);
    }

    #[test]
    fn mirc_codes_are_stripped() {
        assert_eq!(
            strip_mirc_formatting(
                "\x02b\x02 \x1di\x1d \x1fu\x1f \x1es\x1e \x11m\x11 \x16r\x16 \x0fdone"
            ),
            "b i u s m r done"
        );
        assert_eq!(strip_mirc_formatting("\x034,7warning\x03 ok"), "warning ok");
        assert_eq!(strip_mirc_formatting("\x0312deep blue"), "deep blue");
        assert_eq!(strip_mirc_formatting("\x033green\x03"), "green");
        assert_eq!(strip_mirc_formatting("a\x03b"), "ab");
        assert_eq!(strip_mirc_formatting("\x03,5x"), ",5x");
        assert_eq!(strip_mirc_formatting("\x03123"), "3");
        assert_eq!(strip_mirc_formatting("hello #channel"), "hello #channel");
        assert_eq!(strip_mirc_formatting("\x1b[31mred"), "\x1b[31mred");
    }

    #[test]
    fn truncation() {
        assert_eq!(sanitize_and_truncate("short", 10), "short");
        assert_eq!(sanitize_and_truncate("abcdef", 3), "abc...[truncated]");
        assert_eq!(sanitize_and_truncate("\x1b", 400), "\\x1b");
    }
}
