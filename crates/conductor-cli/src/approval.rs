//! `conductor approval {list,show,approve,deny,revoke}` — master plan §4.3,
//! §7.1 and §7.2.
//!
//! # Every verb goes over the socket, and granting has no other route
//!
//! §7.1 lists `conductor approve` and `conductor deny` as **human-only,
//! socket-only** operations. This module is the client for all five verbs and
//! the server behind them, and there is deliberately **no** path from a verb to
//! the store that does not pass through [`crate::socket`]. A fallback that wrote
//! the grant directly whenever the socket was missing would be a grant produced
//! by anything that can run the binary — which is the file-shaped approval §4.3
//! exists to refuse. When there is no socket the answer is §7.2's code `2`, and
//! the request stays `REQUESTED`.
//!
//! Reads go the same way for a different reason: two routes to the same table
//! are two things to keep consistent, and `list`/`show` are what an operator
//! runs immediately before granting. One route means the thing they read is the
//! thing the server will act on.
//!
//! # The sixth verb: `plan.approve`
//!
//! §7.1's `conductor plan approve <version>` is annotated *"human-only,
//! socket-only"*, and §5.2 gives a plan version's `APPROVED` state to *"a human
//! at the control socket"*. It is therefore served **here**, beside the other
//! mutating verbs, rather than by a second dispatcher of its own: a second
//! server would be a second door into the room this one guards, and it would
//! pass a "goes over a socket" test while defeating the point of one. Its client
//! is [`crate::plan`], which holds no store handle at all — asserted by
//! `crates/conductor-cli/tests/plan_approve.rs` in the style of §4.3's existing
//! source scan.
//!
//! # `serve` is hidden, and that is not an oversight
//!
//! §7.1 cuts `daemon start/stop` — *"auto-started on demand; `doctor` reports
//! it"* — and S14 owns the daemon. Until S14 exists something has to start the
//! listener, so `approval serve` exists and is hidden: it is the S8 stand-in for
//! a daemon, not a fourteenth public command, and hiding it keeps §7.1's surface
//! honest in `--help`.
//!
//! # Which tier this actually delivers
//!
//! §4.3's tier table is a statement about the host, not about this code. The
//! socket alone is **tier C** — *"Not a boundary. Approvals are advisory."* —
//! and [`ServeArgs::arm_nonce`] is the only thing here that can reach tier B.
//! Tier A is not reachable by anything in this file: it is a measured
//! `control_surface: Hard` (M10/M11), and §4.2's eligibility check is what acts
//! on it. `serve` therefore prints the tier and §4.3's integrity sentence
//! verbatim at startup, so nobody has to infer it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use conductor_core::effect::OperationId;
use conductor_core::{Fence, PlanVersionState, ProjectId};
use conductor_run::approval::binding::Binding;
use conductor_run::approval::kind::{ApprovalKind, Expiry, ExpiryRule, Subject};
use conductor_run::approval::nonce::{NonceState, OperatorNonce, Tier};
use conductor_run::approval::revoke::{InFlight, RevocationOutcome};
use conductor_run::approval::store::{
    ApprovalGrantRow, ApprovalRequestRow, GrantOptions, NewApprovalRequest, RequestState,
};
use conductor_run::approval::{Authorization, revoke, store as approvals};
use conductor_run::plan::{self, ledger};
use conductor_run::policy::load as policy_load;
use conductor_run::policy::model::{FactSet, Origin, Scope};
use conductor_run::verify::profile;
use conductor_store::Store;
use serde_json::{Value, json};

use crate::exit;
use crate::socket::{self, AfterCall, ControlSocket, RpcRequest, RpcResponse, ServeEnd, rpc_code};
use crate::task::StoreArgs;

/// The default TTL for a grant whose kind requires one, in seconds.
///
/// §4.3's worked example grants at 14:03 and expires at 15:03. One hour is that
/// example, and it is a default rather than a constant because `--ttl` is how an
/// operator says otherwise.
const DEFAULT_GRANT_TTL_SECONDS: i64 = 3_600;

/// §4.3's `approval_grant.channel` for a grant made here.
const CHANNEL: &str = "unix-socket";

/// `conductor approval …`
#[derive(Debug, Subcommand)]
pub enum ApprovalCommand {
    /// Every approval request still waiting for a human.
    List,
    /// One request: its kind, facts, matched rules and any grants.
    Show(ShowArgs),
    /// Grant one request. Human-only, socket-only (§7.1).
    Approve(ApproveArgs),
    /// Refuse one request.
    Deny(DenyArgs),
    /// Take a grant back — §4.3's Scenario S.
    Revoke(RevokeArgs),
    /// Serve the control socket. The S8 stand-in for S14's daemon.
    #[command(hide = true)]
    Serve(ServeArgs),
}

/// Where the control socket is. §7.3's default is `$HOME/.conductor/`.
#[derive(Debug, Args)]
pub struct SocketArgs {
    /// Override §7.3's `$HOME/.conductor/conductor.sock`.
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,
}

/// `conductor approval show <request-id>`
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// The request, e.g. `AR-0031`.
    pub request_id: String,
}

/// `conductor approval approve <request-id>`
#[derive(Debug, Args)]
pub struct ApproveArgs {
    /// The request, e.g. `AR-0031`.
    pub request_id: String,
    /// The grant's id. Derived from the request when absent.
    #[arg(long)]
    pub grant_id: Option<String>,
    /// Scope entries, `key=value`. Repeatable. Defaults to the request's run.
    #[arg(long = "scope", value_name = "KEY=VALUE")]
    pub scope: Vec<String>,
    /// Seconds until the grant expires. Refused for the two kinds §4.3 says do
    /// not expire.
    #[arg(long)]
    pub ttl: Option<i64>,
    /// §4.3's `reuse`. **Off unless asked for**: a grant that silently persisted
    /// would authorize operations nobody was asked about.
    #[arg(long)]
    pub reuse: bool,
    /// The operator nonce, when the server is armed (§4.3 tier B).
    #[arg(long)]
    pub nonce: Option<String>,
}

