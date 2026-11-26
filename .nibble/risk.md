# Risk map

What merging to `main` sets off here, and what can't be undone. `nibble land` gives this to its judge. It is context, not rules: the rules are in `sdlc.toml`.

- `main` is what people install: `install.sh` builds from `main` unless `VERT_REF` says otherwise. Nothing updates by itself, so a bad merge reaches a machine only when someone reinstalls. A revert fixes the next install.
- vert runs the user's coding agent (Claude Code, Codex, pi) on their machine. `explain::agent` and `agent::Access` decide what that agent may read and edit, and `git::commit` decides what vert commits after a fix. A change there widens what an agent can do on the host.
- Agent output is untrusted and is printed to the terminal; `highlight::sanitize` is what stops terminal escape injection.
- The answer cache must stay owner-only (0700 directory, 0600 files).
- `install.sh` runs on other people's machines with their permissions.
- Everything else (feed, labels, diffs, search, hotspots, the Branches tab) only reads the repository and draws the terminal. A bug there is visible and easy to revert.
