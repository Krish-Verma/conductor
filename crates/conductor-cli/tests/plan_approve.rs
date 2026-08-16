//! `conductor plan approve <version>` has exactly one door — master plan §4.3,
//! §5.2, §7.1 and §7.2.
//!
//! > `conductor plan approve <version>     # human-only, socket-only`
//!
//! §5.2 says the same thing from the other side: *"`APPROVED` only via a human
//! at the control socket."* §4.3's argument for why that is a property of the
//! *execution mode* and not of a flag is the whole of its tier table, and its
//! additional control is the one this file copies:
//!
//! > **Additional control, all tiers:** the binary reachable from a workspace,
//! > if any, exposes read-only verbs only and **physically lacks the approval
//! > code path** — asserted by a source-scan test that fails if anyone wires
//! > approval into it.
//!
//! # Absence is proven three ways, for S8's reason
//!
//! `crates/conductor-run/tests/layering.rs` records why one substring scan is
//! not enough: *"a substring scan alone is a rule someone renames their way
//! past."* The same applies here, so `plan approve`'s lack of an in-process
//! route is asserted three ways that fail differently:
//!
//! 1. **A code-shaped needle scan** over the client module: it names nothing on
//!    the approval-writing path ([`the_plan_approve_client_lacks_the_approval_code_path`]).
//! 2. **A no-subset-to-get-wrong rule**: the client names no database handle at
//!    all, so there is no set of "safe" store calls to get wrong
//!    ([`the_plan_approve_client_holds_no_store_handle`]). This is S8's
//!    `FORBIDDEN_CRATES` rule, applied to a module instead of a binary.
//! 3. **An experiment**: run the real command with no server and observe that
//!    nothing anywhere was approved
//!    ([`approving_a_plan_with_no_control_socket_approves_nothing`]) — with a
//!    positive control that the identical command over a live socket *does*
//!    approve, so the refusal is about the socket and not about the command
//!    being broken.
//!
//! And the scanner has its own positive control
//! ([`the_scanner_finds_the_plan_approval_code_path_where_it_is`]): the
//! identical needles, run over the files that genuinely contain the code path,
//! must match. Without it, a typo in a path would turn every assertion above
//! into "the empty string contains no forbidden substring".

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use conductor_store::Store;

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

// ---------------------------------------------------------------------------
// the scan
// ---------------------------------------------------------------------------

/// The client half of `conductor plan approve`. Nothing in this file may be
/// able to record an approval.
const CLIENT: &str = "crates/conductor-cli/src/plan.rs";

/// Substrings whose presence in [`CLIENT`] would mean an in-process approval
/// route was wired into it.
///
/// Code-shaped, for the reason S8's scan records: the bare word `approval`
/// produced a false positive on a *payload* rather than a code path, so each
/// needle here is a path into the writing surface or a name that exists only on
/// it. [`the_scanner_finds_the_plan_approval_code_path_where_it_is`] asserts
/// every one of them matches somewhere in the real code, so a needle that never
/// matches anything cannot quietly shrink the scan.
const FORBIDDEN: &[&str] = &[
    "ledger::approve",
    "Authorization",
    "GrantOptions",
    "NewApprovalRequest",
    "record_plan_approval_content",
    "set_plan_state",
    "PlanVersionState",
];

/// Names that would give [`CLIENT`] a database handle.
///
/// §4.3 asks for "read-only verbs only"; S8's answer was stronger and easier to
/// check — no Conductor surface at all, so there is no read-only subset to get
/// wrong. The same move works one level down: a module that cannot open the
/// store cannot write `plan_version.state`, cannot write an `approval_grant`
/// row, and cannot be argued into it later by someone who believes their new
/// call is read-only.
const FORBIDDEN_HANDLES: &[&str] = &["conductor_store", "Store::open", "rusqlite"];

/// Files that between them contain every needle in [`FORBIDDEN`], and that are
/// unambiguously the plan-approval code path.
const APPROVAL_CORPUS: &[&str] = &[
    "crates/conductor-cli/src/approval.rs",
    "crates/conductor-run/src/plan/ledger.rs",
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

fn hits<'n>(source: &str, needles: &[&'n str]) -> Vec<&'n str> {
    needles
        .iter()
        .copied()
        .filter(|needle| source.contains(needle))
        .collect()
}

#[test]
fn the_plan_approve_client_lacks_the_approval_code_path() {
    let source = read_source(CLIENT);
    let found = hits(&source, FORBIDDEN);
    assert!(
        found.is_empty(),
        "{CLIENT} implements `conductor plan approve` and names the \
         approval-writing surface {found:?}. §5.2 gives `APPROVED` to \"a human \
         at the control socket\" and §4.3 makes that a property of the \
         execution mode — a client that can write the approval itself is the \
         second door, whatever it checks first."
    );
}

#[test]
fn the_plan_approve_client_holds_no_store_handle() {
    let source = read_source(CLIENT);
    let found = hits(&source, FORBIDDEN_HANDLES);
    assert!(
        found.is_empty(),
        "{CLIENT} names {found:?}, so it can reach the database. §4.3's \
         \"read-only verbs only\" has no read-only subset to get wrong when \
         there is no handle at all; adding one re-opens the argument every time \
         somebody needs one more query."
    );
}

#[test]
fn the_plan_approve_client_reaches_the_server_only_through_the_control_socket() {
    // The positive half of the two scans above: having proved the client cannot
    // write an approval, this proves it does something instead of nothing, and
    // that the something is §7.3's socket.
    let source = read_source(CLIENT);
    assert!(
        source.contains("socket::call"),
        "{CLIENT} must reach the control socket; without this the scans above \
         are satisfied by a command that does nothing at all"
    );
    assert!(
        source.contains("plan.approve"),
        "{CLIENT} must name the RPC method it calls"
    );
}

