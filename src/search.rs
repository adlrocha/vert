//! File-name fuzzy search and content search (ripgrep's engine).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::{WalkBuilder, WalkState};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as MatchConfig, Matcher as Fuzzy, Utf32Str};
use thiserror::Error;

/// Search failures.
#[derive(Debug, Error)]
pub enum SearchError {
    /// The query could not be compiled even as a literal.
    #[error("invalid search pattern: {0}")]
    Pattern(#[from] grep_regex::Error),
}

fn walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|e| e.file_name() != ".git");
    builder
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Lists all non-ignored files under `root` as sorted, `/`-separated relative paths.
#[must_use]
pub fn list_files(root: &Path) -> Vec<String> {
    let files = Mutex::new(Vec::new());
    walker(root).build_parallel().run(|| {
        Box::new(|result| {
            if let Ok(entry) = result
                && entry.file_type().is_some_and(|t| t.is_file())
                && let Ok(mut files) = files.lock()
            {
                files.push(relative(root, entry.path()));
            }
            WalkState::Continue
        })
    });
    let mut files = files
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    files.sort_unstable();
    files
}

/// A fuzzy file-name match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatch {
    /// Relative path.
    pub path: String,
    /// Character indices of matched characters, ascending.
    pub indices: Vec<usize>,
}

/// Fuzzy-matches `query` against `files`, best first, at most `limit` results.
///
/// An empty query returns the first `limit` files unchanged.
#[must_use]
pub fn fuzzy_files(files: &[String], query: &str, limit: usize) -> Vec<FileMatch> {
    if query.trim().is_empty() {
        return files
            .iter()
            .take(limit)
            .map(|p| FileMatch {
                path: p.clone(),
                indices: Vec::new(),
            })
            .collect();
    }
    let mut matcher = Fuzzy::new(MatchConfig::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &String)> = files
        .iter()
        .filter_map(|path| {
            let haystack = Utf32Str::new(path, &mut buf);
            pattern.score(haystack, &mut matcher).map(|s| (s, path))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(b.1))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, path)| {
            let mut indices = Vec::new();
            let haystack = Utf32Str::new(path, &mut buf);
            pattern.indices(haystack, &mut matcher, &mut indices);
            indices.sort_unstable();
            indices.dedup();
            FileMatch {
                path: path.clone(),
                indices: indices
                    .into_iter()
                    .filter_map(|i| usize::try_from(i).ok())
                    .collect(),
            }
        })
        .collect()
}

/// One line matching a content search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    /// Relative path.
    pub path: String,
    /// 1-based line number.
    pub line: u64,
    /// Line text without the trailing newline.
    pub text: String,
    /// Byte ranges of matches within `text`.
    pub ranges: Vec<(usize, usize)>,
}

/// Outcome of a content search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextResults {
    /// Matches sorted by path and line.
    pub matches: Vec<TextMatch>,
    /// The query was not a valid regex and was searched literally.
    pub literal: bool,
    /// The result cap was hit.
    pub truncated: bool,
    /// The search was cancelled before finishing.
    pub cancelled: bool,
}

fn build_matcher(query: &str) -> Result<(RegexMatcher, bool), SearchError> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_smart(true);
    match builder.build(query) {
        Ok(m) => Ok((m, false)),
        Err(_) => Ok((builder.fixed_strings(true).build(query)?, true)),
    }
}

