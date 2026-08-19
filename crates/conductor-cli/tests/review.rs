//! `conductor review export | import` — master plan §6.5, §5.2, §4.5 and §7.2.
//!
//! > **Review decision** (imported): `accept | repair | revise_plan | pause |
//! > stop` … Importing is a **mutating** operation and goes through the control
//! > socket, never a file an agent could write.
//!
//! # What these tests are trying to break
//!
//! Three different things, and they fail differently on purpose.
//!
//! 1. **The decision must reach the store only through the socket.** Asserted
//!    the way `plan_approve.rs` asserts it for `plan approve` — a source scan for
//!    a database handle, a source scan for the decision-applying functions, one
//!    server arm, and an *experiment* that runs the real command with no server
//!    and observes that nothing was decided. The scan is over a **region** of
//!    `review.rs` rather than the whole file, because `review export` legitimately
//!    holds a store: see the module's own docs, and see
//!    [`the_scanner_finds_the_forbidden_names_where_they_actually_live`] for the
//!    positive control that stops the region being empty or the needles being
//!    dead.
//! 2. **A decision must be bound to the packet a human read.** Four binding
//!    fields plus the packet hash, each with its own refusal; a hash changed by
//!    one character is a tampered decision.
//! 3. **`accept` must not be a way to write `COMPLETE` for free.** ADR-0019:
//!    acceptance resolves §4.5's criterion 6 and nothing else, so a run with a
//!    failing required check is refused *and the refusal names the criterion*.
//!
//! # Every negative has a positive beside it
//!
//! A review system that refuses everything passes tests 3–8 trivially. So the
//! first test in the file is the one that must succeed:
//! [`exporting_then_accepting_over_the_control_socket_completes_the_run_and_the_task`]
//! drives a real export, a hand-edited decision file and a real import over a
//! live socket, and requires the run *and* the task to reach `COMPLETE`. Every
//! refusal below is a mutation of that flow.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use conductor_core::{PlanVersionId, RunId, TaskId, TaskState, VerificationOutcome};
use conductor_store::Store;

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

// ---------------------------------------------------------------------------
// the scan
// ---------------------------------------------------------------------------

/// The file holding both halves of `conductor review`.
const SOURCE: &str = "crates/conductor-cli/src/review.rs";

/// The marker that opens the `review import` client region.
const CLIENT_BEGIN: &str = "// ==== BEGIN `review import` client — no store handle ====";

/// The marker that closes it.
const CLIENT_END: &str = "// ==== END `review import` client ====";

/// Names that would give the import client a database handle.
///
/// §4.3 asks for "read-only verbs only"; S8's answer was stronger and easier to
/// check — no store surface at all, so there is no read-only subset to argue
/// about one call at a time. `plan_approve.rs` applies the same three needles to
/// `plan.rs`.
const FORBIDDEN_HANDLES: &[&str] = &["conductor_store", "Store::open", "rusqlite"];

/// Names that exist only on the path that *applies* a review decision.
///
/// Code-shaped rather than the bare word "review", for the reason S8's scan
/// records: a bare word matches payloads as well as code paths.
/// [`the_scanner_finds_the_forbidden_names_where_they_actually_live`] asserts
/// every one of them matches outside the client region, so a needle that never
/// matches anything cannot quietly shrink the scan.
const FORBIDDEN_DECISION: &[&str] = &[
    "apply_review_decision",
    "record_review_decision",
    "ReviewOutcome",
    "completion::evaluate",
    "resolve_finding",
    "GrantOptions",
    "NewApprovalRequest",
    "register_plan_version",
    "set_task_state",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/conductor-cli has two ancestors")
        .to_path_buf()
}

/// Read a source file, failing loudly rather than scanning an empty string.
fn read_source(relative: &str) -> String {
    let path = repo_root().join(relative);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("the scan must read {}: {err}", path.display()));
    assert!(
        source.len() > 200,
        "{} is suspiciously short ({} bytes); a scan over nothing proves nothing",
        path.display(),
        source.len()
    );
    source
}

/// The import client's text, and everything that is not it.
///
/// Returned as a pair so every scan of the region is matched by a scan of its
/// complement. A region that shrank to nothing would satisfy every "must not
/// contain" assertion in this file, and the complement is what catches it.
fn client_region() -> (String, String) {
    let source = read_source(SOURCE);
    assert_eq!(
        source.matches(CLIENT_BEGIN).count(),
        1,
        "{SOURCE} must mark the import client's start exactly once; a duplicate \
         or a missing marker makes the scanned region something other than the \
         client"
    );
    assert_eq!(
        source.matches(CLIENT_END).count(),
        1,
        "{SOURCE} must mark its end exactly once"
    );

    let start = source.find(CLIENT_BEGIN).expect("the opening marker");
    let end = source.find(CLIENT_END).expect("the closing marker");
    assert!(
        start < end,
        "{SOURCE}'s client markers are in the wrong order, so the region is empty"
    );
    let region = source[start..end].to_string();
    assert!(
        region.len() > 500,
        "the import client region is {} bytes; a scan over nothing proves nothing",
        region.len()
    );
    let outside = format!("{}{}", &source[..start], &source[end..]);
    (region, outside)
}

fn hits<'n>(source: &str, needles: &[&'n str]) -> Vec<&'n str> {
    needles
        .iter()
        .copied()
        .filter(|needle| source.contains(needle))
        .collect()
}

#[test]
fn the_review_import_client_holds_no_store_handle() {
    // This fails the moment somebody gives the client a "just one read-only
    // query" shortcut — which is how a socket-only verb stops being one.
    let (region, _) = client_region();
    let found = hits(&region, FORBIDDEN_HANDLES);
    assert!(
        found.is_empty(),
        "the `review import` client in {SOURCE} names {found:?}, so it can reach \
         the database. §6.5 makes importing a decision a mutating operation that \
         \"goes through the control socket, never a file an agent could write\"; \
         a client that can write the decision itself is the second door, whatever \
         it checks first."
    );
}

