//! One kill-restart cycle of S8's approval durability check.
//!
//! > **Verify.** Approval state survives 50 kill-restart cycles · no grant
//! > consumed twice.
//!
//! # Why this is a separate process
//!
//! The same reason S3 needed `conductor-s3-worker` and S4 needed
//! `conductor-s4-verifier`: the kill has to be real. A `SIGKILL` cannot be
//! caught, so there is no unwinding, no `Drop`, no flush and no chance to tidy
//! the database — which is exactly what SQLite sees during a power failure. A
//! "restart" simulated by dropping a `Store` and constructing another inside one
//! test process proves that `Store::open_existing` works, and nothing about
//! durability.
//!
//! The kill is **self-inflicted** for the reason the crash matrix records: an
//! external kill has to race a sleep to land between two particular statements,
//! and a self-inflicted one lands there every time.
//!
//! # What one cycle does
//!
//! ```text
//! CONSUME <outcome>       attempt the shared one-shot grant — at most one
//!                         cycle in the whole run may report `consumed`
//! COMMITTED <request-id>  a committed approval request with a TTL unique to
//!                         this cycle, so the parent can tell a preserved TTL
//!                         from a coincidence
//! GRANTED <grant-id>      on even cycles: a committed grant
//! DOOMED <request-id>     on cycles divisible by three: a request written
//!                         inside a transaction that is never committed, so
//!                         the parent can assert it is *absent* afterwards
//! ```
//!
//! then `SIGKILL`. Every line is flushed before the kill, so the parent's record
//! of what the process claimed to have done is complete even though the process
//! never got to say goodbye.

use std::io::Write;
use std::process::ExitCode;

use conductor_core::RunId;
use conductor_run::approval::binding::BindingHash;
use conductor_run::approval::kind::{Expiry, Subject};
use conductor_run::approval::store::{
    self as approvals, Consumption, GrantOptions, NewApprovalRequest,
};
use conductor_run::policy::model::{Action, Fact, FactSet, Scope};
use conductor_store::Store;

/// The run every cycle writes about. Matches the parent's fixture.
const RUN: &str = "r-0041";

/// Base TTL for the per-cycle request. The cycle number is added, so no two
/// cycles can share an expiry and a "preserved" TTL cannot be an accident.
const REQUEST_TTL_BASE_MS: i64 = 1_800_000_000_000;

