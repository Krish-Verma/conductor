//! The layering §4.3 asks for, asserted by reading the source — S8.
//!
//! > **Additional control, all tiers:** the binary reachable from a workspace,
//! > if any, exposes read-only verbs only and physically lacks the approval code
//! > path — asserted by a source-scan test that fails if anyone wires approval
//! > into it.
//!
//! # "if any" — what is actually reachable from a workspace in v1
//!
//! There is no approval shim in v1: nothing hands an agent a Conductor binary
//! and invites it to call verbs. What *is* reachable from a workspace is
//! narrower and more concrete — the binaries Conductor itself executes **in the
//! agent's position**, inside the workspace or inside the probe's sandbox:
//!
//! | Binary | Crate | Why it is workspace-facing |
//! |---|---|---|
//! | `conductor-fake-agent` | `conductor-agent` | It *is* the agent. §6.1 runs it as a subprocess with the workspace as its cwd. |
//! | `conductor-probe-action` | `conductor-run` | §4.2's probe payload, executed under each launcher being measured — i.e. exactly where an agent runs, under exactly the containment an agent gets. |
//!
//! Everything else — the worker, the verifier, the vertical, the CLI, the store
//! harnesses — *is* Conductor. It runs outside the workspace, holds the lease,
//! and is supposed to reach the approval code.
//!
//! `conductor-probe-action` is the interesting one and the reason this file
//! exists: it lives in `conductor-run`, the crate that *contains* the approval
//! module, so nothing about the dependency graph stops someone wiring approval
//! into it. `conductor-fake-agent` is protected structurally as well — its crate
//! does not depend on `conductor-run` at all — and that is asserted separately,
//! because a structural guarantee and a textual one fail in different ways.
//!
//! # Why this test is a source scan and not a call-graph analysis
//!
//! A call-graph analysis would need a compiler. A source scan needs a substring,
//! and the substring is available because there is exactly **one** spelling of
//! the approval surface from outside its module: a path through `approval`.
//! [`the_approval_surface_has_exactly_one_spelling`] is the test that keeps that
//! true — if anyone re-exports an approval type under another name, the scan
//! stops being sufficient and that test fails first.
//!
//! # The scanner's own positive control
//!
//! A source scan that read nothing would pass every assertion here. So
//! [`the_scanner_finds_approval_where_approval_actually_is`] runs the identical
//! scanner over files that *do* contain the approval code path and asserts it
//! reports them. Five consecutive slices shipped a test that passed while
//! proving nothing; this one is not allowed to be the sixth.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Whether a binary runs where an agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Facing {
    /// Executed in the agent's position: inside a workspace, or inside the
    /// probe's sandbox under the launcher being measured.
    Workspace,
    /// *Is* Conductor. Runs outside the workspace and is supposed to be able to
    /// reach the approval code.
    Conductor,
}

/// Every binary target in the repository, classified.
///
/// A census, not a filter: [`every_binary_in_the_repository_is_classified`]
/// walks the tree and fails on anything missing from this table, so adding a
/// binary forces a decision about which side of §4.3's line it is on. A test
/// that only scanned the binaries it already knew about would silently ignore
/// the shim somebody adds next year.
const CENSUS: &[(&str, Facing, &str)] = &[
    (
        "crates/conductor-agent/src/bin/conductor-fake-agent.rs",
        Facing::Workspace,
        "it is the agent: §6.1 runs it as a subprocess with the workspace as cwd",
    ),
    (
        "crates/conductor-run/src/bin/conductor-probe-action.rs",
        Facing::Workspace,
        "§4.2's probe payload, run under the launcher being measured — the \
         agent's position, with the agent's containment",
    ),
    (
        "crates/conductor-run/src/bin/conductor-s3-worker.rs",
        Facing::Conductor,
        "the foreground worker: it holds the lease and drives the agent",
    ),
    (
        "crates/conductor-run/src/bin/conductor-s4-verifier.rs",
        Facing::Conductor,
        "S4's verification runner, executed by Conductor outside the workspace",
    ),
    (
        "crates/conductor-run/src/bin/conductor-s5-vertical.rs",
        Facing::Conductor,
        "the S5 vertical as a separate process — Conductor itself",
    ),
    (
        "crates/conductor-agent/src/bin/conductor-s10-codex-replay.rs",
        Facing::Workspace,
        "S10's recorded Codex. It is spawned exactly where `codex exec` would \
         be — the agent's position, with the agent's containment — so it lives \
         in the crate that *cannot* link the runtime, beside the fake agent, \
         rather than merely promising not to",
    ),
    (
        "crates/conductor-run/src/bin/conductor-s10-codex-worker.rs",
        Facing::Conductor,
        "S3's worker with the Codex adapter substituted: it holds the lease and \
         drives the agent, so it is Conductor and not the thing being driven",
    ),
    (
        "crates/conductor-run/src/bin/conductor-s8-approval-victim.rs",
        Facing::Conductor,
        "S8's kill-restart cycle. It grants and consumes approvals on purpose, \
         which is only safe because Conductor runs it — never an agent",
    ),
    (
        "crates/conductor-store/src/bin/conductor-kill-victim.rs",
        Facing::Conductor,
        "S1's SIGKILL harness: it opens the store, which no agent-side binary does",
    ),
    (
        "crates/conductor-store/src/bin/conductor-claim-bench.rs",
        Facing::Conductor,
        "the claim benchmark: Conductor-side measurement",
    ),
    (
        "crates/conductor-cli/src/main.rs",
        Facing::Conductor,
        "the operator CLI. It is the *other* end of §4.3's socket and must be \
         able to grant",
    ),
];

