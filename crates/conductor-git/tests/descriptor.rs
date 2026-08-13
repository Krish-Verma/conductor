//! `.conductor-run.json` — master plan §4.1.
//!
//! "This is what makes recovery possible with **no database at all**." So the
//! tests that matter are the ones that read it with no store, no clone and no
//! prior knowledge.

mod common;

use std::path::Path;

use conductor_core::{PolicyHash, RunId, TaskId};
use conductor_git::clone::{WorkspaceRequest, create_workspace};
use conductor_git::descriptor::{DESCRIPTOR_FILENAME, RunDescriptor, read_descriptor};

use common::{clean_repo, git_out};

fn request(source: &Path, workspace: &Path, base_commit: &str) -> WorkspaceRequest {
    WorkspaceRequest {
        source: source.to_path_buf(),
        workspace: workspace.to_path_buf(),
        run_id: RunId::new("r-0041").expect("valid"),
        task_id: TaskId::new("T-0012").expect("valid"),
        base_commit: base_commit.to_string(),
        policy_hash: PolicyHash::new("blake3:deadbeef").expect("valid"),
    }
}

#[test]
fn every_workspace_carries_a_descriptor_with_the_five_required_fields() {
    let source = clean_repo();
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    create_workspace(&request(source.path(), &ws, &head)).expect("create");

    let raw = std::fs::read_to_string(ws.join(DESCRIPTOR_FILENAME)).expect("descriptor exists");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
    for field in [
        "run_id",
        "task_id",
        "base_commit",
        "policy_hash",
        "created_at",
    ] {
        assert!(
            value.get(field).is_some(),
            "descriptor is missing {field}: {raw}"
        );
    }
    assert_eq!(value["run_id"], "r-0041");
    assert_eq!(value["task_id"], "T-0012");
    assert_eq!(value["base_commit"], head.as_str());
    assert_eq!(value["policy_hash"], "blake3:deadbeef");
}

#[test]
fn a_descriptor_is_readable_with_no_repository_and_no_database() {
    // The recovery claim in §4.1, tested the way recovery actually happens:
    // a bare file on disk, nothing else.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(DESCRIPTOR_FILENAME),
        r#"{
             "run_id": "r-0007",
             "task_id": "T-0003",
             "base_commit": "abc123",
             "policy_hash": "blake3:cafe",
             "created_at": 1765432100
           }"#,
    )
    .expect("write");

    let descriptor = read_descriptor(dir.path()).expect("read");

    assert_eq!(descriptor.run_id.as_str(), "r-0007");
    assert_eq!(descriptor.task_id.as_str(), "T-0003");
    assert_eq!(descriptor.base_commit, "abc123");
    assert_eq!(descriptor.policy_hash.as_str(), "blake3:cafe");
    assert_eq!(descriptor.created_at, 1_765_432_100);
}

#[test]
fn unknown_fields_are_tolerated_so_an_older_binary_can_still_recover() {
    // §2.2: never `deny_unknown_fields`. A descriptor written by a newer
    // Conductor must not become unreadable — the file exists precisely for the
    // case where nothing else survived.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(DESCRIPTOR_FILENAME),
        r#"{"run_id":"r-1","task_id":"T-1","base_commit":"aa","policy_hash":"blake3:x",
            "created_at":1,"future_field":{"nested":true}}"#,
    )
    .expect("write");

    let descriptor = read_descriptor(dir.path()).expect("read");
    assert_eq!(descriptor.run_id.as_str(), "r-1");
}

#[test]
fn a_missing_descriptor_is_reported_not_guessed_around() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = read_descriptor(dir.path()).expect_err("must fail");
    assert!(
        err.to_string().contains(DESCRIPTOR_FILENAME),
        "the error must name the file: {err}"
    );
}

#[test]
fn a_truncated_descriptor_is_an_error_not_a_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(DESCRIPTOR_FILENAME),
        r#"{"run_id":"r-1","task"#,
    )
    .expect("write");
    let err = read_descriptor(dir.path()).expect_err("must fail");
    assert!(err.to_string().contains("unusable"), "got {err}");
}

#[test]
fn a_descriptor_round_trips_through_json() {
    let descriptor = RunDescriptor {
        run_id: RunId::new("r-9").expect("valid"),
        task_id: TaskId::new("T-9").expect("valid"),
        base_commit: "ff00".to_string(),
        policy_hash: PolicyHash::new("blake3:9").expect("valid"),
        created_at: 42,
    };
    let json = serde_json::to_string(&descriptor).expect("serialize");
    let back: RunDescriptor = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, descriptor);
}
