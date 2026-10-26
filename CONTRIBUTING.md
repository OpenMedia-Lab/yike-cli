# Contributing

Keep changes focused and reproducible. The CLI separates command parsing, immutable source plans, protected local state, transport engines and the private-service adapter.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py'
```

The Python tests cover release packaging on every platform and the optional POSIX launcher (launcher tests skip on Windows). Rust tests use synthetic files and anonymous loopback HTTP fixtures; they require permission to bind localhost sockets. Do not use a real account or upload real media in automated tests.

When a system HTTP proxy is enabled, set `NO_PROXY=127.0.0.1,localhost` and `no_proxy=127.0.0.1,localhost` for the test process. The loopback fixtures must connect directly; no system proxy setting needs to be changed.

Transport changes should cover interrupted operations, durable intent, source mutation, account ownership, original-byte validation and no-overwrite recovery. Never weaken validation to make a failing fixture pass.

Remove credentials, signed URLs, account identities, real filenames and media hashes before sharing diagnostics. Report stage, numeric service code and synthetic reproduction steps instead. Include the target platform and exact command in bug reports.

Prebuilt binaries and packaging are maintained by GitHub Actions; see [release maintenance](docs/releasing.md). Do not commit target directories, release binaries or private transfer state.
