//! Subprocess integration tests: run the real `gwl-jobs` binary and assert
//! on its stdout/stderr/exit code. Complements the in-process tests in
//! `cli_integration.rs` by covering the output contract (`--json`), error
//! paths, and the `--data-dir`/`--config` flag routing through `main.rs`.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A fresh corpus: a temp data dir (which `--data-dir` requires to already
/// exist) plus an empty config file, so tests never read the user's real
/// config or corpus.
struct Corpus {
    _dir: tempfile::TempDir,
    data_dir: PathBuf,
    config: PathBuf,
}

fn corpus() -> Corpus {
    corpus_with_config("")
}

/// A corpus whose config file carries `config_toml` (empty = defaults).
fn corpus_with_config(config_toml: &str) -> Corpus {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, config_toml).unwrap();
    Corpus {
        _dir: dir,
        data_dir,
        config,
    }
}

/// A `gwl-jobs` invocation rooted at the corpus's data dir and config.
fn cmd(c: &Corpus) -> Command {
    let mut cmd = Command::cargo_bin("gwl-jobs").unwrap();
    cmd.arg("--data-dir")
        .arg(&c.data_dir)
        .arg("--config")
        .arg(&c.config);
    cmd
}

/// Parse a command's stdout as JSON.
fn json_stdout(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout is valid JSON")
}

/// Ingest the fixture and return the lead id from the `--json` summary.
fn ingest(c: &Corpus) -> String {
    let fixture = fixture("northwind-staff-backend.html");
    let output = cmd(c)
        .args(["ingest", "--file", fixture.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ingest failed: {:?}",
        output.stderr
    );
    json_stdout(&output)["lead_id"]
        .as_str()
        .unwrap()
        .to_string()
}

// ── JSON output ────────────────────────────────────────────────

#[test]
fn ingest_json_output() {
    let c = corpus();
    let fixture = fixture("northwind-staff-backend.html");
    let output = cmd(&c)
        .args(["ingest", "--file", fixture.to_str().unwrap(), "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v = json_stdout(&output);
    assert_eq!(v["kind"], "ingested");
    assert_eq!(v["adapter"], "drop-in");
    assert_eq!(v["extracted"]["company"], "Northwind Labs");
    assert_eq!(v["extracted"]["req_id"], "NW-2026-042");
    assert_eq!(v["extracted"]["remote"], true);
    assert_eq!(v["extracted"]["comp"]["min"], 180_000);
    assert_eq!(v["extracted"]["comp"]["max"], 240_000);
    assert!(v["score"].is_object(), "gate-passing ingest is scored");
}

#[test]
fn show_json_output() {
    let c = corpus();
    let lead_id = ingest(&c);

    let output = cmd(&c).args(["show", &lead_id, "--json"]).output().unwrap();
    assert!(output.status.success());
    let v = json_stdout(&output);
    assert_eq!(v["lead_id"], lead_id.as_str());
    assert_eq!(v["extracted"]["company"], "Northwind Labs");
}

#[test]
fn list_json_output() {
    let c = corpus();
    ingest(&c);

    let output = cmd(&c).args(["list", "--json"]).output().unwrap();
    assert!(output.status.success());
    let v = json_stdout(&output);
    let entries = v.as_array().expect("list --json is an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["company"], "Northwind Labs");
    assert_eq!(entries[0]["status"], "pending");
}

#[test]
fn edit_json_output() {
    let c = corpus();
    let lead_id = ingest(&c);

    let output = cmd(&c)
        .args([
            "edit",
            &lead_id,
            "--title",
            "Staff Backend Engineer, Platform",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v = json_stdout(&output);
    // Title changed + the adapter flips drop-in → user (decision 0009).
    assert_eq!(v["changed"], serde_json::json!(["title", "adapter"]));
    assert_eq!(v["extracted"]["title"], "Staff Backend Engineer, Platform");
}

// ── --data-dir routing ─────────────────────────────────────────

#[test]
fn data_dir_flag_routes_to_alternate_corpus() {
    let a = corpus();
    let b = corpus();
    let fixture = fixture("northwind-staff-backend.html");

    cmd(&a)
        .args(["ingest", "--file", fixture.to_str().unwrap()])
        .assert()
        .success();

    let list_a = cmd(&a).args(["list", "--json"]).output().unwrap();
    assert_eq!(json_stdout(&list_a).as_array().unwrap().len(), 1);

    let list_b = cmd(&b).args(["list", "--json"]).output().unwrap();
    assert_eq!(json_stdout(&list_b).as_array().unwrap().len(), 0);
}

#[test]
fn config_flag_routes_to_alternate_config() {
    // An empty config is behaviorally identical to the default, so this is
    // the test that proves `--config` is honored rather than ignored: a
    // blacklist entry must actually gate the ingest.
    let c = corpus_with_config("blacklist = [\"Northwind Labs\"]\n");
    let fixture = fixture("northwind-staff-backend.html");
    let output = cmd(&c)
        .args(["ingest", "--file", fixture.to_str().unwrap(), "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v = json_stdout(&output);
    let rejected = v["rejected"].as_array().expect("blacklist gate rejects");
    assert_eq!(rejected[0]["gate"], "blacklist");
    assert!(v["score"].is_null());
}

// ── Error paths ────────────────────────────────────────────────

#[test]
fn ingest_missing_file_errors() {
    let c = corpus();
    // A path inside the fresh corpus is guaranteed absent (unlike a
    // hard-coded absolute path, which could exist on some machine).
    let missing = c.data_dir.join("missing.html");
    cmd(&c)
        .args(["ingest", "--file", missing.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reading"));
}

#[test]
fn show_unknown_lead_errors() {
    let c = corpus();
    cmd(&c)
        .args(["show", "00000000-0000-0000-0000-000000000000"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no lead"));
}

#[test]
fn edit_contradictory_flags_error() {
    let c = corpus();
    let lead_id = ingest(&c);
    cmd(&c)
        .args(["edit", &lead_id, "--remote", "true", "--clear", "remote"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("both set and cleared"));
}

#[test]
fn ingest_without_source_errors() {
    let c = corpus();
    // Clap rejects the missing <url>/--file before any command runs: exit
    // code 2 (usage error) and the "required arguments" diagnostic.
    cmd(&c)
        .arg("ingest")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("required arguments"));
}
