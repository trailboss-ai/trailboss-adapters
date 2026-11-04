# trailboss-adapters

The part of [Trailboss](https://github.com/trailboss-ai/trailboss) that talks to things outside it:

- **Sources** (`src/sources/`): reading each coding agent's sessions into one model ([`model.rs`](src/model.rs): sessions, messages, token usage, file edits). Claude Code, Codex, opencode and pi; about 20 more listed from their files (`catalog`); and any agent that writes JSONL, through a mapping in the config (`custom`). Agents' data is only ever read; databases are opened read-only.
- **Hosts** (`src/hosts/`): terminal apps behind the `Host` trait (list terminals, look at one, type into it, read its screen). Herdr and tmux.

The Trailboss CLI builds on this crate: it indexes what the sources read, prices it, tracks which session runs in which terminal, and resumes them. That core isn't public; the parts that read your transcripts and drive your terminals are, here.

## Adding an agent or a terminal app

- **An agent:** a module in `src/sources/` that turns a transcript into `Batch`es, with fixtures in `tests/fixtures/<agent>/`: a normal transcript, a truncated one, and one with an unknown record type (skipped, not an error). Fixtures are synthetic: never real transcripts.
- **A terminal app:** a module in `src/hosts/` implementing `Host`, added to `hosts::all()`.

Open an issue in [trailboss-ai/trailboss](https://github.com/trailboss-ai/trailboss/issues) first (issues are off here); see [CONTRIBUTING.md](CONTRIBUTING.md).

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

## License

MIT ([LICENSE](LICENSE)).
