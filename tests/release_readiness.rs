use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_yaml::Value;

fn repository_path(path: impl AsRef<Path>) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn read_repository_file(path: impl AsRef<Path>) -> String {
    let path = repository_path(path);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn cargo_package_metadata_is_ready_for_v1_publish() {
    let manifest = read_repository_file("Cargo.toml");

    for required in [
        "name = \"dsql-cli\"",
        "version = \"1.0.0\"",
        "rust-version = \"1.94\"",
        "license = \"Apache-2.0\"",
        "description = ",
        "name = \"dsql\"",
        "path = \"src/main.rs\"",
    ] {
        assert!(
            manifest.contains(required),
            "Cargo.toml must contain release metadata `{required}`"
        );
    }
}

#[test]
fn v1_user_documentation_is_present() {
    let required = [
        "README.md",
        "docs/cli.md",
        "docs/iam.md",
        "docs/output-formats.md",
        "docs/security.md",
        "docs/troubleshooting.md",
    ];
    let missing = required
        .into_iter()
        .filter(|path| !repository_path(path).is_file())
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "v1 documentation is incomplete; missing: {}",
        missing.join(", ")
    );
}

fn release_workflow() -> PathBuf {
    let workflows = repository_path(".github/workflows");
    let entries = fs::read_dir(&workflows)
        .unwrap_or_else(|error| panic!("read {}: {error}", workflows.display()));
    let candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("yml" | "yaml")
                )
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.contains("release"))
        })
        .collect::<Vec<_>>();

    assert_eq!(
        candidates.len(),
        1,
        "expected exactly one release workflow in {}; found: {:?}",
        workflows.display(),
        candidates
    );
    candidates.into_iter().next().expect("one release workflow")
}

fn release_workflow_document() -> Value {
    let workflow = release_workflow();
    let contents = fs::read_to_string(&workflow)
        .unwrap_or_else(|error| panic!("read {}: {error}", workflow.display()));
    serde_yaml::from_str(&contents)
        .unwrap_or_else(|error| panic!("parse {} as YAML: {error}", workflow.display()))
}

fn mapping<'a>(value: &'a Value, context: &str) -> &'a serde_yaml::Mapping {
    value
        .as_mapping()
        .unwrap_or_else(|| panic!("{context} must be a YAML mapping"))
}

fn field<'a>(mapping: &'a serde_yaml::Mapping, name: &str, context: &str) -> &'a Value {
    mapping
        .get(Value::String(name.to_owned()))
        .unwrap_or_else(|| panic!("{context} must define `{name}`"))
}

fn text<'a>(value: &'a Value, context: &str) -> &'a str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{context} must be a string"))
}

fn sequence<'a>(value: &'a Value, context: &str) -> &'a [Value] {
    value
        .as_sequence()
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("{context} must be a YAML sequence"))
}

fn job<'a>(workflow: &'a Value, name: &str) -> &'a serde_yaml::Mapping {
    let root = mapping(workflow, "release workflow");
    let jobs = mapping(
        field(root, "jobs", "release workflow"),
        "release workflow jobs",
    );
    mapping(
        field(jobs, name, "release workflow jobs"),
        &format!("release workflow job `{name}`"),
    )
}

fn step_with_name<'a>(steps: &'a [Value], name: &str) -> &'a serde_yaml::Mapping {
    steps
        .iter()
        .map(|step| mapping(step, "workflow step"))
        .find(|step| {
            step.get(Value::String("name".to_owned()))
                .and_then(Value::as_str)
                == Some(name)
        })
        .unwrap_or_else(|| panic!("workflow must contain step `{name}`"))
}

fn step_run<'a>(step: &'a serde_yaml::Mapping, name: &str) -> &'a str {
    text(field(step, "run", &format!("workflow step `{name}`")), name)
}

