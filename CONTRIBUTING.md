# Contributing to trailboss-adapters

Report problems and propose new agents or terminal apps in [trailboss-ai/trailboss](https://github.com/trailboss-ai/trailboss/issues); link the issue from your pull request.

## Rules

- **Agents' data is read-only.** Never write, move or delete anything in an agent's folder; open databases you don't own read-only.
- **Transcripts are private.** Never log their content or put it in an error message, and never commit real ones. Tests use small synthetic fixtures in `tests/fixtures/`.
- **Every adapter has fixture tests** for a normal transcript, a truncated or partly written one, and an unknown record type, which must be skipped without failing.
- **Typing into terminals** (`Host::send`) passes text as one argument, never through a shell string.
- **No network code** in this crate.
- Stable Rust, edition 2024; no `unwrap()`/`expect()` outside tests unless a comment says why it can't fail; comments say why, not what.

Run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before opening a pull request.

## License and contributor terms

This repository is released under the [MIT License](LICENSE). By submitting a contribution (code, documentation, fixtures or anything else) through a pull request, patch or other means, you agree to the following:

1. **You keep your copyright.** Your contribution is licensed to everyone under the MIT License, the same as the rest of the project.
2. **You also grant the maintainer the right to relicense it.** You grant Manas Talukdar, and anyone the project is transferred to, a perpetual, worldwide, non-exclusive, royalty-free, irrevocable license to use, copy, modify, distribute and sublicense your contribution, and to release it under other license terms, including commercial or proprietary ones, alongside or instead of the MIT License. Versions already released under MIT stay available under MIT.
3. **You have the right to contribute it.** The contribution is your own work, or you have permission to submit it under these terms, and it isn't subject to an employer's or anyone else's claim that would conflict with them. If your employer has rights to your work, you have their permission.
4. **Third-party code is marked.** Anything you didn't write yourself is called out in the pull request, with its source and license, and noted in the README. It must be under a license compatible with these terms (MIT, Apache-2.0, BSD or similar).

Sign off each commit to confirm this (`git commit -s`, which adds `Signed-off-by: Your Name <you@example.com>`). Pull requests whose commits aren't signed off can't be merged.

Why: the core stays MIT, but this keeps the option to offer Trailboss, or parts of it, under other terms later without having to find and ask every past contributor.
