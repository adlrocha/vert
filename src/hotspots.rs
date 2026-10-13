//! Where change concentrates: files or modules ranked by how often they change.

use std::collections::HashMap;

use crate::git::feed::Entry;
use crate::labels::module_of;

/// Time window considered for hotspots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// Last 24 hours.
    Day,
    /// Last 7 days.
    Week,
    /// Last 30 days.
    Month,
    /// Everything loaded into the feed.
    All,
}

impl Window {
    /// Window length in seconds, or `None` for unbounded.
    #[must_use]
    pub const fn seconds(self) -> Option<i64> {
        const DAY: i64 = 24 * 60 * 60;
        match self {
            Self::Day => Some(DAY),
            Self::Week => Some(7 * DAY),
            Self::Month => Some(30 * DAY),
            Self::All => None,
        }
    }

    /// Short label for the UI.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::All => "all",
        }
    }

    /// The next window when cycling.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Day => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::All,
            Self::All => Self::Day,
        }
    }
}

/// Whether hotspots aggregate by file or by module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    /// Individual files.
    Files,
    /// Modules at the configured depth.
    Modules,
}

impl Grouping {
    /// Short label for the UI.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Modules => "modules",
        }
    }

    /// The other grouping.
    #[must_use]
    pub const fn toggle(self) -> Self {
        match self {
            Self::Files => Self::Modules,
            Self::Modules => Self::Files,
        }
    }
}

/// A frequently changed file or module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotspot {
    /// File path or module name.
    pub key: String,
    /// Number of changes (feed entries) touching it.
    pub changes: usize,
    /// Lines added across those changes.
    pub added: u64,
    /// Lines removed across those changes.
    pub removed: u64,
    /// Most recent change, in Unix seconds.
    pub last: i64,
}

impl Hotspot {
    /// Total lines changed.
    #[must_use]
    pub const fn churn(&self) -> u64 {
        self.added + self.removed
    }
}

/// Ranks hotspots among `entries` within `window` of `now`.
///
/// Ordered by number of changes, then churn, then most recent, then name.
#[must_use]
pub fn compute(
    entries: &[Entry],
    now: i64,
    window: Window,
    grouping: Grouping,
    depth: usize,
) -> Vec<Hotspot> {
    let since = window.seconds().map_or(i64::MIN, |s| now.saturating_sub(s));
    let mut map: HashMap<String, Hotspot> = HashMap::new();
    for entry in entries.iter().filter(|e| e.time >= since) {
        let mut per_entry: HashMap<String, (u64, u64)> = HashMap::new();
        for file in &entry.files {
            let key = match grouping {
                Grouping::Files => file.path.clone(),
                Grouping::Modules => module_of(&file.path, depth),
            };
            let slot = per_entry.entry(key).or_default();
            slot.0 += file.added;
            slot.1 += file.removed;
        }
        for (key, (added, removed)) in per_entry {
            let spot = map.entry(key.clone()).or_insert_with(|| Hotspot {
                key,
                changes: 0,
                added: 0,
                removed: 0,
                last: i64::MIN,
            });
            spot.changes += 1;
            spot.added += added;
            spot.removed += removed;
            spot.last = spot.last.max(entry.time);
        }
    }
    let mut spots: Vec<Hotspot> = map.into_values().collect();
    spots.sort_by(|a, b| {
        b.changes
            .cmp(&a.changes)
            .then(b.churn().cmp(&a.churn()))
            .then(b.last.cmp(&a.last))
            .then(a.key.cmp(&b.key))
    });
    spots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{FileStat, FileStatus, Target};
    use crate::labels::Size;

    fn file(path: &str, added: u64, removed: u64) -> FileStat {
        FileStat {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            added,
            removed,
            binary: false,
        }
    }

    fn entry(time: i64, files: Vec<FileStat>) -> Entry {
        Entry {
            target: Target::WorkingTree,
            id: String::new(),
            short: String::new(),
            summary: String::new(),
            body: String::new(),
            author: String::new(),
            time,
            files,
            added: 0,
            removed: 0,
            modules: Vec::new(),
            size: Size::Small,
            wide: false,
            reverted: false,
            is_revert: false,
            root: None,
        }
    }

    #[test]
    fn ranks_by_changes_then_churn() {
        let entries = vec![
            entry(100, vec![file("src/a.rs", 1, 0), file("src/b.rs", 50, 0)]),
            entry(90, vec![file("src/a.rs", 2, 1)]),
            entry(80, vec![file("docs/x.md", 1, 1)]),
        ];
        let spots = compute(&entries, 100, Window::All, Grouping::Files, 1);
        let keys: Vec<_> = spots.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["src/a.rs", "src/b.rs", "docs/x.md"]);
        assert_eq!(spots[0].changes, 2);
        assert_eq!(spots[0].churn(), 4);
        assert_eq!(spots[0].last, 100);
    }

    #[test]
    fn modules_count_once_per_entry() {
        let entries = vec![entry(
            100,
            vec![file("src/a.rs", 1, 0), file("src/b.rs", 1, 0)],
        )];
        let spots = compute(&entries, 100, Window::All, Grouping::Modules, 1);
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].key, "src");
        assert_eq!(spots[0].changes, 1);
        assert_eq!(spots[0].added, 2);
    }

    #[test]
    fn window_excludes_old_entries() {
        let day = 24 * 60 * 60;
        let now = 100 * day;
        let entries = vec![
            entry(now - 10, vec![file("new.rs", 1, 0)]),
            entry(now - 2 * day, vec![file("old.rs", 1, 0)]),
        ];
        let day_spots = compute(&entries, now, Window::Day, Grouping::Files, 1);
        assert_eq!(day_spots.len(), 1);
        assert_eq!(
            compute(&entries, now, Window::Week, Grouping::Files, 1).len(),
            2
        );
    }

    #[test]
    fn ties_break_by_recency_then_name() {
        let entries = vec![
            entry(50, vec![file("b", 1, 0)]),
            entry(50, vec![file("a", 1, 0)]),
            entry(90, vec![file("c", 1, 0)]),
        ];
        let spots = compute(&entries, 100, Window::All, Grouping::Files, 1);
        let keys: Vec<_> = spots.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["c", "a", "b"]);
    }

    #[test]
    fn window_and_grouping_cycle() {
        let mut w = Window::Day;
        let labels: Vec<_> = (0..4)
            .map(|_| {
                let l = w.label();
                w = w.next();
                l
            })
            .collect();
        assert_eq!(labels, ["24h", "7d", "30d", "all"]);
        assert_eq!(w, Window::Day);
        assert_eq!(Window::All.seconds(), None);
        assert_eq!(Window::Month.seconds(), Some(30 * 24 * 3600));
        assert_eq!(Grouping::Files.toggle(), Grouping::Modules);
        assert_eq!(Grouping::Modules.toggle().label(), "files");
        assert_eq!(Grouping::Modules.label(), "modules");
    }
}
