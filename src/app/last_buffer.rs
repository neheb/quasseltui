//! The buffer to start in: the one that was active when quasseltui last
//! exited for the same account, kept under `$XDG_STATE_HOME/quasseltui`.
//!
//! Buffer ids are stable on a core, so the id is enough (and survives
//! renames). The file holds one `key<TAB>id` line per account, newest
//! first. It's a convenience: any failure reading or writing it is logged
//! and otherwise ignored.

use std::path::{Path, PathBuf};

use crate::protocol::types::BufferId;

/// Accounts remembered; older ones drop off.
const MAX_ENTRIES: usize = 50;

/// The XDG-aware default path.
pub fn default_path() -> PathBuf {
    path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn path_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    let base = match xdg {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => home
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
            .join(".local/state"),
    };
    base.join("quasseltui").join("last-buffer")
}

/// The account key: who logged in where.
pub fn key(user: &str, host: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

pub fn load(path: &Path, key: &str) -> Option<BufferId> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(k, _)| *k == key)
        .and_then(|(_, id)| id.trim().parse().ok())
        .map(BufferId)
}

pub fn save(path: &Path, key: &str, buffer: BufferId) {
    if let Err(e) = try_save(path, key, buffer) {
        tracing::warn!(
            "could not remember the last buffer in {}: {e}",
            path.display()
        );
    }
}

fn try_save(path: &Path, key: &str, buffer: BufferId) -> std::io::Result<()> {
    // A key that could break the line format isn't worth remembering.
    if key.contains(['\t', '\n', '\r']) {
        return Ok(());
    }
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = format!("{key}\t{}\n", buffer.0);
    for line in old
        .lines()
        .filter(|line| line.split_once('\t').is_some_and(|(k, _)| k != key))
        .take(MAX_ENTRIES - 1)
    {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write then rename, so a crash can't leave a torn file.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_per_account_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/quasseltui/last-buffer");
        let (home, work) = (key("me", "home", 4242), key("me", "work", 4242));
        assert_eq!(load(&path, &home), None);
        save(&path, &home, BufferId(7));
        save(&path, &work, BufferId(3));
        save(&path, &home, BufferId(9));
        assert_eq!(load(&path, &home), Some(BufferId(9)));
        assert_eq!(load(&path, &work), Some(BufferId(3)));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "me@home:4242\t9\nme@work:4242\t3\n");
    }

    #[test]
    fn keeps_a_bounded_list_and_skips_bad_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-buffer");
        for i in 0..MAX_ENTRIES + 5 {
            save(&path, &format!("u{i}@h:1"), BufferId(1));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), MAX_ENTRIES);
        save(&path, "evil\tkey", BufferId(1));
        assert_eq!(load(&path, "evil"), None);
        // Garbage in the file is skipped, not fatal.
        std::fs::write(&path, "junk\nme@h:1\tnope\nme@h:2\t5\n").unwrap();
        assert_eq!(load(&path, "me@h:1"), None);
        assert_eq!(load(&path, "me@h:2"), Some(BufferId(5)));
    }

    #[test]
    fn xdg_state_home_wins() {
        assert_eq!(
            path_from(Some("/s".into()), Some("/h".into())),
            PathBuf::from("/s/quasseltui/last-buffer")
        );
        assert_eq!(
            path_from(None, Some("/h".into())),
            PathBuf::from("/h/.local/state/quasseltui/last-buffer")
        );
    }
}
