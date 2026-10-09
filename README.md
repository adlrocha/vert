# vert

**Grok what your agents are building.** `vert` is a terminal UI that shows a
live, labelled feed of a git repository's changes, uncommitted work
included. From the feed you can open syntax-highlighted diffs, search files
and contents, and see where change is concentrating.

```
 ◆ vert │ nibble  ⎇ main                                              ✦ 2 new  ● live
  1 Feed    2 Hotspots    3 Search

╭ Changes 143 ─────────────────────────────────────────────────────────────────────────╮
│▌   now  working  Uncommitted changes across 2 modules               +26  ■■■■■  1 file│
│ ✦   1d  0ceae5a  fix(status): replace racy hook   ▴ medium       +55 −38  ■■■■■ 2 files│
│     2d  e0fcba1  Merge remote-tracking branch     ⇄ wide ▲ xl +7044 −13682  ■■■■■ 88 files│
```

## Features

- **Live change feed:** commits plus a pinned `working` row for uncommitted,
  staged and untracked changes. It updates within about 300 ms of an agent
  touching a file. Commits that arrive while vert is open are marked `✦`, and
  `n` jumps to the next one.
- **Labels:** `small/medium/large/xl` by lines changed, `wide` when a change
  touches many modules, plus `reverted` and `revert` detection.
- **Diff view:** a file list and a syntax-highlighted diff with old and new line
  numbers. `n`/`N` jump between hunks. An open working-tree diff refreshes in
  place as files change.
- **Search:** fuzzy file names, or content search using ripgrep's engine (smart
  case, regex with literal fallback, respects `.gitignore`).
- **Hotspots:** files or modules ranked by how often they change over 24h, 7d,
  30d or all loaded history. `⏎` filters the feed to that file or module.
- **File viewer:** shows the full file, with lines added by the change you came
  from marked in the gutter. `e` opens `$VISUAL`/`$EDITOR` at that line
  (vim, nvim, helix, VS Code, Cursor, …).
- **Inherits your terminal theme:** it only uses the 16 ANSI colours.

## Install

Works on Linux and macOS. You need [Rust](https://rustup.rs) and git; on macOS,
`xcode-select --install` provides git and the C compiler the build needs. Then:

```sh
curl -fsSL https://raw.githubusercontent.com/adlrocha/vert/main/install.sh | bash
```

The installer clones the source to `~/.local/share/vert/src`, builds it, and
puts the binary in `~/.local/bin/vert`.

**To update,** run the same command again. It fetches the latest `main`,
rebuilds, and swaps the binary in place, or tells you it's already up to date.

| Option / env var                  | Default            | Purpose                          |
| --------------------------------- | ------------------ | -------------------------------- |
| `--bin-dir DIR` / `VERT_BIN_DIR`  | `~/.local/bin`     | Where the binary goes            |
| `--ref REF` / `VERT_REF`          | `main`             | Branch or tag to build           |
| `--source DIR`                    | —                  | Build a local checkout instead   |
| `--uninstall`                     | —                  | Remove the binary and source     |

Pass options through the pipe with `bash -s --`, for example
`curl -fsSL …/install.sh | bash -s -- --bin-dir /usr/local/bin`.

From a clone you can also run `./install.sh --source .` or `cargo install --path .`.

## Usage

```sh
vert            # the repository containing the current directory
vert ~/code/app # or any path inside a repository
```

Press `?` inside vert for all keys. The essentials:

| Where     | Keys                                                                   |
| --------- | ---------------------------------------------------------------------- |
| Anywhere  | `1` `2` `3` / `tab` switch tab · `j` `k` move · `⏎` open · `esc` back · `q` quit |
| Feed      | `n` next new change · `esc` clear filter · `r` reload                   |
| Hotspots  | `w` time window · `m` files/modules · `⏎` filter feed · `o`/`e` view/edit |
| Change    | `tab` switch pane · `J` `K` scroll · `n` `N` hunks · `←` `→` sideways · `e` edit |
| Search    | type to search · `tab` files/text · `ctrl-w`/`ctrl-u` delete · `ctrl-e` edit |

### Options

| Flag                     | Default | Meaning                                         |
| ------------------------ | ------- | ----------------------------------------------- |
| `--limit`                | 500     | Commits loaded into the feed and hotspots        |
| `--module-depth`         | 1       | Directory depth that defines a module           |
| `--wide-modules`         | 3       | Modules touched for a change to count as wide   |
| `--medium-lines` / `--large-lines` / `--xl-lines` | 50 / 200 / 1000 | Size thresholds (added + removed) |
| `--debounce-ms`          | 300     | Quiet period before reacting to file changes    |
| `--max-search-results`   | 2000    | Content-search match cap                        |
| `--max-highlight-lines`  | 20000   | Longer files and diffs skip syntax colours      |
| `--max-file-kb`          | 4096    | Largest file the viewer opens                   |
| `--no-watch`             | off     | Disable live updates                            |

## Development

```sh
cargo test                                   # unit, render and doc tests
tests/install_test.sh                        # installer end-to-end tests
cargo clippy --all-targets -- -D warnings    # pedantic lints, zero warnings
cargo fmt
cargo llvm-cov --summary-only                # coverage
```

### Architecture

`App::update(msg) -> Vec<Effect>` is a pure state machine. Keys, loads and
file changes come in as `Msg`s. I/O goes out as `Effect`s, which `jobs::run`
performs on worker threads. The binary (`main.rs`) is only terminal setup and
the event loop. This keeps all interaction logic testable without a terminal.

| Module      | Responsibility                                                   |
| ----------- | ---------------------------------------------------------------- |
| `git`       | Feed entries (with a per-commit cache) and structured diffs       |
| `labels`    | Size, breadth and revert classification                          |
| `hotspots`  | Ranking by change frequency                                      |
| `search`    | Fuzzy file names (nucleo) and content search (ripgrep crates)    |
| `watch`     | inotify/FSEvents, ignoring reads and `.gitignore`d paths          |
| `highlight` | syntect with an ANSI theme                                       |
| `ui`        | ratatui rendering                                                |
