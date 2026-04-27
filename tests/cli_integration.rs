use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn list_detects_docker_manifests() {
    let temp = tempfile::tempdir().expect("failed to create temp dir");
    std::fs::write(
        temp.path().join("Dockerfile.dev"),
        "FROM node:20-alpine AS ui\n",
    )
    .expect("failed to write Dockerfile.dev");
    std::fs::write(
        temp.path().join("compose.yml"),
        "services:\n  db:\n    image: postgres:16.4\n",
    )
    .expect("failed to write compose.yml");

    let mut cmd = Command::cargo_bin("ruckup").expect("failed to load ruckup binary");
    cmd.current_dir(temp.path())
        .args(["list", "--only", "docker"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains(
            "docker (Dockerfile.dev, compose.yml)",
        ))
        .stdout(predicate::str::contains("node"))
        .stdout(predicate::str::contains("postgres"));
}

#[test]
fn list_detects_requirements_txt() {
    let temp = tempfile::tempdir().expect("failed to create temp dir");
    std::fs::write(
        temp.path().join("requirements.txt"),
        "requests>=2.31\npytest==8.3.5\n",
    )
    .expect("failed to write requirements.txt");

    let mut cmd = Command::cargo_bin("ruckup").expect("failed to load ruckup binary");
    cmd.current_dir(temp.path())
        .args(["list", "--only", "requirements"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("requirements.txt"))
        .stdout(predicate::str::contains("requests"))
        .stdout(predicate::str::contains("pytest"));
}

#[test]
fn default_check_in_empty_directory_reports_no_supported_files() {
    let temp = tempfile::tempdir().expect("failed to create temp dir");

    let mut cmd = Command::cargo_bin("ruckup").expect("failed to load ruckup binary");
    cmd.current_dir(temp.path());

    cmd.assert().success().stdout(predicate::str::contains(
        "No supported dependency files detected in the current directory.",
    ));
}

#[test]
fn check_with_only_and_filter_in_empty_directory_is_graceful() {
    let temp = tempfile::tempdir().expect("failed to create temp dir");

    let mut cmd = Command::cargo_bin("ruckup").expect("failed to load ruckup binary");
    cmd.current_dir(temp.path())
        .args(["check", "--only", "cargo", "--filter", "serde"]);

    cmd.assert().success().stdout(predicate::str::contains(
        "No supported dependency files detected in the current directory.",
    ));
}

#[test]
fn list_with_only_option_in_empty_directory_is_graceful() {
    let temp = tempfile::tempdir().expect("failed to create temp dir");

    let mut cmd = Command::cargo_bin("ruckup").expect("failed to load ruckup binary");
    cmd.current_dir(temp.path())
        .args(["list", "--only", "requirements"]);

    cmd.assert().success().stdout(predicate::str::contains(
        "No supported dependency files detected in the current directory.",
    ));
}

#[test]
fn update_all_in_empty_directory_is_graceful() {
    let temp = tempfile::tempdir().expect("failed to create temp dir");

    let mut cmd = Command::cargo_bin("ruckup").expect("failed to load ruckup binary");
    cmd.current_dir(temp.path()).args(["update", "--all"]);

    cmd.assert().success().stdout(predicate::str::contains(
        "No supported dependency files detected in the current directory.",
    ));
}

#[test]
fn report_in_empty_directory_is_graceful() {
    let temp = tempfile::tempdir().expect("failed to create temp dir");

    let mut cmd = Command::cargo_bin("ruckup").expect("failed to load ruckup binary");
    cmd.current_dir(temp.path()).args(["report"]);

    cmd.assert().success().stdout(predicate::str::contains(
        "No supported dependency files detected in the current directory.",
    ));
}