#[test]
fn release_workflow_defines_and_validates_all_four_artifact_contracts() {
    let workflow = release_workflow_document();
    let package = job(&workflow, "package");
    let strategy = mapping(
        field(package, "strategy", "package job"),
        "package strategy",
    );
    let matrix = mapping(
        field(strategy, "matrix", "package strategy"),
        "package matrix",
    );
    let targets = sequence(
        field(matrix, "include", "package matrix"),
        "package matrix include",
    );

    let expected = [
        ("ubuntu-24.04", "x86_64-unknown-linux-musl"),
        ("ubuntu-24.04-arm", "aarch64-unknown-linux-musl"),
        ("macos-13", "x86_64-apple-darwin"),
        ("macos-14", "aarch64-apple-darwin"),
    ];
    assert_eq!(
        targets.len(),
        expected.len(),
        "release matrix must have four targets"
    );
    for (runner, target) in expected {
        let entry = targets
            .iter()
            .map(|entry| mapping(entry, "package matrix entry"))
            .find(|entry| {
                entry
                    .get(Value::String("target".to_owned()))
                    .and_then(Value::as_str)
                    == Some(target)
            });
        let entry = entry.unwrap_or_else(|| panic!("release matrix must define `{target}`"));
        assert_eq!(
            text(
                field(entry, "runner", "package matrix entry"),
                "package runner"
            ),
            runner,
            "`{target}` must use its required native runner"
        );
        assert_eq!(
            text(
                field(entry, "archive", "package matrix entry"),
                "package archive"
            ),
            format!("dsql-{target}.tar.gz"),
            "`{target}` archive name must identify its target"
        );
    }

    let steps = sequence(field(package, "steps", "package job"), "package steps");
    let build = step_run(
        step_with_name(steps, "Build release binary"),
        "Build release binary",
    );
    assert!(build.contains("cargo build --locked --release --target ${{ matrix.target }}"));

    let archive = step_run(
        step_with_name(steps, "Archive binary and required notices"),
        "Archive binary and required notices",
    );
    for required in [
        "cp target/${{ matrix.target }}/release/dsql dist/package/dsql",
        "cp LICENSE README.md dist/package/",
        "tar -C dist/package -czf dist/${{ matrix.archive }} dsql LICENSE README.md",
    ] {
        assert!(
            archive.contains(required),
            "archive step must execute `{required}`"
        );
    }

    let smoke = step_run(
        step_with_name(steps, "Smoke packaged artifact"),
        "Smoke packaged artifact",
    );
    for required in [
        "tar -C dist/smoke -xzf dist/${{ matrix.archive }}",
        "dist/smoke/dsql --help",
        "dist/smoke/dsql --version",
    ] {
        assert!(
            smoke.contains(required),
            "smoke step must execute `{required}`"
        );
    }
}

#[test]
fn release_workflow_covers_checksums_supply_chain_evidence_provenance_and_publication_gate() {
    let workflow = release_workflow_document();
    let evidence = job(&workflow, "release-evidence");
    assert_eq!(
        text(
            field(evidence, "needs", "release-evidence job"),
            "release-evidence needs"
        ),
        "package"
    );
    let steps = sequence(
        field(evidence, "steps", "release-evidence job"),
        "release-evidence steps",
    );

    let report = step_run(
        step_with_name(steps, "Create checksums and dependency/license report"),
        "Create checksums and dependency/license report",
    );
    for required in [
        "sha256sum dist/artifacts/*.tar.gz > dist/SHA256SUMS",
        "cargo tree --locked --edges normal > dist/DEPENDENCIES.txt",
        "cargo install cargo-deny --version 0.20.2 --locked",
        "cargo deny check licenses",
        "cargo deny list --format json > dist/LICENSES.json",
    ] {
        assert!(
            report.contains(required),
            "evidence step must execute `{required}`"
        );
    }
    assert!(
        !report.contains("cargo deny check licenses --format json"),
        "license evidence must not use cargo-deny's unsupported `check licenses --format json` interface"
    );

    let sbom = steps
        .iter()
        .map(|step| mapping(step, "workflow step"))
        .find(|step| {
            step.get(Value::String("uses".to_owned()))
                .and_then(Value::as_str)
                == Some("anchore/sbom-action@v0")
        })
        .expect("release evidence must generate an SBOM");
    let sbom_inputs = mapping(field(sbom, "with", "SBOM step"), "SBOM inputs");
    assert_eq!(
        text(
            field(sbom_inputs, "output-file", "SBOM inputs"),
            "SBOM output"
        ),
        "dist/sbom.spdx.json"
    );

    let evidence_upload = steps
        .iter()
        .map(|step| mapping(step, "workflow step"))
        .find(|step| {
            step.get(Value::String("uses".to_owned()))
                .and_then(Value::as_str)
                == Some("actions/upload-artifact@v4")
                && step
                    .get(Value::String("with".to_owned()))
                    .and_then(Value::as_mapping)
                    .and_then(|inputs| inputs.get(Value::String("name".to_owned())))
                    .and_then(Value::as_str)
                    == Some("release-evidence")
        })
        .expect("release evidence must be uploaded");
    let upload_inputs = mapping(
        field(evidence_upload, "with", "evidence upload"),
        "evidence upload inputs",
    );
    let uploaded_paths = text(
        field(upload_inputs, "path", "evidence upload inputs"),
        "evidence upload paths",
    );
    for required in [
        "dist/SHA256SUMS",
        "dist/DEPENDENCIES.txt",
        "dist/LICENSES.json",
        "dist/sbom.spdx.json",
    ] {
        assert!(
            uploaded_paths.contains(required),
            "evidence upload must include `{required}`"
        );
    }

    let attestation = steps
        .iter()
        .map(|step| mapping(step, "workflow step"))
        .find(|step| {
            step.get(Value::String("uses".to_owned()))
                .and_then(Value::as_str)
                == Some("actions/attest-build-provenance@v2")
        })
        .expect("release evidence must attest build provenance");
    let attestation_inputs = mapping(
        field(attestation, "with", "attestation step"),
        "attestation inputs",
    );
    let subjects = text(
        field(attestation_inputs, "subject-path", "attestation inputs"),
        "provenance subjects",
    );
    for required in [
        "dist/artifacts/*.tar.gz",
        "dist/SHA256SUMS",
        "dist/sbom.spdx.json",
    ] {
        assert!(
            subjects.contains(required),
            "provenance must cover `{required}`"
        );
    }

    let publish = job(&workflow, "publish");
    let dependencies = sequence(field(publish, "needs", "publish job"), "publish needs");
    let dependency_names = dependencies
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert_eq!(dependency_names, ["release-evidence", "crates-io-dry-run"]);
    assert_eq!(
        text(field(publish, "if", "publish job"), "publish condition"),
        "github.event_name == 'workflow_dispatch' && inputs.publish",
        "publication must require an explicit dispatch approval"
    );

    let dry_run = job(&workflow, "crates-io-dry-run");
    let dry_steps = sequence(
        field(dry_run, "steps", "crates.io dry-run job"),
        "crates.io dry-run steps",
    );
    assert!(
        dry_steps
            .iter()
            .map(|step| mapping(step, "workflow step"))
            .any(|step| {
                step.get(Value::String("run".to_owned()))
                    .and_then(Value::as_str)
                    == Some("cargo package --locked")
            })
    );

    let publish_steps = sequence(field(publish, "steps", "publish job"), "publish steps");
    let publish = step_with_name(
        publish_steps,
        "Publish crates.io package after protected approval",
    );
    let publish_environment = mapping(
        field(publish, "env", "crates.io publish step"),
        "crates.io publish environment",
    );
    assert_eq!(
        text(
            field(
                publish_environment,
                "CARGO_REGISTRY_TOKEN",
                "crates.io publish environment",
            ),
            "${{ secrets.CRATES_IO_API_KEY }}"
        ),
        "${{ secrets.CRATES_IO_API_KEY }}",
        "the manual publish job must use the existing CRATES_IO_API_KEY secret"
    );
}

