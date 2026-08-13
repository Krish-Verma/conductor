//! The probe cache: exact-match on the version triple, fail closed otherwise.
//!
//! Master plan §4.2: *"a stale or absent probe forces every dimension to `None`
//! — fail closed. This is the model's most important property: sandbox
//! behaviour changes with OS and CLI versions, and a hardcoded table would
//! silently become a lie after an upgrade."*

use conductor_core::containment::{Enforcement, ExecutionCapabilities, Informational};
use conductor_run::containment::cache::{self, CacheLookup, ProbeKey};
use conductor_store::Store;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open_or_create(dir.path().join("conductor.db")).expect("open store");
    (dir, store)
}

fn key() -> ProbeKey {
    ProbeKey::new("codex", "0.142.0", "codex-sandbox", "0.142.0", "macOS 26.6")
}

/// The measured Codex row from §4.2, used as "something worth caching".
fn measured() -> ExecutionCapabilities {
    ExecutionCapabilities {
        filesystem_write: Enforcement::Restricted,
        network_egress: Enforcement::Hard,
        control_surface: Enforcement::Hard,
        credential_read: Enforcement::None,
        tool_interception: Informational::new(Enforcement::None),
        exceptions: vec!["/tmp".into(), "/var/folders/x/T".into()],
    }
}

#[test]
fn an_absent_probe_is_fail_closed_on_every_dimension() {
    let (_dir, store) = store();

    let lookup = cache::lookup(store.conn(), &key()).expect("query");

    assert!(matches!(lookup, CacheLookup::Miss), "got {lookup:?}");
    assert!(
        lookup.capabilities().is_fail_closed(),
        "an unmeasured host must enforce nothing: {:?}",
        lookup.capabilities()
    );
}

#[test]
fn a_stored_probe_round_trips_exactly() {
    let (_dir, mut store) = store();
    cache::upsert(store.conn_mut(), &key(), &measured(), 1_700_000_000_000).expect("upsert");

    let lookup = cache::lookup(store.conn(), &key()).expect("query");

    match lookup {
        CacheLookup::Hit {
            capabilities,
            probed_at_ms,
        } => {
            assert_eq!(capabilities, measured());
            assert_eq!(probed_at_ms, 1_700_000_000_000);
        }
        other => panic!("expected a hit, got {other:?}"),
    }
}

#[test]
fn every_component_of_the_version_triple_invalidates_the_cache_on_its_own() {
    let (_dir, mut store) = store();
    cache::upsert(store.conn_mut(), &key(), &measured(), 1).expect("upsert");

    // Each of these is the same host with one thing upgraded or swapped. §4.2's
    // whole argument is that any of them can change what the sandbox enforces.
    let stale = [
        (
            "adapter upgraded",
            ProbeKey::new("codex", "0.143.0", "codex-sandbox", "0.142.0", "macOS 26.6"),
        ),
        (
            "launcher upgraded",
            ProbeKey::new("codex", "0.142.0", "codex-sandbox", "0.143.0", "macOS 26.6"),
        ),
        (
            "os upgraded",
            ProbeKey::new("codex", "0.142.0", "codex-sandbox", "0.142.0", "macOS 27.0"),
        ),
        (
            "different launcher",
            ProbeKey::new("codex", "0.142.0", "none", "0.142.0", "macOS 26.6"),
        ),
        (
            "different adapter",
            ProbeKey::new(
                "claude",
                "0.142.0",
                "codex-sandbox",
                "0.142.0",
                "macOS 26.6",
            ),
        ),
    ];

    for (why, stale_key) in stale {
        let lookup = cache::lookup(store.conn(), &stale_key).expect("query");
        assert!(
            matches!(lookup, CacheLookup::Miss),
            "{why}: a near-miss key must not hit — got {lookup:?}"
        );
        assert!(
            lookup.capabilities().is_fail_closed(),
            "{why}: a stale probe must force every dimension to None"
        );
    }

    // …and the original key still hits, so the misses above are about the key
    // and not about the row having failed to store.
    assert!(matches!(
        cache::lookup(store.conn(), &key()).expect("query"),
        CacheLookup::Hit { .. }
    ));
}

#[test]
fn re_probing_the_same_host_replaces_the_row_rather_than_accumulating() {
    let (_dir, mut store) = store();
    cache::upsert(store.conn_mut(), &key(), &measured(), 1).expect("first");

    let weaker = ExecutionCapabilities::fail_closed();
    cache::upsert(store.conn_mut(), &key(), &weaker, 2).expect("second");

    let rows: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM containment_probe", [], |r| r.get(0))
        .expect("count");
    assert_eq!(
        rows, 1,
        "the version triple is UNIQUE; re-probing overwrites"
    );

    match cache::lookup(store.conn(), &key()).expect("query") {
        CacheLookup::Hit {
            capabilities,
            probed_at_ms,
        } => {
            assert_eq!(capabilities, weaker, "the newest measurement wins");
            assert_eq!(probed_at_ms, 2);
        }
        other => panic!("expected a hit, got {other:?}"),
    }
}

#[test]
fn a_cached_row_that_cannot_be_parsed_fails_closed_instead_of_crashing() {
    let (_dir, mut store) = store();
    cache::upsert(store.conn_mut(), &key(), &measured(), 1).expect("upsert");
    store
        .conn()
        .execute(
            "UPDATE containment_probe SET capabilities = ?1",
            ["{\"filesystem_write\":\"OMNIPOTENT\"}"],
        )
        .expect("corrupt the row");

    let lookup = cache::lookup(store.conn(), &key()).expect("query");

    assert!(
        matches!(lookup, CacheLookup::Unusable { .. }),
        "got {lookup:?}"
    );
    assert!(
        lookup.capabilities().is_fail_closed(),
        "a row Conductor cannot read is worth exactly as much as no row"
    );
}
