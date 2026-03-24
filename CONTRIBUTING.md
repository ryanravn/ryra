# Contributing to Ryra

## Getting Started

**Requirements:** Rust (stable toolchain)

```
git clone https://github.com/ryanravn/ryra.git
cd ryra
cargo build
```

## Making Changes

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Run checks before submitting:

```
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

4. Open a pull request against `main` — CI requires approval before it runs on PRs from forks

## Code Guidelines

The full coding guidelines are in [`CLAUDE.md`](CLAUDE.md). The key points:

- **Make invalid state unrepresentable** — use enums and pattern matching instead of string comparisons, boolean flags, or optional fields that are only valid in some states
- **No unwraps** — never use `.unwrap()`, `.expect()`, or `panic!()`. Propagate errors with `?` or handle them explicitly
- **Conventional Commits** — prefix commit messages with `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`, or `ci:`

## Architecture & E2E Tests

See the [README](README.md) for project architecture and the [development guidelines](CLAUDE.md) for E2E testing setup.

## Reporting Issues

Open an issue on GitHub. Include:

- What you were trying to do
- What happened instead
- Your OS and Rust version
- Any relevant logs or error output

## License

By contributing, you agree that your contributions will be licensed under the [AGPL-3.0](LICENCE.md).