/// Searches file contents under `root` for `query` (smart-case regex, falling
/// back to a literal), collecting at most `limit` matches.
///
/// `cancelled` is polled between files so a newer query can abort this one.
///
/// # Errors
/// Returns [`SearchError::Pattern`] if the query cannot be compiled at all.
pub fn search_text(
    root: &Path,
    query: &str,
    limit: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<TextResults, SearchError> {
    if query.is_empty() {
        return Ok(TextResults::default());
    }
    let (matcher, literal) = build_matcher(query)?;
    let found = Mutex::new(Vec::new());
    let count = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    walker(root).build_parallel().run(|| {
        let matcher = matcher.clone();
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        let (found, count, stopped) = (&found, &count, &stopped);
        Box::new(move |result| {
            if cancelled() || count.load(Ordering::Relaxed) >= limit {
                stopped.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = relative(root, entry.path());
            let mut local = Vec::new();
            // Unreadable files are skipped rather than failing the whole search.
            let _ = searcher.search_path(
                &matcher,
                entry.path(),
                Lossy(|line, text| {
                    let text = text.trim_end_matches(['\n', '\r']);
                    let mut ranges = Vec::new();
                    let _ = matcher.find_iter(text.as_bytes(), |m| {
                        ranges.push((m.start(), m.end()));
                        true
                    });
                    local.push(TextMatch {
                        path: path.clone(),
                        line,
                        text: text.to_owned(),
                        ranges,
                    });
                    Ok(count.fetch_add(1, Ordering::Relaxed) + 1 < limit)
                }),
            );
            if let Ok(mut found) = found.lock() {
                found.extend(local);
            }
            WalkState::Continue
        })
    });
    let mut collected = found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    collected.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    let truncated = collected.len() >= limit;
    collected.truncate(limit);
    Ok(TextResults {
        matches: collected,
        literal,
        truncated,
        cancelled: cancelled() || (stopped.load(Ordering::Relaxed) && !truncated),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |rel: &str, content: &str| {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(path, content).expect("write");
        };
        // A .git dir makes .gitignore apply.
        fs::create_dir_all(dir.path().join(".git")).expect("git dir");
        write(".gitignore", "target/\n");
        write("src/main.rs", "fn main() {\n    println!(\"Hello\");\n}\n");
        write("src/lib.rs", "pub fn hello() {}\n// hello again\n");
        write("target/out.rs", "fn hello() {}\n");
        write("docs/guide.md", "Say hello.\n");
        fs::write(
            dir.path().join("bin.dat"),
            [0_u8, b'h', b'e', b'l', b'l', b'o'],
        )
        .expect("bin");
        dir
    }

    const NEVER: &(dyn Fn() -> bool + Sync) = &|| false;

    #[test]
    fn lists_files_respecting_gitignore() {
        let dir = tree();
        let files = list_files(dir.path());
        assert_eq!(
            files,
            [
                ".gitignore",
                "bin.dat",
                "docs/guide.md",
                "src/lib.rs",
                "src/main.rs"
            ]
        );
    }

    #[test]
    fn fuzzy_ranks_and_reports_indices() {
        let files: Vec<String> = ["src/main.rs", "src/lib.rs", "docs/guide.md"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let hits = fuzzy_files(&files, "main", 10);
        assert_eq!(hits[0].path, "src/main.rs");
        assert_eq!(hits[0].indices, [4, 5, 6, 7]);
        assert!(fuzzy_files(&files, "zzz", 10).is_empty());
        assert_eq!(fuzzy_files(&files, "", 2).len(), 2);
        assert_eq!(fuzzy_files(&files, "s", 1).len(), 1);
    }

    #[test]
    fn content_search_smart_case_and_ranges() {
        let dir = tree();
        let res = search_text(dir.path(), "hello", 100, NEVER).expect("search");
        assert!(!res.literal && !res.truncated && !res.cancelled);
        let hits: Vec<_> = res
            .matches
            .iter()
            .map(|m| (m.path.as_str(), m.line))
            .collect();
        assert_eq!(
            hits,
            [
                ("docs/guide.md", 1),
                ("src/lib.rs", 1),
                ("src/lib.rs", 2),
                ("src/main.rs", 2)
            ]
        );
        assert_eq!(res.matches[1].ranges, [(7, 12)]);

        let upper = search_text(dir.path(), "Hello", 100, NEVER).expect("search");
        assert_eq!(upper.matches.len(), 1, "uppercase makes it case-sensitive");
    }

    #[test]
    fn invalid_regex_falls_back_to_literal() {
        let dir = tree();
        let res = search_text(dir.path(), "main(", 100, NEVER).expect("search");
        assert!(res.literal);
        assert_eq!(res.matches.len(), 1);
        assert_eq!(res.matches[0].ranges, [(3, 8)]);
    }

    #[test]
    fn empty_query_limit_and_cancel() {
        let dir = tree();
        assert_eq!(
            search_text(dir.path(), "", 100, NEVER).expect("s"),
            TextResults::default()
        );
        let capped = search_text(dir.path(), "hello", 2, NEVER).expect("s");
        assert_eq!(capped.matches.len(), 2);
        assert!(capped.truncated);
        let cancelled = search_text(dir.path(), "hello", 100, &|| true).expect("s");
        assert!(cancelled.cancelled);
        assert!(cancelled.matches.is_empty());
    }
}