/// `conductor approval deny <request-id>`
#[derive(Debug, Args)]
pub struct DenyArgs {
    /// The request, e.g. `AR-0031`.
    pub request_id: String,
    /// Why. Recorded for the human who reads it later.
    #[arg(long, default_value = "")]
    pub reason: String,
    /// The operator nonce, when the server is armed.
    #[arg(long)]
    pub nonce: Option<String>,
}

/// `conductor approval revoke <grant-id>`
#[derive(Debug, Args)]
pub struct RevokeArgs {
    /// The grant, e.g. `AG-0019`.
    pub grant_id: String,
    /// Why.
    #[arg(long, default_value = "")]
    pub reason: String,
    /// The ledger row for the effect this grant authorizes, when there is one.
    /// Without it only §4.3's first row is reachable.
    #[arg(long)]
    pub operation: Option<String>,
    /// The effect is executing **right now** — §4.3's row 3, which "cannot be
    /// cancelled". Only the process performing it knows, so it must be said.
    #[arg(long)]
    pub in_flight: bool,
    /// The operator nonce, when the server is armed.
    #[arg(long)]
    pub nonce: Option<String>,
}

/// `conductor approval serve`
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Arm §4.3's operator nonce and reveal it to the controlling terminal.
    ///
    /// **Default off**, per S8's scope line. Refuses to start when there is no
    /// controlling terminal rather than printing the secret somewhere an
    /// unsandboxed agent could read it — failing loudly to a weaker tier is the
    /// design.
    #[arg(long)]
    pub arm_nonce: bool,
}

// ---------------------------------------------------------------------------
// the client
// ---------------------------------------------------------------------------

/// Run one `approval` subcommand.
pub fn run(command: &ApprovalCommand, shared: &StoreArgs, socket_args: &SocketArgs) -> ExitCode {
    let path = match socket_args
        .socket
        .clone()
        .map(Ok)
        .unwrap_or_else(socket::default_socket_path)
    {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(exit::NOT_INITIALIZED);
        }
    };

    if let ApprovalCommand::Serve(args) = command {
        return serve(args, shared, &path);
    }

    let (method, params) = match request_for(command) {
        Ok(pair) => pair,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(exit::USAGE);
        }
    };

    match socket::call(&path, method, params) {
        Ok(result) => {
            if shared.json {
                match serde_json::to_string_pretty(&result) {
                    Ok(json) => println!("{json}"),
                    Err(err) => {
                        eprintln!("internal error: {err}");
                        return ExitCode::from(exit::INTERNAL);
                    }
                }
            } else {
                print!("{}", render(command, &result));
            }
            ExitCode::from(exit_code_for(command, &result))
        }
        // §7.2's `2` — "no project / not initialized". A missing control socket
        // is precisely that: Conductor is not up. It is deliberately **not** a
        // generic failure, because a wrapper script has to be able to tell "the
        // grant was refused" from "there was nothing to ask".
        Err(err @ socket::SocketError::NotListening { .. }) => {
            eprintln!("{err}\n  start one with `conductor approval serve`");
            ExitCode::from(exit::NOT_INITIALIZED)
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(exit::FAILURE)
        }
    }
}

/// The JSON-RPC call one subcommand makes.
fn request_for(command: &ApprovalCommand) -> Result<(&'static str, Value), String> {
    Ok(match command {
        ApprovalCommand::List => ("approval.list", json!({})),
        ApprovalCommand::Show(args) => ("approval.show", json!({ "request_id": args.request_id })),
        ApprovalCommand::Approve(args) => {
            let mut scope = BTreeMap::new();
            for entry in &args.scope {
                let (key, value) = split_pair(entry, "--scope")?;
                scope.insert(key, value);
            }
            (
                "approval.approve",
                json!({
                    "request_id": args.request_id,
                    "grant_id": args.grant_id,
                    "scope": if args.scope.is_empty() { Value::Null } else { json!(scope) },
                    "ttl_seconds": args.ttl,
                    "reuse": args.reuse,
                    "granted_by": granted_by(),
                    "nonce": args.nonce,
                }),
            )
        }
        ApprovalCommand::Deny(args) => (
            "approval.deny",
            json!({
                "request_id": args.request_id,
                "reason": args.reason,
                "nonce": args.nonce,
            }),
        ),
        ApprovalCommand::Revoke(args) => (
            "approval.revoke",
            json!({
                "grant_id": args.grant_id,
                "reason": args.reason,
                "operation": args.operation,
                "in_flight": args.in_flight,
                "nonce": args.nonce,
            }),
        ),
        ApprovalCommand::Serve(_) => unreachable!("serve does not go over the socket"),
    })
}

/// §7.2's codes, chosen by what the answer says rather than by the verb.
///
/// Code `3` — *"action required — approval or review pending"* — is the whole
/// reason `list` is scriptable: a wrapper can tell "a human is needed" from "the
/// command failed" without parsing anything.
fn exit_code_for(command: &ApprovalCommand, result: &Value) -> u8 {
    match command {
        ApprovalCommand::List => {
            let pending = result["pending"].as_array().map(Vec::len).unwrap_or(0);
            if pending > 0 {
                exit::ACTION_REQUIRED
            } else {
                exit::SUCCESS
            }
        }
        ApprovalCommand::Show(_) => {
            if result["request"]["state"] == RequestState::Requested.as_str() {
                exit::ACTION_REQUIRED
            } else {
                exit::SUCCESS
            }
        }
        _ => exit::SUCCESS,
    }
}

