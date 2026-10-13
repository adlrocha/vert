# AGENTS.md

vert is a terminal UI that shows a live, labelled feed of a git repository's changes — commits, uncommitted work, diffs, search, and hotspots — written in Rust with ratatui. Read [`README.md`](README.md) for features, CLI options, install, and the module map. This file tells agents how to work in the repo.

## Boot sequence

1. Read this file.
2. Read [`README.md`](README.md) — especially the Development and Architecture sections.
3. Before changing interaction logic, read `src/app.rs` (`Msg`, `Effect`, `App::update`) and `src/jobs.rs`. They define the boundary all features cross.
4. Read the nearest module (and its tests) before changing it.

## Architecture rules

- All interaction logic lives in `App::update(msg) -> Vec<Effect>`, a pure state machine. Keys, loads, and file changes arrive as `Msg`s; side effects go out as `Effect`s. No I/O inside app logic.
- `main.rs` is terminal setup and the event loop only. Don't grow it.
- I/O happens only in `jobs::run` on worker threads (git, search, watching). A new side effect becomes a new `Effect` variant plus an arm in `jobs::run`, never a direct call from `update`.
- `src/ui` renders app state read-only. Keep rendering decisions out of `App`.
- Thresholds (size labels, wide modules, debounce, caps) are CLI options. Don't hardcode what is already configurable, and don't add constants that should be options.

## Code rules

- Rust 2024. `unsafe_code` stays forbidden.
- Zero clippy warnings: `all` is denied and `pedantic` enabled in `Cargo.toml`. Keep new code clean under `cargo clippy --all-targets -- -D warnings`.
- `missing_docs` is on: document public items.
- No `unwrap()`/`expect()` outside tests and doc examples. Propagate errors; the binary edge converts them with anyhow.
- The UI uses only the terminal's 16 ANSI colours (`src/theme.rs`) so it inherits the user's theme. Never hardcode RGB values or introduce new named colours.
- When there is a bug, first write the failing test in the reproduction, then fix it.

## Verification

Done means `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt` are clean. Installer changes also run `tests/install_test.sh`; use `cargo llvm-cov --summary-only` when coverage matters. Run the narrowest meaningful check while iterating, the full set before yielding.

## Commits and PRs

Conventional commits, lowercase imperative subject, no trailing period: `feat:`, `fix:`, `perf:`, `refactor:`, `test:`, `docs:`, `ci:`, `build:`. Reviews come from the nibble-review action; resolve each finding by fixing it or replying with evidence why it does not apply, never silently ignoring one.

## Before yielding

- State only verified facts.
- Prefer the smallest correct change; when unsure, read the surrounding code and follow existing patterns.
- If the user asks a question, answer it first; don't refactor unprompted.
