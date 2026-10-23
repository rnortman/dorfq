//! Runs the built `dorfq` binary. Bazel supplies its path through
//! `DORFQ_BIN`; this test does not build under `cargo test`.

use std::process::Command;

#[test]
fn version_flag_prints_name_and_version() {
    let output = Command::new(env!("DORFQ_BIN"))
        .arg("--version")
        .output()
        .expect("spawn dorfq");
    assert!(output.status.success(), "dorfq --version: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let expected = format!("dorfq {}", env!("CARGO_PKG_VERSION"));
    assert!(
        stdout.starts_with(&expected),
        "expected {expected:?}, got {stdout:?}"
    );
}

#[test]
fn no_arguments_prints_name_and_version() {
    let output = Command::new(env!("DORFQ_BIN"))
        .output()
        .expect("spawn dorfq");
    assert!(output.status.success(), "dorfq: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert_eq!(stdout, format!("dorfq {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_argument_is_rejected() {
    let output = Command::new(env!("DORFQ_BIN"))
        .arg("--nope")
        .output()
        .expect("spawn dorfq");
    // clap exits 2 on a usage error.
    assert_eq!(output.status.code(), Some(2), "dorfq --nope: {output:?}");
    assert!(output.stdout.is_empty(), "dorfq --nope: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--nope"), "dorfq --nope stderr: {stderr:?}");
}