fn render(command: &ApprovalCommand, result: &Value) -> String {
    let mut out = String::new();
    match command {
        ApprovalCommand::List => {
            let empty = Vec::new();
            let pending = result["pending"].as_array().unwrap_or(&empty);
            if pending.is_empty() {
                out.push_str("no approval requests are waiting\n");
            }
            for entry in pending {
                out.push_str(&format!(
                    "{}  {}  {}  {}\n",
                    text(&entry["id"]),
                    text(&entry["kind"]),
                    text(&entry["action"]),
                    text(&entry["explanation"]),
                ));
            }
        }
        ApprovalCommand::Show(_) => {
            let request = &result["request"];
            out.push_str(&format!("request   {}\n", text(&request["id"])));
            out.push_str(&format!("kind      {}\n", text(&request["kind"])));
            out.push_str(&format!("action    {}\n", text(&request["action"])));
            out.push_str(&format!("state     {}\n", text(&request["state"])));
            out.push_str(&format!("run       {}\n", text(&request["run_id"])));
            out.push_str(&format!("policy    {}\n", text(&request["policy_hash"])));
            out.push_str(&format!("why       {}\n", text(&request["explanation"])));
            if let Some(facts) = request["facts"].as_array() {
                for fact in facts {
                    out.push_str(&format!(
                        "fact      {}={} ({})\n",
                        text(&fact["key"]),
                        text(&fact["value"]),
                        text(&fact["source"]),
                    ));
                }
            }
            if let Some(rules) = request["matched_rules"].as_array() {
                for rule in rules {
                    out.push_str(&format!("matched   {}\n", text(rule)));
                }
            }
            if let Some(grants) = result["grants"].as_array() {
                for grant in grants {
                    out.push_str(&format!(
                        "grant     {} {} binding {}\n",
                        text(&grant["id"]),
                        text(&grant["state"]),
                        text(&grant["binding_hash"]),
                    ));
                }
            }
        }
        ApprovalCommand::Approve(_) => {
            let grant = &result["grant"];
            out.push_str(&format!(
                "granted {} for request {}\n  binding {}\n  reuse {}\n",
                text(&grant["id"]),
                text(&grant["request_id"]),
                text(&grant["binding_hash"]),
                grant["reuse"],
            ));
        }
        ApprovalCommand::Deny(_) => {
            out.push_str(&format!("denied {}\n", text(&result["request"]["id"])));
        }
        ApprovalCommand::Revoke(_) => {
            out.push_str(&format!(
                "revoked {} — {}\n",
                text(&result["grant"]["id"]),
                text(&result["outcome"]),
            ));
        }
        ApprovalCommand::Serve(_) => {}
    }
    out
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

fn granted_by() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".to_string())
}

fn split_pair(entry: &str, flag: &str) -> Result<(String, String), String> {
    entry
        .split_once('=')
        .map(|(key, value)| (key.trim().to_string(), value.to_string()))
        .filter(|(key, _)| !key.is_empty())
        .ok_or_else(|| format!("{flag} expects KEY=VALUE, got {entry:?}"))
}

// ---------------------------------------------------------------------------
// the server
// ---------------------------------------------------------------------------

/// Publish the control socket and answer until it stops being ours.
fn serve(args: &ServeArgs, shared: &StoreArgs, path: &std::path::Path) -> ExitCode {
    let store_path = match shared
        .store
        .clone()
        .map(Ok)
        .unwrap_or_else(Store::default_path)
    {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(exit::NOT_INITIALIZED);
        }
    };
    let mut store = match Store::open_existing(&store_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(exit::NOT_INITIALIZED);
        }
    };

    // §4.3 tier B. Off unless asked for, and a failure to reveal is a failure to
    // start: printing the secret to a redirected stdout would hand it to the one
    // process it exists to keep it from.
    let nonce = if args.arm_nonce {
        match OperatorNonce::generate().and_then(|nonce| {
            nonce.reveal_to_controlling_terminal()?;
            Ok(nonce)
        }) {
            Ok(nonce) => Some(nonce),
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::from(exit::FAILURE);
            }
        }
    } else {
        None
    };
    let tier = Tier::of(
        conductor_core::containment::Enforcement::None,
        if nonce.is_some() {
            NonceState::Armed
        } else {
            NonceState::Off
        },
    );

    let socket = match ControlSocket::publish(path) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(exit::FAILURE);
        }
    };
    // Printed, never inferred. §4.3's tier table is the honesty this whole
    // module rests on, and an operator who has to guess which tier they are on
    // is an operator who will assume the strongest.
    let mode = match socket.mode() {
        Ok(mode) => format!("{mode:04o}"),
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(exit::FAILURE);
        }
    };
    println!(
        "conductor: serving {} (mode {mode}) — tier {tier}\n  {}\n  {}",
        socket.path().display(),
        tier.mechanism(),
        tier.integrity(),
    );

    let nonce_hash = nonce.as_ref().map(|nonce| nonce.hash().to_string());
    let end = socket.serve(|request| {
        let response = dispatch(&mut store, request, nonce_hash.as_deref());
        (response, AfterCall::Continue)
    });
    match end {
        Ok(ServeEnd::NoLongerPublished) => {
            eprintln!(
                "{}",
                socket::SocketError::NoLongerPublished {
                    path: path.to_path_buf()
                }
            );
            ExitCode::from(exit::FAILURE)
        }
        Ok(ServeEnd::HandlerStopped) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(exit::FAILURE)
        }
    }
}

