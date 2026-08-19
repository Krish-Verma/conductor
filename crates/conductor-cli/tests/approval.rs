//! `conductor approval {list,show,approve,deny,revoke}` over the control socket
//! — master plan §4.3, §7.1 and §7.3.
//!
//! # Why every mutating verb here goes over a socket
//!
//! §4.3 is blunt about what the socket is *not*: *"A `0600` unix socket does not
//! distinguish a human from a same-user subprocess"*. What it **is** is the one
//! surface a *sandboxed* agent cannot reach (M6/M10), and the one surface that
//! does not require Conductor to trust a file. A grant written by dropping a
//! file somewhere would be grantable by anything that can write that path; a
//! grant that only exists because a connection arrived on
//! `$HOME/.conductor/conductor.sock` is at least gated by the tier the host
//! actually measures. So the CLI has **no** direct-to-store granting path at
//! all, and [`granting_without_a_live_socket_creates_no_grant`] is the test that
//! keeps it that way.
//!
//! # Exit codes are §7.2's, not invented here
//!
//! `0` success · `2` not initialized / store unhealthy — which is what "no
//! control socket is listening" is · `3` **action required — approval or review
//! pending**, the scriptable "human needed" slot, which is exactly what a
//! non-empty `approval list` means · `64` usage.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use conductor_core::{RunId, TaskId};
use conductor_run::approval::binding::Binding;
use conductor_run::approval::kind::{Expiry, Subject};
use conductor_run::approval::store::{GrantState, NewApprovalRequest, RequestState};
use conductor_run::approval::{Authorization, authorize, store as approvals};
use conductor_run::policy::evaluate::{Decision, Request, evaluate};
use conductor_run::policy::load;
use conductor_run::policy::model::{Action, Fact, FactSet, Origin, ResolvedPolicy, Scope};
use conductor_store::{NewRun, NewTask, Store};

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");
const RUN: &str = "r-0041";

/// A policy that gates `dependency.add.runtime` behind a human.
const GATING_YAML: &str = r#"
policy:
  rules:
    - id: global.runtime-dependency
      action: dependency.add.runtime
      effect: require_approval
"#;

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

/// A store with one run, plus a socket directory that is *not* `$HOME` so the
/// test never touches the operator's real control surface.
struct World {
    dir: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        World { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("conductor.db")
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join(".conductor").join("conductor.sock")
    }

    fn store(&self) -> Store {
        Store::open_existing(self.db()).expect("open store")
    }

    fn arg(path: &Path) -> String {
        path.to_str().expect("utf8 path").to_string()
    }
}

fn policy() -> ResolvedPolicy {
    let document = load::parse_document(GATING_YAML, Origin::Global).expect("parse");
    load::resolve_documents(Some(document), None, None).expect("resolve")
}

/// Seed the parent rows the schema requires plus one run pinned to `policy`.
fn seed(world: &World, policy: &ResolvedPolicy) {
    let mut store = Store::open_or_create(world.db()).expect("create store");
    let snapshot = load::snapshot(policy);
    conductor_store::with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT OR IGNORE INTO project
               (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES ('p-1', '/repo', 'blake3:repo', 'main', 'blake3:cfg', 0)",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO plan_version
               (id, project_id, version, content_hash, state, source_path)
             VALUES ('pv-1', 'p-1', 1, 'blake3:plan', 'DRAFT', '.conductor/plans/v1/plan.yaml')",
            [],
        )?;
        Ok(())
    })
    .expect("seed parents");
    load::persist(store.conn_mut(), &snapshot, 0).expect("persist snapshot");

    let task_id = TaskId::new("T-0041").expect("task id");
    store
        .create_task(
            &NewTask {
                id: task_id.clone(),
                plan_version_id: "pv-1".to_string(),
                slice_id: "S8".to_string(),
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
                id: RunId::new(RUN).expect("run id"),
                task_id,
                policy_hash: snapshot.hash.clone(),
                base_commit: "abc123".to_string(),
                run_branch: format!("conductor/{RUN}"),
                target_branch: "main".to_string(),
            },
            0,
        )
        .expect("create run");
}

