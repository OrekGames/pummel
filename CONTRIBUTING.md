# Contributing to Pummel

Thanks for your interest in contributing.

## Licensing

By submitting a contribution, you agree that your contribution is licensed under
the MIT License that covers this project. You retain copyright in your
contribution; you are not asked to assign copyright to OrekGames or any
individual.

## Development setup

Requirements:

- Rust matching or newer than the `rust-version` in `Cargo.toml`
- `cargo`, `rustfmt`, and `clippy`

Useful checks before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo check --examples
cargo bench --no-run
cargo publish --dry-run
```

## Pull requests

- Keep changes focused and explain the user-visible impact.
- Prefer conventional commit messages (`feat`, `fix`, `docs`, `test`, `chore`, `ci`, `refactor`).
- Update documentation when behavior or install/release steps change.
- Do not commit secrets, private keys, local tracker state, or editor/OS noise.

## Reporting security issues

See [`.github/SECURITY.md`](.github/SECURITY.md).