/// One request in, one response out.
fn dispatch(store: &mut Store, request: &RpcRequest, nonce_hash: Option<&str>) -> RpcResponse {
    if let Some(expected) = nonce_hash
        && mutating(&request.method)
    {
        let presented = request.params["nonce"].as_str().unwrap_or_default();
        if !OperatorNonce::matches(expected, presented) {
            return RpcResponse::failed(
                request.id,
                rpc_code::REFUSED,
                "the operator nonce is missing or wrong (§4.3 tier B)",
            );
        }
    }

    let outcome = match request.method.as_str() {
        "approval.list" => list(store),
        "approval.show" => show(store, &request.params),
        "approval.approve" => approve(store, &request.params),
        "approval.deny" => deny(store, &request.params),
        "approval.revoke" => revoke_grant(store, &request.params),
        "plan.approve" => plan_approve(store, &request.params),
        // §6.5: "Importing is a **mutating** operation and goes through the
        // control socket, never a file an agent could write." Served here for
        // `plan.approve`'s reason — a second dispatcher would be a second door
        // into the room this one guards. The verb itself lives in
        // [`crate::review`], which also owns the client that may not touch a
        // store.
        "review.import" => {
            crate::review::import(store, &request.params).map_err(|refusal| Refusal {
                code: refusal.code,
                message: refusal.message,
            })
        }
        other => Err(Refusal {
            code: rpc_code::METHOD_NOT_FOUND,
            message: format!("no such method {other}"),
        }),
    };
    match outcome {
        Ok(result) => RpcResponse::ok(request.id, result),
        Err(refusal) => RpcResponse::failed(request.id, refusal.code, refusal.message),
    }
}

/// Whether a verb changes anything. Only these need the nonce: making a human
/// type a secret to *read* a pending list would train them to type it.
fn mutating(method: &str) -> bool {
    matches!(
        method,
        "approval.approve" | "approval.deny" | "approval.revoke" | "plan.approve" | "review.import"
    )
}

/// Why a verb refused.
struct Refusal {
    code: i64,
    message: String,
}

impl Refusal {
    fn invalid(message: impl Into<String>) -> Refusal {
        Refusal {
            code: rpc_code::INVALID_PARAMS,
            message: message.into(),
        }
    }

    fn refused(message: impl Into<String>) -> Refusal {
        Refusal {
            code: rpc_code::REFUSED,
            message: message.into(),
        }
    }
}

fn list(store: &Store) -> Result<Value, Refusal> {
    let pending = approvals::pending_requests(store.conn())
        .map_err(|err| Refusal::refused(err.to_string()))?;
    Ok(json!({
        "pending": pending.iter().map(request_json).collect::<Vec<Value>>(),
    }))
}

fn show(store: &Store, params: &Value) -> Result<Value, Refusal> {
    let id = string(params, "request_id")?;
    let row = approvals::request_row(store.conn(), &id)
        .map_err(|err| Refusal::refused(err.to_string()))?
        .ok_or_else(|| Refusal::invalid(format!("no approval request {id}")))?;
    let grants = grants_for(store, &id)?;
    Ok(json!({
        "request": request_json(&row),
        "grants": grants.iter().map(grant_json).collect::<Vec<Value>>(),
    }))
}

