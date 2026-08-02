# Aurora DSQL CLI manual QA

<!-- manual-qa: safe-local-default -->

This is the canonical manual QA plan for `dsql`. A **cluster** is an Aurora
DSQL regional resource; a **cluster selector** identifies one; and a **database
role** is the PostgreSQL role used for authorization. “Discoverable” means
returned by `ListClusters` for the active IAM identity; it does not prove that
the identity can connect.

## Safety and harness

`scripts/manual-qa.sh` finds the repository root from its own path. With no
arguments it runs only deterministic local checks. It does not access AWS,
Docker, cluster endpoints, credentials, or external resources; it does not
mutate a database, publish, push, or create resources. Results are retained per
check and may be written to a secret-free report.

```sh
scripts/manual-qa.sh --help
scripts/manual-qa.sh --list
scripts/manual-qa.sh --dry-run
scripts/manual-qa.sh --report manual-qa-report.txt
```

All non-local work needs `--tier`. Live work also needs a value-consuming,
literal authorization: `--confirm-live authorized`. Mutating work additionally
needs `--confirm-mutation authorized`. Any other confirmation value (including
`not-confirmed`), missing value, unsupported combination, unknown option/tier,
missing prerequisite, or unwritable `--report` destination fails closed.

```sh
scripts/manual-qa.sh --dry-run --tier docker
scripts/manual-qa.sh --tier live-read-only --confirm-live authorized
scripts/manual-qa.sh --tier live-custom-role --confirm-live authorized
scripts/manual-qa.sh --tier live-mutating --confirm-live authorized --confirm-mutation authorized
scripts/manual-qa.sh --tier release --report manual-qa-release.txt
```

The `--dry-run` and `--list` modes execute no checks. Do not place access keys,
authentication tokens, connection strings, or SQL/result data in reports or
captured evidence. The harness records only its fixed command labels and exit
statuses, never environment values.

## Coverage matrix

| Tier | Automated harness checks | Human observations and evidence |
| --- | --- | --- |
| <!-- manual-qa: tier=local --> `local` | format, Clippy, all locked tests; help/version/subcommand help; locally rejected `clusters -c` misuse | command output, exit status, unit/integration logs |
| <!-- manual-qa: tier=docker --> `docker` | ignored `local_tls_postgres` tests after Docker prerequisite | local TLS protocol, cancellation, disconnect cleanup log |
| <!-- manual-qa: tier=live-read-only --> `live-read-only` | ignored discovery/admin/metrics test after authorization and existing environment gates | IAM identity, discoverable active development cluster, metrics dashboard behavior |
| <!-- manual-qa: tier=live-custom-role --> `live-custom-role` | ignored custom database role test after authorization and existing gates | pre-created custom role/IAM association evidence |
| <!-- manual-qa: tier=live-mutating --> `live-mutating` | ignored DDL/DML/OCC/session-refresh test after both confirmations and existing gates | development-cluster approval and cleanup verification |
| <!-- manual-qa: tier=release --> `release` | `cargo package --locked`; `cargo deny check licenses` (CI-pinned cargo-deny 0.20.2); `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features`; `cargo test --locked --test release_readiness` | CI artifacts, clean-host installation, protected publication validation |

`docker`, every live tier, and clean-host/publication checks are deliberately
opt-in. The release tier performs local readiness only; it never runs `cargo
publish`, GitHub release commands, pushes, tags, credential changes, or resource
creation.

## Scenario procedure

Each scenario is individually identified. **Pass** means its expected result is
observed; **fail** means a mismatch or unexpected successful unsafe action;
**blocked** means a required authorization, development cluster, platform, or
tool is unavailable. Retain only redacted output, exit statuses, artifact names,
and report paths.

### Temporary evidence workspace

Before running a scenario that captures output or history, create one private
workspace in the same shell. `mktemp -d` creates the directory atomically, and
the exit trap removes its contents without relying on predictable paths.

```sh
QA_TEMP_DIR="$(mktemp -d)"
trap 'rm -rf -- "$QA_TEMP_DIR"' EXIT
```

Use the scenario-specific names below within `$QA_TEMP_DIR`. Exit that shell
after retaining any redacted evidence so the trap performs cleanup.

