use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

fn repository_path(path: impl AsRef<Path>) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn read_repository_file(path: &str) -> String {
    let path = repository_path(path);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn assert_contains_all(contents: &str, path: &str, required: &[&str]) {
    for required in required {
        assert!(
            contents.contains(required),
            "{path} must contain contract marker `{required}`"
        );
    }
}

#[cfg(unix)]
struct GuardedPath {
    directory: PathBuf,
}

#[cfg(unix)]
impl GuardedPath {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "dsql-manual-qa-contract-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create command guard directory");
        for command in ["aws", "cargo", "curl", "docker", "gh", "git", "rm"] {
            let path = directory.join(command);
            fs::write(&path, "#!/bin/sh\nexit 97\n").expect("write guarded command");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("make guarded command executable");
        }
        let dirname = directory.join("dirname");
        fs::write(&dirname, "#!/bin/sh\n/bin/dirname \"$@\"\n").expect("write dirname wrapper");
        fs::set_permissions(&dirname, fs::Permissions::from_mode(0o755))
            .expect("make dirname wrapper executable");
        Self { directory }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        let script = repository_path("scripts/manual-qa.sh");
        Command::new("/bin/bash")
            .arg(script)
            .args(arguments)
            .env_clear()
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("HOME", &self.directory)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.directory.display()),
            )
            .output()
            .expect("run manual QA script behind command guards")
    }

    fn run_from_directory(&self, directory: &Path, arguments: &[&str]) -> Output {
        let script = repository_path("scripts/manual-qa.sh");
        Command::new("/bin/bash")
            .arg(script)
            .args(arguments)
            .current_dir(directory)
            .env_clear()
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("HOME", &self.directory)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.directory.display()),
            )
            .output()
            .expect("run manual QA script from caller directory")
    }

    fn write_command(&self, command: &str, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        let path = self.directory.join(command);
        fs::write(&path, contents).expect("write guarded command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("make guarded command executable");
    }

    fn run_without_system_path(&self, arguments: &[&str]) -> Output {
        fs::remove_file(self.directory.join("docker")).expect("remove guarded Docker command");
        let script = repository_path("scripts/manual-qa.sh");
        Command::new("/bin/bash")
            .arg(script)
            .args(arguments)
            .env_clear()
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("HOME", &self.directory)
            .env("PATH", &self.directory)
            .output()
            .expect("run manual QA script without system commands")
    }
}

#[cfg(unix)]
impl Drop for GuardedPath {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(unix)]
fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn manual_qa_plan_documents_the_safe_default_and_all_tiers() {
    let plan = read_repository_file("docs/manual-qa.md");

    assert_contains_all(
        &plan,
        "docs/manual-qa.md",
        &[
            "<!-- manual-qa: safe-local-default -->",
            "<!-- manual-qa: tier=local -->",
            "<!-- manual-qa: tier=docker -->",
            "<!-- manual-qa: tier=live-read-only -->",
            "<!-- manual-qa: tier=live-custom-role -->",
            "<!-- manual-qa: tier=live-mutating -->",
            "<!-- manual-qa: tier=release -->",
            "--confirm-live",
            "--confirm-mutation",
        ],
    );
}

#[test]
fn manual_qa_plan_uses_a_secure_temporary_evidence_workspace() {
    let plan = read_repository_file("docs/manual-qa.md");

    assert_contains_all(
        &plan,
        "docs/manual-qa.md",
        &[
            "QA_TEMP_DIR=\"$(mktemp -d)\"",
            "trap 'rm -rf -- \"$QA_TEMP_DIR\"' EXIT",
            "${QA_TEMP_DIR}/dsql-history.XXXXXX",
        ],
    );
    assert!(
        !plan.contains("mktemp -u"),
        "docs/manual-qa.md must not use mktemp -u"
    );
    assert!(
        !plan.contains("/tmp/dsql-"),
        "docs/manual-qa.md must not use fixed DSQL temporary paths"
    );
}

#[test]
fn manual_qa_plan_is_discoverable_from_the_readme() {
    let readme = read_repository_file("README.md");

    assert!(
        readme.contains("docs/manual-qa.md"),
        "README.md must link to docs/manual-qa.md"
    );
}

