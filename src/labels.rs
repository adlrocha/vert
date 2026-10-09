//! Pure classification of changes: size buckets, breadth ("wide") and revert detection.

use thiserror::Error;

/// How big a change is, by number of lines added plus removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Size {
    /// Below the medium threshold.
    Small,
    /// At or above the medium threshold.
    Medium,
    /// At or above the large threshold.
    Large,
    /// At or above the extra-large threshold.
    Xl,
}

impl Size {
    /// Short lowercase label used in the UI.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Xl => "xl",
        }
    }
}

/// Invalid labelling configuration.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LabelError {
    /// Size thresholds must be strictly increasing and non-zero.
    #[error("size thresholds must satisfy 0 < medium < large < xl (got {medium}, {large}, {xl})")]
    UnorderedThresholds {
        /// Medium threshold.
        medium: u64,
        /// Large threshold.
        large: u64,
        /// Extra-large threshold.
        xl: u64,
    },
    /// A change must touch at least one module to be wide.
    #[error("the wide-module threshold must be at least 1")]
    ZeroWide,
    /// Module depth must be at least one directory level.
    #[error("module depth must be at least 1")]
    ZeroDepth,
}

/// Thresholds that turn raw change statistics into labels.
///
/// ```
/// use vert::labels::{Labeler, Size};
/// let labeler = Labeler::new(50, 200, 1000, 3, 1).unwrap();
/// assert_eq!(labeler.size(120), Size::Medium);
/// assert!(labeler.is_wide(3));
/// assert_eq!(labeler.module_of("src/git/feed.rs"), "src");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Labeler {
    medium: u64,
    large: u64,
    xl: u64,
    wide_modules: usize,
    module_depth: usize,
}

impl Labeler {
    /// Builds a labeler, validating that thresholds are coherent.
    ///
    /// # Errors
    /// Returns [`LabelError`] if thresholds are not strictly increasing or are zero.
    pub const fn new(
        medium: u64,
        large: u64,
        xl: u64,
        wide_modules: usize,
        module_depth: usize,
    ) -> Result<Self, LabelError> {
        if medium == 0 || medium >= large || large >= xl {
            return Err(LabelError::UnorderedThresholds { medium, large, xl });
        }
        if wide_modules == 0 {
            return Err(LabelError::ZeroWide);
        }
        if module_depth == 0 {
            return Err(LabelError::ZeroDepth);
        }
        Ok(Self {
            medium,
            large,
            xl,
            wide_modules,
            module_depth,
        })
    }

    /// Buckets a change by its total changed lines.
    #[must_use]
    pub const fn size(&self, lines: u64) -> Size {
        if lines >= self.xl {
            Size::Xl
        } else if lines >= self.large {
            Size::Large
        } else if lines >= self.medium {
            Size::Medium
        } else {
            Size::Small
        }
    }

    /// Whether touching `modules` distinct modules counts as a wide change.
    #[must_use]
    pub const fn is_wide(&self, modules: usize) -> bool {
        modules >= self.wide_modules
    }

    /// Configured module depth.
    #[must_use]
    pub const fn module_depth(&self) -> usize {
        self.module_depth
    }

    /// The module a path belongs to: its first `module_depth` directories.
    ///
    /// Files at the repository root (or shallower than the depth) belong to the
    /// deepest directory they have, and root-level files belong to `"."`.
    #[must_use]
    pub fn module_of(&self, path: &str) -> String {
        module_of(path, self.module_depth)
    }
}

/// The module of `path` at `depth` directory levels. See [`Labeler::module_of`].
#[must_use]
pub fn module_of(path: &str, depth: usize) -> String {
    let dirs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let dir_count = dirs.len().saturating_sub(1);
    if dir_count == 0 {
        return ".".to_owned();
    }
    dirs[..dir_count.min(depth)].join("/")
}

/// Extracts the target hash from a standard `git revert` message
/// (`This reverts commit <hash>.`).
///
/// ```
/// use vert::labels::reverted_target;
/// let msg = "Revert \"x\"\n\nThis reverts commit 0123abcd.";
/// assert_eq!(reverted_target(msg).as_deref(), Some("0123abcd"));
/// ```
#[must_use]
pub fn reverted_target(message: &str) -> Option<String> {
    const MARKER: &str = "This reverts commit ";
    let start = message.find(MARKER)? + MARKER.len();
    let hash: String = message[start..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    (hash.len() >= 4).then(|| hash.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labeler() -> Labeler {
        Labeler::new(50, 200, 1000, 3, 1).expect("valid thresholds")
    }

    #[test]
    fn sizes_bucket_at_thresholds() {
        let l = labeler();
        assert_eq!(l.size(0), Size::Small);
        assert_eq!(l.size(49), Size::Small);
        assert_eq!(l.size(50), Size::Medium);
        assert_eq!(l.size(199), Size::Medium);
        assert_eq!(l.size(200), Size::Large);
        assert_eq!(l.size(999), Size::Large);
        assert_eq!(l.size(1000), Size::Xl);
    }

    #[test]
    fn size_labels() {
        let labels: Vec<_> = [Size::Small, Size::Medium, Size::Large, Size::Xl]
            .iter()
            .map(|s| s.label())
            .collect();
        assert_eq!(labels, ["small", "medium", "large", "xl"]);
    }

    #[test]
    fn rejects_bad_thresholds() {
        assert!(matches!(
            Labeler::new(0, 200, 1000, 3, 1),
            Err(LabelError::UnorderedThresholds { .. })
        ));
        assert!(matches!(
            Labeler::new(200, 200, 1000, 3, 1),
            Err(LabelError::UnorderedThresholds { .. })
        ));
        assert!(matches!(
            Labeler::new(50, 1000, 1000, 3, 1),
            Err(LabelError::UnorderedThresholds { .. })
        ));
        assert_eq!(Labeler::new(50, 200, 1000, 0, 1), Err(LabelError::ZeroWide));
        assert_eq!(
            Labeler::new(50, 200, 1000, 3, 0),
            Err(LabelError::ZeroDepth)
        );
    }

    #[test]
    fn wide_threshold() {
        let l = labeler();
        assert!(!l.is_wide(2));
        assert!(l.is_wide(3));
        assert_eq!(l.module_depth(), 1);
    }

    #[test]
    fn modules_by_depth() {
        assert_eq!(module_of("README.md", 1), ".");
        assert_eq!(module_of("src/main.rs", 1), "src");
        assert_eq!(module_of("src/git/feed.rs", 1), "src");
        assert_eq!(module_of("src/git/feed.rs", 2), "src/git");
        assert_eq!(module_of("src/main.rs", 2), "src");
        assert_eq!(module_of("", 1), ".");
        assert_eq!(labeler().module_of("a/b/c"), "a");
    }

    #[test]
    fn revert_detection() {
        assert_eq!(
            reverted_target("Revert \"x\"\n\nThis reverts commit ABCDEF0123.\n").as_deref(),
            Some("abcdef0123")
        );
        assert_eq!(reverted_target("just a message"), None);
        assert_eq!(reverted_target("This reverts commit xyz"), None);
        assert_eq!(reverted_target("This reverts commit abc"), None);
    }
}
