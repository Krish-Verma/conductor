//! `.conductor/decisions/D-*.md` — master plan §3.1, §3.6, §5.1 (slice S11
//! task 5).
//!
//! # What these tests are guarding against
//!
//! Four things, each the mirror image of a way this module could quietly fail
//! its job:
//!
//! 1. **A parser that loses prose.** §3.6: *"the value is prose a human reads
//!    and a packet quotes."* A decision that reformats or trims the argument
//!    on the way in is a decision a later packet quotes wrong.
//!    [`a_well_formed_decision_loads_with_its_prose_intact`] is the positive
//!    control this whole file rests on.
//! 2. **A parser that is too permissive.** Unlike a plan document, a
//!    decision's frontmatter is *"schema-validated"* — an unknown status or an
//!    unknown key must refuse, not load and silently mean nothing.
//! 3. **A `supersedes` chain that can dangle.** A reference to an id nothing
//!    knows about must refuse, naming both ends, before anything is written —
//!    not resolve to `None` and quietly stop superseding anything.
//! 4. **Supersession that is not actually append-only.** Flipping the target
//!    to `SUPERSEDED` must be the only thing that happens to it: the row must
//!    still be readable, and the file on disk must still hold its original
//!    prose. A "supersede" that deleted anything would fail S11's whole
//!    objective on its own acceptance line.
//!
//! # Two tiers of setup
//!
//! [`load_all`](conductor_run::decision::load_all) is pure filesystem — no
//! store, no git — so its own tests use a bare `tempfile::tempdir()`.
//! [`register_decisions`](conductor_run::decision::register_decisions) reads
//! `root_path` back out of a registered `project` row (the same control
//! `plan::ledger` draws for every function after `register_project`), so its
//! tests go through a real git repository and `ledger::register_project`,
//! exactly as `tests/plan_ledger.rs` does for the plan half.

use std::path::Path;

use conductor_core::ProjectId;
use conductor_git::run_git_ok;
use conductor_run::decision::{self, DecisionError, DecisionSyncError};
use conductor_run::plan::ledger;
use conductor_store::{DecisionStatus, Store};

const PROJECT_YAML: &str = "\
project:
  id: p-decisions
  default_branch: main
  adapter: codex
";

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the parent directory");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// A `.conductor/decisions/D-*.md` document with the four frontmatter fields
/// §3.6 names, `supersedes` only when asked for.
fn decision_md(id: &str, status: &str, supersedes: Option<&str>, body: &str) -> String {
    let mut text = format!("---\nid: {id}\nstatus: {status}\ndate: 2026-08-15\n");
    if let Some(target) = supersedes {
        text.push_str(&format!("supersedes: {target}\n"));
    }
    text.push_str("---\n");
    text.push_str(body);
    text
}

/// A git repository with one commit and `.conductor/project.yaml`, no
/// decisions written yet.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    run_git_ok(root, &["init", "-q", "-b", "main"]).expect("git init");
    run_git_ok(root, &["config", "user.email", "decisions@example.invalid"]).expect("email");
    run_git_ok(root, &["config", "user.name", "Decisions Test"]).expect("name");
    write(root, ".conductor/project.yaml", PROJECT_YAML);
    run_git_ok(root, &["add", "-A"]).expect("add");
    run_git_ok(root, &["commit", "-q", "-m", "initial"]).expect("commit");
    dir
}

fn store(dir: &tempfile::TempDir) -> Store {
    Store::open_or_create(dir.path().join("conductor.db")).expect("open the store")
}

/// A registered project, no decisions synced into the store yet.
fn registered(dir: &tempfile::TempDir) -> (Store, ProjectId) {
    let mut store = store(dir);
    let project = ledger::register_project(&mut store, dir.path(), 1_000).expect("register");
    (store, project.id)
}

// ---------------------------------------------------------------------------
// `load_all` — pure, no store
// ---------------------------------------------------------------------------

#[test]
fn a_well_formed_decision_loads_with_its_prose_intact() {
    // POSITIVE CONTROL. A loader that refused everything would pass every
    // refusal test below and fail this one.
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        ".conductor/decisions/D-0001-clone-not-worktree.md",
        &decision_md(
            "D-0001",
            "ACCEPTED",
            None,
            "Per-run local clones. The argument in full.\n",
        ),
    );
    let decisions = decision::load_all(dir.path()).expect("loads");
    assert_eq!(decisions.len(), 1);
    let d = &decisions[0];
    assert_eq!(d.id, "D-0001");
    assert_eq!(d.status, DecisionStatus::Accepted);
    assert_eq!(d.date, "2026-08-15");
    assert_eq!(d.supersedes, None);
    assert_eq!(d.body, "Per-run local clones. The argument in full.\n");
    assert_eq!(
        d.source_path,
        ".conductor/decisions/D-0001-clone-not-worktree.md"
    );
}