fn dependency_facts(name: &str) -> FactSet {
    let mut facts = FactSet::new();
    facts.push(Fact::deterministic("dependency", name));
    facts.push(Fact::deterministic("manifest", "Cargo.toml"));
    facts
}

fn decision(policy: &ResolvedPolicy, name: &str) -> Decision {
    let request = Request::new(Action::parse("dependency.add.runtime"), 1_000)
        .with_facts(dependency_facts(name))
        .with_context("run", RUN);
    evaluate(policy, &request)
}

fn run_scope() -> Scope {
    Scope::from_pairs([("run".to_string(), RUN.to_string())])
}

/// Record one `REQUESTED` policy approval, as §4.4's `require_approval` would.
fn request(world: &World, id: &str, decision: &Decision, expires_at_ms: i64) {
    let mut store = world.store();
    let new = NewApprovalRequest {
        id: id.to_string(),
        subject: Subject::PolicyAction {
            action: decision.action.clone(),
        },
        run_id: Some(RunId::new(RUN).expect("run id")),
        facts: decision.facts.iter().cloned().collect(),
        policy_hash: decision.policy_hash.clone(),
        matched_rules: decision.matched.iter().map(|m| m.rule_id.clone()).collect(),
        explanation: "adds a runtime dependency not present at base commit".to_string(),
        evidence_ref: None,
        expires: Expiry::At(expires_at_ms),
    };
    approvals::request(store.conn_mut(), &new, 500).expect("record the request");
}

// ---------------------------------------------------------------------------
// driving the binary
// ---------------------------------------------------------------------------

/// A server process and the socket it published.
struct Server {
    child: Child,
}