#[cfg(unix)]
#[test]
fn manual_qa_script_has_safe_local_help_list_and_dry_run_modes() {
    let guard = GuardedPath::new();

    let help = guard.run(&["--help"]);
    assert!(
        help.status.success(),
        "--help failed: {}",
        output_text(&help)
    );
    assert_contains_all(
        &output_text(&help),
        "manual QA --help output",
        &[
            "--list",
            "--dry-run",
            "--confirm-live",
            "--confirm-mutation",
        ],
    );

    let list = guard.run(&["--list"]);
    assert!(
        list.status.success(),
        "--list failed: {}",
        output_text(&list)
    );
    assert_contains_all(
        &output_text(&list),
        "manual QA --list output",
        &[
            "local",
            "docker",
            "live-read-only",
            "live-custom-role",
            "live-mutating",
            "release",
        ],
    );

    let dry_run = guard.run(&["--dry-run"]);
    assert!(
        dry_run.status.success(),
        "the default local dry run failed: {}",
        output_text(&dry_run)
    );
    assert!(
        output_text(&dry_run).contains("local"),
        "the default dry run must select the local tier: {}",
        output_text(&dry_run)
    );
}

#[cfg(unix)]
#[test]
fn manual_qa_script_writes_reports_and_rejects_unwritable_destinations_without_checks() {
    let guard = GuardedPath::new();
    let report = guard.directory.join("manual qa report.txt");
    let report_text = report.to_string_lossy().into_owned();

    let success = guard.run(&["--dry-run", "--report", &report_text]);
    assert!(
        success.status.success(),
        "a writable report destination must succeed: {}",
        output_text(&success)
    );
    let report_contents = fs::read_to_string(&report).expect("read manual QA report");
    assert_contains_all(
        &report_contents,
        "manual QA report",
        &[
            "Tier: local",
            "Result: DRY-RUN",
            "DRY-RUN: no checks executed",
        ],
    );

    let missing_parent = guard.directory.join("missing").join("report.txt");
    let missing_parent_text = missing_parent.to_string_lossy().into_owned();
    let unwritable = guard.run(&["--dry-run", "--report", &missing_parent_text]);
    assert!(
        !unwritable.status.success(),
        "an unavailable report destination must fail: {}",
        output_text(&unwritable)
    );
    assert!(
        output_text(&unwritable).contains("report destination is not writable"),
        "an unavailable report destination must explain the failure: {}",
        output_text(&unwritable)
    );
}

#[cfg(unix)]
#[test]
fn manual_qa_script_resolves_relative_reports_from_the_callers_directory() {
    let guard = GuardedPath::new();
    let caller_directory = guard.directory.join("caller directory");
    let report_directory = caller_directory.join("reports");
    let report = report_directory.join("manual qa report.txt");
    fs::create_dir_all(&report_directory).expect("create caller report directory");
    guard.write_command(
        "cargo",
        "#!/bin/sh\ncase \"$*\" in\n  *\"clusters -c SELECT 1\"*) echo 'error: the clusters subcommand does not accept -c/--command or -f/--file' >&2; exit 2 ;;\nesac\nexit 0\n",
    );

    let output = guard.run_from_directory(
        &caller_directory,
        &["--report", "reports/manual qa report.txt"],
    );

    assert!(
        output.status.success(),
        "a report relative to the caller must succeed: {}",
        output_text(&output)
    );
    assert!(
        report.is_file(),
        "the report must be written relative to the caller directory: {}",
        report.display()
    );
    assert!(
        !repository_path("reports/manual qa report.txt").exists(),
        "the report must not be written relative to the repository root"
    );
}

#[cfg(unix)]
#[test]
fn manual_qa_script_rejects_unexpected_invalid_argument_exit_statuses() {
    let guard = GuardedPath::new();
    guard.write_command(
        "cargo",
        "#!/bin/sh\ncase \"$*\" in\n  *\"clusters -c SELECT 1\"*) exit 97 ;;\nesac\nexit 0\n",
    );

    let output = guard.run(&[]);

    assert!(
        !output.status.success(),
        "an unrelated cargo failure must fail manual QA: {}",
        output_text(&output)
    );
    assert!(
        output_text(&output).contains("expected CLI exit status 2, got 97"),
        "an unexpected status must include a diagnostic: {}",
        output_text(&output)
    );
}

#[cfg(unix)]
#[test]
fn manual_qa_script_rejects_an_unrelated_error_with_the_expected_status() {
    let guard = GuardedPath::new();
    guard.write_command(
        "cargo",
        "#!/bin/sh\ncase \"$*\" in\n  *\"clusters -c SELECT 1\"*) echo 'unrelated failure' >&2; exit 2 ;;\nesac\nexit 0\n",
    );

    let output = guard.run(&[]);

    assert!(
        !output.status.success(),
        "an unrelated status-2 error must fail manual QA: {}",
        output_text(&output)
    );
    assert!(
        output_text(&output).contains("did not emit the expected invalid-argument diagnostic"),
        "a mismatched diagnostic must explain the failure: {}",
        output_text(&output)
    );
}

