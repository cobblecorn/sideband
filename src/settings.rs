//! The handful of things the window remembers between runs.
//!
//! Deliberately a plain text file rather than the registry or a serialised
//! blob: it lives somewhere a person can open, it can be read at a glance,
//! and a corrupt or hand-edited one degrades to defaults rather than to a
//! parse error. Nothing here is important enough to be worth failing over,
//! every path returns a default rather than an error, because a relay URL
//! that could not be read back is an inconvenience, not a fault.

use std::path::{Path, PathBuf};

/// What gets written, and read back next time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The relay the window was last pointed at. Empty means serve locally.
    pub relay: String,
    /// Let whoever holds the code straight in, with no prompt on this end.
    ///
    /// Off unless it has been turned on, and stays off if the file says
    /// anything this cannot read as a clear yes. A setting that decides who
    /// gets to see your screen is one where the safe reading of a typo is
    /// "no".
    pub auto_approve: bool,
    /// The capture device to use, by endpoint id. Empty means whatever
    /// Windows considers the default.
    ///
    /// The id rather than the name, because names are neither unique nor
    /// stable: this machine has two devices both called "SteelSeries Sonar,
    /// Microphone".
    pub mic_device: String,
}

impl Settings {
    /// Reads what was saved, falling back to the environment and then to
    /// nothing at all.
    ///
    /// The remembered value wins over `SIDEBAND_RELAY`, because it is what the
    /// person most recently typed into the box, a field that quietly ignores
    /// what you put in it is worse than one that never remembered anything.
    /// The variable still seeds a machine that has never been told a relay.
    pub fn load() -> Self {
        let mut settings = Self::default();

        if let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) {
            for (key, value) in parse(&text) {
                match key.as_str() {
                    "relay" => settings.relay = value,
                    "auto_approve" => settings.auto_approve = truthy(&value),
                    "mic_device" => settings.mic_device = value,
                    _ => {}
                }
            }
        }

        if settings.relay.is_empty() {
            settings.relay = std::env::var("SIDEBAND_RELAY").unwrap_or_default();
        }

        // An override that does not depend on a file having been written
        // correctly, because the file is exactly what was in doubt: a window
        // left open from an older build rewrites it in the older format on
        // exit, quietly dropping settings that build had never heard of.
        if std::env::var("SIDEBAND_AUTO_ADMIT").is_ok_and(|v| truthy(&v)) {
            settings.auto_approve = true;
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
            "# Sideband. Written by the app; safe to edit or delete.\nrelay = {}\nauto_approve = {}\nmic_device = {}\n",
            self.relay.trim(),
            self.auto_approve,
            self.mic_device.trim(),
        )
    }
}

/// What counts as a yes in the file.
///
/// Only these. Anything else, including an empty value or something typed by
/// hand that nearly means yes, leaves the prompt in place: the cost of reading
/// a stray word as "let anyone in" is far higher than the cost of asking.
fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
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
/// and so is anything that does not look like a setting, a file someone has
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

    /// One setting out of a serialised file.
    ///
    /// The tests below ask for the key they are about rather than comparing
    /// the whole file, because every one of them broke the day a second
    /// setting was added, and none of them was actually about how many
    /// settings there are.
    fn value(text: &str, key: &str) -> Option<String> {
        parse(text).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    #[test]
    fn a_chosen_microphone_survives_a_round_trip() {
        let written = Settings {
            mic_device: "{0.0.1.00000000}.{abc-123}".into(),
            ..Default::default()
        }
        .serialise();
        assert_eq!(
            value(&written, "mic_device").as_deref(),
            Some("{0.0.1.00000000}.{abc-123}")
        );

        // And the absence of a choice reads back as an absence, not as the
        // word "default" or anything else that could match a real id.
        let none = Settings::default().serialise();
        assert_eq!(value(&none, "mic_device").as_deref(), Some(""));
    }

    #[test]
    fn auto_approve_survives_a_round_trip() {
        let on = Settings { auto_approve: true, ..Default::default() }.serialise();
        assert_eq!(value(&on, "auto_approve").as_deref(), Some("true"));

        let off = Settings::default().serialise();
        assert_eq!(value(&off, "auto_approve").as_deref(), Some("false"));
    }

    #[test]
    fn only_an_unambiguous_yes_turns_the_prompt_off() {
        // The failure to avoid is a hand-edited file letting someone in. Every
        // value that is not clearly a yes has to read as a no, including the
        // ones that look like they were meant to be one.
        for yes in ["true", "yes", "on", "1", "TRUE", " Yes "] {
            assert!(truthy(yes), "{yes:?} should enable it");
        }
        for no in ["false", "no", "off", "0", "", "y", "sure", "true-ish", "2"] {
            assert!(!truthy(no), "{no:?} must not enable it");
        }
    }

    #[test]
    fn a_saved_relay_reads_back_the_same() {
        let written =
            Settings { relay: "https://relay.example/".into(), ..Default::default() }.serialise();
        assert_eq!(value(&written, "relay").as_deref(), Some("https://relay.example/"));
    }

    #[test]
    fn an_empty_relay_round_trips_as_empty() {
        // Clearing the box is a choice, serve locally, and has to survive a
        // restart just as a URL does.
        let written = Settings::default().serialise();
        assert_eq!(value(&written, "relay").as_deref(), Some(""));
    }

    #[test]
    fn urls_keep_the_characters_that_matter() {
        // A relay URL contains `=` in a query string often enough to be worth
        // checking that only the first one splits the line.
        let written =
            Settings { relay: "https://r.example/x?a=1&b=2".into(), ..Default::default() }
                .serialise();
        assert_eq!(value(&written, "relay").as_deref(), Some("https://r.example/x?a=1&b=2"));
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
        // yet, which is every first run.
        let dir = std::env::temp_dir().join(format!("sideband-{}-settings", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("Sideband").join("settings");

        Settings { relay: "https://r.example/".into(), ..Default::default() }.save_to(&file);

        let text = std::fs::read_to_string(&file).expect("the file should exist");
        assert_eq!(value(&text, "relay").as_deref(), Some("https://r.example/"));

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
