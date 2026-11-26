# tasty-network

Networking for [Tasty](https://github.com/zilhak/tasty): the stream protocol and its conventions, the transports that carry it (SSH tunnel and direct peer-to-peer QUIC over a hole-punched UDP path), and a rendezvous server that only reflects addresses and coordinates hole punching. It never relays data.

This repository is meant to provide these pieces to Tasty and other Tasty services. It does not depend on the Tasty repository.

Status: under construction. Nothing here is used by a Tasty release yet.

## Design

See the [design documents](docs/design/index.md). [Examples](docs/examples.md) shows how to run the demo binaries and the SSH integration tests.

## Development

After cloning, run once:

```bash
bash scripts/dev-setup.sh
```

It points `core.hooksPath` at `.githooks`, so commits, merges, and pushes run the checks, and prepares shellcheck and the cargo dependencies. On Linux it installs a pinned shellcheck into `~/.local/bin`; elsewhere install shellcheck with your package manager (for example `brew install shellcheck` or `scoop install shellcheck`). If shellcheck is still not on `PATH`, the script ends with exit code 1, because the pre-commit hook would block commits without it; fix `PATH` and run it again. The pre-push check for tasty dependencies (B.3) also needs Python 3 (`python3` or `python`) on `PATH`. All checks go through one entry point:

```bash
bash scripts/check.sh          # fmt, shellcheck, no-tasty-deps, clippy, test
bash scripts/check.sh help     # every step
```

- [Branches](docs/dev-guide/branching.md): git-flow with `develop`, `release`, and `main`, as in Tasty
- [Git hooks](docs/dev-guide/git-hooks.md)
- [CI](docs/dev-guide/ci.md)

## License

MIT. See [LICENSE](LICENSE).
