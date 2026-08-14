//! `conductor doctor` — what it reports, and which exit code it picks.
//!
//! Exit codes are master plan §7.2: 0 success · 2 store unhealthy / not
//! initialized · 64 usage · 70 internal.

use std::path::Path;
use std::process::{Command, Output};

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

fn run(args: &[&str]) -> Output {
    Command::new(CONDUCTOR)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not json ({e}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn store_arg(path: &Path) -> String {
    path.to_str().expect("utf8 path").to_string()
}

#[test]
fn reporting_on_an_absent_store_does_not_create_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("nested").join("conductor.db");

    let out = run(&["doctor", "--json", "--store", &store_arg(&db)]);

    assert_eq!(
        out.status.code(),
        Some(2),
        "an absent store is 'not initialized' => exit 2"
    );
    let report = json(&out);
    assert_eq!(report["store"]["exists"], false);
    assert_eq!(report["ok"], false);
    assert!(
        !db.exists(),
        "doctor must not create a database as a side effect of reporting"
    );
    assert!(
        !db.parent().expect("parent").exists(),
        "doctor must not create the store directory either"
    );
}

#[test]
fn init_store_is_the_only_way_doctor_creates_a_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("nested").join("conductor.db");

    let out = run(&[
        "doctor",
        "--json",
        "--init-store",
        "--store",
        &store_arg(&db),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(db.exists());

    let report = json(&out);
    assert_eq!(report["ok"], true);
    assert_eq!(report["store"]["exists"], true);
    assert_eq!(report["store"]["healthy"], true);
    assert_eq!(
        report["store"]["schema_version"],
        conductor_store::schema::SUPPORTED_SCHEMA_VERSION
    );
    assert_eq!(report["store"]["integrity_check"][0], "ok");
    assert_eq!(report["store"]["foreign_key_violations"], 0);
    assert_eq!(
        report["store"]["pending_migrations"]
            .as_array()
            .expect("array")
            .len(),
        0
    );

    // The pragma values actually in effect, not the ones we hoped for.
    let pragmas = &report["store"]["pragmas"];
    assert_eq!(pragmas["journal_mode"], "wal");
    assert_eq!(pragmas["synchronous"], "2");
    assert_eq!(pragmas["fullfsync"], "1");
    assert_eq!(pragmas["checkpoint_fullfsync"], "1");
    assert_eq!(pragmas["foreign_keys"], "1");
    assert_eq!(pragmas["busy_timeout"], "5000");

    // A second, plain run is green and still does not migrate anything.
    let again = run(&["doctor", "--json", "--store", &store_arg(&db)]);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(
        json(&again)["store"]["schema_version"],
        conductor_store::schema::SUPPORTED_SCHEMA_VERSION
    );
}

#[test]
fn a_corrupt_store_is_reported_unhealthy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");
    let init = run(&[
        "doctor",
        "--json",
        "--init-store",
        "--store",
        &store_arg(&db),
    ]);
    assert_eq!(init.status.code(), Some(0));

    // Scribble over a page in the middle of the file.
    let mut bytes = std::fs::read(&db).expect("read db");
    assert!(bytes.len() > 16_384, "db too small to corrupt meaningfully");
    for byte in bytes.iter_mut().skip(8_192).take(4_096) {
        *byte = 0xA5;
    }
    std::fs::write(&db, &bytes).expect("write corrupted db");

    let out = run(&["doctor", "--json", "--store", &store_arg(&db)]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a store that fails integrity_check is unhealthy => exit 2"
    );
    let report = json(&out);
    assert_eq!(report["ok"], false);
    assert_eq!(report["store"]["healthy"], false);
}

#[test]
fn a_missing_adapter_is_a_fact_not_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");
    run(&[
        "doctor",
        "--json",
        "--init-store",
        "--store",
        &store_arg(&db),
    ]);

    // An empty PATH guarantees git and both adapters are absent.
    let out = Command::new(CONDUCTOR)
        .args(["doctor", "--json", "--store", &store_arg(&db)])
        .env("PATH", "")
        .output()
        .expect("spawn");

    assert_eq!(
        out.status.code(),
        Some(0),
        "absent adapters and git must not fail the health check"
    );
    let report = json(&out);
    assert_eq!(report["git"]["present"], false);
    let adapters = report["adapters"].as_array().expect("adapters array");
    assert_eq!(adapters.len(), 2);
    let names: Vec<&str> = adapters
        .iter()
        .map(|a| a["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["codex", "claude"]);
    assert!(adapters.iter().all(|a| a["present"] == false));
    assert_eq!(report["ok"], true);
}

#[test]
fn the_socket_directory_is_reported_with_its_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".conductor")).expect("create socket dir");
    std::fs::set_permissions(
        home.join(".conductor"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod");

    let out = Command::new(CONDUCTOR)
        .args([
            "doctor",
            "--json",
            "--init-store",
            "--store",
            &store_arg(&db),
        ])
        .env("HOME", &home)
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(0));

    let report = json(&out);
    let socket = &report["socket_dir"];
    assert_eq!(socket["exists"], true);
    assert_eq!(socket["mode"], "0700");
    assert!(
        socket["path"]
            .as_str()
            .expect("path")
            .ends_with("/.conductor")
    );
    assert_eq!(socket["socket_exists"], false);
}

#[test]
fn human_output_is_readable_and_names_the_sections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");
    let out = run(&["doctor", "--init-store", "--store", &store_arg(&db)]);
    assert_eq!(out.status.code(), Some(0));

    let text = String::from_utf8_lossy(&out.stdout);
    for section in ["store", "git", "adapters", "socket"] {
        assert!(
            text.to_lowercase().contains(section),
            "human output is missing the {section} section:\n{text}"
        );
    }
    assert!(text.contains("fullfsync"), "pragmas must be shown:\n{text}");
}

#[test]
fn a_usage_error_exits_64() {
    let out = run(&["doctor", "--not-a-flag"]);
    assert_eq!(out.status.code(), Some(64), "§7.2: usage error is EX_USAGE");

    let out = run(&["not-a-command"]);
    assert_eq!(out.status.code(), Some(64));
}

#[test]
fn help_and_version_are_not_usage_errors() {
    assert_eq!(run(&["--help"]).status.code(), Some(0));
    assert_eq!(run(&["--version"]).status.code(), Some(0));
}
