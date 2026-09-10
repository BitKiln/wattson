//! The whole tool, driven as a user drives it.
//!
//! Runs the real binary against the real simulator over a real socket, asserting on exit
//! codes as a CI pipeline would. That last part is the point: `assert` is only useful if the
//! exit codes are a contract, and a contract nobody tests is a wish.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use assert_cmd::prelude::*;
use predicates::prelude::*;

/// Exit codes, restated here rather than imported, so a change to them fails this test.
mod exit {
    pub const SUCCESS: i32 = 0;
    pub const ASSERTION_FAILED: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const IO: i32 = 3;
}

fn wattson() -> Command {
    Command::cargo_bin("wattson").expect("the wattson binary must build")
}

/// A `wattson sim` server on an OS-chosen port.
struct Sim {
    child: Child,
    addr: String,
}

impl Sim {
    /// Start the simulator and wait for it to report the port it bound.
    ///
    /// Port 0 rather than a fixed port, so parallel CI jobs cannot collide.
    fn start(profile: &str) -> Sim {
        let mut child = wattson()
            .args([
                "sim",
                "--listen",
                "127.0.0.1:0",
                "--profile",
                profile,
                "--seed",
                "42",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start wattson sim");

        let stdout = child.stdout.take().expect("piped");
        let mut lines = BufReader::new(stdout).lines();
        let addr = loop {
            match lines.next() {
                Some(Ok(line)) if line.starts_with("listening on ") => {
                    break line.trim_start_matches("listening on ").trim().to_string();
                }
                Some(Ok(_)) => continue,
                _ => panic!("the simulator exited before reporting its address"),
            }
        };
        // Keep reading in the background so the pipe cannot fill and stall the child.
        std::thread::spawn(move || lines.for_each(|_| {}));

        Sim { child, addr }
    }

    fn uri(&self) -> String {
        format!("tcp://{}", self.addr)
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Capture from a simulator over TCP into `out`.
fn capture(profile: &str, out: &Path, duration: &str) {
    let sim = Sim::start(profile);
    wattson()
        .args(["capture", "--device", &sim.uri(), "--duration", duration])
        .arg("--out")
        .arg(out)
        .arg("--quiet")
        .assert()
        .success();
    assert!(out.exists(), "capture produced no file");
}

/// Capture through the in-process simulator, which needs no socket at all.
fn capture_in_process(profile: &str, out: &Path, duration: &str) {
    wattson()
        .args([
            "capture",
            "--device",
            &format!("sim://{profile}?rate=50000&seed=42"),
            "--duration",
            duration,
        ])
        .arg("--out")
        .arg(out)
        .arg("--quiet")
        .assert()
        .success();
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn json_of(args: &[&str]) -> serde_json::Value {
    let output = wattson().args(args).output().expect("run");
    assert!(output.status.success(), "command failed: {args:?}");
    serde_json::from_slice(&output.stdout).expect("valid JSON on stdout")
}

// ---------------------------------------------------------------------------
// the documented walkthrough
// ---------------------------------------------------------------------------

/// The quick start from the README, run for real over a socket.
#[test]
fn the_documented_quick_start_works_over_tcp() {
    let dir = tmp();
    let cap = dir.path().join("smoke.pprof");
    capture("ble-sensor", &cap, "2s");

    wattson()
        .arg("info")
        .arg(&cap)
        .assert()
        .success()
        .stdout(predicate::str::contains("SIM-0001"))
        .stdout(predicate::str::contains("Effective rate"));

    wattson()
        .arg("analyze")
        .arg(&cap)
        .arg("--events")
        .assert()
        .success()
        .stdout(predicate::str::contains("BLE_TX"))
        .stdout(predicate::str::contains("Occurrences"));
}

/// `--device sim://...` must work with no socket, so a first look needs nothing set up.
#[test]
fn a_capture_can_be_taken_with_no_socket_at_all() {
    let dir = tmp();
    let cap = dir.path().join("inproc.pprof");
    capture_in_process("ble-sensor", &cap, "1s");

    let info = json_of(&["info", cap.to_str().unwrap(), "--json"]);
    assert!(info["sample_count"].as_u64().unwrap() > 10_000);
    assert_eq!(info["device_serial"], "SIM-0001");
    assert!(info["finalized"].as_bool().unwrap());
}

// ---------------------------------------------------------------------------
// the CI gate, both halves
// ---------------------------------------------------------------------------

/// The whole reason the project exists, as a CI pipeline would run it.
#[test]
fn the_documented_budget_passes_on_good_firmware_and_fails_on_the_regression() {
    let dir = tmp();
    let good = dir.path().join("good.pprof");
    let bad = dir.path().join("bad.pprof");
    capture_in_process("ble-sensor", &good, "2s");
    capture_in_process("ble-sensor-regressed", &bad, "2s");

    // Exit 0, and the measured figure is printed so a log shows the margin.
    wattson()
        .args(["assert"])
        .arg(&good)
        .args(["--event", "BLE_TX", "--max-energy", "260uJ"])
        .assert()
        .code(exit::SUCCESS)
        .stdout(predicate::str::contains("PASS"))
        .stdout(predicate::str::contains("BLE_TX"));

    // Exit 1, with a report naming the metric, the limit and the measurement.
    wattson()
        .args(["assert"])
        .arg(&bad)
        .args(["--event", "BLE_TX", "--max-energy", "260uJ"])
        .assert()
        .code(exit::ASSERTION_FAILED)
        .stdout(predicate::str::contains("POWER REGRESSION"))
        .stdout(predicate::str::contains("BLE_TX"))
        .stdout(predicate::str::contains("Expected: <= 260.000 uJ"))
        .stdout(predicate::str::contains("Measured:"));
}

/// A baseline catches regressions in events nobody wrote an explicit budget for.
#[test]
fn a_baseline_and_tolerance_catch_an_unbudgeted_regression() {
    let dir = tmp();
    let good = dir.path().join("good.pprof");
    let bad = dir.path().join("bad.pprof");
    let base = dir.path().join("base.json");
    capture_in_process("ble-sensor", &good, "2s");
    capture_in_process("ble-sensor-regressed", &bad, "2s");

    wattson()
        .args(["assert"])
        .arg(&good)
        .args(["--event", "BLE_TX", "--max-energy", "10J"])
        .arg("--write-baseline")
        .arg(&base)
        .assert()
        .code(exit::SUCCESS);
    assert!(base.exists(), "no baseline was written");

    wattson()
        .args(["assert"])
        .arg(&bad)
        .arg("--baseline")
        .arg(&base)
        .args(["--tolerance", "5%"])
        .assert()
        .code(exit::ASSERTION_FAILED)
        .stdout(predicate::str::contains("Regression:"));

    // The same comparison against itself must pass.
    wattson()
        .args(["assert"])
        .arg(&good)
        .arg("--baseline")
        .arg(&base)
        .args(["--tolerance", "5%"])
        .assert()
        .code(exit::SUCCESS);
}

/// The failure this project can least afford: a gate that passes because there was nothing
/// left to check.
#[test]
fn asserting_on_an_event_that_is_not_in_the_capture_fails() {
    let dir = tmp();
    let cap = dir.path().join("c.pprof");
    capture_in_process("ble-sensor", &cap, "1s");

    wattson()
        .args(["assert"])
        .arg(&cap)
        .args(["--event", "TYPO_TX", "--max-energy", "260uJ"])
        .assert()
        .code(exit::ASSERTION_FAILED)
        .stdout(predicate::str::contains("TYPO_TX"))
        .stdout(predicate::str::contains("not declared"));
}

#[test]
fn asserting_with_no_budget_at_all_is_a_usage_error_not_a_pass() {
    let dir = tmp();
    let cap = dir.path().join("c.pprof");
    capture_in_process("ble-sensor", &cap, "1s");

    let out = wattson().args(["assert"]).arg(&cap).output().unwrap();
    assert_ne!(
        out.status.code(),
        Some(exit::SUCCESS),
        "an empty gate must not pass"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no budgets given"), "{stderr}");
}

#[test]
fn a_junit_report_is_well_formed_and_counts_its_failures() {
    let dir = tmp();
    let bad = dir.path().join("bad.pprof");
    capture_in_process("ble-sensor-regressed", &bad, "2s");

    let out = wattson()
        .args(["assert"])
        .arg(&bad)
        .args([
            "--event",
            "BLE_TX",
            "--max-energy",
            "260uJ",
            "--format",
            "junit",
        ])
        .output()
        .unwrap();
    let xml = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(exit::ASSERTION_FAILED));
    assert!(xml.starts_with("<?xml"), "{xml}");
    assert!(xml.contains("failures=\"1\""), "{xml}");
    assert!(xml.contains("</testsuite>"), "{xml}");
}

// ---------------------------------------------------------------------------
// analysis and export
// ---------------------------------------------------------------------------

#[test]
fn analyze_json_reports_the_events_and_their_energy() {
    let dir = tmp();
    let cap = dir.path().join("c.pprof");
    capture_in_process("ble-sensor", &cap, "2s");

    let report = json_of(&["analyze", cap.to_str().unwrap(), "--json"]);
    let events = report["events"].as_array().expect("events array");
    let tx = events
        .iter()
        .find(|e| e["name"] == "BLE_TX")
        .expect("BLE_TX must be reported");

    assert!(tx["occurrences"].as_u64().unwrap() >= 10);
    let mean = tx["energy_uj"]["mean"].as_f64().unwrap();
    assert!(
        (150.0..400.0).contains(&mean),
        "BLE_TX mean energy {mean:.1} uJ is not near the expected 245 uJ"
    );
    assert!(report["region"]["current_max_ua"].as_f64().unwrap() > 60_000.0);
}

#[test]
fn a_time_window_narrows_the_analysis() {
    let dir = tmp();
    let cap = dir.path().join("c.pprof");
    capture_in_process("ble-sensor", &cap, "2s");

    let whole = json_of(&["analyze", cap.to_str().unwrap(), "--json", "--regions"]);
    let part = json_of(&[
        "analyze",
        cap.to_str().unwrap(),
        "--json",
        "--regions",
        "--from",
        "0.5s",
        "--to",
        "1s",
    ]);

    let whole_n = whole["region"]["sample_count"].as_u64().unwrap();
    let part_n = part["region"]["sample_count"].as_u64().unwrap();
    assert!(part_n < whole_n, "the window did not narrow anything");
    assert!(
        part_n > 1_000,
        "the window returned almost nothing: {part_n}"
    );
}

#[test]
fn csv_export_has_a_header_and_decimates_as_asked() {
    let dir = tmp();
    let cap = dir.path().join("c.pprof");
    let csv = dir.path().join("out.csv");
    capture_in_process("ble-sensor", &cap, "1s");

    wattson()
        .args(["export"])
        .arg(&cap)
        .args(["--format", "csv", "--what", "samples", "--decimate", "100"])
        .arg("--out")
        .arg(&csv)
        .arg("--quiet")
        .assert()
        .success();

    let text = std::fs::read_to_string(&csv).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "time_s,current_uA,voltage_uV,power_uW");
    // 1 s at 50 ksps, decimated by 100, is a few hundred rows.
    assert!(
        lines.len() > 100 && lines.len() < 1_000,
        "got {} rows",
        lines.len()
    );
}

#[test]
fn event_export_resolves_names_from_the_captures_metadata() {
    let dir = tmp();
    let cap = dir.path().join("c.pprof");
    capture_in_process("ble-sensor", &cap, "1s");

    let output = wattson()
        .args(["export"])
        .arg(&cap)
        .args(["--format", "csv", "--what", "events"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("BLE_TX"), "{text}");
    assert!(text.contains("0x0101"), "{text}");
}

/// A planned format must say so, rather than failing with an unknown-value error.
#[test]
fn a_planned_export_format_is_refused_by_name() {
    let dir = tmp();
    let cap = dir.path().join("c.pprof");
    capture_in_process("ble-sensor", &cap, "1s");

    let out = wattson()
        .args(["export"])
        .arg(&cap)
        .args(["--format", "vcd"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(exit::IO));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("vcd"), "{stderr}");
    assert!(stderr.contains("not implemented"), "{stderr}");
}

// ---------------------------------------------------------------------------
// error handling
// ---------------------------------------------------------------------------

#[test]
fn a_missing_capture_file_exits_with_the_io_code() {
    wattson()
        .args(["info", "definitely-not-here.pprof"])
        .assert()
        .code(exit::IO)
        .stderr(predicate::str::contains("definitely-not-here.pprof"));
}

#[test]
fn a_file_that_is_not_a_capture_says_so() {
    let dir = tmp();
    let path = dir.path().join("nope.pprof");
    std::fs::write(&path, b"this is not a capture, it is a poem about one").unwrap();

    wattson()
        .arg("info")
        .arg(&path)
        .assert()
        .code(exit::IO)
        .stderr(predicate::str::contains("not a wattson capture"));
}

#[test]
fn an_unparseable_unit_is_a_usage_error() {
    let dir = tmp();
    let cap = dir.path().join("c.pprof");
    capture_in_process("ble-sensor", &cap, "1s");

    let out = wattson()
        .args(["assert"])
        .arg(&cap)
        .args(["--event", "BLE_TX", "--max-energy", "260 bananas"])
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(exit::SUCCESS));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bananas"),
        "the bad input must be quoted back: {stderr}"
    );
}

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    wattson().arg("frobnicate").assert().code(exit::USAGE);
}

// ---------------------------------------------------------------------------
// discoverability
// ---------------------------------------------------------------------------

/// Someone with no hardware must be told what to do, not left at a dead end.
#[test]
fn devices_points_at_the_simulator_when_nothing_is_attached() {
    let output = wattson().arg("devices").output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    if text.contains("No profiler devices found") {
        assert!(text.contains("wattson sim"), "{text}");
    }
}

#[test]
fn every_subcommand_has_help() {
    for cmd in [
        "devices", "capture", "info", "analyze", "export", "assert", "sim",
    ] {
        wattson()
            .args([cmd, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage"));
    }
}

#[test]
fn shell_completions_are_generated() {
    wattson()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wattson"));
}

/// A capture killed part-way must still be analysable, because Ctrl-C is normal.
#[test]
fn an_interrupted_capture_is_still_analysable() {
    let dir = tmp();
    let cap: PathBuf = dir.path().join("killed.pprof");
    let sim = Sim::start("always-on");

    let mut child = wattson()
        .args(["capture", "--device", &sim.uri(), "--out"])
        .arg(&cap)
        .arg("--quiet")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn capture");

    // Let it record for a while, then kill it outright — no clean shutdown.
    std::thread::sleep(Duration::from_millis(800));
    let _ = child.kill();
    let _ = child.wait();

    // The file has no footer and no index, and must be recovered by scanning.
    wattson()
        .arg("info")
        .arg(&cap)
        .assert()
        .success()
        .stdout(predicate::str::contains("never finalized"))
        .stdout(predicate::str::contains("rebuilt by scanning"));

    let info = json_of(&["info", cap.to_str().unwrap(), "--json"]);
    assert!(
        info["sample_count"].as_u64().unwrap() > 1_000,
        "recovery kept almost nothing: {}",
        info["sample_count"]
    );
}
