//! Runs the built `dorfq-sim` binary. Bazel supplies its path through
//! `DORFQ_SIM_BIN` and the shipped example's through `DORFQ_SIM_EXAMPLE`;
//! this test does not build under `cargo test`.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn sim(args: &[&str]) -> Output {
    Command::new(env!("DORFQ_SIM_BIN"))
        .args(args)
        .output()
        .expect("spawn dorfq-sim")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

#[test]
fn the_example_runs_and_prints_every_policy_and_the_cost_metric() {
    let output = sim(&["run", "--config", env!("DORFQ_SIM_EXAMPLE")]);
    assert!(output.status.success(), "dorfq-sim run: {output:?}");
    let table = stdout(&output);
    let header = table.lines().next().expect("a header");
    for name in ["all-at-once", "bisect"] {
        assert!(header.contains(name), "no column {name} in {header:?}");
    }
    assert!(table.contains("runner_hours_per_landed"), "{table}");
    let failure = table.lines().find(|l| l.starts_with("failure"));
    let cells: Vec<&str> = failure.expect("a failure row").split_whitespace().collect();
    assert!(cells[1..].iter().all(|&c| c == "-"), "{cells:?}");
}

#[test]
fn trace_prints_each_policy_from_time_zero() {
    let output = sim(&["run", "--config", env!("DORFQ_SIM_EXAMPLE"), "--trace"]);
    assert!(output.status.success(), "dorfq-sim run --trace: {output:?}");
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().position(|&l| l == "policy bisect");
    let at = at.unwrap_or_else(|| panic!("no policy bisect line"));
    assert!(lines[at + 1].starts_with("t=0 "), "{}", lines[at + 1]);
}

#[test]
fn an_invalid_probability_exits_one_naming_it() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"));
    let path = dir.join("bad.toml");
    let example = fs::read_to_string(env!("DORFQ_SIM_EXAMPLE")).expect("read example");
    let bad = example.replace("p_bad = 0.05", "p_bad = 1.5");
    assert_ne!(bad, example, "the example sets p_bad = 0.05");
    fs::write(&path, bad).expect("write config");
    let output = sim(&["run", "--config", path.to_str().expect("UTF-8 path")]);
    assert_eq!(output.status.code(), Some(1), "dorfq-sim: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("p_bad"), "{stderr}");
}

#[test]
fn no_arguments_is_a_usage_error() {
    let output = sim(&[]);
    // clap exits 2 on a usage error.
    assert_eq!(output.status.code(), Some(2), "dorfq-sim: {output:?}");
    assert!(output.stdout.is_empty(), "dorfq-sim: {output:?}");
}
