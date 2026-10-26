# Contributor guidance

Yike CLI is a Rust command-line client for the account owner's original media. Keep API adapters, protected authentication, immutable plans and transfer state separate.

- Never commit credentials, real media, account catalogs, signed URLs or private transfer state. Tests use anonymous fixtures and synthetic bytes only.
- A verified backup requires streaming the cloud original and matching the immutable local SHA-256. Upload success and metadata matches are separate states.
- Preserve uncertain precreate/final-create results for reconciliation across jobs; never blindly resubmit. Source changes invalidate planned items.
- Preserve account isolation, native-filesystem private state, dedicated-browser login and exact-origin credential boundaries.
- Do not implement cloud deletion, mirror deletion, source deletion, compression or splitting to bypass service limits.
- Run cargo fmt --check, cargo clippy --locked --all-targets -- -D warnings and cargo test --locked --all-targets for Rust changes. Optional POSIX launcher tests use Python's unittest.
- Do not claim an untested platform or an incomplete transfer as complete. Changes to transport behavior need meaningful interruption and validation regressions.