#[cfg(unix)]
#[test]
fn manual_qa_script_labels_the_expected_invalid_argument_rejection_as_a_pass() {
    let guard = GuardedPath::new();
    guard.write_command(
        "cargo",
        "#!/bin/sh\ncase \"$*\" in\n  *\"clusters -c SELECT 1\"*) echo 'error: the clusters subcommand does not accept -c/--command or -f/--file' >&2; exit 2 ;;\nesac\nexit 0\n",
    );

    let output = guard.run(&[]);
    let text = output_text(&output);

    assert!(output.status.success(), "manual QA failed: {text}");
    assert!(
        text.contains("Observed expected invalid-argument rejection"),
        "the expected rejection must be labelled as successful: {text}"
    );
    assert!(
        !text.contains("error: the clusters subcommand"),
        "a successful negative check must not print a raw error: {text}"
    );
}

#[cfg(unix)]
#[test]
fn manual_qa_script_rejects_unsafe_or_invalid_input_before_running_a_tier() {
    let guard = GuardedPath::new();
    let cases = [
        (vec!["--unknown"], "unknown option"),
        (vec!["--tier"], "missing value"),
        (
            vec!["--tier", "live-mutating", "--confirm-live"],
            "missing value for --confirm-live",
        ),
        (vec!["--tier", "unknown"], "unknown tier"),
        (vec!["--tier", "live-read-only"], "--confirm-live"),
        (
            vec![
                "--tier",
                "live-read-only",
                "--confirm-live",
                "not-confirmed",
            ],
            "invalid confirmation",
        ),
        (
            vec![
                "--tier",
                "live-mutating",
                "--confirm-live",
                "not-confirmed",
                "--confirm-mutation",
                "not-confirmed",
            ],
            "invalid confirmation",
        ),
        (
            vec!["--confirm-live", "authorized"],
            "--confirm-live is valid only for live tiers",
        ),
        (
            vec!["--tier", "docker", "--confirm-mutation", "authorized"],
            "--confirm-mutation is valid only for live-mutating",
        ),
        (
            vec!["--tier", "release", "--confirm-live", "authorized"],
            "--confirm-live is valid only for live tiers",
        ),
    ];

    for (arguments, diagnostic) in cases {
        let output = guard.run(&arguments);
        assert!(
            !output.status.success(),
            "{arguments:?} must fail before a tier runs"
        );
        assert!(
            output_text(&output).contains(diagnostic),
            "{arguments:?} must report `{diagnostic}`: {}",
            output_text(&output)
        );
    }
}

#[cfg(unix)]
#[test]
fn manual_qa_script_checks_tier_prerequisites_before_running_commands() {
    let guard = GuardedPath::new();
    let cases = [
        (
            vec!["--tier", "live-read-only", "--confirm-live", "authorized"],
            "missing required live-test environment gate: AURORA_DSQL_LIVE_TEST",
        ),
        (
            vec!["--tier", "live-custom-role", "--confirm-live", "authorized"],
            "missing required live-test environment gate: AURORA_DSQL_LIVE_TEST",
        ),
        (
            vec![
                "--tier",
                "live-mutating",
                "--confirm-live",
                "authorized",
                "--confirm-mutation",
                "authorized",
            ],
            "missing required live-test environment gate: AURORA_DSQL_LIVE_TEST",
        ),
    ];

    for (arguments, diagnostic) in cases {
        let output = guard.run(&arguments);
        assert!(
            !output.status.success(),
            "{arguments:?} must fail before it can invoke a tier command"
        );
        assert!(
            output_text(&output).contains(diagnostic),
            "{arguments:?} must report `{diagnostic}`: {}",
            output_text(&output)
        );
        assert!(
            !output_text(&output).contains("Running "),
            "{arguments:?} must not start a check: {}",
            output_text(&output)
        );
    }

    let docker = guard.run_without_system_path(&["--tier", "docker"]);
    assert!(
        !docker.status.success(),
        "docker must fail when it is unavailable: {}",
        output_text(&docker)
    );
    assert!(
        output_text(&docker).contains("docker tier requires Docker"),
        "docker prerequisite failure must be actionable: {}",
        output_text(&docker)
    );
    assert!(
        !output_text(&docker).contains("Running "),
        "docker prerequisite failure must not start a check: {}",
        output_text(&docker)
    );
}

#[cfg(unix)]
#[test]
fn manual_qa_script_is_executable() {
    use std::os::unix::fs::PermissionsExt;

    let path = repository_path("scripts/manual-qa.sh");
    let metadata =
        fs::metadata(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    assert_ne!(
        metadata.permissions().mode() & 0o111,
        0,
        "{} must be executable",
        path.display()
    );
}
