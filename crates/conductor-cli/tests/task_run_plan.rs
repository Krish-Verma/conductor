//! `conductor task run` executes a task the **approved plan** declares — master
//! plan §7.1's core verb, §3.7, §4.2, §4.3, §5.2 and acceptance row 21.
//!
//! # Why this file exists
//!
//! §7.1 calls `task run` *"the core verb"*, and §3.2 makes `.conductor/` the
//! authority for what work exists: *"a plan is a file you write"*. S11 built the
//! whole path — `register_project`, `register_plan_version`, socket-only
//! `approve`, `materialize`, supersession — and S12 found that **the core verb
//! did not use any of it.** `task run` read the S5-era `.conductor/task.yaml`
//! and *fabricated* a `plan_version` row at `version 0`, `state DRAFT`, so:
//!
//! * §4.3's approval gate never ran — a task could execute with no human ever
//!   having approved a plan;
//! * §5.2's `DRAFT → APPROVED` prohibition was irrelevant, because the row the
//!   task hung off was never meant to be approved at all;
//! * `task.declared_actions`, `task.depends_on`, `task.acceptance_criteria` and
//!   `task.execution_requirements` — the four columns S11's materializer writes
//!   and §4.2/§4.3's gates read — were `NULL` on every real run, so both gates
//!   compared nothing and proceeded;
//! * acceptance row 21 (*"approve v4 during a v3 run"*) had no product path at
//!   all, because no product path ever created a versioned plan.
//!
//! Everything in this file is therefore asserted **through the shipped binary**,
//! not through a library call. §7.1's verb is the boundary the defect was on, so
//! it is the boundary the proof has to be on.
//!
//! # Every negative control has a positive control
//!
//! A `task run` that refuses everything would satisfy every refusal test here
//! and be completely broken. So [`the_approved_plan_is_the_authority_for_what_runs`]
//! runs first in spirit: the same fixture, the same command, an approved plan —
//! and it must reach `COMPLETE`. Each refusal below is a *one-fact* mutation of
//! that fixture.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use conductor_store::Store;

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

// ---------------------------------------------------------------------------
// the fixture
// ---------------------------------------------------------------------------

