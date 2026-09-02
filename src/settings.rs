//! The handful of things the window remembers between runs.
//!
//! Deliberately a plain text file rather than the registry or a serialised
//! blob: it lives somewhere a person can open, it can be read at a glance,
//! and a corrupt or hand-edited one degrades to defaults rather than to a
//! parse error. Nothing here is important enough to be worth failing over —
//! every path returns a default rather than an error, because a relay URL
//! that could not be read back is an inconvenience, not a fault.

use std::path::{Path, PathBuf};

/// What gets written, and read back next time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The relay the window was last pointed at. Empty means serve locally.
    pub relay: String,
}

impl Settings {
    /// Reads what was saved, falling back to the environment and then to
    /// nothing at all.
    ///
    /// The remembered value wins over `SIDEBAND_RELAY`, because it is what the
    /// person most recently typed into the box — a field that quietly ignores
    /// what you put in it is worse than one that never remembered anything.
    /// The variable still seeds a machine that has never been told a relay.
    pub fn load() -> Self {
        let mut settings = Self::default();

        if let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) {
            for (key, value) in parse(&text) {
                if key == "relay" {
                    settings.relay = value;
                }
            }
        }

        if settings.relay.is_empty() {
            settings.relay = std::env::var("SIDEBAND_RELAY").unwrap_or_default();
        }

        // Trimmed here rather than at every use, so the value held in memory
        // is byte-for-byte the one that would be written back and the window
        // can tell "unchanged" from "edited" by comparing them.
        settings.relay = settings.relay.trim().to_owned();
        settings
    }

    /// Best effort. A read-only profile directory is not a reason to interrupt
    /// someone who is trying to share their screen.
    pub fn save(&self) {
        let Some(file) = path() else { return };
        self.save_to(&file);
    }

    fn save_to(&self, file: &Path) {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, self.serialise());
    }

    fn serialise(&self) -> String {
        format!(
            "# Sideband. Written by the app; safe to edit or delete.\nrelay = {}\n",
            self.relay.trim()
        )
    }
}

/// `%APPDATA%\Sideband\settings`, or nowhere if the profile has no such thing.
fn path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    if base.is_empty() {
        return None;
    }
    Some(PathBuf::from(base).join("Sideband").join("settings"))
}

/// `key = value` a line at a time. Blank lines and `#` comments are skipped,
/// and so is anything that does not look like a setting — a file someone has
/// typed into by hand should lose the line they got wrong, not the rest.
fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_relay_reads_back_the_same() {
        let written = Settings { relay: "https://relay.example/".into() }.serialise();
        assert_eq!(parse(&written), vec![("relay".into(), "https://relay.example/".into())]);
    }

    #[test]
    fn an_empty_relay_round_trips_as_empty() {
        // Clearing the box is a choice — serve locally — and has to survive a
        // restart just as a URL does.
        let written = Settings::default().serialise();
        assert_eq!(parse(&written), vec![("relay".into(), String::new())]);
    }

    #[test]
    fn urls_keep_the_characters_that_matter() {
        // A relay URL contains `=` in a query string often enough to be worth
        // checking that only the first one splits the line.
        let written = Settings { relay: "https://r.example/x?a=1&b=2".into() }.serialise();
        assert_eq!(parse(&written)[0].1, "https://r.example/x?a=1&b=2");
    }

    #[test]
    fn comments_blank_lines_and_nonsense_are_ignored() {
        let text = "\n# a note\n\n  relay = https://x/  \nnot a setting\n";
        assert_eq!(parse(text), vec![("relay".into(), "https://x/".into())]);
    }

    #[test]
    fn keys_are_matched_regardless_of_case() {
        assert_eq!(parse("Relay = https://x/")[0].0, "relay");
    }

    #[test]
    fn saving_makes_the_directory_it_needs_and_reads_back() {
        // The write path itself, including the profile directory not existing
        // yet — which is every first run.
        let dir = std::env::temp_dir().join(format!("sideband-{}-settings", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("Sideband").join("settings");

        Settings { relay: "https://r.example/".into() }.save_to(&file);

        let text = std::fs::read_to_string(&file).expect("the file should exist");
        assert_eq!(parse(&text), vec![("relay".into(), "https://r.example/".into())]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_lives_beside_the_users_other_settings() {
        // Skipped rather than failed where there is no profile at all, which
        // is how this behaves in a stripped-down container.
        if let Some(p) = path() {
            assert!(p.ends_with("Sideband\\settings") || p.ends_with("Sideband/settings"), "{p:?}");
        }
    }
}
