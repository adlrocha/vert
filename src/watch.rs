//! Live file watching: turns file-system events into feed refreshes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// What kind of refresh a file-system change calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// Branches or `HEAD` moved: reload history and the working tree.
    Refs,
    /// Files in the working tree changed: reload uncommitted changes.
    Workdir,
}

/// Locations relevant to classifying events.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Working directory root.
    pub workdir: PathBuf,
    /// The repository's `.git` directory.
    pub git_dir: PathBuf,
    /// Shared directory holding refs (differs from `git_dir` in linked worktrees).
    pub common_dir: PathBuf,
}

impl Layout {
    /// Reads the layout of `repo`.
    ///
    /// # Errors
    /// Returns [`crate::git::GitError::Bare`] for bare repositories.
    /// Paths are canonicalised because some watchers (`FSEvents` on macOS)
    /// report fully resolved paths, e.g. `/private/var/…` for `/var/…`.
    pub fn of(repo: &git2::Repository) -> Result<Self, crate::git::GitError> {
        Ok(Self {
            workdir: canonical(&crate::git::workdir(repo)?),
            git_dir: canonical(repo.path()),
            common_dir: canonical(repo.commondir()),
        })
    }

    /// Classifies a changed `path`; `is_ignored` receives workdir-relative paths.
    pub fn classify(&self, path: &Path, is_ignored: impl Fn(&Path) -> bool) -> Option<Change> {
        for dir in [&self.git_dir, &self.common_dir] {
            if let Ok(rel) = path.strip_prefix(dir) {
                return git_internal(rel);
            }
        }
        let rel = path.strip_prefix(&self.workdir).ok()?;
        if rel.as_os_str().is_empty() || rel.components().any(|c| c.as_os_str() == ".git") {
            return None;
        }
        (!is_ignored(rel)).then_some(Change::Workdir)
    }
}

/// Resolves symlinks, keeping the path as given if it cannot be resolved.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn git_internal(rel: &Path) -> Option<Change> {
    let first = rel.components().next()?.as_os_str().to_str()?;
    match first {
        "HEAD" | "refs" | "packed-refs" | "ORIG_HEAD" | "FETCH_HEAD" => Some(Change::Refs),
        "index" => Some(Change::Workdir),
        _ => None,
    }
}

/// Keeps the watcher alive; dropping it stops watching.
pub struct Watch {
    _watcher: RecommendedWatcher,
}

/// Whether an event reflects an actual change. Reads (`Access`) are dropped:
/// vert itself reads files constantly, and reacting to that would loop.
const fn is_change(kind: EventKind) -> bool {
    !matches!(kind, EventKind::Access(_))
}

/// Starts watching `layout`, calling `on_change` once per quiet period of
/// `debounce` with the set of changes seen.
///
/// # Errors
/// Returns the underlying watcher error (e.g. inotify limits reached).
pub fn start(
    layout: &Layout,
    debounce: Duration,
    on_change: impl Fn(BTreeSet<Change>) + Send + 'static,
) -> Result<Watch, notify::Error> {
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res
            && is_change(event.kind)
        {
            // The receiver only goes away when watching stops.
            let _ = tx.send(event.paths);
        }
    })?;
    watcher.watch(&layout.workdir, RecursiveMode::Recursive)?;
    for dir in [&layout.git_dir, &layout.common_dir] {
        if !dir.starts_with(&layout.workdir) {
            watcher.watch(dir, RecursiveMode::Recursive)?;
        }
    }
    let layout = layout.clone();
    thread::spawn(move || debounce_loop(&layout, &rx, debounce, &on_change));
    Ok(Watch { _watcher: watcher })
}

