//! Embeds the commit this build is made from, so vert can tell whether `main`
//! has moved past it (see `src/jobs.rs`).
//!
//! Without `rerun-if-changed` directives cargo reruns this script whenever
//! any package file changes, which keeps the embedded commit fresh after a
//! `git pull`. When git is unavailable the build simply carries no commit and
//! the update check stays off.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Only vert's own checkout counts. Without a `.git` of its own (a
    // vendored copy, a source tarball) git's discovery would climb into an
    // enclosing repository and embed its HEAD instead.
    let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    if !PathBuf::from(&manifest).join(".git").exists() {
        return;
    }
    let Ok(out) = Command::new("git")
        .args(["log", "-1", "--format=%H %ct"])
        .current_dir(&manifest)
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    let (Some(sha), Some(time)) = (fields.next(), fields.next()) else {
        return;
    };
    println!("cargo:rustc-env=VERT_SHA={sha}");
    println!("cargo:rustc-env=VERT_TIME={time}");
}