/// Substrings whose presence in a workspace-facing binary means the approval
/// code path was wired into it.
///
/// # Why the bare word `approval` is not one of them
///
/// It was, and it produced a false positive that is worth recording rather than
/// silencing: `conductor-fake-agent` contains the literal
/// `b"grant approval please\n"`, which it writes down the control socket in
/// acceptance row 28's scenario — *"agent reaches for the control socket →
/// connect denied (sandboxed) or attempt logged; finding; no grant created"*.
/// That is a **payload**, not a code path. It is the attack this layering
/// defends against, deliberately placed in the fixture that measures whether the
/// attack works. A scan that failed on it would be forbidding the test of the
/// property, which is the wrong direction.
///
/// So the needles are code-shaped: a path into the module, or a name that only
/// exists inside it. Every one of them is asserted to match somewhere in the
/// real approval code by [`the_scanner_finds_approval_where_approval_actually_is`],
/// so a needle that never matches anything cannot quietly shrink the scan.
///
/// `crate::approval` is deliberately absent: a `src/bin/*.rs` links its own
/// library as an external crate, so from a binary the only spelling is
/// `conductor_run::approval`. `crate::` would not compile there.
const FORBIDDEN: &[&str] = &[
    "conductor_run::approval",
    "approval::",
    "approvals::",
    "BindingHash",
    "binding_hash",
    "OperatorNonce",
    "GrantOptions",
    "NewApprovalRequest",
    "ApprovalKind",
    "RevocationOutcome",
    "POST_HOC_REVOCATION",
    "REVOKED_WHILE_IN_FLIGHT",
];

/// Files that between them contain every needle in [`FORBIDDEN`], and that are
/// unambiguously approval code.
const APPROVAL_CORPUS: &[&str] = &[
    "crates/conductor-run/src/approval/store.rs",
    "crates/conductor-run/src/approval/binding.rs",
    "crates/conductor-run/src/approval/revoke.rs",
    "crates/conductor-run/src/approval/nonce.rs",
    "crates/conductor-run/src/approval/kind.rs",
    "crates/conductor-run/tests/approval.rs",
    "crates/conductor-cli/src/approval.rs",
];

/// Crates a workspace-facing binary must not name.
///
/// §4.3 asks for "read-only verbs only". S8's answer is stronger and easier to
/// check: a workspace-facing binary exposes **no Conductor verbs at all**,
/// because it links neither the runtime nor the store. There is no read-only
/// subset to get wrong because there is no subset.
const FORBIDDEN_CRATES: &[&str] = &["conductor_run", "conductor_store"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/conductor-run has two ancestors")
        .to_path_buf()
}

/// Read a file, failing loudly rather than scanning an empty string.
///
/// The failure mode this guards against is the one that makes a source scan
/// worthless: a path typo turns every assertion into "the empty string contains
/// no forbidden substring".
fn read(relative: &str) -> String {
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

/// Every forbidden substring present in `source`.
fn approval_hits(source: &str) -> Vec<&'static str> {
    FORBIDDEN
        .iter()
        .copied()
        .filter(|needle| source.contains(needle))
        .collect()
}

