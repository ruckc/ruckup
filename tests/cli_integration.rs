use assert_cmd::Command;
use predicates::prelude::*;

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
    cmd.current_dir(temp.path()).args(["list", "--only", "npm"]);

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