impl Server {
    /// Start `conductor approval serve` and wait until the socket is published.
    ///
    /// The wait is generous because M29 measured macOS taking 21.7 s to scan a
    /// freshly built binary before its first instruction runs; a short timeout
    /// here would make a cold binary look like a broken one.
    fn start(world: &World) -> Server {
        let child = Command::new(CONDUCTOR)
            .args([
                "approval",
                "serve",
                "--store",
                &World::arg(&world.db()),
                "--socket",
                &World::arg(&world.socket()),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"));
        let server = Server { child };
        wait_for_socket(&world.socket());
        server
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait until something is genuinely *listening*, not merely until a file
/// exists.
///
/// Existence is the wrong question: one of the tests below deliberately leaves a
/// **regular file** at the socket path to stand for a socket a dead process left
/// behind, and a helper that stopped at `path.exists()` would return before the
/// server had replaced it and then blame the server for the fixture.
fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("nothing was listening at {}", path.display());
}

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

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or_else(|| {
        panic!(
            "no exit code; stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

// ---------------------------------------------------------------------------
// the socket itself (§4.3, §7.3)
// ---------------------------------------------------------------------------

#[test]
fn the_control_socket_is_published_at_0600_inside_a_0700_directory() {
    // §7.3: "Unix socket at `$HOME/.conductor/conductor.sock`, mode `0600`".
    // The directory matters as much as the file: §4.3 tier A rests on the agent
    // being unable to *write* `$HOME/.conductor/`, "so it cannot squat or
    // replace it".
    let world = World::new();
    seed(&world, &policy());
    let _server = Server::start(&world);

    assert_eq!(
        mode_of(&world.socket()),
        0o600,
        "the control socket must be 0600"
    );
    assert_eq!(
        mode_of(world.socket().parent().expect("parent")),
        0o700,
        "the socket directory must be 0700"
    );
}

#[test]
fn a_stale_socket_left_by_a_dead_process_is_replaced_and_is_not_fatal() {
    // A machine that lost power mid-run comes back with a socket file whose
    // server is gone. Refusing to start there would mean a crash makes
    // approvals permanently unreachable — the opposite of §4.7's "restart
    // converges with no human input".
    let world = World::new();
    seed(&world, &policy());
    std::fs::create_dir_all(world.socket().parent().expect("parent")).expect("mkdir");
    std::fs::write(world.socket(), b"not a socket, and nothing is listening").expect("write");

    let _server = Server::start(&world);
    let out = run(&[
        "approval",
        "list",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(
        code(&out),
        0,
        "a stale socket must be replaced, not fatal; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_second_server_does_not_steal_a_socket_a_live_one_is_serving() {
    // The other half of stale handling. "Replace anything that is there" would
    // let a second process silently take over the name and leave the first
    // accepting on an inode nothing can reach.
    let world = World::new();
    seed(&world, &policy());
    let _server = Server::start(&world);

    let out = run(&[
        "approval",
        "serve",
        "--store",
        &World::arg(&world.db()),
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_ne!(code(&out), 0, "a live socket must not be stolen");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already"),
        "the refusal must say why: {stderr}"
    );
}

#[test]
fn deleting_the_socket_while_serving_is_detected_rather_than_served_into_the_void() {
    // Failure injection named by the slice: "socket file deleted". A listener
    // whose path was unlinked keeps accepting on an inode no client can name.
    // Silently continuing would mean `conductor approval approve` hangs forever
    // against a server that believes it is healthy.
    let world = World::new();
    seed(&world, &policy());
    let mut server = Command::new(CONDUCTOR)
        .args([
            "approval",
            "serve",
            "--store",
            &World::arg(&world.db()),
            "--socket",
            &World::arg(&world.socket()),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"));
    // **Sync on the banner, not on connectability.** `wait_for_socket` returns as
    // soon as `connect` succeeds, and that is true the instant `publish`'s atomic
    // rename lands — while two `stat`s of the socket are still ahead: `publish`
    // reads dev+inode immediately after the rename, and `approval serve` then
    // calls `socket.mode()` for §4.3's tier line. Unlinking in that window makes
    // whichever comes next fail with a bare `stat … No such file or directory`,
    // the server exits non-zero having **never served**, and this test fails on
    // its *message* while the product behaved correctly — it did fail closed.
    //
    // Those two call sites are the only places in `socket.rs` that can emit that
    // string for the socket path, and both are upstream of `serve()`, so the
    // attribution is from the message alone rather than from a guess.
    //
    // That is a different scenario from the one this test is named for. The banner
    // is printed after `mode()` and immediately before `serve()`, so reading one
    // line of it is a deterministic "the server is now serving" — which is the
    // precondition the assertion below actually depends on.
    //
    // Observed as a real intermittent failure, not a hypothetical: one full-suite
    // run reported `stat /var/folders/…/conductor.sock: No such file or directory`
    // at this assertion.
    wait_for_socket(&world.socket());
    let mut banner = String::new();
    {
        let pipe = server.stdout.take().expect("stdout is piped");
        let mut reader = BufReader::new(pipe);
        reader
            .read_line(&mut banner)
            .expect("the server must announce itself before it serves");
    }
    assert!(
        banner.contains("serving"),
        "the sync point must be the serving banner: {banner}"
    );

    std::fs::remove_file(world.socket()).expect("unlink the socket");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = server.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = server.kill();
            panic!("the server did not notice its socket had been unlinked");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_ne!(status.code(), Some(0), "an unpublished socket is a failure");
    let mut stderr = String::new();
    if let Some(pipe) = server.stderr.take() {
        let mut reader = BufReader::new(pipe);
        while reader.read_line(&mut stderr).unwrap_or(0) > 0 {}
    }
    assert!(
        stderr.contains("no longer"),
        "the server must say what happened: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// the verbs (§7.1, §7.2)
// ---------------------------------------------------------------------------

#[test]
fn listing_a_pending_approval_exits_3_because_a_human_is_needed() {
    // §7.2: "3 action required — approval or review pending ← scriptable
    // 'human needed'".
    let world = World::new();
    let policy = policy();
    seed(&world, &policy);
    request(
        &world,
        "AR-0031",
        &decision(&policy, "serde_yaml"),
        10_000_000,
    );
    let _server = Server::start(&world);

    let out = run(&[
        "approval",
        "list",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(code(&out), 3, "a pending approval is §7.2's code 3");
    let body = json(&out);
    assert_eq!(body["pending"][0]["id"], "AR-0031");
    assert_eq!(body["pending"][0]["kind"], "POLICY_APPROVAL");
}

#[test]
fn listing_with_nothing_pending_exits_0() {
    // POSITIVE CONTROL for the test above: without this, "exit 3" could be what
    // `approval list` always returns, and the assertion would be about the
    // command rather than about pendency.
    let world = World::new();
    seed(&world, &policy());
    let _server = Server::start(&world);

    let out = run(&[
        "approval",
        "list",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(code(&out), 0, "nothing pending is success");
    assert_eq!(json(&out)["pending"].as_array().expect("array").len(), 0);
}

#[test]
fn showing_a_request_prints_the_kind_the_facts_and_the_rules_that_matched() {
    let world = World::new();
    let policy = policy();
    seed(&world, &policy);
    request(
        &world,
        "AR-0031",
        &decision(&policy, "serde_yaml"),
        10_000_000,
    );
    let _server = Server::start(&world);

    let out = run(&[
        "approval",
        "show",
        "AR-0031",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(code(&out), 3, "an unanswered request still needs a human");
    let body = json(&out);
    assert_eq!(body["request"]["kind"], "POLICY_APPROVAL");
    assert_eq!(body["request"]["action"], "dependency.add.runtime");
    assert_eq!(body["request"]["state"], "REQUESTED");
    assert!(
        body["request"]["matched_rules"]
            .as_array()
            .expect("array")
            .iter()
            .any(|rule| rule == "global.runtime-dependency"),
        "the explanation must name the rule: {body}"
    );
}

#[test]
fn approving_over_the_socket_authorizes_that_operation_and_no_other() {
    let world = World::new();
    let policy = policy();
    seed(&world, &policy);
    let serde_yaml = decision(&policy, "serde_yaml");
    request(&world, "AR-0031", &serde_yaml, 10_000_000);
    let _server = Server::start(&world);

    let out = run(&[
        "approval",
        "approve",
        "AR-0031",
        "--ttl",
        "3600",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(
        code(&out),
        0,
        "approving must succeed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let grant_id = json(&out)["grant"]["id"]
        .as_str()
        .expect("a grant id")
        .to_string();

    let store = world.store();
    assert_eq!(
        approvals::request_row(store.conn(), "AR-0031")
            .expect("read")
            .expect("row")
            .state,
        RequestState::Granted
    );

    // The grant authorizes exactly what it was issued for …
    match authorize::authorize(store.conn(), &serde_yaml, &run_scope(), 700).expect("authorize") {
        Authorization::Authorized { grant_id: found } => assert_eq!(found, grant_id),
        other => panic!("the grant must authorize its own operation: {other:?}"),
    }
    // … and §4.3's "cannot authorize `…:bar` (different facts)" still holds for
    // a grant that arrived over the socket.
    let other_dependency = decision(&policy, "serde_json");
    match authorize::authorize(store.conn(), &other_dependency, &run_scope(), 700)
        .expect("authorize")
    {
        Authorization::Refused(_) => {}
        other => panic!("a socket grant must not be broader than a local one: {other:?}"),
    }
}

#[test]
fn approving_defaults_to_one_shot_because_4_3_says_reuse_is_false() {
    let world = World::new();
    let policy = policy();
    seed(&world, &policy);
    request(
        &world,
        "AR-0031",
        &decision(&policy, "serde_yaml"),
        10_000_000,
    );
    let _server = Server::start(&world);

    let out = run(&[
        "approval",
        "approve",
        "AR-0031",
        "--ttl",
        "3600",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(code(&out), 0);
    assert_eq!(json(&out)["grant"]["reuse"], false);
}

#[test]
fn denying_over_the_socket_produces_no_grant() {
    let world = World::new();
    let policy = policy();
    seed(&world, &policy);
    let serde_yaml = decision(&policy, "serde_yaml");
    request(&world, "AR-0031", &serde_yaml, 10_000_000);
    let _server = Server::start(&world);

    let out = run(&[
        "approval",
        "deny",
        "AR-0031",
        "--reason",
        "not this one",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(code(&out), 0);

    let store = world.store();
    assert_eq!(
        approvals::request_row(store.conn(), "AR-0031")
            .expect("read")
            .expect("row")
            .state,
        RequestState::Denied
    );
    match authorize::authorize(store.conn(), &serde_yaml, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(_) => {}
        other => panic!("a denied request authorizes nothing: {other:?}"),
    }
}

#[test]
fn revoking_over_the_socket_takes_the_grant_back() {
    let world = World::new();
    let policy = policy();
    seed(&world, &policy);
    let serde_yaml = decision(&policy, "serde_yaml");
    request(&world, "AR-0031", &serde_yaml, 10_000_000);
    let _server = Server::start(&world);

    let approved = run(&[
        "approval",
        "approve",
        "AR-0031",
        "--ttl",
        "3600",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(code(&approved), 0);
    let grant_id = json(&approved)["grant"]["id"]
        .as_str()
        .expect("a grant id")
        .to_string();

    // POSITIVE CONTROL: it authorizes before the revocation, so the refusal
    // afterwards is the revocation and not an empty fixture.
    {
        let store = world.store();
        match authorize::authorize(store.conn(), &serde_yaml, &run_scope(), 700).expect("authorize")
        {
            Authorization::Authorized { .. } => {}
            other => panic!("the grant must work before it is revoked: {other:?}"),
        }
    }

    let out = run(&[
        "approval",
        "revoke",
        &grant_id,
        "--reason",
        "changed my mind",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(
        code(&out),
        0,
        "revoking must succeed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let store = world.store();
    assert_eq!(
        approvals::grant_row(store.conn(), &grant_id)
            .expect("read")
            .expect("row")
            .state,
        GrantState::Revoked
    );
    match authorize::authorize(store.conn(), &serde_yaml, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(_) => {}
        other => panic!("a revoked grant authorizes nothing: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// the layering the socket exists to create
// ---------------------------------------------------------------------------

#[test]
fn granting_without_a_live_socket_creates_no_grant() {
    // §4.3: granting is a mutating operation and goes over the socket. There is
    // no fallback that writes the grant directly, because a fallback is exactly
    // the file an agent could produce.
    let world = World::new();
    let policy = policy();
    seed(&world, &policy);
    let serde_yaml = decision(&policy, "serde_yaml");
    request(&world, "AR-0031", &serde_yaml, 10_000_000);

    let out = run(&[
        "approval",
        "approve",
        "AR-0031",
        "--ttl",
        "3600",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
        "--store",
        &World::arg(&world.db()),
    ]);
    assert_eq!(
        code(&out),
        2,
        "no control socket is §7.2's 'not initialized'"
    );

    let store = world.store();
    assert_eq!(
        approvals::request_row(store.conn(), "AR-0031")
            .expect("read")
            .expect("row")
            .state,
        RequestState::Requested,
        "the request must be untouched"
    );
    match authorize::authorize(store.conn(), &serde_yaml, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(_) => {}
        other => panic!("no socket must mean no grant: {other:?}"),
    }
    // And nothing wrote a grant row by another route.
    let grants: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM approval_grant", [], |row| row.get(0))
        .expect("count grants");
    assert_eq!(grants, 0, "a grant appeared without a socket");
}

#[test]
fn the_binding_a_socket_grant_carries_is_the_one_the_runtime_recomputes() {
    // The socket path must not become a second way of computing a binding: if
    // the server stored anything other than `blake3(action ‖ canonical(facts) ‖
    // policy_hash ‖ scope)`, the grant would authorize nothing and the failure
    // would look like a policy problem.
    let world = World::new();
    let policy = policy();
    seed(&world, &policy);
    let serde_yaml = decision(&policy, "serde_yaml");
    request(&world, "AR-0031", &serde_yaml, 10_000_000);
    let _server = Server::start(&world);

    let out = run(&[
        "approval",
        "approve",
        "AR-0031",
        "--ttl",
        "3600",
        "--json",
        "--socket",
        &World::arg(&world.socket()),
    ]);
    assert_eq!(code(&out), 0);

    let expected = Binding::for_decision(&serde_yaml, &run_scope()).hash();
    let store = world.store();
    let row = approvals::grant_row(
        store.conn(),
        json(&out)["grant"]["id"].as_str().expect("id"),
    )
    .expect("read")
    .expect("row");
    assert_eq!(row.stored_binding, expected);
}

#[test]
fn an_unknown_verb_is_a_usage_error() {
    let out = run(&["approval", "bless", "AR-0031"]);
    assert_eq!(code(&out), 64, "§7.2: 64 usage error");
}