const PROJECT_YAML: &str = "\
project:
  id: p-taskrun
  default_branch: main
  adapter: fake
  scope_defaults:
    allowed_globs: [\"src/**\"]
    forbidden_globs: [\".conductor/**\"]
";

const VERIFICATION_YAML: &str = "\
verification:
  toolchain_fingerprint:
    - [\"/bin/echo\", \"toolchain-v1\"]
  required:
    - id: unit-tests
      command: [\"/bin/sh\", \"-c\", \"echo ok\"]
      timeout_seconds: 60
";

const POLICY_YAML: &str = "\
policy:
  rules: []
";

/// The fixture plan: one milestone, one slice, one task, one bound criterion.
///
/// `verification_profile` is a **path** relative to the repository root. §4.5's
/// clarification 3 left the reading open — *"§5.1 makes `verification_profile` a
/// per-task path while this section names a single `verification.yaml`; until
/// S11's persistence step settles that…"* — and this slice settles it: a path,
/// because `.conductor/verification.yaml` is one document holding one profile
/// and there is no second profile for a name to select between.
const PLAN_V1: &str = "\
plan:
  id: p-taskrun
  version: 1
  objective: \"Prove the core verb runs an approved plan's task.\"
  milestones:
    - id: M-01
      title: \"The core verb\"
      slices:
        - id: S-01
          title: \"task run\"
          tasks:
            - id: T-0012
              objective: \"Add a greeting helper to the library.\"
              rationale: \"The vertical needs a task that changes a file.\"
              depends_on: []
              scope:
                allowed_globs: [\"src/**\"]
                forbidden_globs: [\".conductor/**\"]
              verification_profile: .conductor/verification.yaml
              attempt_budget: 3
              acceptance_criteria:
                - id: AC-1
                  statement: \"The library gains a function.\"
                  verified_by: [unit-tests]
";

/// The same plan with an unfinished predecessor, for §5.2's `deps met` edge.
///
/// T-0011 is declared **before** T-0012 deliberately: §3.7 refuses a dependency
/// that points forward in declaration order, so the fixture has to be ordered
/// the way a real plan would be or `plan approve` refuses it for the wrong
/// reason.
const PLAN_WITH_DEPENDENCY: &str = "\
plan:
  id: p-taskrun
  version: 1
  objective: \"Prove the dependency edge has a product path.\"
  milestones:
    - id: M-01
      title: \"The core verb\"
      slices:
        - id: S-01
          title: \"task run\"
          tasks:
            - id: T-0011
              objective: \"First, and left unfinished.\"
              rationale: \"T-0012 must wait for it.\"
              depends_on: []
              scope:
                allowed_globs: [\"src/**\"]
              verification_profile: .conductor/verification.yaml
              acceptance_criteria:
                - id: AC-0
                  statement: \"The first task is done.\"
                  verified_by: [unit-tests]
            - id: T-0012
              objective: \"Second, and blocked.\"
              rationale: \"It depends on T-0011.\"
              depends_on: [T-0011]
              scope:
                allowed_globs: [\"src/**\"]
              verification_profile: .conductor/verification.yaml
              acceptance_criteria:
                - id: AC-1
                  statement: \"The second task is done.\"
                  verified_by: [unit-tests]
";

/// The same plan with §4.2's per-task override demanding more than any host here
/// has been measured to provide.
///
/// The value is a **YAML block carrying its own `execution_requirements:` key**,
/// because that is the one dialect §4.2 defines and `.conductor/project.yaml`
/// writes. A bare mapping would be a second dialect, and
/// `enforce::launch::requirements_for` treats a block that yields no requirement
/// as unreadable rather than as "nothing is gated".
const PLAN_WITH_HARD_REQUIREMENT: &str = "\
plan:
  id: p-taskrun
  version: 1
  objective: \"Prove row 30 has a product path.\"
  milestones:
    - id: M-01
      title: \"The core verb\"
      slices:
        - id: S-01
          title: \"task run\"
          tasks:
            - id: T-0012
              objective: \"Work that may not run on an unmeasured host.\"
              rationale: \"Row 30's refusal must be reachable from a plan.\"
              depends_on: []
              scope:
                allowed_globs: [\"src/**\"]
              verification_profile: .conductor/verification.yaml
              attempt_budget: 3
              execution_requirements: |
                execution_requirements:
                  filesystem_write: hard
              acceptance_criteria:
                - id: AC-1
                  statement: \"The library gains a function.\"
                  verified_by: [unit-tests]
";

/// An agent that does the work and reports it.
const SCENARIO: &str = r#"{"id":"plan-success","steps":[
  {"step":"emit","kind":"agent.started","detail":"cli"},
  {"step":"write_file","path":"src/added.rs","contents":"pub fn added() -> u32 { 1 }\n"},
  {"step":"checkpoint","name":"after-edits"},
  {"step":"report_on_stdout","claim":"COMPLETE","files_touched":["src/added.rs"],
   "summary":"added a function"},
  {"step":"exit","code":0}]}"#;

struct World {
    dir: tempfile::TempDir,
}

impl World {
    /// A git repository holding a complete `.conductor/`, and a scenario file.
    fn new() -> World {
        World::with_plan(PLAN_V1)
    }

    fn with_plan(plan: &str) -> World {
        let world = World {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        let repo = world.repo();
        std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
        write(&repo, ".conductor/project.yaml", PROJECT_YAML);
        write(&repo, ".conductor/verification.yaml", VERIFICATION_YAML);
        write(&repo, ".conductor/policy.yaml", POLICY_YAML);
        write(&repo, ".conductor/plans/v1/plan.yaml", plan);
        write(&repo, "src/lib.rs", "pub fn base() -> u32 { 0 }\n");

        for args in [
            vec!["init", "-q", "--initial-branch=main"],
            vec!["config", "user.email", "taskrun@example.invalid"],
            vec!["config", "user.name", "Task Run Test"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["add", "-A"],
            vec!["commit", "-q", "-m", "initial"],
        ] {
            conductor_git::run_git_ok(&repo, &args).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        }

        std::fs::write(world.dir.path().join("scenario.json"), SCENARIO).expect("scenario");
        // `approval serve` refuses to create a store — §7.2's 2 is "store
        // unhealthy", and a control socket that silently conjures a database
        // would be a second way for project truth to appear. So the fixture
        // creates it, exactly as an operator's `conductor init` run would leave
        // it.
        Store::open_or_create(world.db()).expect("create the store");
        world
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canonicalize")
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn db(&self) -> PathBuf {
        self.root().join("conductor.db")
    }

    /// Deliberately short, and **not** canonicalized: a `sockaddr_un` path is
    /// capped at `SUN_LEN`, macOS resolves `/var/folders/…` to a `/private`
    /// prefix eight characters longer, and the bind goes through a
    /// `.<name>.<pid>.staging` sibling that adds sixteen more. A descriptive
    /// name here fails as "path must be shorter than SUN_LEN", which reads like
    /// a Conductor defect and is not one.
    fn socket(&self) -> PathBuf {
        self.dir.path().join("cs").join("s")
    }

    fn scenario(&self) -> PathBuf {
        self.root().join("scenario.json")
    }

    fn store(&self) -> Store {
        Store::open_existing(self.db()).expect("open the store")
    }

    /// The fake agent from this build.
    fn fake_agent(&self) -> PathBuf {
        let exe = std::env::current_exe().expect("current_exe");
        let target = exe.parent().and_then(Path::parent).expect("target dir");
        let path = target.join("conductor-fake-agent");
        assert!(
            path.exists(),
            "the fake agent is missing at {}; `cargo test --all` builds it",
            path.display()
        );
        path
    }

    /// Approve a plan version the way §5.2 requires: a human at the control
    /// socket. The server lives only for the call, so nothing else in the test
    /// shares the store with it.
    fn approve(&self, version: u32) -> Output {
        let _server = Server::start(self);
        Command::new(CONDUCTOR)
            .args([
                "plan",
                "approve",
                &version.to_string(),
                "--repo",
                &arg(&self.repo()),
                "--socket",
                &arg(&self.socket()),
                "--json",
            ])
            .output()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
    }

    fn approve_ok(&self, version: u32) {
        let out = self.approve(version);
        assert_eq!(code(&out), 0, "approving v{version}: {}", said(&out));
    }

    /// `conductor task run <id> --json`, with the fixture's plumbing.
    fn task_run(&self, task: &str) -> Output {
        Command::new(CONDUCTOR)
            .args([
                "task",
                "run",
                task,
                "--json",
                "--repo",
                &arg(&self.repo()),
                "--store",
                &arg(&self.db()),
                "--adapter",
                "fake",
                "--agent-binary",
                &arg(&self.fake_agent()),
                "--scenario",
                &arg(&self.scenario()),
            ])
            .output()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
    }

    fn count(&self, table: &str) -> i64 {
        self.store()
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|e| panic!("count {table}: {e}"))
    }

    /// The packet artifact one attempt was given — §6.5's *"stored as an
    /// artifact"*, under `artifacts/<run>/<ordinal>/`.
    fn packet(&self, run: &str, ordinal: i64) -> String {
        let path = self
            .root()
            .join("artifacts")
            .join(run)
            .join(ordinal.to_string())
            .join("packet.yaml");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("§6.5 stores every packet: {} — {e}", path.display()))
    }
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn arg(path: &Path) -> String {
    path.to_str().expect("utf8 path").to_string()
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .unwrap_or_else(|| panic!("no exit code; {}", said(out)))
}

fn said(out: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not json ({e}); {}", said(out)))
}

/// A `conductor approval serve` process and the socket it published.
struct Server {
    child: Child,
}

impl Server {
    fn start(world: &World) -> Server {
        std::fs::create_dir_all(world.socket().parent().expect("a parent")).expect("mkdir socket");
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
        let mut server = Server { child };
        // Generous, because M29 measured macOS taking 21.7 s to scan a freshly
        // built binary before its first instruction runs.
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(world.socket()).is_ok() {
                return server;
            }
            if let Ok(Some(status)) = server.child.try_wait() {
                use std::io::Read;
                let mut said = String::new();
                if let Some(mut err) = server.child.stderr.take() {
                    let _ = err.read_to_string(&mut said);
                }
                if let Some(mut out) = server.child.stdout.take() {
                    let _ = out.read_to_string(&mut said);
                }
                panic!(
                    "the control socket server exited {status} before listening at \
                     {}: {said}",
                    world.socket().display(),
                );
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

/// The `plan_version` row a task hangs off, as three facts.
fn plan_version_of(world: &World, task: &str) -> (String, i64, String) {
    world
        .store()
        .conn()
        .query_row(
            "SELECT pv.id, pv.version, pv.state
               FROM task t JOIN plan_version pv ON pv.id = t.plan_version_id
              WHERE t.id = ?1",
            rusqlite::params![task],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or_else(|e| panic!("no plan_version behind task {task}: {e}"))
}

// ---------------------------------------------------------------------------
// the positive control — every refusal below is measured against this
// ---------------------------------------------------------------------------

#[test]
fn the_approved_plan_is_the_authority_for_what_runs() {
    let world = World::new();
    world.approve_ok(1);

    let out = world.task_run("T-0012");
    assert_eq!(code(&out), 0, "success is §7.2's 0; {}", said(&out));
    let report = json(&out);
    assert_eq!(report["task"], "T-0012");
    assert_eq!(report["state"], "COMPLETE");

    // The whole point: the run's task hangs off the **approved** plan version,
    // at the version the directory names, not off a fabricated `version 0`.
    let (id, version, state) = plan_version_of(&world, "T-0012");
    assert_eq!(version, 1, "the task belongs to v1, not to a placeholder");
    assert_eq!(state, "APPROVED", "§4.3's gate must have been the real one");
    assert!(
        id.ends_with("-v1"),
        "the id is derived from the project and the version: {id}"
    );

    // And there is exactly one plan version — no synthetic row alongside it.
    assert_eq!(
        world.count("plan_version"),
        1,
        "a second plan_version row means something fabricated one"
    );
    let source: String = world
        .store()
        .conn()
        .query_row("SELECT source_path FROM plan_version", [], |row| row.get(0))
        .expect("a source path");
    assert_eq!(
        source, ".conductor/plans/v1/plan.yaml",
        "the row must point at the document a human approved"
    );

    // The four columns S11's materializer writes, and both gates read. Before
    // this slice every one of them was NULL on a real run.
    let store = world.store();
    let task = conductor_core::TaskId::new("T-0012").expect("task id");
    assert_eq!(
        store.declared_actions(&task).expect("actions"),
        Some("[]".to_string()),
        "§4.3's binding rule needs a declaration, and `[]` is one"
    );
    assert!(
        store
            .acceptance_criteria(&task)
            .expect("criteria")
            .is_some_and(|json| json.contains("AC-1")),
        "§4.5's criteria must come from the plan"
    );
    assert!(
        store.depends_on(&task).expect("depends_on").is_some(),
        "§5.2's `deps met` edge needs a dependency list to consult"
    );
}

#[test]
fn no_legacy_task_spec_file_is_involved() {
    // The S5-era stopgap must be gone rather than merely unused: a file that is
    // still read is a second authority for what a task is, and §3.2 gives that
    // authority to the plan. The fixture never writes `.conductor/task.yaml`,
    // and the positive control above passes — so the path that ran cannot have
    // needed it. This asserts the other half: the flag that named it is gone.
    let world = World::new();
    assert!(
        !world.repo().join(".conductor/task.yaml").exists(),
        "the fixture must not contain the legacy spec"
    );
    let out = Command::new(CONDUCTOR)
        .args([
            "task",
            "run",
            "T-0012",
            "--repo",
            &arg(&world.repo()),
            "--store",
            &arg(&world.db()),
            "--spec",
            &arg(&world.repo().join(".conductor/task.yaml")),
        ])
        .output()
        .expect("spawn");
    assert_eq!(
        code(&out),
        64,
        "`--spec` must be an unknown argument, not an accepted one; {}",
        said(&out)
    );
}

// ---------------------------------------------------------------------------
// negative controls — one fact changed each time
// ---------------------------------------------------------------------------

#[test]
fn a_task_with_no_approved_plan_does_not_run() {
    // The defect this file exists for, stated as a test: with no approval, the
    // core verb must refuse. It used to succeed.
    let world = World::new();

    let out = world.task_run("T-0012");
    assert_ne!(
        code(&out),
        0,
        "an unapproved plan ran a task; {}",
        said(&out)
    );
    assert_eq!(
        code(&out),
        2,
        "§7.2's 2 — there is no project truth to run against; {}",
        said(&out)
    );
    assert!(
        !world.root().join("workspaces").exists(),
        "a workspace was cloned for a task nobody approved"
    );
    assert_eq!(world.count("attempt"), 0, "an agent was launched");
    assert_eq!(world.count("run"), 0, "a run was created");
}

#[test]
fn a_plan_that_is_only_validated_does_not_run_its_tasks() {
    // §5.2: "`APPROVED` only via a human at the control socket", and
    // materialisation refuses anything short of it. `VALIDATED` is the state
    // that is *closest* to approved and still not it, which makes it the one
    // worth testing.
    use conductor_run::plan::{self, ledger};
    use conductor_run::verify::profile;

    let world = World::new();
    {
        let mut store = Store::open_or_create(world.db()).expect("store");
        let project = ledger::register_project(&mut store, &world.repo(), 1_000).expect("register");
        let catalogue = plan::check_ids(
            &profile::load(&world.repo().join(".conductor/verification.yaml"))
                .expect("profile")
                .profile,
        );
        let registered = ledger::register_plan_version(&mut store, &project.id, 1, &catalogue)
            .expect("register v1");
        assert_eq!(
            registered.row.state,
            conductor_core::PlanVersionState::Validated,
            "the fixture must stop at VALIDATED"
        );
    }

    let out = world.task_run("T-0012");
    assert_ne!(code(&out), 0, "a VALIDATED plan ran a task; {}", said(&out));
    assert_eq!(world.count("attempt"), 0, "an agent was launched");
    assert!(
        said(&out).contains("VALIDATED") || said(&out).to_lowercase().contains("approv"),
        "the refusal must say the plan is not approved; {}",
        said(&out)
    );
}

#[test]
fn a_task_the_approved_plan_does_not_declare_is_refused_by_name() {
    let world = World::new();
    world.approve_ok(1);

    let out = world.task_run("T-9999");
    assert_ne!(code(&out), 0, "{}", said(&out));
    assert!(
        said(&out).contains("T-9999"),
        "the refusal must name the id that was asked for; {}",
        said(&out)
    );
    assert_eq!(world.count("attempt"), 0, "an agent was launched");
}

#[test]
fn a_task_whose_dependency_is_not_complete_does_not_launch_an_agent() {
    // §5.2: `PENDING ──deps met──► READY`. The dependency list only exists
    // because the plan declared it, so before this slice this edge had no
    // product path to be tested on.
    let world = World::with_plan(PLAN_WITH_DEPENDENCY);
    world.approve_ok(1);

    let out = world.task_run("T-0012");
    assert_ne!(code(&out), 0, "an unmet dependency ran; {}", said(&out));
    assert_eq!(world.count("attempt"), 0, "an agent was launched");
    assert!(
        said(&out).contains("T-0011"),
        "the refusal must name the dependency that is not done; {}",
        said(&out)
    );

    // §5.2's fifth correction: it stays PENDING, not BLOCKED — a task merely
    // waiting must not be stranded in a state with no way out.
    let state: String = world
        .store()
        .conn()
        .query_row("SELECT state FROM task WHERE id = 'T-0012'", [], |row| {
            row.get(0)
        })
        .expect("a task row");
    assert_eq!(state, "PENDING", "a waiting task must stay claimable later");
}

#[test]
fn a_plan_declared_execution_requirement_blocks_an_unmeasured_host() {
    // Acceptance row 30, reached from the plan for the first time. §4.2's
    // per-task override lives in `plan.yaml`; before this slice nothing on the
    // product path could put a value in `task.execution_requirements`, so the
    // gate compared nothing on every real run.
    let world = World::with_plan(PLAN_WITH_HARD_REQUIREMENT);
    world.approve_ok(1);

    let out = world.task_run("T-0012");
    assert_eq!(
        code(&out),
        3,
        "row 30 ends BLOCKED, which is §7.2's 3 — a human is needed; {}",
        said(&out)
    );
    let report = json(&out);
    assert_eq!(report["state"], "BLOCKED");
    assert_eq!(
        world.count("attempt"),
        0,
        "row 30: the attempt never starts"
    );
    let findings = report["findings"].as_array().expect("findings");
    assert!(
        !findings.is_empty(),
        "a refusal nobody can read afterwards is not one a human can act on"
    );
    assert!(
        findings
            .iter()
            .any(|f| f.as_str().is_some_and(|t| t.contains("filesystem_write"))),
        "the refusal must name the dimension; {findings:?}"
    );
}

#[test]
fn editing_an_approved_plan_stops_the_run_until_a_human_re_approves() {
    // §5.2's restart clause: "re-hash on load; a mismatch on an `APPROVED` plan
    // is a hard error, cleared by re-running `conductor plan approve`". Nothing
    // on the product path could reach it before, because no product path read
    // an approved plan.
    let world = World::new();
    world.approve_ok(1);

    // A semantic edit, not a reformat: §3.6 hashes canonical content, so a
    // comment or whitespace change must *not* trip this.
    let path = world.repo().join(".conductor/plans/v1/plan.yaml");
    let edited = std::fs::read_to_string(&path)
        .expect("read")
        .replace("attempt_budget: 3", "attempt_budget: 9");
    std::fs::write(&path, edited).expect("write");

    let out = world.task_run("T-0012");
    assert_ne!(code(&out), 0, "an edited plan ran anyway; {}", said(&out));
    assert_eq!(world.count("attempt"), 0, "an agent was launched");
    assert!(
        said(&out).to_lowercase().contains("hash")
            || said(&out).to_lowercase().contains("disagree"),
        "the refusal must point at the disagreement; {}",
        said(&out)
    );
}

#[test]
fn reformatting_an_approved_plan_does_not_invalidate_it() {
    // POSITIVE CONTROL for the test above. §3.6: "canonical reserialisation,
    // comments excluded" — so the hash check must be about semantics. Without
    // this, "an edit stops the run" is satisfied by a check that refuses any
    // byte change, which would make every approved plan unformattable.
    let world = World::new();
    world.approve_ok(1);

    let path = world.repo().join(".conductor/plans/v1/plan.yaml");
    let text = std::fs::read_to_string(&path).expect("read");
    std::fs::write(&path, format!("# a comment nobody hashed\n{text}\n\n")).expect("write");

    let out = world.task_run("T-0012");
    assert_eq!(
        code(&out),
        0,
        "a comment must not invalidate an approval; {}",
        said(&out)
    );
}

#[test]
fn a_task_run_from_outside_the_registered_tree_is_refused() {
    // §3.3's control 2: "Conductor reads plan approval **only** from the
    // registered repository's working tree, never from a run branch." A second
    // checkout of the same project — which is what a run clone is — must not be
    // able to present itself as the project.
    let world = World::new();
    world.approve_ok(1);

    let clone = world.root().join("elsewhere");
    conductor_git::run_git_ok(
        &world.root(),
        &["clone", "-q", &arg(&world.repo()), &arg(&clone)],
    )
    .expect("clone");

    let out = Command::new(CONDUCTOR)
        .args([
            "task",
            "run",
            "T-0012",
            "--json",
            "--repo",
            &arg(&clone),
            "--store",
            &arg(&world.db()),
            "--adapter",
            "fake",
            "--agent-binary",
            &arg(&world.fake_agent()),
            "--scenario",
            &arg(&world.scenario()),
        ])
        .output()
        .expect("spawn");
    assert_ne!(
        code(&out),
        0,
        "a second tree presented itself as the project; {}",
        said(&out)
    );
    assert_eq!(world.count("attempt"), 0, "an agent was launched");
}

// ---------------------------------------------------------------------------
// §6.5 — what the agent is told is the packet, not a line of prose
// ---------------------------------------------------------------------------

#[test]
fn the_agent_is_given_the_packet_and_it_is_stored_as_an_artifact() {
    // §6.5: "Every packet is generated from durable state, content-hashed, and
    // stored as an artifact." Until S12 the agent got `spec.objective()` — one
    // string — so none of the packet's other fields reached it, and nothing was
    // stored. The packet was *built* and proven deterministic by S12's unit tests
    // and had never been *used*.
    let world = World::new();
    world.approve_ok(1);

    let out = world.task_run("T-0012");
    assert_eq!(code(&out), 0, "{}", said(&out));
    let run = json(&out)["run"].as_str().expect("a run id").to_string();

    let packet = world.packet(&run, 1);

    // It is §6.5's document, not the objective string.
    assert!(
        packet.contains("packet: implementation"),
        "the artifact must be §6.5's implementation packet: {packet}"
    );
    for expected in [
        "plan_version: 1", // which approved version authorized this
        "T-0012",          // the task
        "AC-1",            // the acceptance criterion, bound to a check
        "unit-tests",      // …and the check it binds to
        "src/**",          // the scope the task may write
        ".conductor/**",   // §3.3's always-forbidden path
        "report_schema",   // where the report shape is defined
    ] {
        assert!(
            packet.contains(expected),
            "§6.5 names {expected:?} and the packet the agent got does not carry \
             it: {packet}"
        );
    }

    // The objective is in there — but as one field of many, which is the whole
    // point. Asserted so that a packet that somehow degraded back to a bare
    // prompt would still fail the checks above rather than pass this one.
    assert!(
        packet.contains("Add a greeting helper"),
        "the objective is still carried: {packet}"
    );
}

#[test]
fn the_stored_packet_is_the_one_the_state_produces() {
    // §6.6: "Packets … must serialize byte-identically for identical state." The
    // artifact is therefore checkable *against the store* after the fact, and
    // that is the property that makes it evidence rather than a log line: a
    // reviewer can re-derive what the agent was told without trusting the file.
    use conductor_core::RunId;

    let world = World::new();
    world.approve_ok(1);
    let out = world.task_run("T-0012");
    assert_eq!(code(&out), 0, "{}", said(&out));
    let run = json(&out)["run"].as_str().expect("a run id").to_string();

    let stored = world.packet(&run, 1);
    let mut store = Store::open_existing(world.db()).expect("open");
    let rebuilt = conductor_run::packet::implementation::build(
        &mut store,
        &RunId::new(&run).expect("run id"),
    )
    .expect("the packet rebuilds from durable state alone")
    .to_yaml();

    assert_eq!(
        stored, rebuilt,
        "the stored packet and the one this state produces differ, so §6.6's \
         byte-identical claim is false somewhere"
    );
}

// ---------------------------------------------------------------------------
// one task, one run — the claim must be the run this command created
// ---------------------------------------------------------------------------

#[test]
fn running_one_task_never_claims_another_tasks_run() {
    // §4.7's claim predicate selects "the next eligible run" — `ORDER BY
    // priority, created_at LIMIT 1` — and until S12 that was safe by accident:
    // the S5-era path created one task and one run per store, so "the next run"
    // and "this task's run" could not disagree. A plan materializes **many**
    // tasks into one store, and they can now differ.
    //
    // The sequence below is the ordinary one, not a contrived one:
    //
    //   1. `task run T-0012` — its dependency is unmet, so it refuses. But the
    //      run row was already created, and it stays `READY`.
    //   2. `task run T-0011` — the dependency task, which is genuinely runnable.
    //
    // At step 2 the claim takes T-0012's run, because it was created first.
    // T-0011 is then marked `RUNNING` while an agent works in
    // `conductor/T-0012/r-0001` — findings, attempts and the workspace land on
    // one task's run while the state writes land on another's.
    //
    // `conductor_store` already ships `claim_run` for exactly this distinction:
    // *"startup recovery needs the run it is recovering, not 'the next one'"*.
    let world = World::with_plan(PLAN_WITH_DEPENDENCY);
    world.approve_ok(1);

    // Step 1: refused on its dependency, having created its run.
    let blocked = world.task_run("T-0012");
    assert_ne!(code(&blocked), 0, "T-0012's dependency is unmet");
    let t12_run: String = world
        .store()
        .conn()
        .query_row("SELECT id FROM run WHERE task_id = 'T-0012'", [], |row| {
            row.get(0)
        })
        .expect("the refused command still created T-0012's run");

    // Step 2: the runnable task.
    let out = world.task_run("T-0011");
    assert_eq!(code(&out), 0, "T-0011 is runnable; {}", said(&out));

    // The property: whatever ran, it ran on T-0011's own run.
    let report = json(&out);
    assert_ne!(
        report["run"],
        t12_run,
        "T-0011's command claimed T-0012's run ({t12_run}); {}",
        said(&out)
    );

    // And T-0012's run is untouched — still READY, still no attempt.
    let (state, attempts): (String, i64) = world
        .store()
        .conn()
        .query_row(
            "SELECT r.state, (SELECT COUNT(*) FROM attempt a WHERE a.run_id = r.id)
               FROM run r WHERE r.id = ?1",
            rusqlite::params![t12_run],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("T-0012's run row");
    assert_eq!(state, "READY", "T-0012's run was driven by another task");
    assert_eq!(attempts, 0, "an agent ran against T-0012's run");
}

#[test]
fn a_task_that_is_already_complete_does_not_start_a_second_run() {
    // §5.2 makes `COMPLETE` terminal. Nothing was checking it before the run row
    // was written, so a second `task run` on a finished task created a second
    // `READY` run — `active_run_for_task` returns `None` for a terminal one — and
    // then failed at the state transition, leaving the row behind. A refusal
    // that has already written a row is not a refusal.
    let world = World::new();
    world.approve_ok(1);
    assert_eq!(code(&world.task_run("T-0012")), 0);
    assert_eq!(world.count("run"), 1);

    let out = world.task_run("T-0012");
    assert_ne!(code(&out), 0, "a finished task ran again; {}", said(&out));
    assert_eq!(
        world.count("run"),
        1,
        "a second run row was created for a terminal task"
    );
    assert!(
        said(&out).contains("COMPLETE"),
        "the refusal must name the state that makes it terminal; {}",
        said(&out)
    );
}

// ---------------------------------------------------------------------------
// acceptance row 21 — approve v2 while v1's task is in flight
// ---------------------------------------------------------------------------

#[test]
fn approving_a_new_version_supersedes_untouched_tasks_and_carries_the_finished_one() {
    // Row 21: "approve v4 during a v3 run → run keeps `plan_version=3` → finish
    // under v3; new tasks under v4". Reachable from the product path for the
    // first time, because `plan approve` is now what materialises.
    let world = World::new();
    world.approve_ok(1);
    assert_eq!(code(&world.task_run("T-0012")), 0);

    // v2 drops T-0012 and adds T-0013.
    let v2 = PLAN_V1
        .replace("version: 1", "version: 2")
        .replace("- id: T-0012", "- id: T-0013")
        .replace(
            "objective: \"Add a greeting helper to the library.\"",
            "objective: \"Add a second helper.\"",
        );
    write(&world.repo(), ".conductor/plans/v2/plan.yaml", &v2);
    conductor_git::run_git_ok(&world.repo(), &["add", "-A"]).expect("add");
    conductor_git::run_git_ok(&world.repo(), &["commit", "-q", "-m", "v2"]).expect("commit");
    world.approve_ok(2);

    let store = world.store();
    let states: Vec<(String, String, i64)> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT t.id, t.state, pv.version FROM task t
                   JOIN plan_version pv ON pv.id = t.plan_version_id ORDER BY t.id",
            )
            .expect("prepare");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    };

    // T-0012 finished under v1 and stays there. §5.2 makes `COMPLETE` terminal,
    // so it is carried rather than superseded — "finish under v3", exactly.
    assert!(
        states
            .iter()
            .any(|(id, state, version)| id == "T-0012" && state == "COMPLETE" && *version == 1),
        "T-0012 must stay COMPLETE on v1: {states:?}"
    );
    // T-0013 is new work, under v2.
    assert!(
        states
            .iter()
            .any(|(id, state, version)| id == "T-0013" && state == "PENDING" && *version == 2),
        "T-0013 must be PENDING on v2: {states:?}"
    );
    // And v1 is no longer authoritative.
    let v1_state: String = store
        .conn()
        .query_row(
            "SELECT state FROM plan_version WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .expect("v1 row");
    assert_eq!(
        v1_state, "SUPERSEDED",
        "§5.2: superseded by a later APPROVED"
    );
}
