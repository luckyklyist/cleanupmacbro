# Contributing to Clean You

Thanks for helping! A few guidelines keep the app small, fast and safe.

## Getting started

```bash
git clone https://github.com/luckyklyist/cleanupmacbro.git && cd cleanupmacbro
cargo run
```

Before opening a pull request:

```bash
cargo test
cargo build --release
```

## Principles

- **Safety first.** Anything that deletes must show the user what goes, default to the Trash, and never touch `/System`.
  New Smart Clean targets must be truly regenerable (caches, logs, build output).
- **No telemetry, no network calls.**
- **Native and light.** Prefer the standard library and macOS tools over new crates.
- **Tests for logic.** Scanning rules, matchers and charge logic have unit tests; please add one for new rules.

## Reporting bugs

Open an issue with your macOS version, Mac model (Apple Silicon / Intel), what you did, and what happened.
For battery issues include the output of:

```bash
"/Applications/Clean You.app/Contents/MacOS/clean-you" --smc-read
```

## Pull requests

- Keep PRs focused on one change.
- Describe what changed and how you tested it (screenshots for UI changes help a lot).
