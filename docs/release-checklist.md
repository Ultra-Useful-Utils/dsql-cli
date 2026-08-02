# Compatibility matrix and release checklist

This is the Milestone 6 release-candidate record. A checked local item has
repository-local evidence; an unchecked item requires an authorized live or
publication action and must not be inferred from CI configuration.

## Compatibility matrix

| Area | Local contract/evidence | Release result required | Status |
| --- | --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` archive, `--help`, and `--version` smoke step | Smoke the uploaded archive on a clean Linux x86_64 host | automated workflow contract passed; clean-host execution pending |
| Linux arm64 | `aarch64-unknown-linux-musl` archive, `--help`, and `--version` smoke step | Smoke the uploaded archive on a clean Linux arm64 host | automated workflow contract passed; clean-host execution pending |
| macOS x86_64 | `x86_64-apple-darwin` archive, `--help`, and `--version` smoke step | Smoke the uploaded archive on a clean Intel macOS host | automated workflow contract passed; clean-host execution pending |
| macOS arm64 | `aarch64-apple-darwin` archive, `--help`, and `--version` smoke step | Smoke the uploaded archive on a clean Apple Silicon host | automated workflow contract passed; clean-host execution pending |
| Credentials and targeting | Unit/integration coverage plus opt-in live suite | Validate discovery, `admin`, and custom database roles with authorized credentials | automated non-live coverage passed; live validation pending |
| Input and output | CLI, scanner, renderer, and JSONL v1 tests | Recheck table/CSV/TSV/JSONL, scripts, stdin, and broken-pipe behavior | automated local coverage passed; release recheck pending |
| Interactive shell | PTY/unit coverage and shell documentation | Recheck cancellation, history, terminal restoration, and every advertised command | automated local coverage passed; release recheck pending |
| Metrics dashboard | Deterministic model tests and opt-in live shape test | Validate `GetMetricData`, no-data behavior, and permission diagnostics | automated non-live coverage passed; live validation pending |

## Local release checklist

- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo clippy --locked --all-targets --all-features -- -D warnings`.
- [x] Run `cargo test --locked --all-targets --all-features`.
- [ ] Run the ignored local TLS PostgreSQL suite where Docker is available.
- [x] Run `cargo package --locked` (Cargo's package dry-run) and inspect the
  generated package file list.
- [x] Run the executable local release-artifact contract: it parses the workflow,
  validates all four target definitions and their archive/smoke commands, and
  builds, checksums, extracts, and smokes the native-host archive.
- [x] Validate workflow checksum coverage, uploaded SBOM/dependency/license
  evidence, provenance subjects, crates.io dry run, and tagged publication gate.

## Automated local compatibility record (2026-08-01)

- `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`, and
  `cargo test --locked --all-targets --all-features` passed. The test suite ran
  280 non-ignored tests across targets; Docker, live Aurora DSQL, or
  large-fixture tests remained ignored.
- `tests/release_readiness.rs` parses `.github/workflows/release.yml` and checks
  each of the four target/runner/archive definitions, required archive entries,
  packaged `--help` and `--version` commands, checksum glob coverage,
  SBOM/dependency/license uploads, provenance subjects, the crates.io package
  dry run, and the version-tag publication gate. It also creates
  and smokes an archive from the current native-host test binary.
- This is local contract evidence only. It does **not** execute the Linux arm64
  or macOS artifacts, run a live Aurora DSQL suite, publish an artifact, or
  perform post-publication verification. Those unchecked protected checklist
  items remain required.

## Protected external checklist

- [ ] With explicit authorization and a development cluster, run the opt-in live
  Aurora DSQL read-only and custom-role suites described in [testing](testing.md).
- [ ] Run the separately authorized mutating suite only against its confirmed
  development cluster.
- [ ] Review no critical/high dependency or code-security findings remain.
- [x] Protect the GitHub `release` environment and version tags with the
  repository's required-reviewer and tag rules before pushing a release tag.
- [x] Confirm `CRATES_IO_API_KEY` is available for the initial crates.io publish.
  After the first release, configure crates.io Trusted Publishing for
  `.github/workflows/release.yml`, migrate the job to
  `rust-lang/crates-io-auth-action@v1`, and remove the long-lived secret.
- [ ] Publish no artifacts until the RC review recommends the M6 gate.
- [x] Create and push a tag exactly matching `v` plus the `Cargo.toml` version;
  the release workflow publishes crates.io first and then creates the GitHub
  Release with archives and evidence.
- [ ] After approved publication, verify release archives, checksums,
  attestations, SBOM, `cargo install dsql-cli`, and first-run documentation on
  all four clean hosts.

## v1.0.0 publication record (2026-08-02)

- Published `dsql-cli` 1.0.0 to crates.io and generated its docs.rs page.
- Published all four platform archives plus checksums, SBOM, dependency, and
  license evidence to the GitHub Release.
- Verified the downloaded checksums and Linux x86_64 archive, archive and SBOM
  provenance, and a clean-root `cargo install dsql-cli --version 1.0.0 --locked`.
- Clean-host verification on Linux arm64 and both macOS architectures remains
  pending, as do the separately authorized live Aurora DSQL suites.

## Consumer verification

After publication, download the release checksum file and verify the selected
archive before extraction:

```sh
sha256sum --check SHA256SUMS
tar -xzf dsql-*.tar.gz
./dsql --version
./dsql --help
```

On macOS, use `shasum -a 256 -c SHA256SUMS` if GNU `sha256sum` is unavailable.
