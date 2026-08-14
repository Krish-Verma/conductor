//! `conductor policy explain <action>` — master plan §7.1 and §4.4 line 633.
//!
//! > "prints: action · resolved effect · the ceiling that applied · every rule
//! > that matched **and every rule considered that did not, with the reason** ·
//! > facts and their sources · policy hash · any exception with scope and
//! > expiry. Negative results are what people debug."
//!
//! The command is the 2 a.m. command, so these tests are about what it *says*,
//! not only about what it computes.

use std::path::Path;
use std::process::{Command, Output};

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

const PROJECT_POLICY: &str = r#"
policy:
  rules:
    - id: project.no-runtime-deps
      action: dependency.add.runtime
      effect: deny
    - id: project.deploy-needs-a-human
      action: deployment.execute
      effect: require_approval
    - id: project.scoped-elsewhere
      action: dependency.add.runtime
      effect: deny
      scope: {repo: some-other-repo}
    - id: project.needs-a-fact
      action: dependency.add.runtime
      effect: deny
      when: [lockfile_modified]
"#;

fn run(args: &[&str]) -> Output {
    Command::new(CONDUCTOR)
        // A developer's real `~/.config/conductor/policy.yaml` must not decide
        // whether these assertions hold.
        .env("XDG_CONFIG_HOME", "/nonexistent-for-tests")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
}

fn stdout(out: &Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(0),
        "explain must succeed even when it explains a deny:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo_with_policy(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let conductor = dir.path().join(".conductor");
    std::fs::create_dir_all(&conductor).expect("mkdir");
    std::fs::write(conductor.join("policy.yaml"), body).expect("write policy");
    dir
}

fn arg(path: &Path) -> String {
    path.to_str().expect("utf8 path").to_string()
}

#[test]
fn explain_prints_the_matched_rule_the_non_matching_rules_and_why_each_failed() {
    let repo = repo_with_policy(PROJECT_POLICY);
    let out = run(&[
        "policy",
        "explain",
        "dependency.add.runtime",
        "--repo",
        &arg(repo.path()),
        "--scope",
        "repo=acme",
    ]);
    let text = stdout(&out);

    assert!(text.contains("effect:  deny"), "{text}");
    assert!(text.contains("project.no-runtime-deps"), "{text}");
    assert!(
        text.contains("blake3:"),
        "the policy hash must be printed: {text}"
    );

    // Every rule considered and rejected, each on its own line, each with the
    // reason it did not apply.
    for (id, needle) in [
        ("project.deploy-needs-a-human", "does not cover"),
        ("project.scoped-elsewhere", "scope repo=some-other-repo"),
        ("project.needs-a-fact", "lockfile_modified"),
    ] {
        let line = text
            .lines()
            .find(|line| line.contains(id))
            .unwrap_or_else(|| panic!("{id} missing from:\n{text}"));
        assert!(
            line.contains(needle),
            "{id} was listed without saying why: {line}"
        );
    }
}

#[test]
fn explain_denies_an_action_outside_the_taxonomy_and_says_that_is_why() {
    let repo = repo_with_policy(PROJECT_POLICY);
    let out = run(&[
        "policy",
        "explain",
        "quantum.entangle",
        "--repo",
        &arg(repo.path()),
    ]);
    let text = stdout(&out);

    assert!(text.contains("effect:  deny"), "{text}");
    assert!(text.contains("taxonomy"), "{text}");

    // Positive control: the same repository, a known action with no rule
    // against it, allows. So the deny above is the unknown-action floor.
    let out = run(&[
        "policy",
        "explain",
        "git.commit.local",
        "--repo",
        &arg(repo.path()),
    ]);
    assert!(stdout(&out).contains("effect:  allow"));
}

#[test]
fn a_deny_that_rests_on_a_model_assisted_fact_is_reported_as_capped() {
    let repo = repo_with_policy(
        "policy:\n  rules:\n    - id: p.arch\n      action: architecture.change\n      \
         effect: deny\n      when: [architecture_change]\n",
    );

    let out = run(&[
        "policy",
        "explain",
        "architecture.change",
        "--repo",
        &arg(repo.path()),
        "--model-fact",
        "architecture_change=crates/conductor-core/src/state.rs",
    ]);
    let text = stdout(&out);
    assert!(text.contains("effect:  require_approval"), "{text}");
    assert!(text.contains("capped"), "{text}");
    assert!(text.contains("model_assisted"), "{text}");

    // Positive control: the identical rule and fact key, declared deterministic,
    // does deny.
    let out = run(&[
        "policy",
        "explain",
        "architecture.change",
        "--repo",
        &arg(repo.path()),
        "--fact",
        "architecture_change=crates/conductor-core/src/state.rs",
    ]);
    assert!(stdout(&out).contains("effect:  deny"));
}