/// TTL for the per-cycle grant, far enough out that nothing expires mid-run.
const GRANT_TTL_MS: i64 = 1_900_000_000_000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [db_path, cycle, shared_grant, binding] = args.as_slice() else {
        eprintln!("usage: conductor-s8-approval-victim <db> <cycle> <grant-id> <binding-hash>");
        return ExitCode::from(64);
    };
    let cycle: i64 = match cycle.parse() {
        Ok(cycle) => cycle,
        Err(err) => {
            eprintln!("cycle: {err}");
            return ExitCode::from(64);
        }
    };

    let mut store = match Store::open_existing(db_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("open {db_path}: {err}");
            return ExitCode::from(70);
        }
    };
    let policy_hash: String = match store.conn().query_row(
        "SELECT hash FROM policy_snapshot ORDER BY created_at DESC, hash LIMIT 1",
        [],
        |row| row.get(0),
    ) {
        Ok(hash) => hash,
        Err(err) => {
            eprintln!("the parent must have seeded a policy snapshot: {err}");
            return ExitCode::from(70);
        }
    };

    // 1. The shared one-shot grant. This is the line the whole test is about:
    //    across every cycle, at most one of these may say `consumed`.
    let outcome = match approvals::consume(
        store.conn_mut(),
        shared_grant,
        &BindingHash::from_stored(binding.clone()),
        now_ms(),
    ) {
        Ok(Consumption::Consumed { .. }) => "consumed".to_string(),
        Ok(Consumption::Reusable { .. }) => "reusable".to_string(),
        Ok(Consumption::Refused(refusal)) => format!("refused:{refusal}"),
        Err(err) => format!("error:{err}"),
    };
    say(&format!("CONSUME {outcome}"));

    // 2. A committed request whose TTL is unique to this cycle.
    let request_id = format!("AR-c{cycle}");
    let request = NewApprovalRequest {
        id: request_id.clone(),
        subject: Subject::PolicyAction {
            action: Action::parse("dependency.add.runtime"),
        },
        run_id: Some(RunId::new(RUN).expect("run id")),
        facts: facts(cycle),
        policy_hash: policy_hash.clone(),
        matched_rules: vec!["global.runtime-dependency".to_string()],
        explanation: format!("cycle {cycle}"),
        evidence_ref: None,
        expires: Expiry::At(REQUEST_TTL_BASE_MS + cycle),
    };
    if let Err(err) = approvals::request(store.conn_mut(), &request, now_ms()) {
        eprintln!("request: {err}");
        return ExitCode::from(70);
    }
    say(&format!("COMMITTED {request_id}"));

    // 3. Half the cycles also grant, so the grant machine is exercised across
    //    restarts and not only the request machine.
    if cycle % 2 == 0 {
        let grant_id = format!("AG-c{cycle}");
        let granted = approvals::grant(
            store.conn_mut(),
            &request_id,
            &GrantOptions {
                id: grant_id.clone(),
                scope: Scope::from_pairs([("run".to_string(), RUN.to_string())]),
                reuse: false,
                expires: Expiry::At(GRANT_TTL_MS),
                granted_by: "kill-cycle".to_string(),
                channel: "unix-socket".to_string(),
                nonce_hash: None,
            },
            now_ms(),
        );
        match granted {
            Ok(_) => say(&format!("GRANTED {grant_id}")),
            Err(err) => {
                eprintln!("grant: {err}");
                return ExitCode::from(70);
            }
        }
    }

    // 4. A write that must **not** survive. `BEGIN IMMEDIATE` and no commit: the
    //    kill below arrives with the row in an open transaction, and the parent
    //    asserts it is absent. Without this the run would only prove that
    //    committed rows survive, which is the easy half — the half that matters
    //    is that a half-finished approval never becomes a real one.
    if cycle % 3 == 0 {
        let doomed = format!("AR-doomed{cycle}");
        if let Err(err) = begin_and_write_without_committing(&mut store, &doomed, &policy_hash) {
            eprintln!("doomed write: {err}");
            return ExitCode::from(70);
        }
        say(&format!("DOOMED {doomed}"));
    }

    // 5. Die. `SIGKILL` because it cannot be caught: no unwinding, no `Drop`, no
    //    flush — the database is left exactly as a power cut would leave it.
    unsafe { libc::raise(libc::SIGKILL) };
    // Unreachable in practice. If the signal were ever blocked, exiting
    // non-zero and unmistakably is better than exiting `0` and letting the
    // parent record a kill that did not happen.
    eprintln!("SIGKILL did not arrive");
    ExitCode::from(97)
}

/// A request row written inside a transaction that is deliberately left open.
fn begin_and_write_without_committing(
    store: &mut Store,
    id: &str,
    policy_hash: &str,
) -> Result<(), rusqlite::Error> {
    let conn = store.conn_mut();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    conn.execute(
        "INSERT INTO approval_request
           (id, kind, subject, run_id, action, facts, facts_source, policy_hash,
            matched_rules, explanation, evidence_ref, state, requested_at, expires_at)
         VALUES (?1, 'POLICY_APPROVAL', NULL, ?2, 'dependency.add.runtime', '[]',
                 'deterministic', ?3, '[]', 'never committed', NULL, 'REQUESTED', 0, ?4)",
        rusqlite::params![id, RUN, policy_hash, REQUEST_TTL_BASE_MS],
    )?;
    Ok(())
}

/// Facts unique to the cycle, so each cycle's request binds to its own
/// operation rather than colliding with its predecessor's.
fn facts(cycle: i64) -> FactSet {
    let mut facts = FactSet::new();
    facts.push(Fact::deterministic("dependency", format!("dep{cycle}")));
    facts.push(Fact::deterministic("manifest", "Cargo.toml"));
    facts
}

/// One protocol line, flushed immediately — the kill will not flush it later.
fn say(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}
