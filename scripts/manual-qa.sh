#!/usr/bin/env bash
# Safe-by-default manual QA harness for Aurora DSQL CLI.
set -euo pipefail

readonly CONFIRMATION_LITERAL="authorized"
readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly REPOSITORY_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
readonly CALLER_WORKING_DIRECTORY="$(pwd -P)"
readonly EXPECTED_INVALID_ARGUMENT_EXIT_STATUS=2
readonly EXPECTED_INVALID_ARGUMENT_DIAGNOSTIC="the clusters subcommand does not accept -c/--command or -f/--file"

tier="local"
tier_explicit=0
list_only=0
dry_run=0
report_path=""
confirm_live=""
confirm_mutation=""

usage() {
    cat <<'EOF'
Usage: scripts/manual-qa.sh [OPTIONS]

Run Aurora DSQL CLI manual-QA harness checks. The default is the local tier.
It does not access AWS, Docker, a cluster endpoint, credentials, or external
resources, and it never mutates a database or publishes an artifact.

Options:
  --help                         Show this help and exit.
  --list                         List all tiers and checks without running them.
  --dry-run                      Print selected checks without running them.
  --tier NAME                    Select: local, docker, live-read-only,
                                 live-custom-role, live-mutating, or release.
  --report PATH                  Write a human-readable, secret-free report.
  --confirm-live authorized      Required for every live tier.
  --confirm-mutation authorized  Also required for live-mutating.

Examples:
  scripts/manual-qa.sh
  scripts/manual-qa.sh --dry-run --tier docker
  scripts/manual-qa.sh --tier live-read-only --confirm-live authorized
  scripts/manual-qa.sh --tier live-mutating --confirm-live authorized \
    --confirm-mutation authorized --report manual-qa-report.txt

The docker and live tiers are opt-in. Live tiers additionally require the
environment gates in docs/testing.md. The release tier performs local readiness
checks only; it does not publish, push, create releases, or install on a clean host.
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

while (($#)); do
    case "$1" in
        --help)
            usage
            exit 0
            ;;
        --list)
            list_only=1
            ;;
        --dry-run)
            dry_run=1
            ;;
         --tier|--report|--confirm-live|--confirm-mutation)
             option="$1"
             if (($# < 2)); then
                 die "missing value for ${option}"
             fi
            value="$2"
            case "$option" in
                --tier)
                    tier="$value"
                    tier_explicit=1
                    ;;
                --report) report_path="$value" ;;
                --confirm-live) confirm_live="$value" ;;
                --confirm-mutation) confirm_mutation="$value" ;;
            esac
            shift
            ;;
        *) die "unknown option: $1" ;;
    esac
    shift
done

case "$tier" in
    local|docker|live-read-only|live-custom-role|live-mutating|release) ;;
    *) die "unknown tier: ${tier}" ;;
esac

if [[ "$tier" != "local" && $tier_explicit -ne 1 ]]; then
    die "non-local tiers require an explicit --tier selection"
fi

case "$tier" in
    live-read-only|live-custom-role)
        [[ "$confirm_live" == "$CONFIRMATION_LITERAL" ]] || {
            [[ -n "$confirm_live" ]] && die "invalid confirmation for --confirm-live; use '${CONFIRMATION_LITERAL}'"
            die "${tier} requires --confirm-live ${CONFIRMATION_LITERAL}"
        }
        ;;
    live-mutating)
        [[ -z "$confirm_live" || "$confirm_live" == "$CONFIRMATION_LITERAL" ]] \
            || die "invalid confirmation for --confirm-live; use '${CONFIRMATION_LITERAL}'"
        [[ "$confirm_mutation" == "$CONFIRMATION_LITERAL" ]] || {
            [[ -n "$confirm_mutation" ]] && die "invalid confirmation for --confirm-mutation; use '${CONFIRMATION_LITERAL}'"
            die "live-mutating requires --confirm-mutation ${CONFIRMATION_LITERAL}"
        }
        [[ "$confirm_live" == "$CONFIRMATION_LITERAL" ]] \
            || die "live-mutating requires --confirm-live ${CONFIRMATION_LITERAL}"
        ;;
    *)
        [[ -z "$confirm_live" ]] || die "--confirm-live is valid only for live tiers"
        ;;