#[test]
fn the_review_import_client_lacks_the_decision_application_code_path() {
    let (region, _) = client_region();
    let found = hits(&region, FORBIDDEN_DECISION);
    assert!(
        found.is_empty(),
        "the `review import` client in {SOURCE} names {found:?}, which exists \
         only where a decision is applied. §5.2's exit from AWAITING_REVIEW and \
         §4.5's completion gate belong behind the socket."
    );
}

#[test]
fn the_review_import_client_reaches_the_server_only_through_the_control_socket() {
    // The positive half of the two scans above: having proved the client cannot
    // apply a decision, this proves it does something instead of nothing, and
    // that the something is §7.3's socket.
    let (region, _) = client_region();
    assert!(
        region.contains("socket::call"),
        "the client must reach the control socket; without this the scans above \
         are satisfied by a command that does nothing at all"
    );
    assert!(
        region.contains("IMPORT_METHOD"),
        "the client must name the RPC method it calls"
    );
}

#[test]
fn the_control_socket_has_exactly_one_server_for_review_import() {
    // A second dispatcher would be a second door that still went "over a socket"
    // and still passed every other test in this file. §4.3 allows one server.
    let src = repo_root().join("crates/conductor-cli/src");
    let mut servers = Vec::new();
    for entry in std::fs::read_dir(&src).expect("read crates/conductor-cli/src") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .to_string();
        let source = std::fs::read_to_string(&path).expect("read");
        // The client names the method to *call* it; a server names it in a
        // dispatch arm, which is the only place it sits beside a `=>`.
        if source.contains("\"review.import\" =>") {
            servers.push(name);
        }
    }
    assert_eq!(
        servers,
        vec!["approval.rs".to_string()],
        "`review.import` must be dispatched by the one server §4.3 allows"
    );
}

#[test]
fn the_scanner_finds_the_forbidden_names_where_they_actually_live() {
    // Without this, every assertion above could be satisfied by a scanner that
    // read nothing, extracted an empty region, or carried needles that match no
    // code anywhere. Each needle must match *outside* the client region — which
    // is where the export path and the import server genuinely live.
    let (region, outside) = client_region();
    for needle in FORBIDDEN_HANDLES {
        assert!(
            outside.contains(needle),
            "{needle:?} matches nothing outside the client region, so forbidding \
             it inside proves nothing"
        );
    }
    for needle in FORBIDDEN_DECISION {
        assert!(
            outside.contains(needle),
            "{needle:?} matches nothing outside the client region, so it \
             contributes nothing to the scan"
        );
    }
    // And the extraction is real: the region is a proper, non-empty part of a
    // file that is much larger than it.
    assert!(
        region.len() < read_source(SOURCE).len() / 2,
        "the extracted region is most of the file, so the split is not doing \
         what the scan assumes"
    );
}

// ---------------------------------------------------------------------------
// the experiment
// ---------------------------------------------------------------------------

/// The project id `conductor init` derives from a directory called `repo`.
///
/// Asserted rather than assumed by [`World::new`]: the plan document has to
/// declare the same id, and a change to the derivation would otherwise show up
/// as an unrelated §3.7 refusal.
const PROJECT_ID: &str = "p-repo";

/// The tree every seeded verification result is bound to.
///
/// §4.5's criterion 1 is "PASS **at the current tree hash**", and the review
/// path reads that tree off the newest recorded check. One constant means the
/// tests can move a *single* check onto a different tree to prove the binding
/// bites.
const TREE: &str = "tree-review-0001";

const RUN: &str = "r-0001";
const TASK: &str = "T-0001";
const REVIEW: &str = "rv-0001";

const VERIFICATION_YAML: &str = "\
verification:
  toolchain_fingerprint: []
  required:
    - id: unit-tests
      command: cargo test
  invariants: []
";

/// A plan §3.7 accepts whose one acceptance criterion is bound to a real check.
///
/// The scaffold `conductor init` writes marks `AC-1` `manual: true`, which
/// §3.7's escape hatch says "forces a review boundary" — and which §4.5's
/// criterion 5 can never satisfy. A fixture built on it could not reach
/// `COMPLETE` by any route, so the positive control would be vacuous.
fn plan_yaml(version: u32) -> String {
    format!(
        "plan:\n  id: {PROJECT_ID}\n  version: {version}\n  \
         objective: \"Exercise the review bridge.\"\n  milestones:\n    - id: M-01\n      \
         title: \"Review\"\n      slices:\n        - id: S-13\n          \
         title: \"Review bridge\"\n          tasks:\n            - id: {TASK}\n              \
         objective: \"Produce something a human reviews.\"\n              \
         rationale: \"S13 needs a task to review.\"\n              depends_on: []\n              \
         scope:\n                allowed_globs: [\"src/**\"]\n                \
         forbidden_globs: [\".conductor/**\"]\n              \
         verification_profile: .conductor/verification.yaml\n              \
         attempt_budget: 3\n              acceptance_criteria:\n                - id: AC-1\n                  \
         statement: \"The unit tests pass.\"\n                  verified_by: [unit-tests]\n"
    )
}

/// What the agent claimed, so §6.5's side-by-side has a left-hand column.
const REPORT_JSON: &str = r#"{
  "claim": "COMPLETE",
  "task_id": "T-0001",
  "summary": "Wrote the thing.",
  "files_touched": ["src/thing.rs"],
  "commands_run": ["cargo test"],
  "acceptance_criteria": ["AC-1"],
  "deviations": ["renamed the module"],
  "blockers": [],
  "unverified_claims": ["it is also faster"]
}"#;

