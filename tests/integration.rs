//! Integration tests live here. See `src/lib.rs` for unit tests.

use rustyhip::greet;

#[test]
fn greet_library_entrypoint() {
    assert_eq!(greet("cookiecutter"), "Hello, cookiecutter!");
}

use assert_cmd::Command;

#[test]
fn bin_prints_version() {
    let mut cmd = Command::cargo_bin("rustyhip").unwrap();
    cmd.arg("--version").assert().success();
}