esac

if [[ "$tier" != "live-mutating" && -n "$confirm_mutation" ]]; then
    die "--confirm-mutation is valid only for live-mutating"
fi

if [[ -n "$report_path" ]]; then
    if [[ "$report_path" != /* ]]; then
        report_path="${CALLER_WORKING_DIRECTORY}/${report_path}"
    fi
    report_directory="$(dirname -- "$report_path")"
    [[ -d "$report_directory" && -w "$report_directory" ]] \
        || die "report destination is not writable: ${report_path}"
    : >"$report_path" || die "report destination is not writable: ${report_path}"
fi

print_all_checks() {
    cat <<'EOF'
local
  local-quality-gates: cargo fmt --all -- --check; cargo clippy --locked --all-targets --all-features -- -D warnings; cargo test --locked --all-targets --all-features
  local-cli-smoke: cargo run --locked -- --help; cargo run --locked -- --version; cargo run --locked -- clusters --help; cargo run --locked -- clusters -c 'SELECT 1'
docker
  docker-local-tls: cargo test --locked --all-targets --all-features local_tls_postgres -- --ignored
live-read-only
  live-read-only: cargo test --locked live_dsql_read_only_discovery_admin_and_metrics -- --ignored
live-custom-role
  live-custom-role: cargo test --locked live_dsql_custom_role_authentication -- --ignored
live-mutating
  live-mutating: cargo test --locked live_dsql_mutating_occ_ddl_reconnect_and_limits -- --ignored
release
  release-package: cargo package --locked
  release-license: cargo deny check licenses (requires the CI-pinned cargo-deny 0.20.2)
  release-documentation: RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features
  release-contract: cargo test --locked --test release_readiness
EOF
}

if ((list_only)); then
    print_all_checks
    exit 0
fi

declare -a check_ids=()
declare -a check_commands=()
case "$tier" in
    local)
        check_ids=(local-format local-clippy local-tests local-help local-version local-subcommand-help local-invalid-arguments)
        check_commands=(
            'cargo fmt --all -- --check'
            'cargo clippy --locked --all-targets --all-features -- -D warnings'
            'cargo test --locked --all-targets --all-features'
            'cargo run --locked -- --help'
            'cargo run --locked -- --version'
            'cargo run --locked -- clusters --help'
            "cargo run --locked -- clusters -c 'SELECT 1'"
        )
        ;;
    docker)
        check_ids=(docker-local-tls)
        check_commands=('cargo test --locked --all-targets --all-features local_tls_postgres -- --ignored')
        ;;
    live-read-only)
        check_ids=(live-read-only)
        check_commands=('cargo test --locked live_dsql_read_only_discovery_admin_and_metrics -- --ignored')
        ;;
    live-custom-role)
        check_ids=(live-custom-role)
        check_commands=('cargo test --locked live_dsql_custom_role_authentication -- --ignored')
        ;;
    live-mutating)
        check_ids=(live-mutating)
        check_commands=('cargo test --locked live_dsql_mutating_occ_ddl_reconnect_and_limits -- --ignored')
        ;;
    release)
        check_ids=(release-package release-license release-documentation release-contract)
        check_commands=(
            'cargo package --locked'
            'cargo deny check licenses (requires cargo-deny 0.20.2)'
            "RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features"
            'cargo test --locked --test release_readiness'
        )
        ;;
esac

write_report() {
    [[ -n "$report_path" ]] || return 0
    {
        printf 'Aurora DSQL CLI manual QA report\n'
        printf 'Tier: %s\n' "$tier"
        printf 'Repository: %s\n' "$REPOSITORY_ROOT"
        printf 'Result: %s\n\n' "$1"
        printf 'Check results (commands are intentionally free of credential values):\n'
        for result in "${results[@]}"; do
            printf '%s\n' "$result"
        done
    } >"$report_path"
}

if ((dry_run)); then
    printf 'Dry run: selected tier %s\n' "$tier"
    for index in "${!check_ids[@]}"; do
        printf '%s: %s\n' "${check_ids[index]}" "${check_commands[index]}"
    done
    results=("DRY-RUN: no checks executed")
    write_report "DRY-RUN"
    exit 0
fi

require_environment() {
    for variable in "$@"; do
        [[ -n "${!variable:-}" ]] || die "missing required live-test environment gate: ${variable}; see docs/testing.md"
    done
}

case "$tier" in
    docker)
        command -v docker >/dev/null 2>&1 || die "docker tier requires Docker; see docs/testing.md"
        ;;
    live-read-only)
        require_environment AURORA_DSQL_LIVE_TEST AURORA_DSQL_LIVE_CLUSTER_ID AURORA_DSQL_LIVE_REGION
        ;;
    live-custom-role)
        require_environment AURORA_DSQL_LIVE_TEST AURORA_DSQL_LIVE_CLUSTER_ID AURORA_DSQL_LIVE_REGION AURORA_DSQL_LIVE_CUSTOM_ROLE
        ;;
    live-mutating)
        require_environment AURORA_DSQL_LIVE_TEST AURORA_DSQL_LIVE_CLUSTER_ID AURORA_DSQL_LIVE_REGION AURORA_DSQL_LIVE_MUTATING AURORA_DSQL_LIVE_MUTATING_CLUSTER_ID AURORA_DSQL_LIVE_ACCOUNT_ID
        ;;
esac

run_check() {
    local check_id="$1"
    case "$check_id" in
        local-invalid-arguments)
            local actual_exit_status
            local output
            if output="$(cargo run --locked -- clusters -c 'SELECT 1' 2>&1)"; then
                printf 'error: %s expected CLI exit status %s, got 0\n' \
                    "$check_id" "$EXPECTED_INVALID_ARGUMENT_EXIT_STATUS" >&2
                return 1
            else
                actual_exit_status=$?
            fi
            if ((actual_exit_status != EXPECTED_INVALID_ARGUMENT_EXIT_STATUS)); then
                printf 'error: %s expected CLI exit status %s, got %s\n' \
                    "$check_id" "$EXPECTED_INVALID_ARGUMENT_EXIT_STATUS" "$actual_exit_status" >&2
                printf '%s\n' "$output" >&2
                return 1
            fi
            if [[ "$output" != *"$EXPECTED_INVALID_ARGUMENT_DIAGNOSTIC"* ]]; then
                printf 'error: %s did not emit the expected invalid-argument diagnostic\n' \
                    "$check_id" >&2
                printf '%s\n' "$output" >&2
                return 1
            fi
            printf 'Observed expected invalid-argument rejection (exit %s)\n' \
                "$actual_exit_status"
            ;;
        *)
            case "$check_id" in
                local-format) cargo fmt --all -- --check ;;
                local-clippy) cargo clippy --locked --all-targets --all-features -- -D warnings ;;
                local-tests) cargo test --locked --all-targets --all-features ;;
                local-help) cargo run --locked -- --help ;;
                local-version) cargo run --locked -- --version ;;
                local-subcommand-help) cargo run --locked -- clusters --help ;;
                docker-local-tls) cargo test --locked --all-targets --all-features local_tls_postgres -- --ignored ;;
                live-read-only) cargo test --locked live_dsql_read_only_discovery_admin_and_metrics -- --ignored ;;
                live-custom-role) cargo test --locked live_dsql_custom_role_authentication -- --ignored ;;
                live-mutating) cargo test --locked live_dsql_mutating_occ_ddl_reconnect_and_limits -- --ignored ;;
                release-package) cargo package --locked ;;
                release-license) cargo deny check licenses ;;
                release-documentation) RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features ;;
                release-contract) cargo test --locked --test release_readiness ;;
            esac
            ;;
    esac
}

results=()
failed=0
cd -- "$REPOSITORY_ROOT"
for index in "${!check_ids[@]}"; do
    check_id="${check_ids[index]}"
    command_text="${check_commands[index]}"
    printf 'Running %s\n' "$check_id"
    if run_check "$check_id"; then
        results+=("PASS ${check_id}: ${command_text}")
    else
        results+=("FAIL ${check_id}: ${command_text}")
        failed=1
    fi
done

if ((failed)); then
    write_report "FAIL"
    exit 1
fi
write_report "PASS"