/// A git repository with a `.conductor/`, a store beside it, and a socket path
/// that is **not** `$HOME` — the test must never touch the operator's real
/// control surface.
struct World {
    dir: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let world = World {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        let repo = world.repo();
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "review@example.invalid"],
            vec!["config", "user.name", "Review Test"],
        ] {
            conductor_git::run_git_ok(&repo, &args).expect("git");
        }

        let out = Command::new(CONDUCTOR)
            .args(["init", "--repo", &arg(&repo)])
            .output()
            .expect("spawn init");
        assert!(
            out.status.success(),
            "the fixture starts from a scaffold: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let project = std::fs::read_to_string(repo.join(".conductor/project.yaml")).expect("read");
        assert!(
            project.contains(&format!("id: {PROJECT_ID}")),
            "the plan fixture declares {PROJECT_ID}, and §3.7 refuses a plan whose \
             id disagrees with the project: {project}"
        );

        std::fs::write(repo.join(".conductor/verification.yaml"), VERIFICATION_YAML)
            .expect("write verification.yaml");
        std::fs::write(repo.join(".conductor/plans/v1/plan.yaml"), plan_yaml(1))
            .expect("write plan v1");
        // v2 exists on disk and is registered by nothing. `revise_plan` is what
        // registers it, and the other tests prove it stays unregistered.
        std::fs::create_dir_all(repo.join(".conductor/plans/v2")).expect("mkdir v2");
        std::fs::write(repo.join(".conductor/plans/v2/plan.yaml"), plan_yaml(2))
            .expect("write plan v2");

        conductor_git::run_git_ok(&repo, &["add", "-A"]).expect("git add");
        conductor_git::run_git_ok(&repo, &["commit", "-q", "-m", "conductor init"])
            .expect("git commit");

        Store::open_or_create(world.db()).expect("create the store");
        world
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("conductor.db")
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join(".conductor").join("conductor.sock")
    }

    fn artifacts(&self) -> conductor_run::ArtifactRoot {
        conductor_run::ArtifactRoot::new(self.dir.path().join("artifacts"))
    }

    fn store(&self) -> Store {
        Store::open_existing(self.db()).expect("open the store")
    }

    fn count(&self, sql: &str) -> i64 {
        self.store()
            .conn()
            .query_row(sql, [], |row| row.get(0))
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn run_state(&self) -> String {
        self.store()
            .conn()
            .query_row("SELECT state FROM run WHERE id = ?1", [RUN], |row| {
                row.get(0)
            })
            .expect("a run row")
    }

    fn task_state(&self) -> TaskState {
        self.store()
            .task(&TaskId::new(TASK).expect("id"))
            .expect("read")
            .expect("a task row")
            .state
    }

    fn review_state(&self) -> String {
        self.store()
            .review(REVIEW)
            .expect("read")
            .expect("a review row")
            .state
            .as_str()
            .to_string()
    }

    /// `conductor plan approve 1` over the live socket — the only thing that
    /// creates the task this review is about (§5.2, S12's materializer).
    fn approve_plan(&self) -> Output {
        Command::new(CONDUCTOR)
            .args([
                "plan",
                "approve",
                "1",
                "--repo",
                &arg(&self.repo()),
                "--socket",
                &arg(&self.socket()),
                "--json",
            ])
            .output()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
    }

    fn export(&self) -> Output {
        self.export_with(&["--run", RUN])
    }

    fn export_with(&self, extra: &[&str]) -> Output {
        let mut args = vec!["review", "export", "--store"];
        let db = arg(&self.db());
        args.push(&db);
        args.push("--json");
        args.extend_from_slice(extra);
        Command::new(CONDUCTOR)
            .args(&args)
            .output()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
    }

    fn import(&self, file: &Path) -> Output {
        Command::new(CONDUCTOR)
            .args([
                "review",
                "import",
                &arg(file),
                "--socket",
                &arg(&self.socket()),
                "--json",
            ])
            .output()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
    }
}