#[test]
fn native_release_archive_contains_required_files_and_smokes() {
    let binary = PathBuf::from(
        std::env::var("CARGO_BIN_EXE_dsql").expect("Cargo must provide the dsql test binary"),
    );
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        (os, arch) => panic!("unsupported release-contract host `{arch}-{os}`"),
    };
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "dsql-release-contract-{}-{unique}",
        std::process::id()
    ));
    let package = root.join("package");
    let smoke = root.join("smoke");
    fs::create_dir_all(&package).expect("create package directory");
    fs::copy(&binary, package.join("dsql")).expect("copy dsql into package");
    fs::copy(repository_path("LICENSE"), package.join("LICENSE"))
        .expect("copy license into package");
    fs::copy(repository_path("README.md"), package.join("README.md"))
        .expect("copy readme into package");
    let archive = root.join(format!("dsql-{target}.tar.gz"));

    let archive_status = Command::new("tar")
        .args(["-C", package.to_str().expect("UTF-8 package path"), "-czf"])
        .arg(&archive)
        .args(["dsql", "LICENSE", "README.md"])
        .status()
        .expect("run tar to create native archive");
    assert!(
        archive_status.success(),
        "tar must create native release archive"
    );

    let archive_listing = Command::new("tar")
        .args(["-tzf"])
        .arg(&archive)
        .output()
        .expect("list native archive");
    assert!(archive_listing.status.success());
    assert_eq!(
        String::from_utf8(archive_listing.stdout).expect("archive listing UTF-8"),
        "dsql\nLICENSE\nREADME.md\n"
    );

    fs::create_dir_all(&smoke).expect("create smoke directory");
    let extract_status = Command::new("tar")
        .args(["-C", smoke.to_str().expect("UTF-8 smoke path"), "-xzf"])
        .arg(&archive)
        .status()
        .expect("extract native archive");
    assert!(
        extract_status.success(),
        "tar must extract native release archive"
    );
    for argument in ["--help", "--version"] {
        let output = Command::new(smoke.join("dsql"))
            .arg(argument)
            .output()
            .unwrap_or_else(|error| panic!("run packaged dsql {argument}: {error}"));
        assert!(
            output.status.success(),
            "packaged dsql {argument} must succeed"
        );
        assert!(
            !output.stdout.is_empty(),
            "packaged dsql {argument} must print output"
        );
    }

    let sums = Command::new("sha256sum")
        .arg(&archive)
        .output()
        .expect("run sha256sum for native archive");
    assert!(sums.status.success(), "sha256sum must cover native archive");
    assert!(
        String::from_utf8(sums.stdout)
            .expect("checksum UTF-8")
            .contains(
                archive
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("archive file name")
            ),
        "checksum output must name the native archive"
    );

    fs::remove_dir_all(root).expect("remove release-contract temporary directory");
}