#[test]
fn a_repository_with_no_decisions_directory_yields_no_decisions_not_a_refusal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let decisions = decision::load_all(dir.path()).expect("a missing directory is not a refusal");
    assert!(decisions.is_empty());
}

#[test]
fn load_all_orders_decisions_by_id_not_by_filename_or_discovery_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Filenames deliberately out of id order, and out of filename-alphabetic
    // order too (the "b" file would sort before the "a" file by name alone).
    write(
        dir.path(),
        ".conductor/decisions/D-0010-b-later.md",
        &decision_md("D-0010", "OPEN", None, "third\n"),
    );
    write(
        dir.path(),
        ".conductor/decisions/D-0001-a-first.md",
        &decision_md("D-0001", "OPEN", None, "first\n"),
    );
    write(
        dir.path(),
        ".conductor/decisions/D-0002-c-second.md",
        &decision_md("D-0002", "OPEN", None, "second\n"),
    );
    let loaded = decision::load_all(dir.path()).expect("loads");
    let ids: Vec<&str> = loaded.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, vec!["D-0001", "D-0002", "D-0010"]);
}

#[test]
fn a_file_not_matching_the_d_star_md_pattern_is_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        ".conductor/decisions/README.md",
        "not a decision\n",
    );
    write(
        dir.path(),
        ".conductor/decisions/D-0001-x.md",
        &decision_md("D-0001", "OPEN", None, "…\n"),
    );
    let decisions = decision::load_all(dir.path()).expect("loads");
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].id, "D-0001");
}

#[test]
fn an_unknown_status_is_refused_and_names_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        ".conductor/decisions/D-0001-x.md",
        &decision_md("D-0001", "MAYBE", None, "…\n"),
    );
    let error = decision::load_all(dir.path()).expect_err("refused");
    let rendered = error.to_string();
    assert!(rendered.contains("MAYBE"), "{rendered}");
    assert!(
        matches!(
            error,
            DecisionSyncError::Decision {
                source: DecisionError::UnknownStatus { .. },
                ..
            }
        ),
        "must be an unknown-status refusal, not something else"
    );
}

#[test]
fn a_document_with_no_frontmatter_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        ".conductor/decisions/D-0001-x.md",
        "Just prose, no frontmatter at all.\n",
    );
    let error = decision::load_all(dir.path()).expect_err("refused");
    assert!(
        matches!(
            error,
            DecisionSyncError::Decision {
                source: DecisionError::MissingFrontmatter,
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn duplicate_ids_across_files_are_refused_naming_the_id_and_both_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        ".conductor/decisions/D-0001-first.md",
        &decision_md("D-0001", "OPEN", None, "one\n"),
    );
    write(
        dir.path(),
        ".conductor/decisions/D-0001-second.md",
        &decision_md("D-0001", "OPEN", None, "two\n"),
    );
    let error = decision::load_all(dir.path()).expect_err("refused");
    let rendered = error.to_string();
    assert!(rendered.contains("D-0001"), "{rendered}");
    assert!(rendered.contains("D-0001-first.md"), "{rendered}");
    assert!(rendered.contains("D-0001-second.md"), "{rendered}");
    assert!(matches!(error, DecisionSyncError::DuplicateId { .. }));
}

// ---------------------------------------------------------------------------
// `register_decisions` — the store half
// ---------------------------------------------------------------------------

#[test]
fn registering_decisions_records_them_in_the_store_as_open() {
    // POSITIVE CONTROL for the write path.
    let dir = repo();
    write(
        dir.path(),
        ".conductor/decisions/D-0001-x.md",
        &decision_md("D-0001", "ACCEPTED", None, "…\n"),
    );
    let (mut store, project) = registered(&dir);
    let rows = decision::register_decisions(&mut store, &project).expect("registers");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "D-0001");
    // The frontmatter's own claimed status (ACCEPTED) is not what the store
    // records: `upsert_decision` always starts a decision `OPEN`, and this
    // decision names no `supersedes`, so nothing moves it further. See
    // `decision::model`'s "`status` is validated, not obeyed".
    assert_eq!(rows[0].status, DecisionStatus::Open);
    assert_eq!(rows[0].supersedes, None);
}