fn arg(path: &Path) -> String {
    path.to_str().expect("utf8 path").to_string()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or_else(|| {
        panic!(
            "no exit code; stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("--json must print JSON ({e}): {}", said(out)))
}

/// A `conductor approval serve` process and the socket it published.
struct Server {
    child: Child,
}

impl Server {
    fn start(world: &World) -> Server {
        let child = Command::new(CONDUCTOR)
            .args([
                "approval",
                "serve",
                "--store",
                &arg(&world.db()),
                "--socket",
                &arg(&world.socket()),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"));
        let server = Server { child };
        // Generous, because M29 measured macOS taking 21.7 s to scan a freshly
        // built binary before its first instruction runs.
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(world.socket()).is_ok() {
                return server;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("nothing was listening at {}", world.socket().display());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How the seeded run's one check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// `unit-tests` passed at [`TREE`] — the shape a review can accept.
    Passing,
    /// `unit-tests` failed. §4.5 keeps verification authoritative, so accepting
    /// this review must still refuse.
    Failing,
}

/// Put the world in the state a review boundary leaves behind.
///
/// Walked through the real writers rather than hand-set: the run is created,
/// claimed, reconciled and routed to `AWAITING_REVIEW` by
/// `conductor_store::lease`, the task mirrors it through `set_task_state`'s
/// legality table, and the verification result goes in through §4.5's cache. The
/// only thing invented is the tree hash, because no agent ran.
///
/// **The one thing with no product caller yet is `open_review`.** §6.5's boundary
/// detection — which milestone, which repeated failure, which policy violation
/// opens a review — is the other half of S13 and is not this task's. Naming that
/// here rather than hiding it is the standing check CLAUDE.md asks for: the chain
/// is `open_review` (no product caller yet) → `review export` → `review import`,
/// and the last two links are exercised end to end below.
fn seed_review(world: &World, check: Check) {
    let repo = world.repo();
    let mut store = world.store();

    let policy_hash = {
        use conductor_run::policy::load;
        use conductor_run::policy::model::Origin;
        let project = load::load_document(&repo.join(load::PROJECT_POLICY_PATH), Origin::Project)
            .expect("the scaffolded policy loads");
        let resolved = load::resolve_documents(None, Some(project), None).expect("resolve");
        let snapshot = load::snapshot(&resolved);
        load::persist(store.conn_mut(), &snapshot, 1_000).expect("persist the snapshot");
        snapshot.hash
    };

    let run_id = RunId::new(RUN).expect("run id");
    let task_id = TaskId::new(TASK).expect("task id");
    store
        .create_run(
            &conductor_store::NewRun {
                id: run_id.clone(),
                task_id: task_id.clone(),
                policy_hash,
                base_commit: conductor_git::run_git(&repo, &["rev-parse", "HEAD"])
                    .expect("git")
                    .stdout_trimmed(),
                run_branch: format!("conductor/{TASK}/{RUN}"),
                target_branch: "main".to_string(),
            },
            1_000,
        )
        .expect("create the run");

    let claimed = store
        .claim_run(&run_id, "worker-1", 1_000, conductor_store::LEASE_MS)
        .expect("claim")
        .expect("the run is claimable");
    let fence = claimed.fence();

    // The artifact directory the attempt would have owned, with the report §6.5
    // puts on the left of the side-by-side. Claimed through the real writer, so
    // `review export`'s adoption of an existing worker's provenance is exercised
    // rather than bypassed.
    let owned = world
        .artifacts()
        .claim_attempt_dir(&run_id, 1, &conductor_run::Owner::new("worker-1", 4242))
        .expect("claim the attempt directory");
    owned
        .write_new("report.json", REPORT_JSON.as_bytes())
        .expect("write the agent report");

    let outcome = match check {
        Check::Passing => VerificationOutcome::Pass,
        Check::Failing => VerificationOutcome::Fail,
    };
    conductor_store::verification::record(
        store.conn_mut(),
        &fence,
        &conductor_store::verification::CacheKey {
            tree_hash: TREE,
            check_id: "unit-tests",
            command_hash: "blake3:command",
            toolchain_fingerprint: "blake3:toolchain",
        },
        &conductor_store::verification::VerificationRecord {
            id: "vc-0001".to_string(),
            attempt_id: None,
            commit_sha: "0000000000000000000000000000000000000000".to_string(),
            exit_code: Some(if outcome == VerificationOutcome::Pass {
                0
            } else {
                1
            }),
            duration_ms: Some(42),
            outcome,
            log_path: Some("artifacts/r-0001/verification/unit-tests-1.log".to_string()),
        },
        1_100,
    )
    .expect("record the check");

    // A finding, unresolved, because that is why a run reaches a review boundary
    // — and because `accept` resolving it is the step §4.8 reserves for a person.
    store
        .record_finding(
            &fence,
            "f-r-0001-CONTRADICTED",
            "CONTRADICTED",
            conductor_run::vertical::BLOCKING_FINDING_SEVERITY,
            "the agent claimed COMPLETE and the tree disagrees",
            1_150,
        )
        .expect("raise a finding");

    let attempt = conductor_core::Attempt::create(
        conductor_core::AttemptId::new("r-0001-a1").expect("id"),
        run_id.clone(),
        1,
    )
    .starting()
    .active(4242, Some(1))
    .exited(0);
    store
        .advance_to_reconciling(&fence, &attempt.evidence(), 1_200)
        .expect("RUNNING → RECONCILING");
    // The detail string is the one `policy_gate::route_reconciliation` writes,
    // because it is the only durable record of §4.8's verdict — `review export`
    // reads the verdict back out of it.
    store
        .route_reconciled(
            &fence,
            conductor_core::ReconciledRoute::AwaitingReview,
            "verdict=CONTRADICTED; a human must look",
            1_300,
        )
        .expect("RECONCILING → AWAITING_REVIEW");
    store.release_lease(&fence, 1_400).expect("release");

    for state in [
        TaskState::Ready,
        TaskState::Running,
        TaskState::Reconciling,
        TaskState::AwaitingReview,
    ] {
        store
            .set_task_state(&task_id, state)
            .unwrap_or_else(|e| panic!("task → {state}: {e}"));
    }

    let plan_version_id = PlanVersionId::new(
        store
            .task(&task_id)
            .expect("read")
            .expect("a task row")
            .plan_version_id,
    )
    .expect("plan version id");
    store
        .open_review(
            &conductor_store::NewReview {
                id: REVIEW.to_string(),
                run_id,
                task_id,
                plan_version_id,
                boundary: "task".to_string(),
            },
            1_500,
        )
        .expect("open the review");
}

/// Build the world, approve the plan, and leave a run waiting for a person.
fn waiting_for_review(check: Check) -> (World, Server) {
    let world = World::new();
    let server = Server::start(&world);
    let approved = world.approve_plan();
    assert_eq!(
        code(&approved),
        0,
        "the fixture needs an approved plan: {}",
        said(&approved)
    );
    seed_review(&world, check);
    (world, server)
}

/// Fill in the human's half of the decision stub.
///
/// `decision:` ships as `null`, which is the stub saying "you have not decided
/// yet". Replacing exactly that text is what a person does in an editor.
fn decide(path: &Path, decision: &str) {
    edit(path, "decision: null", &format!("decision: {decision}"));
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).expect("read the decision file");
    assert!(
        text.contains(from),
        "the decision stub does not contain {from:?}:\n{text}"
    );
    std::fs::write(path, text.replacen(from, to, 1)).expect("write the decision file");
}

fn exported(world: &World) -> PathBuf {
    let out = world.export();
    assert_eq!(code(&out), 0, "export: {}", said(&out));
    PathBuf::from(
        json(&out)["decision_path"]
            .as_str()
            .expect("export reports where the stub went"),
    )
}

// ---------------------------------------------------------------------------
// 1. the positive control
// ---------------------------------------------------------------------------

#[test]
fn exporting_then_accepting_over_the_control_socket_completes_the_run_and_the_task() {
    // THE POSITIVE CONTROL, and it is first on purpose: every refusal below is a
    // mutation of this flow, and a review bridge that refused everything would
    // pass all of them. §5.2's `AWAITING_REVIEW → COMPLETE` is the edge ADR-0019
    // says nothing could take before S13; this is the test that takes it.
    //
    // This fails if `accept` stops running §4.5's gate, if the token stops being
    // required, if the findings are no longer resolved (criterion 4 would refuse),
    // or if the task stops mirroring its run.
    let (world, _server) = waiting_for_review(Check::Passing);

    let stub = exported(&world);
    decide(&stub, "accept");

    let out = world.import(&stub);
    assert_eq!(code(&out), 0, "import: {}", said(&out));
    let result = json(&out);

    assert_eq!(result["decision"], "accept");
    assert_eq!(world.review_state(), "DECIDED");
    assert_eq!(world.run_state(), "COMPLETE");
    assert_eq!(
        world.task_state(),
        TaskState::Complete,
        "§5.2 draws one machine: the run mirrors its task, and a task left in \
         AWAITING_REVIEW while its run is COMPLETE is a state nothing else can read"
    );

    // §4.3: the acceptance exists because a grant of the right kind was made,
    // not because a command was run.
    assert_eq!(
        world.count("SELECT COUNT(*) FROM approval_request WHERE kind = 'REVIEW_ACCEPTANCE'"),
        1,
        "§4.3's four kinds never collapse; a review acceptance is a \
         REVIEW_ACCEPTANCE"
    );
    assert!(
        result["grant"].as_str().is_some(),
        "the grant that authorized the acceptance must be reported: {result}"
    );

    // §4.8: findings never auto-resolve, and this is the human path that resolves
    // one. It is a *precondition* of the completion above — criterion 4 counts
    // unresolved findings — so a change that stopped resolving them would fail
    // the state assertions rather than this one.
    assert_eq!(
        world.count("SELECT COUNT(*) FROM finding WHERE resolution IS NOT NULL"),
        1
    );
}

// ---------------------------------------------------------------------------
// 2. the other four decisions
// ---------------------------------------------------------------------------

#[test]
fn repair_sends_the_run_back_and_does_not_buy_it_another_attempt() {
    // ADR-0009 makes the repair ceiling durable: a review round trip must not be
    // a way to buy attempts. This fails if `repair` ever resets
    // `task.attempt_budget` on its way through.
    let (world, _server) = waiting_for_review(Check::Failing);
    let budget_before = world.count(&format!(
        "SELECT attempt_budget FROM task WHERE id = '{TASK}'"
    ));

    let stub = exported(&world);
    decide(&stub, "repair");
    let out = world.import(&stub);
    assert_eq!(code(&out), 0, "import: {}", said(&out));

    assert_eq!(world.run_state(), "REPAIRING");
    assert_eq!(world.task_state(), TaskState::Repairing);
    assert_eq!(world.review_state(), "DECIDED");
    assert_eq!(
        world.count(&format!(
            "SELECT attempt_budget FROM task WHERE id = '{TASK}'"
        )),
        budget_before,
        "ADR-0009: the ceiling is durable, and a review is not a way to raise it"
    );
}

#[test]
fn revise_plan_supersedes_the_run_and_registers_the_new_version_without_approving_it() {
    // §5.2 gives `APPROVED` to "a human at the control socket" through `plan
    // approve`. A review decision that also approved would be the second door
    // §4.3's tier table exists to close, so this asserts the registered version
    // is emphatically **not** APPROVED.
    let (world, _server) = waiting_for_review(Check::Failing);
    assert_eq!(
        world.count("SELECT COUNT(*) FROM plan_version"),
        1,
        "v2 is on disk and registered by nothing until a decision asks for it"
    );

    let stub = exported(&world);
    decide(&stub, "revise_plan");
    edit(&stub, "target_plan_version: null", "target_plan_version: 2");
    let out = world.import(&stub);
    assert_eq!(code(&out), 0, "import: {}", said(&out));

    assert_eq!(world.run_state(), "SUPERSEDED");
    assert_eq!(world.task_state(), TaskState::Superseded);
    assert_eq!(world.review_state(), "DECIDED");

    let state: String = world
        .store()
        .conn()
        .query_row(
            "SELECT state FROM plan_version WHERE version = 2",
            [],
            |row| row.get(0),
        )
        .expect("revise_plan registers the version it names");
    assert_eq!(
        state, "VALIDATED",
        "§3.7's refusals ran over the new document, and §5.2 keeps APPROVED for \
         `conductor plan approve`"
    );
    assert_eq!(
        world.count("SELECT COUNT(*) FROM plan_version WHERE state = 'APPROVED'"),
        1,
        "only v1 is approved; the revision is registered, never approved"
    );
}

#[test]
fn revise_plan_without_a_target_version_is_refused_rather_than_guessed() {
    // The stub ships `target_plan_version: null`. Guessing "the next one" would
    // supersede a task in favour of a document nobody named.
    let (world, _server) = waiting_for_review(Check::Failing);
    let stub = exported(&world);
    decide(&stub, "revise_plan");

    let out = world.import(&stub);
    assert_eq!(code(&out), 1, "{}", said(&out));
    assert!(
        said(&out).contains("target_plan_version"),
        "the refusal must name the field: {}",
        said(&out)
    );
    assert_eq!(world.run_state(), "AWAITING_REVIEW");
    assert_eq!(world.review_state(), "EXPORTED");
}

#[test]
fn pause_records_the_decision_and_changes_no_run_or_task_state() {
    // §6.5's one decision that moves nothing. Before S13 "nobody has looked" and
    // "a human looked and wants it left alone" were the same row; this is the
    // test that they no longer are.
    let (world, _server) = waiting_for_review(Check::Failing);
    let stub = exported(&world);
    decide(&stub, "pause");

    let out = world.import(&stub);
    assert_eq!(code(&out), 0, "import: {}", said(&out));

    assert_eq!(world.run_state(), "AWAITING_REVIEW");
    assert_eq!(world.task_state(), TaskState::AwaitingReview);
    assert_eq!(
        world.review_state(),
        "DECIDED",
        "pausing is still an answer: §5.2 makes DECIDED terminal, so the review \
         is closed even though nothing moved"
    );
    let decision: String = world
        .store()
        .conn()
        .query_row(
            "SELECT decision FROM review WHERE id = ?1",
            [REVIEW],
            |row| row.get(0),
        )
        .expect("a decision column");
    assert_eq!(decision, "pause");
}

#[test]
fn stop_cancels_the_run_and_its_task() {
    let (world, _server) = waiting_for_review(Check::Failing);
    let stub = exported(&world);
    decide(&stub, "stop");

    let out = world.import(&stub);
    assert_eq!(code(&out), 0, "import: {}", said(&out));
    assert_eq!(world.run_state(), "CANCELLED");
    assert_eq!(world.task_state(), TaskState::Cancelled);
    assert_eq!(world.review_state(), "DECIDED");
}

// ---------------------------------------------------------------------------
// 3. the experiment: no socket decides nothing
// ---------------------------------------------------------------------------

#[test]
fn importing_a_decision_with_no_control_socket_decides_nothing() {
    // The proof that is an experiment rather than a reading, and the twin of
    // `plan_approve.rs`'s `approving_a_plan_with_no_control_socket_approves_
    // nothing`. §7.2's `2` — "no project / not initialized" — is the answer,
    // because "Conductor is not up" is not the same event as "the decision was
    // refused", and a wrapper script has to tell them apart.
    let (world, server) = waiting_for_review(Check::Passing);
    let stub = exported(&world);
    decide(&stub, "accept");
    drop(server);

    let out = world.import(&stub);
    assert_eq!(
        code(&out),
        2,
        "no control socket is §7.2's 'not initialized': {}",
        said(&out)
    );

    assert_eq!(
        world.review_state(),
        "EXPORTED",
        "the review was answered without a socket"
    );
    assert_eq!(
        world.run_state(),
        "AWAITING_REVIEW",
        "the run moved without a socket"
    );
    assert_eq!(world.task_state(), TaskState::AwaitingReview);
    assert_eq!(
        world.count("SELECT COUNT(*) FROM approval_request WHERE kind = 'REVIEW_ACCEPTANCE'"),
        0,
        "a review acceptance appeared without a socket"
    );
    assert_eq!(
        world.count("SELECT COUNT(*) FROM finding WHERE resolution IS NOT NULL"),
        0,
        "a finding was resolved without a socket"
    );
}

// ---------------------------------------------------------------------------
// 4–7. what the binding refuses
// ---------------------------------------------------------------------------

#[test]
fn a_decision_whose_packet_hash_was_edited_is_refused_as_tampered() {
    // §4.3's REVIEW_ACCEPTANCE authorizes *one review packet*. A decision
    // carrying a hash the review never exported is a decision about bytes nobody
    // has seen — which is the pairing of an edited packet with a genuine decision
    // that the binding exists to make impossible.
    let (world, _server) = waiting_for_review(Check::Passing);
    let stub = exported(&world);
    decide(&stub, "accept");

    let text = std::fs::read_to_string(&stub).expect("read");
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("packet_hash:"))
        .expect("the stub binds a packet hash")
        .to_string();
    // One character, at the end of the digest. Anything larger would also be
    // caught by a length check, and the point is that the *content* is bound.
    let mut tampered = line.clone();
    let last = tampered.pop().expect("a non-empty hash");
    tampered.push(if last == 'a' { 'b' } else { 'a' });
    edit(&stub, &line, &tampered);

    let out = world.import(&stub);
    assert_eq!(code(&out), 1, "{}", said(&out));
    assert!(
        said(&out).contains("packet hash"),
        "the refusal must name what disagreed: {}",
        said(&out)
    );
    assert_eq!(world.review_state(), "EXPORTED");
    assert_eq!(world.run_state(), "AWAITING_REVIEW");
}

#[test]
fn a_decision_naming_a_different_run_is_refused() {
    // The other four binding fields. A decision file moved onto another review
    // would otherwise apply to work its author never read.
    let (world, _server) = waiting_for_review(Check::Passing);
    let stub = exported(&world);
    decide(&stub, "accept");
    edit(&stub, &format!("run_id: {RUN}"), "run_id: r-9999");

    let out = world.import(&stub);
    assert_eq!(code(&out), 1, "{}", said(&out));
    assert!(
        said(&out).contains("run"),
        "the refusal must name what disagreed: {}",
        said(&out)
    );
    assert_eq!(world.review_state(), "EXPORTED");
    assert_eq!(world.run_state(), "AWAITING_REVIEW");
}

#[test]
fn a_decision_naming_a_different_plan_version_is_refused() {
    let (world, _server) = waiting_for_review(Check::Passing);
    let stub = exported(&world);
    decide(&stub, "accept");
    let text = std::fs::read_to_string(&stub).expect("read");
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("plan_version:"))
        .expect("the stub binds a plan version")
        .to_string();
    edit(&stub, &line, "plan_version: p-repo/v99");

    let out = world.import(&stub);
    assert_eq!(code(&out), 1, "{}", said(&out));
    assert!(
        said(&out).contains("plan version"),
        "the refusal must name what disagreed: {}",
        said(&out)
    );
    assert_eq!(world.review_state(), "EXPORTED");
}

#[test]
fn importing_the_same_decision_twice_is_refused_and_the_first_answer_survives() {
    // §5.2 makes `DECIDED` terminal, and §6.5 makes the import "somewhere an
    // attacker would like to arrive twice". The second arrival must be told no
    // rather than recorded as a second answer.
    let (world, _server) = waiting_for_review(Check::Failing);
    let stub = exported(&world);
    decide(&stub, "stop");

    let first = world.import(&stub);
    assert_eq!(code(&first), 0, "{}", said(&first));
    assert_eq!(world.run_state(), "CANCELLED");

    let second = world.import(&stub);
    assert_eq!(
        code(&second),
        1,
        "a replay must be refused: {}",
        said(&second)
    );

    assert_eq!(world.review_state(), "DECIDED");
    let decision: String = world
        .store()
        .conn()
        .query_row(
            "SELECT decision FROM review WHERE id = ?1",
            [REVIEW],
            |row| row.get(0),
        )
        .expect("a decision column");
    assert_eq!(decision, "stop", "the first answer must survive the replay");
    assert_eq!(world.run_state(), "CANCELLED");
}

#[test]
fn a_paused_review_is_the_replay_nothing_but_the_review_row_can_refuse() {
    // The sharp case, and the reason it is a second test rather than a second
    // assertion. Replaying `stop` is refused twice over: the review row is
    // `DECIDED` **and** the run is no longer `AWAITING_REVIEW`, so
    // `apply_review_decision`'s `WHERE` clause would catch it even if the review
    // machine did not. `pause` moves no run state at all — §6.5's one decision
    // that is deliberately not a transition — so the *only* thing standing
    // between a second import and a second answer is §5.2 making `DECIDED`
    // terminal.
    //
    // Measured, not assumed: disabling `bind`'s EXPORTED check leaves this test
    // passing, because `review::record_decision`'s `WHERE id = ? AND state =
    // 'EXPORTED'` is the authoritative guard and this exercises it through the
    // product path. The check in `bind` is the redundant one that produces the
    // better message, in the same spirit as `mark_exported` refusing twice.
    let (world, _server) = waiting_for_review(Check::Failing);
    let stub = exported(&world);
    decide(&stub, "pause");

    assert_eq!(code(&world.import(&stub)), 0);
    let second = world.import(&stub);
    assert_eq!(
        code(&second),
        1,
        "a second pause must be refused, not recorded as a second answer: {}",
        said(&second)
    );

    assert_eq!(world.review_state(), "DECIDED");
    assert_eq!(world.run_state(), "AWAITING_REVIEW");
    assert_eq!(
        world.count("SELECT COUNT(*) FROM review WHERE id = 'rv-0001' AND decision = 'pause'"),
        1
    );
}

#[test]
fn a_decision_word_that_is_not_one_of_section_6_5_s_five_is_refused_rather_than_defaulted() {
    // §4.4's fail-closed rule applied to the most typo-exposed value in the
    // system. `ACCEPT` must not silently mean `accept`, and `approve` must not
    // resolve to the nearest known word — one of the five advances a task to
    // COMPLETE.
    //
    // Two of these are **quoted** on purpose: YAML strips trailing whitespace
    // from a plain scalar, so `decision: accept ` is not a way to express
    // `"accept "` at all. Quoting is, and it is what proves the client sends the
    // value verbatim rather than trimming it into the legal one.
    for word in ["approve", "ACCEPT", "Accept", "\"accept \"", "\"accept;\""] {
        let (world, _server) = waiting_for_review(Check::Passing);
        let stub = exported(&world);
        decide(&stub, word);

        let out = world.import(&stub);
        assert_ne!(
            code(&out),
            0,
            "{word:?} was accepted as a decision: {}",
            said(&out)
        );
        assert_eq!(
            world.review_state(),
            "EXPORTED",
            "{word:?} decided something"
        );
        assert_eq!(world.run_state(), "AWAITING_REVIEW");
    }

    // POSITIVE CONTROL: the exact spelling §6.5 uses does reach the verb. Without
    // it the loop above would pass against a server that refused every word.
    let (world, _server) = waiting_for_review(Check::Passing);
    let stub = exported(&world);
    decide(&stub, "accept");
    let out = world.import(&stub);
    assert_eq!(code(&out), 0, "{}", said(&out));
    assert_eq!(world.review_state(), "DECIDED");
}

// ---------------------------------------------------------------------------
// 8. acceptance does not overrule verification
// ---------------------------------------------------------------------------

#[test]
fn accept_is_refused_when_the_completion_gate_still_refuses_and_names_the_criterion() {
    // ADR-0019's sharpest claim: acceptance resolves the **review boundary** and
    // nothing else. §4.5 keeps verification authoritative, so a `FAIL` at the
    // current tree still refuses — and the message has to say which criterion, or
    // the human cannot tell "your decision was rejected" from "the work is not
    // done".
    //
    // This fails if `accept` ever writes `COMPLETE` without running the gate, or
    // if `AcceptedAtReview` is ever read as a blanket pass over the seven.
    let (world, _server) = waiting_for_review(Check::Failing);
    let stub = exported(&world);
    decide(&stub, "accept");

    let out = world.import(&stub);
    assert_eq!(code(&out), 1, "{}", said(&out));
    let message = said(&out);
    assert!(
        message.contains("RequiredChecks"),
        "the refusal must name the criterion: {message}"
    );
    assert!(
        message.contains("unit-tests"),
        "the refusal must name the check: {message}"
    );

    assert_eq!(
        world.run_state(),
        "AWAITING_REVIEW",
        "a refused acceptance must not move the run"
    );
    assert_eq!(world.task_state(), TaskState::AwaitingReview);
    assert_eq!(
        world.review_state(),
        "EXPORTED",
        "the review stays answerable — repair, revise_plan and stop are the \
         decisions that exist for a failing check"
    );
}

#[test]
fn accept_is_refused_when_a_passing_check_belongs_to_a_different_tree() {
    // §4.5's criterion 1 is "PASS **at the current tree hash**", and the review
    // path reads that tree off the newest recorded check. A `PASS` recorded
    // against an older tree therefore has to refuse — otherwise a stale green
    // result would be the cheapest way through the gate.
    let (world, _server) = waiting_for_review(Check::Passing);
    {
        let mut store = world.store();
        let run_id = RunId::new(RUN).expect("id");
        let (_, epoch) = store.run_state(&run_id).expect("read").expect("a run");
        let fence = conductor_core::Fence::new(run_id, epoch);
        // A second check, at a *newer* tree, which is what a re-verification
        // after a mutation leaves behind. The `unit-tests` PASS above is now
        // bound to a tree that is no longer current.
        conductor_store::verification::record(
            store.conn_mut(),
            &fence,
            &conductor_store::verification::CacheKey {
                tree_hash: "tree-review-0002",
                check_id: "unit-tests",
                command_hash: "blake3:command",
                toolchain_fingerprint: "blake3:toolchain",
            },
            &conductor_store::verification::VerificationRecord {
                id: "vc-0002".to_string(),
                attempt_id: None,
                commit_sha: "0000000000000000000000000000000000000000".to_string(),
                exit_code: None,
                duration_ms: Some(7),
                // `VOID` is §4.5's "the tree moved under the check", which is
                // exactly the situation being modelled.
                outcome: VerificationOutcome::Void,
                log_path: None,
            },
            1_600,
        )
        .expect("record the second check");
    }

    let stub = exported(&world);
    decide(&stub, "accept");
    let out = world.import(&stub);
    assert_eq!(code(&out), 1, "{}", said(&out));
    assert!(
        said(&out).contains("RequiredChecks"),
        "the refusal must name criterion 1: {}",
        said(&out)
    );
    assert_eq!(world.run_state(), "AWAITING_REVIEW");
}

// ---------------------------------------------------------------------------
// the export half
// ---------------------------------------------------------------------------

#[test]
fn exporting_writes_the_packet_and_the_stub_and_binds_the_review_to_the_hash() {
    // §6.5: "Every packet is generated from durable state, content-hashed, and
    // stored as an artifact." All three halves are asserted — the artifact on
    // disk, the hash in the row, and the stub carrying that same hash so the
    // decision can be bound to it.
    let (world, _server) = waiting_for_review(Check::Passing);

    let out = world.export();
    assert_eq!(code(&out), 0, "{}", said(&out));
    let result = json(&out);
    let packet_path = PathBuf::from(result["packet_path"].as_str().expect("a packet path"));
    let hash = result["packet_hash"].as_str().expect("a packet hash");

    let packet = std::fs::read_to_string(&packet_path).expect("the packet artifact");
    // §6.5's side-by-side: what the agent claimed, next to what the repository
    // measured. Acceptance row 6 is the pair this exists for.
    assert!(packet.contains("renamed the module"), "{packet}");
    assert!(packet.contains("CONTRADICTED"), "{packet}");
    // "every verification command with exit code and duration" — and the command
    // text, which §5.1 does not store and the profile does.
    assert!(packet.contains("cargo test"), "{packet}");
    assert!(packet.contains("unit-tests"), "{packet}");
    // "unresolved findings"
    assert!(packet.contains("f-r-0001-CONTRADICTED"), "{packet}");

    let stub_path = PathBuf::from(result["decision_path"].as_str().expect("a stub path"));
    let stub = std::fs::read_to_string(&stub_path).expect("the decision stub");
    assert!(stub.contains(hash), "the stub must bind the hash:\n{stub}");
    assert!(stub.contains("decision: null"), "{stub}");
    assert!(stub.contains(REVIEW), "{stub}");

    let row = world
        .store()
        .review(REVIEW)
        .expect("read")
        .expect("a review");
    assert_eq!(row.state.as_str(), "EXPORTED");
    assert_eq!(row.packet_hash.as_deref(), Some(hash));
    assert_eq!(
        row.packet_path.as_deref(),
        Some(packet_path.display().to_string().as_str())
    );
}

#[test]
fn a_second_export_of_one_review_is_refused_so_one_review_has_one_packet_hash() {
    // Two exports would mint two packet hashes for one review, and a decision
    // could then be bound to whichever suited whoever wrote it. §5.2 refuses the
    // self-transition, and this is the product-level proof of it.
    let (world, _server) = waiting_for_review(Check::Passing);
    let first = world.export();
    assert_eq!(code(&first), 0, "{}", said(&first));
    let hash = json(&first)["packet_hash"]
        .as_str()
        .expect("a hash")
        .to_string();

    let second = world.export();
    assert_eq!(code(&second), 1, "{}", said(&second));

    let row = world
        .store()
        .review(REVIEW)
        .expect("read")
        .expect("a review");
    assert_eq!(row.packet_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(row.state.as_str(), "EXPORTED");
}

#[test]
fn exporting_without_naming_a_run_finds_the_one_waiting_review_and_section_7_1_s_since_filters_it()
{
    // §7.1's signature is `review export [--since …]` — no `--run`. The default
    // path therefore has to work, and `--since` has to be a real filter rather
    // than an accepted-and-ignored flag: a `--since` in the future must find
    // nothing, and the same command without it must find the review. Without the
    // second half the first would pass against a command that never finds
    // anything.
    let (world, _server) = waiting_for_review(Check::Passing);

    let future = world.export_with(&["--since", "99999999999999"]);
    assert_eq!(code(&future), 1, "{}", said(&future));
    assert!(
        said(&future).contains("no review is waiting"),
        "{}",
        said(&future)
    );
    assert_eq!(
        world.review_state(),
        "PENDING",
        "a filtered-out review must not have been exported anyway"
    );

    let found = world.export_with(&[]);
    assert_eq!(code(&found), 0, "{}", said(&found));
    assert_eq!(json(&found)["review"], REVIEW);
    assert_eq!(world.review_state(), "EXPORTED");
}

#[test]
fn exporting_with_no_open_review_refuses_rather_than_reporting_success() {
    // A script that exports on a schedule must be able to tell "there is nothing
    // to review" from "here is a packet". Exit 0 with no file would be the worst
    // of both.
    let world = World::new();
    let _server = Server::start(&world);
    assert_eq!(code(&world.approve_plan()), 0);

    let out = world.export();
    assert_eq!(code(&out), 1, "{}", said(&out));
    assert!(said(&out).contains("no open review"), "{}", said(&out));
}
