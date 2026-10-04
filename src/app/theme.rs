//! Colors.
//!
//! The UI draws on the terminal's own default foreground and background and
//! uses the named ANSI colors for meaning (yellow for highlights, red for
//! errors). A themed terminal therefore themes the client too, including
//! live theme switches.
//!
//! The ANSI palette has no slot for an accent color, though. On Omarchy the
//! theme's `accent`, `selection` and `muted` colors are read from
//! `~/.local/state/omarchy/current/theme/colors.toml` and used for focus
//! borders, the active buffer, and the cursor row. A theme switch replaces
//! that file (and the `current/theme` symlink), so [`ThemeWatcher`]
//! re-checks the resolved file and reloads when it changes. Without Omarchy,
//! or on a terminal without truecolor, the accent falls back to ANSI blue.
//! `NO_COLOR` turns colors off and leaves only bold/dim/reverse.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub muted: Color,
    /// Background of the cursor row; `None` means reverse video.
    pub selection: Option<Color>,
    pub highlight: Color,
    pub warning: Color,
    pub error: Color,
    pub no_color: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Blue,
            muted: Color::DarkGray,
            selection: None,
            highlight: Color::Yellow,
            warning: Color::Yellow,
            error: Color::Red,
            no_color: false,
        }
    }
}

impl Theme {
    pub fn monochrome() -> Self {
        Self {
            accent: Color::Reset,
            muted: Color::Reset,
            selection: None,
            highlight: Color::Reset,
            warning: Color::Reset,
            error: Color::Reset,
            no_color: true,
        }
    }

    /// The default theme with Omarchy's colors applied where present.
    pub fn from_colors_toml(text: &str, truecolor: bool) -> Self {
        let mut theme = Self::default();
        if !truecolor {
            return theme;
        }
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            // A quoted value ends at its closing quote; anything after it
            // (a comment) is ignored.
            let value = match value.chars().next() {
                Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or_default(),
                _ => value.split_whitespace().next().unwrap_or_default(),
            };
            let Some(color) = parse_hex(value) else {
                continue;
            };
            match key {
                "accent" => theme.accent = color,
                "muted" => theme.muted = color,
                "selection" | "selection_background" => theme.selection = Some(color),
                _ => {}
            }
        }
        theme
    }

    pub fn fg(&self, color: Color) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(color)
        }
    }

    pub fn accent_style(&self) -> Style {
        self.fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn muted_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.muted)
        }
    }

    pub fn border_style(&self, focused: bool) -> Style {
        if focused {
            self.fg(self.accent)
        } else {
            self.muted_style()
        }
    }

    /// The style of a selected (cursor) row.
    pub fn selection_style(&self) -> Style {
        match self.selection {
            Some(bg) if !self.no_color => Style::default().bg(bg),
            _ => Style::default().add_modifier(Modifier::REVERSED),
        }
    }
}

/// `#rrggbb` to an RGB color.
pub fn parse_hex(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Color::Rgb(channel(0)?, channel(2)?, channel(4)?))
}

/// Whether the terminal advertises 24-bit color.
pub fn terminal_has_truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| matches!(v.to_lowercase().as_str(), "truecolor" | "24bit"))
        .unwrap_or(false)
}

fn no_color_requested() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// Omarchy's current theme colors.
pub fn omarchy_colors_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/omarchy/current/theme/colors.toml"))
}

/// Reloads the theme when the Omarchy theme changes.
pub struct ThemeWatcher {
    path: Option<PathBuf>,
    truecolor: bool,
    no_color: bool,
    /// What the path resolved to last time, and its mtime.
    signature: Option<(PathBuf, Option<SystemTime>, u64)>,
}

impl ThemeWatcher {
    pub fn new() -> Self {
        Self::with_path(
            omarchy_colors_path(),
            terminal_has_truecolor(),
            no_color_requested(),
        )
    }

    pub fn with_path(path: Option<PathBuf>, truecolor: bool, no_color: bool) -> Self {
        Self {
            path,
            truecolor,
            no_color,
            signature: None,
        }
    }

    fn signature_of(path: &Path) -> Option<(PathBuf, Option<SystemTime>, u64)> {
        // Resolve symlinks: a theme switch repoints `current/theme`.
        let resolved = std::fs::canonicalize(path).ok()?;
        let meta = std::fs::metadata(&resolved).ok()?;
        Some((resolved, meta.modified().ok(), meta.len()))
    }

    /// The current theme.
    pub fn load(&mut self) -> Theme {
        if self.no_color {
            return Theme::monochrome();
        }
        let Some(path) = &self.path else {
            return Theme::default();
        };
        self.signature = Self::signature_of(path);
        match std::fs::read_to_string(path) {
            Ok(text) => Theme::from_colors_toml(&text, self.truecolor),
            Err(_) => Theme::default(),
        }
    }

    /// A new theme if the file changed since the last load.
    pub fn poll(&mut self) -> Option<Theme> {
        if self.no_color {
            return None;
        }
        let path = self.path.as_ref()?;
        let current = Self::signature_of(path);
        if current == self.signature {
            return None;
        }
        Some(self.load())
    }
}

impl Default for ThemeWatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OMARCHY: &str = "mode = \"dark\"\n# a comment\naccent = \"#d9a56c\"  # trailing\nselection = \"#230e18\"\nmuted = \"#7a5c66\"\nbackground = \"#0d0509\"\n";

    #[test]
    fn parses_omarchy_colors() {
        let theme = Theme::from_colors_toml(OMARCHY, true);
        assert_eq!(theme.accent, Color::Rgb(0xd9, 0xa5, 0x6c));
        assert_eq!(theme.selection, Some(Color::Rgb(0x23, 0x0e, 0x18)));
        assert_eq!(theme.muted, Color::Rgb(0x7a, 0x5c, 0x66));
        // Semantic colors stay on the terminal palette.
        assert_eq!(theme.highlight, Color::Yellow);
    }

    #[test]
    fn without_truecolor_falls_back_to_ansi() {
        assert_eq!(Theme::from_colors_toml(OMARCHY, false), Theme::default());
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#FFD60A"), Some(Color::Rgb(0xff, 0xd6, 0x0a)));
        assert_eq!(parse_hex("FFD60A"), None);
        assert_eq!(parse_hex("#FFD6"), None);
        assert_eq!(parse_hex("#GGGGGG"), None);
    }

    #[test]
    fn watcher_reloads_when_the_theme_switches() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.toml");
        let b = dir.path().join("b.toml");
        std::fs::write(&a, "accent = \"#111111\"\n").unwrap();
        std::fs::write(&b, "accent = \"#222222\"\n").unwrap();
        let link = dir.path().join("colors.toml");
        std::os::unix::fs::symlink(&a, &link).unwrap();

        let mut watcher = ThemeWatcher::with_path(Some(link.clone()), true, false);
        assert_eq!(watcher.load().accent, Color::Rgb(0x11, 0x11, 0x11));
        assert!(watcher.poll().is_none());

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert_eq!(watcher.poll().unwrap().accent, Color::Rgb(0x22, 0x22, 0x22));
        assert!(watcher.poll().is_none());
    }

    #[test]
    fn missing_file_and_no_color() {
        let mut watcher =
            ThemeWatcher::with_path(Some("/nonexistent/colors.toml".into()), true, false);
        assert_eq!(watcher.load(), Theme::default());
        assert!(watcher.poll().is_none());
        let mut mono = ThemeWatcher::with_path(None, true, true);
        assert!(mono.load().no_color);
    }
}
