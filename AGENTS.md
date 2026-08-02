# Agent guide

`AGENTS.md` is the canonical instruction set for coding agents. Read it and
`CONTEXT.md` before changing code or documentation.

## Terminology

Use the Aurora DSQL terminology defined in `CONTEXT.md`:

- A **cluster** is a regional Aurora DSQL resource, not a database or instance.
- A **discoverable cluster** is returned by `ListClusters`; that does not prove
  connection permission.
- A **cluster selector** is a cluster ID, cluster ARN, or canonical endpoint.
- A **database role** is a PostgreSQL authorization role, not an IAM role.
- `admin` is the elevated predefined database role; other roles are **custom
  roles**.
- The terminal session is the **interactive shell**; its CloudWatch view is the
  **metrics dashboard**; safe idle-connection replacement is a **session
  refresh**.

## Repository map

| Path | Purpose |
| --- | --- |
| `src/cli.rs` | CLI parsing, command orchestration, and output selection. |
| `src/aws/` | AWS configuration, cluster discovery, identity, and metrics adapters. |
| `src/db/` | IAM authentication, TLS, PostgreSQL session, and execution adapters. |
| `src/shell/`, `src/output/`, `src/sql/`, `src/dashboard/` | Interactive shell, renderers, SQL framing/metadata, and metrics dashboard. |
| `tests/` | Deterministic unit/integration tests plus explicitly ignored Docker/live suites. |
| `docs/` | Public user documentation; `docs/adr/` records accepted decisions. |
| `.github/workflows/` | Deterministic CI and manually dispatched release automation. |

## Invariants

- Preserve AWS SDK default credential-chain behavior. Never log, commit, print,
  or add fixtures containing credentials, authentication tokens, signed URLs,
  real account IDs, cluster endpoints, or user data.
- TLS certificate-chain and hostname verification are mandatory. Do not add an
  insecure, plaintext, or verification-bypass mode.
- Aurora DSQL is authoritative for SQL support. Do not add a client SQL
  allowlist or automatically replay SQL after cancellation, an OCC conflict, or
  a disconnect.
- Keep machine-readable output on stdout and prompts/diagnostics on stderr.
- Do not widen supported backslash commands or claim `psql` compatibility
  without updating the documented public contract and tests.

## Validation

Run the standard local gates for relevant changes:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

For package-facing changes, also inspect `cargo package --locked --list`.
Do not run ignored tests merely to satisfy a gate.

## Live-test and release boundaries

Never run ignored live Aurora DSQL tests without explicit documented
authorization and an appropriate development cluster. Never change credentials
or external resources without approval. CI must not use AWS credentials or
connect to Aurora DSQL.

Publication remains manual and protected: do not run `cargo publish`, create a
release, push tags, or alter release credentials unless explicitly authorized.

## Documentation synchronization

When public behavior changes, update the applicable `README.md` and files in
`docs/` in the same change. Keep `docs/cli.md`, `docs/iam.md`,
`docs/interactive-shell.md`, `docs/security.md`, `docs/testing.md`, and
`docs/troubleshooting.md` aligned with the implementation. Internal plans,
reviews, local automation state, and validation evidence are intentionally
ignored and excluded from the crates.io package.