| ID and tier | Prerequisites/setup | Exact command or interaction | Expected result and evidence | Cleanup |
| --- | --- | --- | --- | --- |
| L-01 `local` build/help/errors | Rust toolchain; no AWS configuration needed | `cargo build --locked --release`; `./target/release/dsql --help`; `./target/release/dsql --version`; `./target/release/dsql clusters --help`; `./target/release/dsql clusters -c 'SELECT 1'` | Build and help/version succeed without AWS; invalid clusters input exits 2 before AWS. Save redacted statuses. | None. |
| L-02 `local` malformed cluster selector | Built binary; no AWS configuration | `./target/release/dsql not-a-cluster --region us-east-1 -U admin -c 'SELECT 1' >"$QA_TEMP_DIR/L-02.out" 2>"$QA_TEMP_DIR/L-02.err"; printf '%s\n' "$?"` | Exit `2`; stderr identifies an invalid cluster selector; no AWS call is made. Retain the redacted stderr and status. | Exit the workspace shell after retaining evidence. |
| L-03 `local` empty database role | Built binary; no AWS configuration | `./target/release/dsql 0123456789abcdefghijklmnop --region us-east-1 -U '' -c 'SELECT 1' >"$QA_TEMP_DIR/L-03.out" 2>"$QA_TEMP_DIR/L-03.err"; printf '%s\n' "$?"` | Exit `2`; stderr says `--username must not be empty`; no token or credential is printed. | Exit the workspace shell after retaining evidence. |
| L-04 `local` malformed explicit Region | Built binary; no AWS configuration | `./target/release/dsql 0123456789abcdefghijklmnop --region not-a-region -U admin -c 'SELECT 1' >"$QA_TEMP_DIR/L-04.out" 2>"$QA_TEMP_DIR/L-04.err"; printf '%s\n' "$?"` | Exit `2`; stderr says `--region has invalid Region syntax` before AWS work. | Exit the workspace shell after retaining evidence. |
| L-05 `local` selector/explicit Region conflict | Built binary; no AWS configuration | `./target/release/dsql arn:aws:dsql:us-east-1:123456789012:cluster/0123456789abcdefghijklmnop --region eu-west-1 -U admin -c 'SELECT 1' >"$QA_TEMP_DIR/L-05.out" 2>"$QA_TEMP_DIR/L-05.err"; printf '%s\n' "$?"` | Exit `2`; stderr says that `--region` conflicts with the Region encoded in the cluster selector. | Exit the workspace shell after retaining evidence. |
| L-06 `local` missing option values | Built binary; no AWS configuration | Run each command separately: `./target/release/dsql -c`; `./target/release/dsql -f`; `./target/release/dsql --region`; `./target/release/dsql --profile`; `./target/release/dsql --unknown`. Record `$?` after each. | Every command exits `2` and names the missing value or unknown argument; none loads AWS configuration. | None. |
| L-07 `local` Region/profile/default precedence | Checkout; no AWS configuration | `cargo test --locked clap_contract_accepts_global_options_and_clusters_subcommand`; `cargo test --locked explicit_region_wins_over_the_sdk_provider_without_prompting`; `cargo test --locked arn_and_endpoint_inferred_regions_win_over_the_sdk_provider_without_prompting`; `cargo test --locked fake_environment_and_shared_config_sdk_regions_resolve_without_prompting`; `cargo test --locked interactive_prompt_is_only_used_after_all_other_sources_are_absent` | All selected deterministic tests pass. They prove that `--profile` and `--format` parse globally, and Region resolution is `--region`, selector Region, AWS SDK configuration (environment/shared profile/default chain), then prompt. Retain statuses and test output. | None. |
| L-08 `local` harness/report | Checkout | `scripts/manual-qa.sh --dry-run --report 'manual qa report.txt'`; `scripts/manual-qa.sh --list`; `scripts/manual-qa.sh --tier unknown`; `scripts/manual-qa.sh --tier live-read-only`; `scripts/manual-qa.sh --tier live-mutating --confirm-live`; `scripts/manual-qa.sh --confirm-live authorized`. | The report contains `Tier: local` and `Result: DRY-RUN`; list/dry-run execute no checks. Each invalid invocation exits nonzero and respectively reports the invalid tier, missing authorization, missing `--confirm-live` value, or invalid non-live confirmation. | Remove the report unless retained as evidence. |
| L-09 `local` unit/integration | Checkout | `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; `cargo test --locked --all-targets --all-features` | All local gates pass; ignored external fixtures remain unrun. Retain test log. | None. |
| L-10 `local` non-live CLI behavior | Built binary | Use `-c` and `-f` with complete/multiple SQL and an incomplete SQL suffix; pipe incomplete SQL to the binary; redirect stdout/stderr separately. | Noninteractive input requires cluster selector and `-U`; SQL framing and missing file/UTF-8 errors are actionable; stdout remains machine-readable where applicable. | Remove temporary SQL file. |
| D-01 `docker` protocol/TLS | Docker available; no AWS credentials | `scripts/manual-qa.sh --tier docker` | Only the documented local PostgreSQL TLS suite runs; it validates TLS, streaming, notices, empty results, cancellation, disconnect/no replay, settings restoration, and temporary-container cleanup. Retain Docker/test log. | Test removes its container and temporary certificate directory. |
| R-01 `live-read-only` discovery/auth | Authorized IAM identity; active development cluster; set the three environment gates in [testing](testing.md) | `scripts/manual-qa.sh --tier live-read-only --confirm-live authorized` | Existing test validates discoverability, active status, admin database-role connection, and 17 CloudWatch series. `No data` samples are valid. Retain redacted log. | None. |
| R-02 `live-read-only` CLI discovery/output | R-01 prerequisites | Run `dsql clusters --region REGION --format table`, then repeat with `csv`, `tsv`, and `jsonl`; use a valid cluster selector as ID, ARN, and canonical endpoint. | Inventory sorting/fields, endpoint derivation, Region/profile handling, stdout/stderr separation, and access-denied/unavailable enrichment are actionable. Do not capture result data unnecessarily. | None. |
| R-03 `live-read-only` table output | R-01 prerequisites; set `CLUSTER_ID=$AURORA_DSQL_LIVE_CLUSTER_ID` and `REGION=$AURORA_DSQL_LIVE_REGION` in the terminal only | `dsql "$CLUSTER_ID" --region "$REGION" -U admin --format table -c "SELECT NULL::text AS null_value, ''::text AS empty_value, 'π'::text AS unicode_value;" >table.out 2>table.err` | Exit `0`; table output distinguishes NULL, empty, and Unicode values; stderr has no result rows. Retain redacted status and files. | Remove `table.out` and `table.err`. |
| R-04 `live-read-only` delimited and JSONL output | R-03 prerequisites | Run separately: `dsql "$CLUSTER_ID" --region "$REGION" -U admin --format csv -c "SELECT 'a,b' AS value;" >csv.out 2>csv.err`; replace `csv` with `tsv`; then `dsql "$CLUSTER_ID" --region "$REGION" -U admin --format jsonl -c "SELECT 1 AS value UNION ALL SELECT 2;" >jsonl.out 2>jsonl.err`. | Each exits `0`; CSV/TSV escape delimiters, JSONL has one frame per row, and result data is only on stdout. Retain redacted files and statuses. | Remove output files. |
| R-05 `live-read-only` SQL and multiple-result diagnostics | R-03 prerequisites; no mutating SQL | Run separately: `dsql "$CLUSTER_ID" --region "$REGION" -U admin --format csv -c 'SELECT 1; SELECT 2;'`; `dsql "$CLUSTER_ID" --region "$REGION" -U admin -c 'SELECT * FROM definitely_missing_dsql_qa_table;'`. | Both exit `1`; CSV reports its one-row-producing-result limitation and the invalid SQL reports the server error on stderr. Neither diagnostic includes credentials or a token. | None. |
| R-06 `live-read-only` DNS failure | R-01 authorization plus an approved isolated network/resolver test that makes the selected development cluster endpoint unresolvable; do not change shared DNS | Under that test resolver, run `dsql "$CLUSTER_ID" --region "$REGION" -U admin -c 'SELECT 1;' >"$QA_TEMP_DIR/R-06.out" 2>"$QA_TEMP_DIR/R-06.err"`. | Exit `1`; stderr identifies the cluster-endpoint connection failure without exposing credentials or a token. Retain redacted stderr and resolver-test evidence. | Restore the isolated resolver; the exit trap removes temporary files. |
| R-07 `live-read-only` TLS validation failure | R-01 authorization plus an approved isolated TLS test endpoint that presents an untrusted or expired certificate for the canonical selected endpoint; do not weaken system trust | Run `dsql "$CLUSTER_ID" --region "$REGION" -U admin -c 'SELECT 1;' >"$QA_TEMP_DIR/R-07.out" 2>"$QA_TEMP_DIR/R-07.err"` while the isolated endpoint is active. | Exit `1`; stderr says TLS verification failed and advises checking the endpoint/trusted roots; no secret appears. | Remove the isolated endpoint override; the exit trap removes temporary files. |
| R-08 `live-read-only` expired authentication/permission failure | R-01 authorization plus a pre-approved IAM test identity whose temporary credentials are expired, then separately one lacking only the required `dsql:DbConnectAdmin` permission | For each identity, run `dsql "$CLUSTER_ID" --region "$REGION" -U admin -c 'SELECT 1;' >"$QA_TEMP_DIR/R-08.out" 2>"$QA_TEMP_DIR/R-08.err"`. | Each exits `1`; the diagnostic distinguishes authentication-token generation/connection authorization and names the relevant permission without printing credential values. | Restore the normal test identity; the exit trap removes temporary files. |
| R-09 `live-read-only` interactive shell basics | R-01 prerequisites and an interactive terminal | Start `dsql "$CLUSTER_ID" --region "$REGION" -U admin`; enter, one line at a time: `\?`, `\conninfo`, `\d`, `\dt`, `\dn`, `\du`, `\x auto`, `\timing on`, `\refresh`, then `SELECT 1` followed by `;`, then `\q`. | Shell commands succeed or return documented permission-aware output; `\conninfo` redacts endpoint/account data, timing applies after enabling, `\refresh` is safe while idle, and `\q` exits `0`. Retain a redacted transcript. | Exit with `\q`. |
| R-10 `live-read-only` cancellation and EOF | R-09 prerequisites | Start the shell, type `SELECT pg_sleep(30);`, press Ctrl+C once while it runs, then at an empty prompt press Ctrl+D. | Ctrl+C cancels without replaying SQL; Ctrl+D exits only from an empty buffer. Record status/transcript without SQL text or result data. | None. |
| R-11 `live-read-only` history | R-09 prerequisites; securely create a private history file: `HISTORY_FILE="$(mktemp "${QA_TEMP_DIR}/dsql-history.XXXXXX")"` | Run `dsql "$CLUSTER_ID" --region "$REGION" -U admin --history-file "$HISTORY_FILE"`, execute a harmless statement, then `\q`; run again with `--no-history --history-file "$HISTORY_FILE"`, then `\q`. | First session uses the private history file as documented; second does not add history. Do not retain history contents. | The exit trap removes `$HISTORY_FILE`. |
| R-12 `live-read-only` pager fallback and resize | R-09 prerequisites; terminal with `less` unavailable or intentionally early-exiting on the test host | Start the shell, enter `\pager on`, run a harmless multi-row query, resize the terminal narrower and wider, then enter `\pager off` and `\q`. | Pager failure falls back cleanly; table layout follows the current terminal width; no shell command evaluation occurs. Retain only a redacted transcript. | Restore terminal size and exit shell. |
| R-13 `live-read-only` metrics dashboard success/empty data | R-01 prerequisites including `cloudwatch:GetMetricData`; terminal | Start the shell, enter `\metrics`, then `1`, `2`, `3`, `4`, `r`, and `q`. | Each range is selectable, refresh works, `q` returns to the existing shell, and missing samples display as `No data` or gaps. | Exit shell with `\q`. |
| R-14 `live-read-only` metrics permission/unavailable failure | R-01 prerequisites except use a pre-approved identity without `cloudwatch:GetMetricData` or a documented unavailable-metrics condition | Start the shell, enter `\metrics`, observe the diagnostic, then `\q`. | Metrics failure is actionable and returns to the existing database connection; it does not print credentials or a token. | Restore normal identity and exit shell. |
| R-15 `live-read-only` multiline input and transaction refresh gate | R-09 prerequisites; no mutation | Start the shell and enter `SELECT` then `1;` on the next line. Next enter `BEGIN;`, then `\refresh`, then `ROLLBACK;`, then `\q`. | The multiline statement runs only after its semicolon; `\refresh` refuses while a transaction is active and succeeds after rollback. Retain a redacted transcript and statuses. | Exit shell with `\q`; no database state is changed. |
| R-16 `live-read-only` malformed shell command | R-09 prerequisites | Start the shell, enter `\x invalid`, then `\pager invalid`, then `\q`. | Each malformed argument fails locally with shell help/validation guidance; the connection remains usable and `\q` exits safely. | Exit shell with `\q`. |
| C-01 `live-custom-role` | R-01 gates plus pre-created lowercase custom database role and IAM `dsql:DbConnect` association; set `AURORA_DSQL_LIVE_CUSTOM_ROLE` | `scripts/manual-qa.sh --tier live-custom-role --confirm-live authorized` | Connects as the custom role and validates `current_user`/`postgres`; test does not create, alter, map, or drop the role. Retain authorization evidence/log. | None. |
| M-01 `live-mutating` | Explicit development-cluster approval; all mutation gates in [testing](testing.md), including separate matching cluster ID/account confirmation | `scripts/manual-qa.sh --tier live-mutating --confirm-live authorized --confirm-mutation authorized` | Existing safeguards validate target identity, unique `dsql_cli_live_*` prefix and ownership marker; test validates DDL, OCC, row limit, refresh, then cleanup. | Verify cleanup log. If interrupted/cleanup fails, inspect only the named suite-owned table reported by the test. |
| P-01 `release` readiness | Checkout, local Rust toolchain, and the CI-pinned `cargo-deny 0.20.2` already installed; no publication credentials | `scripts/manual-qa.sh --tier release` | Cargo package dry run, `cargo deny check licenses`, rustdoc with warnings denied, and the local release-workflow/archive contract pass. The contract covers four targets, package help/version, checksums, SBOM/dependency/license/provenance workflow evidence, and publication gate. This tier never installs cargo-deny, publishes, pushes, tags, or creates a release. | Remove local `target/package` only if desired; do not use destructive cleanup commands as QA. |
| P-02 `release` CI/clean hosts | Approved CI artifacts; Linux x86_64/aarch64 and macOS x86_64/Apple Silicon clean hosts | On each host, download approved archive/evidence, run `sha256sum --check SHA256SUMS` (or `shasum -a 256 -c SHA256SUMS` on macOS), extract, then run `./dsql --version` and `./dsql --help`. | Record target, artifact checksum, SBOM/provenance locations, output, operator, and date. This is manual external evidence, not harness work. | Remove extracted test directory. |
| P-03 `release` publication | Protected maintainer approval and all previous evidence | Review the protected `workflow_dispatch` publication gate manually; do not run publication as manual QA. After approved publication, validate `cargo install dsql-cli --locked` only on a clean host. | Publication remains a separately approved action; record crates.io/GitHub release links and first-run evidence. | Uninstall only on the clean-host test environment if policy requires. |

## Platform and sign-off

Run local-safe checks on the current platform. Docker is locally executable where
Docker is available. Linux and macOS target artifacts require their respective
clean-host evidence; Windows is a documented manual compatibility observation,
not a current packaged target. Record blocked platform checks rather than
inferring success from another host or CI configuration.

| Field | Record |
| --- | --- |
| Result | Pass / fail / blocked |
| Operator and date |  |
| Host OS/architecture and terminal |  |
| CLI version and artifact identity/checksum |  |
| IAM identity class and development cluster/Region (redacted as required) |  |
| Tiers/scenario IDs run and report/evidence locations |  |
| Blockers, cleanup verification, and residual risks |  |

Residual risks normally include unrun Docker/live/mutating tiers, unavailable
clean-host coverage, and publication not yet independently verified. Never mark
these as passed without the corresponding evidence.