fn debounce_loop(
    layout: &Layout,
    rx: &Receiver<Vec<PathBuf>>,
    debounce: Duration,
    on_change: &impl Fn(BTreeSet<Change>),
) {
    let repo = git2::Repository::open(&layout.workdir).ok();
    let ignored = |rel: &Path| {
        repo.as_ref()
            .is_some_and(|r| r.is_path_ignored(rel).unwrap_or(false))
    };
    while let Ok(first) = rx.recv() {
        let mut paths = first;
        loop {
            match rx.recv_timeout(debounce) {
                Ok(more) => paths.extend(more),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let changes: BTreeSet<Change> = paths
            .iter()
            .filter_map(|p| layout.classify(p, ignored))
            .collect();
        if !changes.is_empty() {
            on_change(changes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout {
            workdir: PathBuf::from("/repo"),
            git_dir: PathBuf::from("/repo/.git"),
            common_dir: PathBuf::from("/repo/.git"),
        }
    }

    #[test]
    fn classifies_paths() {
        let l = layout();
        let never = |_: &Path| false;
        let cases = [
            ("/repo/.git/HEAD", Some(Change::Refs)),
            ("/repo/.git/refs/heads/main", Some(Change::Refs)),
            ("/repo/.git/packed-refs", Some(Change::Refs)),
            ("/repo/.git/index", Some(Change::Workdir)),
            ("/repo/.git/objects/ab/cdef", None),
            ("/repo/.git/index.lock", None),
            ("/repo/src/main.rs", Some(Change::Workdir)),
            ("/repo", None),
            ("/elsewhere/file", None),
            ("/repo/sub/.git/HEAD", None),
        ];
        for (path, expected) in cases {
            assert_eq!(l.classify(Path::new(path), never), expected, "{path}");
        }
        let ignore_target = |p: &Path| p.starts_with("target");
        assert_eq!(
            l.classify(Path::new("/repo/target/debug/x"), ignore_target),
            None
        );
    }

    #[test]
    fn access_events_are_not_changes() {
        use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind};
        assert!(!is_change(EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(!is_change(EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(is_change(EventKind::Modify(ModifyKind::Any)));
        assert!(is_change(EventKind::Create(CreateKind::File)));
    }

    #[cfg(unix)]
    #[test]
    fn layout_resolves_symlinked_repo_paths() {
        let t = crate::git::testutil::TestRepo::new();
        let links = tempfile::tempdir().expect("tempdir");
        let link = links.path().join("repo-link");
        std::os::unix::fs::symlink(t.path(), &link).expect("symlink");
        let repo = git2::Repository::open(&link).expect("open via symlink");
        let layout = Layout::of(&repo).expect("layout");
        let real = t.path().canonicalize().expect("canon");
        assert_eq!(layout.workdir, real);
        assert_eq!(layout.git_dir, real.join(".git"));
        // A resolved event path is recognised as a working-tree change.
        let event = real.join("src/main.rs");
        assert_eq!(layout.classify(&event, |_| false), Some(Change::Workdir));
        assert_eq!(
            canonical(Path::new("/no/such/path")),
            PathBuf::from("/no/such/path")
        );
    }

    #[test]
    fn linked_worktree_common_dir() {
        let l = Layout {
            workdir: PathBuf::from("/wt"),
            git_dir: PathBuf::from("/repo/.git/worktrees/wt"),
            common_dir: PathBuf::from("/repo/.git"),
        };
        assert_eq!(
            l.classify(Path::new("/repo/.git/refs/heads/x"), |_| false),
            Some(Change::Refs)
        );
        assert_eq!(
            l.classify(Path::new("/repo/.git/worktrees/wt/HEAD"), |_| false),
            Some(Change::Refs)
        );
    }

    #[test]
    fn watches_real_changes() {
        let t = crate::git::testutil::TestRepo::new();
        t.write(".gitignore", "ignored/\n");
        t.write("a.txt", "a\n");
        t.commit("init");
        let workdir = t.path().canonicalize().expect("canon");
        let repo = git2::Repository::open(&workdir).expect("open");
        let layout = Layout::of(&repo).expect("layout");
        let (tx, rx) = mpsc::channel();
        let _watch = start(&layout, Duration::from_millis(50), move |c| {
            let _ = tx.send(c);
        })
        .expect("watch");
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(workdir.join("a.txt"), "b\n").expect("write");
        let saw_workdir = std::iter::from_fn(|| rx.recv_timeout(Duration::from_secs(5)).ok())
            .take(10)
            .any(|c| c.contains(&Change::Workdir));
        assert!(saw_workdir);

        std::fs::create_dir_all(workdir.join("ignored")).expect("mkdir");
        while rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
        std::fs::write(workdir.join("ignored/x"), "x").expect("write");
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "ignored path"
        );
    }
}