fn approve(store: &mut Store, params: &Value) -> Result<Value, Refusal> {
    let request_id = string(params, "request_id")?;
    let row = approvals::request_row(store.conn(), &request_id)
        .map_err(|err| Refusal::refused(err.to_string()))?
        .ok_or_else(|| Refusal::invalid(format!("no approval request {request_id}")))?;

    // §4.3's fourth column is the kind's, not the operator's. Refusing here as
    // well as at the write means the error names the flag rather than the table.
    let expires = match (row.kind.expiry_rule(), params["ttl_seconds"].as_i64()) {
        (ExpiryRule::Forbidden, Some(_)) => {
            return Err(Refusal::invalid(format!(
                "a {} does not expire (§4.3), so --ttl cannot be given",
                row.kind
            )));
        }
        (ExpiryRule::Forbidden, None) => Expiry::Never,
        (ExpiryRule::Mandatory, ttl) => {
            Expiry::At(now_ms() + ttl.unwrap_or(DEFAULT_GRANT_TTL_SECONDS) * 1_000)
        }
    };

    let scope = match params["scope"].as_object() {
        Some(pairs) => Scope::from_pairs(
            pairs
                .iter()
                .map(|(key, value)| (key.clone(), text(value)))
                .collect::<Vec<_>>(),
        ),
        // The request's own run is the default because §4.3's worked example
        // scopes a policy approval to `{run: r-0041}` and because a grant that
        // defaulted to *everywhere* would be the broadest possible reading of
        // the narrowest possible question.
        None => match &row.run_id {
            Some(run) => Scope::from_pairs([("run".to_string(), run.as_str().to_string())]),
            None => Scope::everywhere(),
        },
    };

    let grant_id = params["grant_id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| derive_grant_id(&request_id));

    let granted = approvals::grant(
        store.conn_mut(),
        &request_id,
        &GrantOptions {
            id: grant_id,
            scope,
            reuse: params["reuse"].as_bool().unwrap_or(false),
            expires,
            granted_by: params["granted_by"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            channel: CHANNEL.to_string(),
            nonce_hash: None,
        },
        now_ms(),
    )
    .map_err(|err| Refusal::refused(err.to_string()))?;

    // Recomputed from the request, exactly as the runtime will at use time. If
    // these ever disagreed the grant would authorize nothing and the failure
    // would look like a policy bug, so it is checked where it is cheap.
    let recomputed = Binding {
        subject: row.subject.clone(),
        facts: row.facts.clone(),
        policy_hash: row.policy_hash.clone(),
        scope: granted.scope.clone(),
    }
    .hash();
    if recomputed != granted.stored_binding {
        return Err(Refusal::refused(format!(
            "the grant stored {} but this request recomputes {recomputed}",
            granted.stored_binding
        )));
    }

    Ok(json!({ "grant": grant_json(&granted) }))
}

fn deny(store: &mut Store, params: &Value) -> Result<Value, Refusal> {
    let request_id = string(params, "request_id")?;
    let reason = params["reason"].as_str().unwrap_or_default().to_string();
    let row = approvals::deny(store.conn_mut(), &request_id, &reason, now_ms())
        .map_err(|err| Refusal::refused(err.to_string()))?;
    Ok(json!({ "request": request_json(&row) }))
}

fn revoke_grant(store: &mut Store, params: &Value) -> Result<Value, Refusal> {
    let grant_id = string(params, "grant_id")?;
    let reason = params["reason"].as_str().unwrap_or_default().to_string();

    // §4.3's rows 2, 3 and 4 are decided by the side-effect ledger, and the
    // ledger row is named by the operation the run intended. A revocation
    // without one can only reach row 1 — which is the common case and the
    // correct answer for a grant nothing has acted on yet.
    let operation = params["operation"].as_str().map(OperationId::from_stored);
    let in_flight = if params["in_flight"].as_bool().unwrap_or(false) {
        InFlight::Yes
    } else {
        InFlight::No
    };

    let run = approvals::run_for_grant(store.conn(), &grant_id)
        .map_err(|err| Refusal::refused(err.to_string()))?
        .ok_or_else(|| {
            // Honest gap rather than a synthetic fence: §4.3's plan approval and
            // review acceptance have no run, and the findings and run-state
            // changes revocation writes are statements *about a run* (§4.7).
            // S11 and S13 own those kinds; inventing a run id here would put a
            // fabricated identifier in the fencing token.
            Refusal::invalid(format!(
                "grant {grant_id} is not attached to a run; revoking a plan \
                 approval or a review acceptance is owned by the slices that \
                 create them (S11, S13) and is not reachable in S8"
            ))
        })?;
    let (_, epoch) = store
        .run_state(&run)
        .map_err(|err| Refusal::refused(err.to_string()))?
        .ok_or_else(|| Refusal::refused(format!("run {run} is not in the store")))?;

    let outcome = revoke::revoke(
        store.conn_mut(),
        &Fence::new(run, epoch),
        &grant_id,
        operation.as_ref(),
        in_flight,
        now_ms(),
        &reason,
    )
    .map_err(|err| Refusal::refused(err.to_string()))?;

    let row = approvals::grant_row(store.conn(), &grant_id)
        .map_err(|err| Refusal::refused(err.to_string()))?
        .ok_or_else(|| Refusal::refused(format!("grant {grant_id} vanished")))?;
    Ok(json!({
        "outcome": outcome_name(&outcome),
        "grant": grant_json(&row),
    }))
}

// ---------------------------------------------------------------------------
// `plan.approve` — §7.1's human-only, socket-only verb
// ---------------------------------------------------------------------------

/// Make one plan version authoritative — §5.2's `AWAITING_APPROVAL → APPROVED`.
///
/// # Why one call does both halves of §4.3's request/grant pair
///
/// Every other approval in §4.3 has two actors: Conductor's policy engine
/// *raises* the request when a gated action comes up, and a human *answers* it.
/// A plan approval has one — §4.3's table gives it no policy action and no
/// gate, and §7.1's surface has no `plan request` verb for a machine to call.
/// The human running this command is both the question and the answer, so the
/// request and the grant are written together, and the request/grant pair is
/// the *record* of that decision rather than a conversation between two parties.
/// It is still a real §4.3 grant: [`ledger::approve`] re-derives it from the
/// rows and refuses anything that is not a live `PLAN_APPROVAL` over
/// [`Subject::PlanVersion`] for this exact version.
///
/// The ids carry `now_ms` because §4.3 spends a grant on use. §5.2's restart
/// clause — a content-hash mismatch on an `APPROVED` plan is *"cleared by
/// re-running `conductor plan approve`"* — therefore needs a **new** grant each
/// time, which is exactly the human decision it is asking for.
///
/// # §3.3's control 2 is enforced here, not trusted
///
/// > Conductor reads plan approval **only** from the registered repository's
/// > working tree, never from a run branch.
///
/// The client offers a root; this refuses it if the project is already
/// registered somewhere else. Without that check, an agent that reached the
/// socket could re-point a registered project at its own workspace clone and
/// have its edited plan approved — which is the precise failure §3.3 is written
/// against. A project that is *not* yet registered is registered from the
/// offered root, because [`ledger::register_project`] is the only function in
/// the ledger that takes a path at all, and something has to be first.
fn plan_approve(store: &mut Store, params: &Value) -> Result<Value, Refusal> {
    let offered = PathBuf::from(string(params, "repo_root")?);
    let root = offered
        .canonicalize()
        .map_err(|err| Refusal::invalid(format!("repo_root {}: {err}", offered.display())))?;
    let version = params["version"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| Refusal::invalid("version is required and is a whole number"))?;

    let config = plan::project::load(&root).map_err(|err| Refusal::invalid(err.to_string()))?;
    if let Some(declared) = params["project_id"].as_str()
        && declared != config.id
    {
        return Err(Refusal::refused(format!(
            "the client named project {declared} but {} declares {}; §3.3 reads \
             approval only from the registered tree, and two answers to \"which \
             project is this?\" is not one of them",
            root.display(),
            config.id
        )));
    }
    let project_id =
        ProjectId::new(config.id.clone()).map_err(|err| Refusal::invalid(err.to_string()))?;

    let registered = store
        .project(&project_id)
        .map_err(|err| Refusal::refused(err.to_string()))?;
    match &registered {
        Some(row) if Path::new(&row.root_path) != root => {
            return Err(Refusal::refused(format!(
                "project {project_id} is registered at {} and this approval was \
                 offered {}; §3.3 control 2 reads plan approval only from the \
                 registered working tree, so re-pointing it at another tree is \
                 refused rather than followed",
                row.root_path,
                root.display()
            )));
        }
        Some(_) => {}
        None => {
            ledger::register_project(store, &root, now_ms())
                .map_err(|err| Refusal::refused(err.to_string()))?;
        }
    }

    // §3.7's clarification 3: "the validator takes the catalogue as a parameter
    // and the caller assembles it". Read from the registered tree, like
    // everything else this verb reads.
    let catalogue_path = root.join(".conductor/verification.yaml");
    let catalogue = plan::check_ids(
        &profile::load(&catalogue_path)
            .map_err(|err| Refusal::invalid(err.to_string()))?
            .profile,
    );

    let plan_version = ledger::plan_version_id(&project_id, version);
    if store
        .plan_version(&plan_version)
        .map_err(|err| Refusal::refused(err.to_string()))?
        .is_none()
    {
        ledger::register_plan_version(store, &project_id, version, &catalogue)
            .map_err(|err| Refusal::refused(err.to_string()))?;
    }
    let row = store
        .plan_version(&plan_version)
        .map_err(|err| Refusal::refused(err.to_string()))?
        .ok_or_else(|| Refusal::refused(format!("plan version {plan_version} vanished")))?;

    // §5.2's `──request──►` edge. `plan approve` walks it because §7.1 has no
    // separate verb for it and a plan approval has nobody else to raise it.
    match row.state {
        PlanVersionState::Validated => {
            store
                .set_plan_state(&plan_version, PlanVersionState::AwaitingApproval)
                .map_err(|err| Refusal::refused(err.to_string()))?;
        }
        // Already asked, or already answered — the second is §5.2's restart
        // clause, which re-approves an edited document without moving the state.
        PlanVersionState::AwaitingApproval | PlanVersionState::Approved => {}
        // §5.2: "Invalid: `DRAFT → APPROVED`". A row nothing validated must not
        // become authoritative because a human typed the version number.
        PlanVersionState::Draft => {
            return Err(Refusal::refused(format!(
                "plan version {plan_version} is DRAFT; §5.2 makes DRAFT → \
                 APPROVED invalid, because approving a document nothing \
                 validated is approving §3.7's refusals along with it"
            )));
        }
        PlanVersionState::Superseded => {
            return Err(Refusal::refused(format!(
                "plan version {plan_version} is SUPERSEDED; §5.2 gives it no \
                 successor, and a later version is already authoritative"
            )));
        }
    }

    let policy_hash = plan_policy_hash(&root)?;
    let subject = Subject::PlanVersion {
        plan_version_id: plan_version.as_str().to_string(),
    };
    let stamp = now_ms();
    let request_id = format!("AR-{plan_version}-{stamp}");
    approvals::request(
        store.conn_mut(),
        &NewApprovalRequest {
            id: request_id.clone(),
            subject,
            // §4.3's plan approval is not about a run, and `approval revoke`
            // says so out loud: a grant with no run cannot be revoked in S8
            // because the findings revocation writes are statements about a run.
            run_id: None,
            facts: FactSet::new(),
            policy_hash: policy_hash.clone(),
            matched_rules: Vec::new(),
            explanation: format!(
                "a human at the control socket is making plan version v{version} \
                 authoritative (§5.2)"
            ),
            evidence_ref: None,
            // §4.3's fourth column: a plan approval does not expire.
            expires: Expiry::Never,
        },
        stamp,
    )
    .map_err(|err| Refusal::refused(err.to_string()))?;

    let granted = approvals::grant(
        store.conn_mut(),
        &request_id,
        &GrantOptions {
            id: format!("AG-{plan_version}-{stamp}"),
            // One plan version, which is §4.3's granularity for this kind. A
            // grant scoped wider would be a grant that could be spent on a
            // version nobody was asked about.
            scope: Scope::from_pairs([(
                "plan_version".to_string(),
                plan_version.as_str().to_string(),
            )]),
            reuse: false,
            expires: Expiry::Never,
            granted_by: params["granted_by"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            channel: CHANNEL.to_string(),
            nonce_hash: None,
        },
        stamp,
    )
    .map_err(|err| Refusal::refused(err.to_string()))?;

    let approval = ledger::approve(
        store,
        &project_id,
        version,
        &Authorization::Authorized {
            grant_id: granted.id,
        },
        stamp,
    )
    .map_err(|err| Refusal::refused(err.to_string()))?;

    // §5.2's task list, written here because **this** is the event that changes
    // what work exists (added at S12).
    //
    // `materialize` refuses anything short of `APPROVED`, so before this call
    // site existed the only things that could reach it were the library, its
    // tests and `conductor recover` — which meant that after a human approved a
    // plan, `conductor task list` was empty and `task run` had no task to claim.
    // It is also what makes acceptance row 21 reachable from the product path:
    // approving v4 supersedes v3's idle tasks and *carries* the one holding an
    // active run, and nothing else in the system performs that transition.
    //
    // Decisions are synced first because §6.5's packet resolves a task's
    // `decisions:` refs against registered rows, and a task materialized ahead of
    // the decision it names would produce a refusal on its first run.
    //
    // Order relative to `approve`: after. `materialize` reads the plan through
    // the store's `plan_version` row and refuses a version that is not
    // `APPROVED`, so it cannot run first — and a materialisation that failed
    // must not un-approve a decision a human already made at the socket. The
    // failure is reported instead, and re-running `plan approve` completes it,
    // because both calls are idempotent.
    let validated = revalidate(&root, version, approval.content_hash.as_str(), &catalogue)?;
    conductor_run::decision::register_decisions(store, &project_id)
        .map_err(|err| Refusal::refused(err.to_string()))?;
    let materialized = plan::materialize(store, &project_id, version, &validated, now_ms())
        .map_err(|err| Refusal::refused(err.to_string()))?;

    Ok(json!({
        "plan_version": approval.plan_version_id.as_str(),
        "version": approval.version,
        "content_hash": approval.content_hash.as_str(),
        "approver": approval.approver,
        "approved_at": approval.approved_at_ms,
        "policy_hash": approval.policy_hash,
        "sidecar": plan::approved_path(approval.version),
        "tasks_created": materialized
            .created
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        "tasks_carried": materialized
            .carried
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        "tasks_superseded": materialized
            .superseded
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
    }))
}

/// The `ValidatedPlan` [`plan::materialize`] needs, re-derived from the document
/// that was just approved.
///
/// # Why this exists rather than reusing the one `register_plan_version` returns
///
/// [`ledger::register_plan_version`] hands back the validated document with the
/// row — but it only runs when the row does **not** exist yet. Re-approving an
/// existing version (§5.2's restart clause) takes the other branch, and
/// materialisation still needs the document. So the choice is between a second
/// read here and a `register_plan_version` call that would try to walk
/// `APPROVED → VALIDATED`, which §5.2 forbids.
///
/// # The window this closes
///
/// A second read is a second chance for the file to have changed. Rather than
/// ignore that, the text is hashed and compared against the hash the approval was
/// *granted over*: if they differ, somebody wrote to the plan between
/// [`ledger::approve`] and here, and the answer is a refusal naming both hashes —
/// §3.3's *"execution halts — it is never resynced"*, applied to a window a few
/// microseconds wide. The approval itself stands; it was real. What is refused is
/// materialising work from a document nobody approved.
fn revalidate(
    root: &Path,
    version: u32,
    approved_hash: &str,
    catalogue: &std::collections::BTreeSet<String>,
) -> Result<plan::ValidatedPlan, Refusal> {
    let path = root.join(plan::plan_path(version));
    let text = std::fs::read_to_string(&path)
        .map_err(|err| Refusal::refused(format!("plan document {}: {err}", path.display())))?;
    let hash = plan::content_hash(&text).map_err(|err| Refusal::refused(err.to_string()))?;
    if hash.as_str() != approved_hash {
        return Err(Refusal::refused(format!(
            "{} changed between the approval and materialising its tasks: \
             approved {approved_hash}, now {}. §3.3 halts on a disagreement \
             rather than resyncing it — re-run `conductor plan approve {version}` \
             on the document you mean",
            path.display(),
            hash.as_str()
        )));
    }
    let document = plan::parse(&text).map_err(|err| Refusal::refused(err.to_string()))?;
    plan::validate(&document, catalogue).map_err(|report| Refusal::refused(report.to_string()))
}

/// The policy snapshot the human is approving under — §3.1's sidecar carries it.
///
/// Resolved from disk rather than defaulted, because §3.1 names *"policy hash"*
/// as one of the four things `.conductor/plans/vN/APPROVED` records, and a
/// constant there would make every approval look like it happened under the
/// same rules. A file that is absent is not an error — an operator with no
/// global policy has no global policy — but a file that is present and
/// unreadable is, for `crate::policy::load_if_present`'s reason: carrying on
/// would record a hash for a policy that is not the one in force.
fn plan_policy_hash(root: &Path) -> Result<String, Refusal> {
    let global = policy_load::global_policy_path();
    let project = root.join(policy_load::PROJECT_POLICY_PATH);
    let resolved = policy_load::resolve_documents(
        crate::policy::load_if_present(global.as_deref(), Origin::Global)
            .map_err(Refusal::invalid)?,
        crate::policy::load_if_present(Some(&project), Origin::Project)
            .map_err(Refusal::invalid)?,
        None,
    )
    .map_err(|err| Refusal::invalid(err.to_string()))?;
    Ok(policy_load::snapshot(&resolved).hash)
}

/// §4.3's four rows, named so a script can tell them apart.
fn outcome_name(outcome: &RevocationOutcome) -> &'static str {
    match outcome {
        RevocationOutcome::NotYetConsumed { .. } => "NOT_YET_CONSUMED",
        RevocationOutcome::AbortedBeforeStarting { .. } => "ABORTED_BEFORE_STARTING",
        RevocationOutcome::CannotCancelInFlight { .. } => "CANNOT_CANCEL_IN_FLIGHT",
        RevocationOutcome::PostHocRevocation { .. } => "POST_HOC_REVOCATION",
        RevocationOutcome::AlreadyRevoked { .. } => "ALREADY_REVOKED",
    }
}

fn grants_for(store: &Store, request_id: &str) -> Result<Vec<ApprovalGrantRow>, Refusal> {
    let ids: Vec<String> = {
        let mut stmt = store
            .conn()
            .prepare("SELECT id FROM approval_grant WHERE request_id = ?1 ORDER BY granted_at, id")
            .map_err(|err| Refusal::refused(err.to_string()))?;
        let rows = stmt
            .query_map([request_id], |row| row.get::<_, String>(0))
            .map_err(|err| Refusal::refused(err.to_string()))?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(|err| Refusal::refused(err.to_string()))?
    };
    ids.iter()
        .map(|id| {
            approvals::grant_row(store.conn(), id)
                .map_err(|err| Refusal::refused(err.to_string()))?
                .ok_or_else(|| Refusal::refused(format!("grant {id} vanished")))
        })
        .collect()
}

fn request_json(row: &ApprovalRequestRow) -> Value {
    json!({
        "id": row.id,
        "kind": row.kind.as_str(),
        "action": row.subject.action_column(),
        "subject": row.subject.to_string(),
        "run_id": row.run_id.as_ref().map(|id| id.as_str()),
        "state": row.state.as_str(),
        "policy_hash": row.policy_hash,
        "matched_rules": row.matched_rules,
        "explanation": row.explanation,
        "evidence_ref": row.evidence_ref,
        "facts_source": row.facts_source.as_str(),
        "facts": row.facts.iter().map(|fact| json!({
            "key": fact.key,
            "value": fact.value,
            "source": fact.source.as_str(),
        })).collect::<Vec<Value>>(),
        "requested_at": row.requested_at,
        "expires_at": row.expires.as_millis(),
        "granularity": granularity(row.kind),
    })
}

fn grant_json(row: &ApprovalGrantRow) -> Value {
    json!({
        "id": row.id,
        "request_id": row.request_id,
        "binding_hash": row.stored_binding.as_str(),
        "scope": row.scope.pairs().map(|(k, v)| (k.clone(), v.clone()))
            .collect::<BTreeMap<String, String>>(),
        "reuse": row.reuse,
        "state": row.state.as_str(),
        "channel": row.channel,
        "granted_by": row.granted_by,
        "granted_at": row.granted_at,
        "expires_at": row.expires.as_millis(),
        "resolved_at": row.resolved_at,
    })
}

fn granularity(kind: ApprovalKind) -> &'static str {
    kind.granularity()
}

/// `AR-0031` → `AG-0031`. §4.3's own example pairs them that way, and a derived
/// id means a retry that lost its answer writes the same row rather than a
/// second grant for one question.
fn derive_grant_id(request_id: &str) -> String {
    match request_id.strip_prefix("AR-") {
        Some(rest) => format!("AG-{rest}"),
        None => format!("AG-{request_id}"),
    }
}

fn string(params: &Value, key: &str) -> Result<String, Refusal> {
    params[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Refusal::invalid(format!("{key} is required")))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty but real store. Empty is enough for the nonce tests: what they
    /// assert is *which check refused*, and the nonce check runs before any row
    /// is looked at.
    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open_or_create(dir.path().join("conductor.db")).expect("store");
        (dir, store)
    }

    fn rpc(method: &str, params: Value) -> RpcRequest {
        RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 7,
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn a_mutating_verb_is_refused_without_the_operator_nonce_and_allowed_with_it() {
        // §4.3 tier B: "grant requires a nonce printed **only** to the
        // controlling terminal". The check has to sit in front of the mutating
        // verbs, and it has to be the *hash* that is compared, because the
        // server may have been restarted since the nonce was revealed.
        let (_dir, mut store) = store();
        let nonce = OperatorNonce::generate().expect("/dev/urandom");
        let hash = nonce.hash().to_string();

        for method in ["approval.approve", "approval.deny", "approval.revoke"] {
            let refused = dispatch(
                &mut store,
                &rpc(method, json!({"request_id": "AR-1", "grant_id": "AG-1"})),
                Some(&hash),
            );
            let error = refused.error.expect("a refusal");
            assert_eq!(error.code, rpc_code::REFUSED);
            assert!(
                error.message.contains("operator nonce"),
                "{method} must refuse on the nonce: {}",
                error.message
            );

            let wrong = dispatch(
                &mut store,
                &rpc(
                    method,
                    json!({"request_id": "AR-1", "grant_id": "AG-1", "nonce": "not the nonce"}),
                ),
                Some(&hash),
            );
            assert!(
                wrong
                    .error
                    .expect("a refusal")
                    .message
                    .contains("operator nonce"),
                "{method} must refuse a wrong nonce"
            );
        }

        // POSITIVE CONTROL: with the right nonce the verb gets **past** the
        // nonce gate and fails on the thing it should — the request that is not
        // there. Without this, the refusals above would only be asserting that
        // `AR-1` does not exist.
        let past_the_gate = dispatch(
            &mut store,
            &rpc(
                "approval.approve",
                json!({"request_id": "AR-1", "nonce": "PLACEHOLDER"}),
            ),
            Some(&conductor_core::effect::content_hash(
                "PLACEHOLDER".as_bytes(),
            )),
        );
        let error = past_the_gate.error.expect("a refusal");
        assert_eq!(error.code, rpc_code::INVALID_PARAMS);
        assert!(
            error.message.contains("no approval request AR-1"),
            "the right nonce must reach the verb: {}",
            error.message
        );
    }

    #[test]
    fn a_read_verb_does_not_ask_for_the_nonce() {
        // Making a human type a secret to *read* a pending list is how a secret
        // stops being treated as one.
        let (_dir, mut store) = store();
        let nonce = OperatorNonce::generate().expect("/dev/urandom");
        let answer = dispatch(
            &mut store,
            &rpc("approval.list", json!({})),
            Some(nonce.hash()),
        );
        assert!(answer.error.is_none(), "{:?}", answer.error);
        assert_eq!(answer.result.expect("a result")["pending"], json!([]));
    }

    #[test]
    fn with_no_nonce_armed_a_mutating_verb_is_not_asked_for_one() {
        // §4.3's S8 scope: the nonce is "default off". A server that demanded
        // one anyway would make tier C unusable, and an unusable approval path
        // is an approval path people work around.
        let (_dir, mut store) = store();
        let answer = dispatch(
            &mut store,
            &rpc("approval.approve", json!({"request_id": "AR-1"})),
            None,
        );
        let error = answer.error.expect("a refusal");
        assert_eq!(error.code, rpc_code::INVALID_PARAMS);
        assert!(error.message.contains("no approval request AR-1"));
    }

    #[test]
    fn an_unknown_method_is_not_a_silently_successful_one() {
        let (_dir, mut store) = store();
        let answer = dispatch(&mut store, &rpc("approval.bless", json!({})), None);
        assert_eq!(
            answer.error.expect("a refusal").code,
            rpc_code::METHOD_NOT_FOUND
        );
    }

    #[test]
    fn a_grant_id_is_derived_from_its_request_id() {
        assert_eq!(derive_grant_id("AR-0031"), "AG-0031");
        assert_eq!(derive_grant_id("weird"), "AG-weird");
    }
}
