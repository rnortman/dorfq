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
    for name in ["github-like", "bisect"] {
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
fn json_writes_the_document_and_still_prints_the_table() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"));
    let path = dir.join("report.json");
    let out = path.to_str().expect("UTF-8 path");
    let output = sim(&["run", "--config", env!("DORFQ_SIM_EXAMPLE"), "--json", out]);
    assert!(output.status.success(), "dorfq-sim run --json: {output:?}");
    let table = stdout(&output);
    assert!(table.starts_with("metric "), "{table}");
    assert!(!table.contains("policy bisect"), "{table}");
    let document = fs::read_to_string(&path).expect("the document is written");
    assert!(
        document.starts_with(r#"{"schema":4,"name":"sim","config":"#),
        "{document:.80}"
    );
    for name in ["github-like", "bisect", "bayes", "bayes-tree"] {
        let column = format!(r#"{{"name":"{name}","replications":[{{"metrics":"#);
        assert!(document.contains(&column), "no policy {name}");
    }
    assert!(document.contains(r#""trace":[{"kind":"step","t":0,"#));
    assert!(
        document.contains(r##""config_text":"# A week of arrivals"##),
        "no config_text"
    );
}

#[test]
fn an_invalid_probability_exits_one_naming_it() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"));
    let path = dir.join("bad.toml");
    let example = fs::read_to_string(env!("DORFQ_SIM_EXAMPLE")).expect("read example");
    let bad = example.replacen("p_flake = 0.02", "p_flake = 1.5", 1);
    assert_ne!(bad, example, "the example sets p_flake = 0.02");
    fs::write(&path, bad).expect("write config");
    let output = sim(&["run", "--config", path.to_str().expect("UTF-8 path")]);
    assert_eq!(output.status.code(), Some(1), "dorfq-sim: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("p_flake"), "{stderr}");
}

#[test]
fn no_arguments_is_a_usage_error() {
    let output = sim(&[]);
    // clap exits 2 on a usage error.
    assert_eq!(output.status.code(), Some(2), "dorfq-sim: {output:?}");
    assert!(output.stdout.is_empty(), "dorfq-sim: {output:?}");
}

/// The example as a two-point sweep at one replication, against
/// `github-like`, written to `TEST_TMPDIR` as `<stem>.toml`; each test
/// names its own file, since tests run concurrently.
fn sweep_file(stem: &str) -> PathBuf {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"));
    let path = dir.join(stem).with_extension("toml");
    let example = fs::read_to_string(env!("DORFQ_SIM_EXAMPLE")).expect("read example");
    let base = example.replacen("replications = 4", "replications = 1", 1);
    assert_ne!(base, example, "the example sets replications = 4");
    let axis = "\n[[axis]]\nkeys = [\"world.p_flake\"]\nvalues = [0.0, 0.1]\nlabels = [\"calm\", \"flaky\"]\n";
    fs::write(&path, format!("baseline = \"github-like\"\n{base}{axis}")).expect("write sweep");
    path
}

#[test]
fn sweep_prints_the_grid_and_the_summary_and_writes_the_document() {
    let path = sweep_file("grid");
    let out = path.with_extension("json");
    let output = sim(&[
        "sweep",
        "--config",
        path.to_str().expect("UTF-8 path"),
        "--json",
        out.to_str().expect("UTF-8 path"),
        "--jobs",
        "2",
    ]);
    assert!(output.status.success(), "dorfq-sim sweep: {output:?}");
    let text = stdout(&output);
    let header = text.lines().next().expect("a header");
    assert!(header.starts_with("point  p_flake  policy"), "{header}");
    assert!(text.contains("flaky"), "{text}");
    let summary = text
        .lines()
        .find(|l| l.starts_with("against github-like"))
        .unwrap_or_else(|| panic!("no summary in {text}"));
    for name in ["bisect", "bayes"] {
        assert!(summary.contains(name), "no column {name} in {summary:?}");
    }
    let document = fs::read_to_string(&out).expect("the document is written");
    assert!(
        document.starts_with(r#"{"schema":2,"name":"grid","config_text":"#),
        "{document:.80}"
    );
}

#[test]
fn sweep_with_zero_jobs_is_a_usage_error() {
    let path = sweep_file("zero-jobs");
    let output = sim(&["sweep", "--config", path.to_str().unwrap(), "--jobs", "0"]);
    assert_eq!(output.status.code(), Some(2), "dorfq-sim: {output:?}");
}

#[test]
fn a_sweep_whose_document_cannot_be_written_still_prints_and_exits_one() {
    let path = sweep_file("unwritable");
    let out = path.with_file_name("missing").join("unwritable.json");
    let output = sim(&[
        "sweep",
        "--config",
        path.to_str().expect("UTF-8 path"),
        "--json",
        out.to_str().expect("UTF-8 path"),
        "--jobs",
        "2",
    ]);
    assert_eq!(output.status.code(), Some(1), "dorfq-sim sweep: {output:?}");
    let text = stdout(&output);
    assert!(text.contains("against github-like"), "{text}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("unwritable.json"), "{stderr}");
}
