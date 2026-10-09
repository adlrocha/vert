//! Opening files: reading them for the viewer and launching the user's editor.

use std::io::Read;
use std::path::Path;
use std::process::Command;

/// How many leading bytes are inspected to detect binary content.
const BINARY_SNIFF_BYTES: usize = 8000;

/// Reads a text file for display, or explains why it can't be shown.
///
/// # Errors
/// Returns a user-facing message for missing, oversized, binary or unreadable files.
pub fn read_text(path: &Path, max_bytes: u64) -> Result<String, String> {
    let meta = match path.metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("It no longer exists in the working tree.".to_owned());
        }
        Err(e) => return Err(format!("Couldn't read it: {e}")),
    };
    if meta.is_dir() {
        return Err("It's a directory.".to_owned());
    }
    if meta.len() > max_bytes {
        return Err(format!(
            "It's {} KiB, above the {} KiB viewer limit (--max-file-kb).",
            meta.len() / 1024,
            max_bytes / 1024
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|e| format!("Couldn't read it: {e}"))?;
    if bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0) {
        return Err("It's a binary file.".to_owned());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The editor to launch: `$VISUAL`, then `$EDITOR`, then `vi`.
#[must_use]
pub fn editor_from_env() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".to_owned())
}

/// Builds the command that opens `path` at `line` in `editor`.
///
/// `editor` may include arguments (e.g. `"code --wait"`). The line is passed
/// in the form each well-known editor understands.
///
/// ```
/// let cmd = vert::editor::command("nvim", std::path::Path::new("a.rs"), 12);
/// let args: Vec<_> = cmd.get_args().collect();
/// assert_eq!(args, ["+12", "a.rs"]);
/// ```
#[must_use]
pub fn command(editor: &str, path: &Path, line: u32) -> Command {
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let mut cmd = Command::new(program);
    cmd.args(parts);
    let name = Path::new(program)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let located = format!("{}:{line}", path.display());
    match name.as_str() {
        "hx" | "helix" | "zed" | "subl" => {
            cmd.arg(located);
        }
        "code" | "code-insiders" | "cursor" | "codium" | "windsurf" => {
            cmd.arg("--goto").arg(located);
        }
        _ => {
            cmd.arg(format!("+{line}")).arg(path);
        }
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(editor: &str) -> (String, Vec<String>) {
        let cmd = command(editor, Path::new("src/a.rs"), 7);
        (
            cmd.get_program().to_string_lossy().into_owned(),
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
        )
    }

    #[test]
    fn line_syntax_per_editor() {
        assert_eq!(
            args("nvim"),
            ("nvim".into(), vec!["+7".into(), "src/a.rs".into()])
        );
        assert_eq!(
            args("/usr/bin/hx"),
            ("/usr/bin/hx".into(), vec!["src/a.rs:7".into()])
        );
        assert_eq!(
            args("code --wait"),
            (
                "code".into(),
                vec!["--wait".into(), "--goto".into(), "src/a.rs:7".into()]
            )
        );
        assert_eq!(args("kak").1, ["+7", "src/a.rs"]);
        assert_eq!(args("").0, "vi");
    }

    #[test]
    fn reads_text_and_explains_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let text = dir.path().join("a.txt");
        std::fs::write(&text, "hi\n").expect("write");
        assert_eq!(read_text(&text, 1024).as_deref(), Ok("hi\n"));
        assert!(read_text(&text, 1).expect_err("too big").contains("limit"));
        let bin = dir.path().join("b.bin");
        std::fs::write(&bin, [1_u8, 0, 2]).expect("write");
        assert!(
            read_text(&bin, 1024)
                .expect_err("binary")
                .contains("binary")
        );
        assert!(
            read_text(&dir.path().join("nope"), 1024)
                .expect_err("missing")
                .contains("no longer")
        );
        assert!(
            read_text(dir.path(), 1024)
                .expect_err("dir")
                .contains("directory")
        );
    }

    #[test]
    fn editor_env_fallback() {
        // Only checks that some editor is always produced.
        assert!(!editor_from_env().is_empty());
    }
}
