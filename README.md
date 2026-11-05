# vert

**Understand what your agents are building.** `vert` is a terminal UI that shows a
live, labelled feed of a git repository's changes, uncommitted work
included. From the feed you can open syntax-highlighted diffs, search files
and contents, and see where change is concentrating. The Branches tab lists
every worktree, branch and open pull request, so you can follow agents that
work in parallel. Press `x` on any diff and the coding agent you already use
(Claude Code, Codex or pi) explains it: what it is for, what order to read it
in, and what deserves a second look.

```
 ◆ vert │ nibble  ⎇ main                                              ✦ 2 new  ● live
  1 Feed    2 Hotspots    3 Search    4 Branches

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
- **Branches, worktrees and pull requests:** one row per line of work. A
  row shows the checkout it's in, uncommitted files (`●3`), commits ahead and
  behind the base branch, its size, and the matching open pull request with
  its CI checks (✓ passing, ✗ failing, ● running). Pull requests without a local branch get their own rows. `⏎` opens the most
  relevant diff: uncommitted work first, then the branch against the point
  where it left the base, then the pull request. `b` and `p` pick the branch
  or pull request diff. `f` filters by kind, `/` narrows rows by name, and
  `d` removes a line of work — a linked worktree together with its branch —
  after a confirmation that warns about unmerged commits and open pull
  requests. vert never removes the checkout it runs in, the repository's
  main checkout, remote-tracking branches or pull requests, and a checkout
  with uncommitted changes is refused — re-checked at removal time, so files
  that arrived while the confirmation was open are safe too. Diffs of
  linked worktrees stay live, and files open
  from the worktree they belong to.
- **Collisions between lines of work:** a row marked `⚠2` would fail to merge
  with two other lines of work, and `≈3` changes the same files as three
  others. Each pair is compared from where the two diverged, so stacked
  branches don't count. Uncommitted work counts too, but only as `≈`, since
  there is no commit to merge yet. `c` lists what a row collides with, and
  `⏎` opens the diff on that file. Merge checks run in memory, so they write
  nothing to your repository. vert can only tell work apart by branch or
  worktree: two agents sharing one checkout look like one line of work.
- **Explanations:** press `x` in a diff and a coding agent you already use
  (Claude Code, Codex or pi) explains the change for a human reviewer: its
  intent, the order to read it in, a small diagram when a picture helps
  (components, states, a flow or data), and what deserves a second look.
  Steps and boxes point at the hunks the agent placed in the diff, and `⏎`
  jumps to them. vert checks each pointer against the real diff, drops the
  ones that name nothing, and lists the hunks nothing explains, which is
  where to look first. See [Explanations](#explanations).
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
vert also checks once a day whether `main` has moved past the commit it was
built from, and shows `↑ update available` in the header when it has.
`--no-update-check` (or `--no-network`) turns that off.

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
| Anywhere  | `1` `2` `3` `4` / `tab` switch tab · `j` `k` move · `⏎` open · `esc` back · `q` quit |
| Feed      | `n` next new change · `esc` clear filter · `r` reload                   |
| Hotspots  | `w` time window · `m` files/modules · `⏎` filter feed · `o`/`e` view/edit |
| Change    | `tab` switch pane · `J` `K` scroll · `n` `N` hunks · `←` `→` sideways · `e` edit · `x` explain |
| Explain   | `tab` switch pane · `j` `k` move · `⏎` go to the code · `J` `K` scroll · `r` ask again |
| Search    | type to search · `tab` files/text · `ctrl-w`/`ctrl-u` delete · `ctrl-e` edit |
| Branches  | `⏎` open · `b` branch diff · `p` PR diff · `f` filter kinds · `/` filter names · `c` collisions · `d` remove · `r` refresh |

### Pull requests

vert lists open pull requests when `origin` is on GitHub. It needs a token
and looks for one in this order:

1. `GH_TOKEN`
2. `GITHUB_TOKEN`
3. `gh auth token`, if the [GitHub CLI](https://cli.github.com) is logged in

If no token is found, the Branches tab says so and shows everything else.
vert only reads from GitHub: it lists open pull requests with their check
status, and fetches a pull request's diff when you open it. It never fetches
or writes to your repository. Besides your repository, vert sends one
unauthenticated request a day to `api.github.com` for its own latest commit —
no account, token or repository data involved — which is what powers the
update notice. `--no-network` turns all GitHub access off.

### Explanations

Press `x` in any diff (a commit, the working tree, a branch or a pull
request) and a coding agent explains the change for a human reviewer. vert
doesn't call a model itself. It runs the agent CLI you already have, so the
agent's login, subscription and settings apply, and vert needs no API key.

**Setup.** Install and log in to at least one of
[Claude Code](https://docs.anthropic.com/en/docs/claude-code),
[Codex](https://github.com/openai/codex) or pi, then press `x`. By default
vert uses the first of `claude`, `codex` and `pi` it finds on `PATH`. The
agent and model in use are shown at the top of each explanation.

**Choosing the agent and model.** Set them once per machine in your shell
profile (`~/.zshrc`, `~/.bashrc`, …) and they apply to every run:

```sh
export VERT_EXPLAIN_AGENT=pi                          # auto (default), claude, codex or pi
export VERT_EXPLAIN_MODEL=anthropic/claude-haiku-5-5  # optional; otherwise the agent's default
```

Each variable has a matching flag (see [Options](#options)), and a flag on
the command line overrides the variable, e.g. for one run:
`vert --explain-agent claude --explain-model sonnet`. Name the model the way
the agent itself does:

| Agent       | Model examples                  | Where the names come from                 |
| ----------- | ------------------------------- | ----------------------------------------- |
| Claude Code | `sonnet`, `opus`, `haiku`       | `claude --model`                          |
| Codex       | any model your Codex login offers | `codex --model`                         |
| pi          | `anthropic/claude-haiku-5-5`    | `pi --list-models`. Always write `provider/model`, because a bare name fails when several providers are logged in |

**Any other tool.** `VERT_EXPLAIN_CMD` (or `--explain-cmd`) runs a shell
command instead of a known agent (it takes precedence over
`VERT_EXPLAIN_AGENT`), for example
`export VERT_EXPLAIN_CMD='llm -m gpt-5'`. The command must read the prompt
on standard input and print the JSON answer the prompt asks for. It gets the
model, if one is set, in `VERT_EXPLAIN_MODEL`.

**What you get.** The answer opens as an outline:

- an overview: the intent and, when the agent thinks one helps, a small
  diagram of components, states, a flow or data;
- the steps, in the order to read them;
- concerns, i.e. what deserves a second look;
- the hunks that no step or diagram box covers. This is where to look first.

Steps and boxes point at hunks of the diff, and `⏎` jumps to the code. vert
checks every pointer against the real diff and drops any that point at
nothing. In the diagram, `+` marks what the change added, `~` what it changed
and `−` what it removed. `↺` points back to a box already drawn.

**How the agent is run.** The agent runs in the checkout the change belongs
to, with the prompt on standard input: the change's message and its diff,
with every hunk numbered. Claude Code, Codex and pi get read-only tools, so
they can open the surrounding code but can't change it. No session is saved.
These are the full commands:

| Agent         | Runs as                                                                            |
| ------------- | ---------------------------------------------------------------------------------- |
| Claude Code   | `claude -p --output-format text --no-session-persistence --tools Read,Grep,Glob`   |
| Codex         | `codex exec --sandbox read-only --skip-git-repo-check --ephemeral --color never -` |
| pi            | `pi -p --no-session --tools read,grep,find,ls`                                     |
| `--explain-cmd` | `sh -c CMD`, unconfined, with your own permissions                               |

When a model is set, `--model MODEL` is added before Codex's final `-`, or at
the end for the others. Each agent gets `--explain-timeout-secs` (default
600) to answer. A diff larger than `--explain-max-diff-kb` (default 256) is
sent in full only up to that size; later hunks are sent by their header
alone.

**Cache and privacy.** Answers are cached in `$XDG_CACHE_HOME/vert/explain`
(`~/.cache/vert/explain` by default), readable only by you. The cache key
covers the agent, the model and the exact diff, so each commit is explained
once and a working tree that has moved on gets a fresh answer. `r` asks
again. The diff leaves your machine through the agent, as with any prompt
you give it. `--no-network` turns explanations off.

**Troubleshooting.** When there is no agent to ask, `x` shows
*Can't explain: …* at the bottom of the screen with the reason:

- `no coding agent found`: none of `claude`, `codex` or `pi` is on the
  `PATH` vert was started with. Install one, or set `VERT_EXPLAIN_CMD`.
- `pi is not on PATH (--explain-agent)`: `VERT_EXPLAIN_AGENT` names an agent
  that isn't installed.
- `network access is off`: vert was started with `--no-network`.

When the agent runs but fails, for example because it is logged out, the
model is unknown or it timed out, the explanation reads *Couldn't explain
this change* followed by the agent's own error output, and `r` tries again.
For the full output, run the command from the table above in a terminal.

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
| `--base`                 | auto    | Branch to compare against (`origin/HEAD`, else `main`/`master`) |
| `--max-branches`         | 30      | Branches without a worktree to show, newest first |
| `--overlap-ignore`       | common lock files | Files that never count as a collision, by name or path, comma-separated |
| `--max-overlap-checks`   | 100     | Pairs of branches to merge-check in one pass; the rest show as overlaps until a later pass |
| `--branch-refresh-secs`  | 5       | Branches tab rescan interval                    |
| `--pr-refresh-secs`      | 60      | Pull request refresh interval                   |
| `--max-prs`              | 50      | Open pull requests to list (at most 100)        |
| `--github-api`           | `https://api.github.com` | API URL; for GitHub Enterprise `https://HOST/api/v3` |
| `--github-timeout-secs`  | 15      | GitHub request timeout                          |
| `--no-network`           | off     | Never contact GitHub, and no explanations       |
| `--no-update-check`      | off     | Never ask GitHub whether a newer vert exists    |
| `--explain-agent` / `VERT_EXPLAIN_AGENT` | `auto` | Agent for `x`: `auto`, `claude`, `codex` or `pi` |
| `--explain-model` / `VERT_EXPLAIN_MODEL` | agent's default | Model the agent uses, in its own naming |
| `--explain-cmd` / `VERT_EXPLAIN_CMD` | — | Shell command to use instead of a known agent |
| `--explain-timeout-secs` / `VERT_EXPLAIN_TIMEOUT_SECS` | 600 | How long an agent may take |
| `--explain-max-diff-kb` / `VERT_EXPLAIN_MAX_DIFF_KB` | 256 | Diff sent in full; later hunks go by header only |

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
| `git`       | Feed entries (with a per-commit cache), branches and worktrees, collisions between them, structured diffs |
| `github`    | `origin` detection, token lookup, open pull requests and their diffs |
| `explain`   | Prompts, agent routing, answer validation and caching, diagram layout |
| `labels`    | Size, breadth and revert classification                          |
| `hotspots`  | Ranking by change frequency                                      |
| `search`    | Fuzzy file names (nucleo) and content search (ripgrep crates)    |
| `watch`     | inotify/FSEvents, ignoring reads and `.gitignore`d paths          |
| `highlight` | syntect with an ANSI theme                                       |
| `ui`        | ratatui rendering                                                |