#[test]
fn json_output_carries_the_matched_and_non_matching_rules() {
    let repo = repo_with_policy(PROJECT_POLICY);
    let out = run(&[
        "policy",
        "explain",
        "dependency.add.runtime",
        "--repo",
        &arg(repo.path()),
        "--json",
    ]);
    let report: serde_json::Value =
        serde_json::from_str(&stdout(&out)).expect("--json must emit json");

    assert_eq!(report["action"], "dependency.add.runtime");
    assert_eq!(report["effect"], "deny");
    assert_eq!(report["matched"][0]["rule_id"], "project.no-runtime-deps");
    assert!(
        report["not_matched"]
            .as_array()
            .expect("array")
            .iter()
            .any(|r| r["rule_id"] == "project.scoped-elsewhere"),
        "{report}"
    );
    assert!(
        report["policy_hash"]
            .as_str()
            .expect("hash")
            .starts_with("blake3:")
    );
}

#[test]
fn explaining_a_run_uses_the_snapshot_it_is_pinned_to_not_the_file_on_disk() {
    // §4.4: "a run evaluates against its snapshot for its entire life". The 2
    // a.m. question is about a run that was blocked, so `--run` must answer from
    // the pin and not from whatever the file says after someone edited it.
    use conductor_core::{RunId, TaskId};
    use conductor_run::policy::load;
    use conductor_run::policy::model::Origin;
    use conductor_store::{NewRun, NewTask, Store};

    let repo = repo_with_policy(PROJECT_POLICY);
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");
    let mut store = Store::open_or_create(&db).expect("open store");

    let at_creation = load::parse_document(PROJECT_POLICY, Origin::Project).expect("parse");
    let policy = load::resolve_documents(None, Some(at_creation), None).expect("resolve");
    let snapshot = load::snapshot(&policy);
    load::persist(store.conn_mut(), &snapshot, 0).expect("persist");

    conductor_store::with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT INTO project (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES ('p-1', '/repo', 'blake3:repo', 'main', 'blake3:cfg', 0)",
            [],
        )?;
        tx.execute(
            "INSERT INTO plan_version (id, project_id, version, content_hash, state, source_path)
             VALUES ('pv-1', 'p-1', 1, 'blake3:plan', 'DRAFT', '.conductor/plans/v1/plan.yaml')",
            [],
        )?;
        Ok(())
    })
    .expect("seed parents");

    let task_id = TaskId::new("T-0001").expect("task id");
    store
        .create_task(
            &NewTask {
                id: task_id.clone(),
                plan_version_id: "pv-1".to_string(),
                slice_id: "S7".to_string(),
                scope_globs: vec!["crates/**".to_string()],
                verification_profile: "default".to_string(),
                attempt_budget: 3,
            },
            0,
        )
        .expect("create task");
    store
        .create_run(
            &NewRun {
                id: RunId::new("r-0001").expect("run id"),
                task_id,
                policy_hash: snapshot.hash.clone(),
                base_commit: "abc".to_string(),
                run_branch: "conductor/r-0001".to_string(),
                target_branch: "main".to_string(),
            },
            0,
        )
        .expect("create run");
    drop(store);

    // The operator now removes the rule from the file entirely.
    std::fs::write(
        repo.path().join(".conductor").join("policy.yaml"),
        "policy:\n  rules: []\n",
    )
    .expect("loosen the file");

    let db_arg = arg(&db);
    let out = run(&[
        "policy",
        "explain",
        "dependency.add.runtime",
        "--run",
        "r-0001",
        "--store",
        &db_arg,
    ]);
    let text = stdout(&out);
    assert!(text.contains("effect:  deny"), "{text}");
    assert!(text.contains(&snapshot.hash), "{text}");

    // Positive control: without `--run`, the same command reads the file, which
    // no longer contains the rule — so the deny above came from the pin.
    let out = run(&[
        "policy",
        "explain",
        "dependency.add.runtime",
        "--repo",
        &arg(repo.path()),
    ]);
    assert!(stdout(&out).contains("effect:  allow"));
}

#[test]
fn a_malformed_policy_file_stops_the_command_rather_than_explaining_nothing() {
    let repo = repo_with_policy("policy:\n  rules:\n    - {id: a, action: git.push}\n");
    let out = run(&["policy", "explain", "git.push", "--repo", &arg(repo.path())]);

    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("effect"), "{stderr}");

    // Positive control: a repository with no policy file at all explains fine —
    // "absent" and "malformed" are different situations.
    let empty = tempfile::tempdir().expect("tempdir");
    let out = run(&[
        "policy",
        "explain",
        "git.push",
        "--repo",
        &arg(empty.path()),
    ]);
    // `git.push` is a built-in invariant, so an empty policy still denies it.
    assert!(stdout(&out).contains("never-push-to-a-remote"));
}
