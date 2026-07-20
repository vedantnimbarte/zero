# Contributing to Zero

Thanks for your interest in Zero — India's ground-up, privacy-first web browser
written in Rust. Contributions of all kinds are welcome: code, docs, design,
bug reports, and ideas.

> **Status:** Zero is in early development (see [`docs/03-ROADMAP.md`](docs/03-ROADMAP.md)).
> APIs and architecture are still moving. Open an issue to discuss larger changes
> before writing code.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By
participating, you agree to uphold it. Report unacceptable behavior to
nimbartevedant@gmail.com.

## Getting started

1. Install a recent stable [Rust toolchain](https://rustup.rs/).
2. Fork and clone the repo.
3. Build and test the workspace:

   ```sh
   cargo build
   cargo test
   ```

## Making changes

1. Create a branch off `main`.
2. Keep changes focused — one logical change per PR.
3. Before pushing, make sure the workspace is clean:

   ```sh
   cargo fmt --all
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test
   ```

4. Write clear commit messages describing the *why*, not just the *what*.
5. Open a pull request against `main` and describe your change and how you
   tested it. Link any related issue.

## Reporting bugs & requesting features

Open a [GitHub issue](../../issues). For bugs, include steps to reproduce, what
you expected, what happened, and your OS/toolchain versions.

## Security issues

**Do not** file security vulnerabilities as public issues. See
[SECURITY.md](SECURITY.md) for how to report them privately.

## License

By contributing, you agree that your contributions will be licensed under the
[Apache License 2.0](LICENSE).