/// Every binary source file in the repository, relative to the root.
fn discover_binaries() -> BTreeSet<String> {
    let root = repo_root();
    let crates = root.join("crates");
    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir(&crates).expect("crates/ must exist") {
        let crate_dir = entry.expect("read crates/").path();
        let src = crate_dir.join("src");
        let main = src.join("main.rs");
        if main.is_file() {
            found.insert(relative(&root, &main));
        }
        let bin = src.join("bin");
        if bin.is_dir() {
            for entry in std::fs::read_dir(&bin).expect("read src/bin") {
                let path = entry.expect("read src/bin").path();
                if path.extension().is_some_and(|ext| ext == "rs") {
                    found.insert(relative(&root, &path));
                }
            }
        }
    }
    found
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("inside the repository")
        .to_string_lossy()
        .to_string()
}

// ---------------------------------------------------------------------------
// the census
// ---------------------------------------------------------------------------

#[test]
fn every_binary_in_the_repository_is_classified() {
    let discovered = discover_binaries();
    let classified: BTreeSet<String> = CENSUS
        .iter()
        .map(|(path, _, _)| (*path).to_string())
        .collect();

    let unclassified: Vec<&String> = discovered.difference(&classified).collect();
    assert!(
        unclassified.is_empty(),
        "these binaries are not classified in CENSUS, so §4.3's source scan does \
         not cover them: {unclassified:?}. Decide whether each one runs in the \
         agent's position before adding it."
    );
    let missing: Vec<&String> = classified.difference(&discovered).collect();
    assert!(
        missing.is_empty(),
        "CENSUS names binaries that do not exist: {missing:?}"
    );
    assert!(
        CENSUS
            .iter()
            .any(|(_, facing, _)| *facing == Facing::Workspace),
        "a census with no workspace-facing binary would make every scan below \
         vacuously true"
    );
}

// ---------------------------------------------------------------------------
// the scan §4.3 asks for
// ---------------------------------------------------------------------------

#[test]
fn no_workspace_facing_binary_contains_the_approval_code_path() {
    let mut scanned = 0;
    for (path, facing, why) in CENSUS {
        if *facing != Facing::Workspace {
            continue;
        }
        scanned += 1;
        let source = read(path);
        let hits = approval_hits(&source);
        assert!(
            hits.is_empty(),
            "{path} is workspace-facing ({why}) and names the approval surface \
             {hits:?}. §4.3: such a binary must \"physically lack the approval \
             code path\" — an agent that can run it must not be one call away \
             from granting itself an approval."
        );
    }
    assert!(
        scanned >= 2,
        "only {scanned} workspace-facing binaries scanned"
    );
}

#[test]
fn no_workspace_facing_binary_links_the_runtime_or_the_store() {
    // §4.3's "read-only verbs only", made checkable: there is no read-only
    // subset to get wrong, because there is no Conductor surface at all.
    for (path, facing, why) in CENSUS {
        if *facing != Facing::Workspace {
            continue;
        }
        let source = read(path);
        for krate in FORBIDDEN_CRATES {
            assert!(
                !source.contains(krate),
                "{path} is workspace-facing ({why}) and names `{krate}`. A binary \
                 an agent can run must not carry Conductor's verbs, read-only or \
                 otherwise."
            );
        }
    }
}

#[test]
fn the_crate_that_holds_the_agent_binary_cannot_link_the_approval_module() {
    // The structural half. `conductor-fake-agent` cannot reach the approval code
    // even by accident, because the crate it lives in does not depend on the
    // crate the approval code is in. A textual scan and a dependency edge fail
    // in different ways — the scan catches a call, this catches the *ability* to
    // make one — so both are asserted.
    let manifest = std::fs::read_to_string(repo_root().join("crates/conductor-agent/Cargo.toml"))
        .expect("conductor-agent's manifest");
    assert!(
        !manifest.contains("conductor-run"),
        "conductor-agent has grown a dependency on conductor-run, so the fake \
         agent can now link the approval module. §4.3 wants that impossible, not \
         merely unused."
    );
    // POSITIVE CONTROL: the manifest is real and does declare dependencies, so
    // the assertion above is about `conductor-run` and not about an empty file.
    assert!(
        manifest.contains("conductor-core"),
        "the manifest read back does not look like conductor-agent's"
    );
}