#[test]
fn a_supersedes_pointing_at_nothing_is_refused_and_names_both_ends() {
    let dir = repo();
    write(
        dir.path(),
        ".conductor/decisions/D-0002-x.md",
        &decision_md("D-0002", "ACCEPTED", Some("D-0099"), "…\n"),
    );
    let (mut store, project) = registered(&dir);
    let error = decision::register_decisions(&mut store, &project).expect_err("refused");
    let rendered = error.to_string();
    assert!(
        rendered.contains("D-0002") && rendered.contains("D-0099"),
        "the refusal must name both ends: {rendered}"
    );
    assert!(matches!(error, DecisionSyncError::UnknownSupersedes { .. }));
    assert!(
        store
            .decisions_for_project(&project)
            .expect("read")
            .is_empty(),
        "a batch refused for one bad file must leave no row behind"
    );
}

#[test]
fn superseding_flips_the_targets_status_and_the_superseded_decision_stays_readable() {
    let dir = repo();
    write(
        dir.path(),
        ".conductor/decisions/D-0001-old.md",
        &decision_md("D-0001", "ACCEPTED", None, "The original argument.\n"),
    );
    write(
        dir.path(),
        ".conductor/decisions/D-0002-new.md",
        &decision_md(
            "D-0002",
            "ACCEPTED",
            Some("D-0001"),
            "The revised argument.\n",
        ),
    );
    let (mut store, project) = registered(&dir);
    let rows = decision::register_decisions(&mut store, &project).expect("registers");
    assert_eq!(rows.len(), 2);

    let old = store
        .decision("D-0001")
        .expect("read")
        .expect("still present — append-only, nothing is deleted");
    assert_eq!(
        old.status,
        DecisionStatus::Superseded,
        "superseding flips the target"
    );

    let new = store.decision("D-0002").expect("read").expect("present");
    assert_eq!(
        new.status,
        DecisionStatus::Open,
        "the superseding decision itself is unaffected by the act of superseding"
    );

    // Append-only in full: the superseded decision is still readable from
    // disk, with its own prose completely intact. Nothing was deleted.
    let reloaded = decision::load_all(dir.path()).expect("reload");
    let reloaded_old = reloaded
        .iter()
        .find(|d| d.id == "D-0001")
        .expect("still on disk");
    assert_eq!(reloaded_old.body, "The original argument.\n");
    assert_eq!(reloaded_old.content_hash(), old.content_hash);
}

#[test]
fn a_supersedes_naming_a_sibling_from_the_same_sync_resolves_regardless_of_id_order() {
    let dir = repo();
    // D-0001 supersedes D-0002 — the reverse of the usual "newer supersedes
    // older" direction, exercised to prove resolution does not depend on
    // which id happens to sort first.
    write(
        dir.path(),
        ".conductor/decisions/D-0001-x.md",
        &decision_md("D-0001", "ACCEPTED", Some("D-0002"), "…\n"),
    );
    write(
        dir.path(),
        ".conductor/decisions/D-0002-x.md",
        &decision_md("D-0002", "OPEN", None, "…\n"),
    );
    let (mut store, project) = registered(&dir);
    decision::register_decisions(&mut store, &project).expect("registers");
    assert_eq!(
        store
            .decision("D-0002")
            .expect("read")
            .expect("present")
            .status,
        DecisionStatus::Superseded
    );
}

#[test]
fn re_registering_the_same_decisions_is_idempotent() {
    let dir = repo();
    write(
        dir.path(),
        ".conductor/decisions/D-0001-old.md",
        &decision_md("D-0001", "ACCEPTED", None, "…\n"),
    );
    write(
        dir.path(),
        ".conductor/decisions/D-0002-new.md",
        &decision_md("D-0002", "ACCEPTED", Some("D-0001"), "…\n"),
    );
    let (mut store, project) = registered(&dir);
    decision::register_decisions(&mut store, &project).expect("first sync");
    // A second sync must not fail trying to re-supersede an already
    // SUPERSEDED target: SUPERSEDED is terminal in the store's own machine,
    // and §3.5's recovery path re-syncs `.conductor/` after total local loss
    // — it must be safe to run more than once.
    let rows = decision::register_decisions(&mut store, &project).expect("second sync");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        store
            .decision("D-0001")
            .expect("read")
            .expect("present")
            .status,
        DecisionStatus::Superseded
    );
}

#[test]
fn registering_decisions_for_an_unregistered_project_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open_or_create(dir.path().join("conductor.db")).expect("open the store");
    let project = ProjectId::new("p-nonexistent").expect("id");
    let error = decision::register_decisions(&mut store, &project).expect_err("refused");
    assert!(matches!(error, DecisionSyncError::UnknownProject { .. }));
}