#[test]
fn the_control_socket_has_exactly_one_server_for_plan_approve() {
    // A second dispatcher would be a second door that still went "over a
    // socket" and still failed this file's other tests for nothing. `approval`
    // is the one server (§4.3), so `plan.approve` must be answered there and
    // nowhere else.
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
        if source.contains("\"plan.approve\" =>") {
            servers.push(name);
        }
    }
    assert_eq!(
        servers,
        vec!["approval.rs".to_string()],
        "`plan.approve` must be dispatched by the one server §4.3 allows"
    );
}

#[test]
fn the_scanner_finds_the_plan_approval_code_path_where_it_is() {
    // Without this, every assertion above could be satisfied by a scanner that
    // read nothing, matched nothing, or was pointed at the wrong tree.
    for path in APPROVAL_CORPUS {
        assert!(
            !hits(&read_source(path), FORBIDDEN).is_empty(),
            "the scanner found nothing in {path}, which is approval code; the \
             scan of {CLIENT} therefore proves nothing"
        );
    }
    let corpus: String = APPROVAL_CORPUS
        .iter()
        .map(|path| read_source(path))
        .collect::<Vec<String>>()
        .join("\n");
    for needle in FORBIDDEN {
        assert!(
            corpus.contains(needle),
            "{needle:?} matches nothing in the approval code, so it contributes \
             nothing to the scan"
        );
    }
    // And the handle rule is about names that genuinely exist: the server half
    // holds the store, which is exactly why the client must not.
    assert!(
        !hits(
            &read_source("crates/conductor-cli/src/approval.rs"),
            FORBIDDEN_HANDLES
        )
        .is_empty(),
        "the server must hold a store handle, or the client's lack of one means \
         nothing"
    );
}

// ---------------------------------------------------------------------------
// the experiment
// ---------------------------------------------------------------------------

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
            vec!["config", "user.email", "plan@example.invalid"],
            vec!["config", "user.name", "Plan Test"],
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
        conductor_git::run_git_ok(&repo, &["add", "-A"]).expect("git add");
        conductor_git::run_git_ok(&repo, &["commit", "-q", "-m", "conductor init"])
            .expect("git commit");

        // Created up front so the no-socket case can prove nothing was written
        // *into an existing store*, rather than proving only that no file
        // appeared.
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

    fn sidecar(&self) -> PathBuf {
        self.repo().join(".conductor/plans/v1/APPROVED")
    }

    fn store(&self) -> Store {
        Store::open_existing(self.db()).expect("open the store")
    }

    fn count(&self, table: &str) -> i64 {
        self.store()
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|e| panic!("count {table}: {e}"))
    }

    fn approve(&self) -> Output {
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

#[test]
fn approving_a_plan_with_no_control_socket_approves_nothing() {
    // The third proof, and the only one that is an experiment rather than a
    // reading. §7.2's `2` — "no project / not initialized" — is the answer,
    // because "Conductor is not up" is not the same event as "the plan was
    // refused", and a wrapper script has to be able to tell them apart.
    let world = World::new();

    let out = world.approve();
    assert_eq!(
        code(&out),
        2,
        "no control socket is §7.2's 'not initialized': {}",
        said(&out)
    );

    assert!(
        !world.sidecar().exists(),
        "§3.1's APPROVED sidecar was written without a socket"
    );
    assert_eq!(
        world.count("approval_grant"),
        0,
        "a grant appeared without a socket"
    );
    assert_eq!(
        world.count("approval_request"),
        0,
        "a request appeared without a socket"
    );
    assert_eq!(
        world.count("plan_version"),
        0,
        "the ledger was written without a socket"
    );
}

#[test]
fn approving_a_plan_over_the_control_socket_records_both_halves_of_section_3_1() {
    // POSITIVE CONTROL for the test above: the identical command, with a server
    // listening, must actually approve. Without this, "no socket approves
    // nothing" is satisfied by a command that approves nothing ever.
    let world = World::new();
    let _server = Server::start(&world);

    let out = world.approve();
    assert_eq!(code(&out), 0, "said: {}", said(&out));

    // §3.1's git half: the sidecar, carrying "plan content hash · approver ·
    // timestamp · policy hash".
    let sidecar = std::fs::read_to_string(world.sidecar()).expect("§3.1's APPROVED sidecar");
    assert!(sidecar.contains("content_hash"), "{sidecar}");
    assert!(sidecar.contains("approver"), "{sidecar}");
    assert!(sidecar.contains("policy_hash"), "{sidecar}");

    // §3.3's control 3: "the store records the approval independently at grant
    // time". Both halves, or the disagreement §3.3 halts on is undetectable.
    let store = world.store();
    let row = store
        .conn()
        .query_row("SELECT state, approved_by FROM plan_version", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .expect("a plan_version row");
    assert_eq!(row.0, "APPROVED", "§5.2's terminal state for an approval");
    assert!(row.1.is_some(), "the approver must be recorded");

    // §4.3: the approval exists because a grant of the right kind was made and
    // consumed, not because a command was run.
    assert_eq!(world.count("approval_grant"), 1);
    let kind: String = store
        .conn()
        .query_row("SELECT kind FROM approval_request", [], |row| row.get(0))
        .expect("an approval_request row");
    assert_eq!(
        kind, "PLAN_APPROVAL",
        "§4.3's four kinds never collapse; a plan approval is a PLAN_APPROVAL"
    );
}