#[test]
fn the_approval_surface_has_exactly_one_spelling() {
    // The scan above is a substring match on `approval`, and that is sufficient
    // only while every route into the approval module goes through a path that
    // contains the word. An alias — `pub use approval::store::grant as grant;`
    // somewhere else in the crate — would let a workspace-facing binary reach
    // the code without the substring ever appearing. That is the hole this test
    // exists to close, and it closes it before the scan can be fooled rather
    // than after.
    let root = repo_root().join("crates/conductor-run/src");
    let mut aliases = Vec::new();
    let mut files = 0;
    visit(&root, &mut |path| {
        // Inside the approval module itself, re-exports are the module's own
        // business — the `mod.rs` facade is exactly that.
        if path.components().any(|part| part.as_os_str() == "approval") {
            return;
        }
        files += 1;
        let source = std::fs::read_to_string(path).expect("read");
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("pub use") && trimmed.contains("approval") {
                aliases.push(format!("{}: {trimmed}", path.display()));
            }
        }
    });
    assert!(
        files > 10,
        "only {files} files scanned; that cannot be right"
    );
    assert!(
        aliases.is_empty(),
        "the approval surface is re-exported outside its module, so a source \
         scan for `approval` no longer sees every route into it: {aliases:?}"
    );
}

fn visit(dir: &Path, f: &mut impl FnMut(&Path)) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            visit(&path, f);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            f(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// the scanner's own positive control
// ---------------------------------------------------------------------------

#[test]
fn the_scanner_finds_approval_where_approval_actually_is() {
    // Without this, every assertion above could be satisfied by a scanner that
    // read nothing, matched nothing, or was pointed at the wrong tree. Each file
    // below genuinely contains the approval code path, and the scanner must say
    // so — using the identical `read` + `approval_hits` pair the real scan uses.
    for path in APPROVAL_CORPUS {
        let hits = approval_hits(&read(path));
        assert!(
            !hits.is_empty(),
            "the scanner found nothing in {path}, which is approval code. The \
             scan of the workspace-facing binaries therefore proves nothing."
        );
    }

    // And each individual needle matches something, so a typo in FORBIDDEN
    // cannot quietly reduce the scan to a shorter list.
    let corpus: String = APPROVAL_CORPUS
        .iter()
        .map(|path| read(path))
        .collect::<Vec<String>>()
        .join("\n");
    for needle in FORBIDDEN {
        assert!(
            corpus.contains(needle),
            "{needle:?} matches nothing in the approval code, so it contributes \
             nothing to the scan"
        );
    }
}

#[test]
fn the_agent_may_reach_for_the_socket_but_may_not_link_the_code_behind_it() {
    // Acceptance row 28 — "**Agent reaches for the control socket**: agent
    // connects to `conductor.sock` → connect denied (sandboxed) or attempt
    // logged · finding · **no grant created**".
    //
    // The two halves of that row are asserted here together because they look
    // contradictory from a distance and are not. The fake agent *must* be able
    // to attempt the connection — that is the measurement — and must *not* be
    // able to reach the code that would answer it. The first is a socket write;
    // the second is a link edge.
    let agent = read("crates/conductor-agent/src/bin/conductor-fake-agent.rs");
    assert!(
        agent.contains("UnixStream::connect"),
        "row 28 needs the agent to actually try the socket"
    );
    assert!(
        approval_hits(&agent).is_empty(),
        "and to have no way of servicing its own attempt"
    );
}

#[test]
fn the_conductor_side_binaries_are_not_scanned_and_that_is_deliberate() {
    // Recorded as an assertion rather than a comment: the CLI is *supposed* to
    // contain the approval code path — it is the operator's end of §4.3's
    // socket. If the scan ever grew to cover it, granting would stop working and
    // the failure would look like a layering violation.
    let cli = read("crates/conductor-cli/src/main.rs");
    assert!(
        cli.contains("approval"),
        "the operator CLI must reach the approval verbs; §7.1 lists them"
    );
    assert!(
        CENSUS.iter().any(
            |(path, facing, _)| *path == "crates/conductor-cli/src/main.rs"
                && *facing == Facing::Conductor
        ),
        "the CLI must be classified Conductor-side, or the scan above would fail \
         on it for doing its job"
    );
}
