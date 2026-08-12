//! `conductor doctor` — report on the store, git, the agent adapters and the
//! control-socket directory.
//!
//! **Reporting never creates state.** Without `--init-store`, doctor opens an
//! existing store read-write but never creates one and never migrates: a
//! diagnostic that brings a database into existence has changed the thing it
//! was asked to describe, and "the store is missing" is a finding, not a
//! problem to paper over. `--init-store` is the explicit initialisation path.
//!
//! Absent adapters and absent git are reported as **facts**, not failures: this
//! machine may legitimately have neither, and S1 does not need them.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use conductor_store::Store;
use conductor_store::schema::SUPPORTED_SCHEMA_VERSION;
use serde::Serialize;

/// Agent adapters v1 knows about (master plan §6.2, §6.3).
const ADAPTERS: &[&str] = &["codex", "claude"];

/// Arguments for `conductor doctor`.
#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Emit the report as JSON.
    #[arg(long)]
    pub json: bool,

    /// Store path. Defaults to `~/.local/share/conductor/conductor.db`.
    #[arg(long, value_name = "PATH")]
    pub store: Option<PathBuf>,

    /// Create and migrate the store if it is absent. Reporting alone never
    /// creates a database.
    #[arg(long)]
    pub init_store: bool,
}

/// The whole report.
#[derive(Debug, Serialize)]
pub struct Report {
    /// True when every check that can fail, passed.
    pub ok: bool,
    /// Exit code this report implies (§7.2).
    pub exit_code: u8,
    /// Store health.
    pub store: StoreReport,
    /// The `git` binary.
    pub git: ToolReport,
    /// Agent adapters, in v1 order.
    pub adapters: Vec<ToolReport>,
    /// The control-socket directory (§7.3). S1 only reports on it.
    pub socket_dir: SocketDirReport,
}

/// Store health.
#[derive(Debug, Serialize)]
pub struct StoreReport {
    /// Path inspected.
    pub path: String,
    /// Whether the database file exists.
    pub exists: bool,
    /// Whether this report was allowed to create it.
    pub initialized_by_this_command: bool,
    /// Highest applied schema version.
    pub schema_version: Option<i64>,
    /// Highest version this binary supports.
    pub supported_schema_version: i64,
    /// Versions known but not applied.
    pub pending_migrations: Vec<i64>,
    /// `PRAGMA integrity_check` output.
    pub integrity_check: Vec<String>,
    /// `PRAGMA foreign_key_check` violation count.
    pub foreign_key_violations: Option<usize>,
    /// Pragma values actually in effect on the connection.
    pub pragmas: BTreeMap<String, String>,
    /// Pragmas whose readback did not match, as `name: expected != actual`.
    pub pragma_mismatches: Vec<String>,
    /// Overall verdict.
    pub healthy: bool,
    /// Why not, when unhealthy.
    pub error: Option<String>,
}

/// An external binary.
#[derive(Debug, Serialize)]
pub struct ToolReport {
    /// Program name.
    pub name: String,
    /// Whether it was found on `PATH`.
    pub present: bool,
    /// Resolved path.
    pub path: Option<String>,
    /// First line of `--version`.
    pub version: Option<String>,
}

/// `$HOME/.conductor/`.
#[derive(Debug, Serialize)]
pub struct SocketDirReport {
    /// Directory path.
    pub path: Option<String>,
    /// Whether it exists.
    pub exists: bool,
    /// Permission bits, `0700`-style.
    pub mode: Option<String>,
    /// Where the socket will live (§7.3).
    pub socket_path: Option<String>,
    /// Whether that path exists yet. S1 never creates it.
    pub socket_exists: bool,
}

/// Build the report and pick the exit code.
pub fn build(args: &DoctorArgs) -> Report {
    let store_path = match args.store.clone() {
        Some(path) => Ok(path),
        None => Store::default_path(),
    };

    let store = match store_path {
        Ok(path) => inspect_store(&path, args.init_store),
        Err(err) => StoreReport {
            path: "<unknown>".to_string(),
            exists: false,
            initialized_by_this_command: false,
            schema_version: None,
            supported_schema_version: SUPPORTED_SCHEMA_VERSION,
            pending_migrations: Vec::new(),
            integrity_check: Vec::new(),
            foreign_key_violations: None,
            pragmas: BTreeMap::new(),
            pragma_mismatches: Vec::new(),
            healthy: false,
            error: Some(err.to_string()),
        },
    };

    let healthy = store.healthy;
    Report {
        ok: healthy,
        // §7.2: 2 is "no project / not initialized / store unhealthy". A missing
        // adapter or missing git is not an error here.
        exit_code: if healthy { 0 } else { 2 },
        store,
        git: inspect_tool("git"),
        adapters: ADAPTERS.iter().map(|name| inspect_tool(name)).collect(),
        socket_dir: inspect_socket_dir(),
    }
}

fn inspect_store(path: &Path, init: bool) -> StoreReport {
    let mut report = StoreReport {
        path: path.display().to_string(),
        exists: path.exists(),
        initialized_by_this_command: false,
        schema_version: None,
        supported_schema_version: SUPPORTED_SCHEMA_VERSION,
        pending_migrations: Vec::new(),
        integrity_check: Vec::new(),
        foreign_key_violations: None,
        pragmas: BTreeMap::new(),
        pragma_mismatches: Vec::new(),
        healthy: false,
        error: None,
    };

    let opened = if init {
        let existed = report.exists;
        let store = Store::open_or_create(path);
        if store.is_ok() {
            report.exists = true;
            report.initialized_by_this_command = !existed;
        }
        store
    } else if report.exists {
        Store::open_existing(path)
    } else {
        report.error = Some(format!(
            "no store at {} — run `conductor doctor --init-store` to create it",
            path.display()
        ));
        return report;
    };

    let store = match opened {
        Ok(store) => store,
        Err(err) => {
            report.error = Some(err.to_string());
            return report;
        }
    };

    match store.pragmas() {
        Ok(pragmas) => {
            report.pragma_mismatches = pragmas
                .mismatches()
                .into_iter()
                .map(|(name, expected, actual)| {
                    format!("{name}: expected {expected}, got {actual}")
                })
                .collect();
            report.pragmas = pragmas.values;
        }
        Err(err) => report.error = Some(err.to_string()),
    }

    match store.schema_version() {
        Ok(version) => report.schema_version = version,
        Err(err) => report.error = Some(err.to_string()),
    }

    match conductor_store::migrate::pending(store.conn()) {
        Ok(pending) => report.pending_migrations = pending.iter().map(|m| m.version).collect(),
        Err(err) => {
            // A store from the future is a real diagnosis, not a crash.
            if report.error.is_none() {
                report.error = Some(err.to_string());
            }
        }
    }

    match store.integrity_check() {
        Ok(check) => report.integrity_check = check,
        Err(err) => {
            report.integrity_check = vec![format!("integrity_check failed: {err}")];
            if report.error.is_none() {
                report.error = Some(err.to_string());
            }
        }
    }

    match store.foreign_key_check() {
        Ok(count) => report.foreign_key_violations = Some(count),
        Err(err) => {
            if report.error.is_none() {
                report.error = Some(err.to_string());
            }
        }
    }

    report.healthy = report.error.is_none()
        && report.exists
        && report.integrity_check == ["ok"]
        && report.foreign_key_violations == Some(0)
        && report.pragma_mismatches.is_empty()
        && report.schema_version.is_some()
        && report.pending_migrations.is_empty();

    if !report.healthy && report.error.is_none() {
        report.error = Some(match () {
            _ if report.integrity_check != ["ok"] => {
                format!("integrity_check returned {:?}", report.integrity_check)
            }
            _ if !report.pending_migrations.is_empty() => format!(
                "{} migration(s) pending: {:?}",
                report.pending_migrations.len(),
                report.pending_migrations
            ),
            _ if !report.pragma_mismatches.is_empty() => report.pragma_mismatches.join("; "),
            _ => "store is unhealthy".to_string(),
        });
    }

    report
}

fn inspect_tool(name: &str) -> ToolReport {
    let path = which(name);
    let version = path.as_ref().and_then(|p| first_line_of_version(p));
    ToolReport {
        name: name.to_string(),
        present: path.is_some(),
        path: path.map(|p| p.display().to_string()),
        version,
    }
}

/// Resolve `name` against `PATH` without spawning anything.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            return Some(candidate);
        }
    }
    None
}

fn first_line_of_version(program: &Path) -> Option<String> {
    let output = Command::new(program).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(|line| line.trim().to_string())
}

fn inspect_socket_dir() -> SocketDirReport {
    let Some(home) = std::env::var_os("HOME") else {
        return SocketDirReport {
            path: None,
            exists: false,
            mode: None,
            socket_path: None,
            socket_exists: false,
        };
    };
    let dir = PathBuf::from(home).join(".conductor");
    let socket = dir.join("conductor.sock");
    let meta = std::fs::metadata(&dir).ok();
    SocketDirReport {
        exists: meta.is_some(),
        mode: meta.map(|m| format!("{:04o}", m.permissions().mode() & 0o7777)),
        socket_exists: socket.exists(),
        socket_path: Some(socket.display().to_string()),
        path: Some(dir.display().to_string()),
    }
}

/// Human-readable rendering.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let yes_no = |b: bool| if b { "yes" } else { "no" };

    out.push_str("store\n");
    out.push_str(&format!("  path                {}\n", report.store.path));
    out.push_str(&format!(
        "  exists              {}{}\n",
        yes_no(report.store.exists),
        if report.store.initialized_by_this_command {
            " (created by --init-store)"
        } else {
            ""
        }
    ));
    out.push_str(&format!(
        "  schema version      {} (supported {})\n",
        report
            .store
            .schema_version
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string()),
        report.store.supported_schema_version
    ));
    out.push_str(&format!(
        "  pending migrations  {}\n",
        if report.store.pending_migrations.is_empty() {
            "none".to_string()
        } else {
            format!("{:?}", report.store.pending_migrations)
        }
    ));
    out.push_str(&format!(
        "  integrity_check     {}\n",
        if report.store.integrity_check.is_empty() {
            "-".to_string()
        } else {
            report.store.integrity_check.join(", ")
        }
    ));
    out.push_str(&format!(
        "  foreign keys        {}\n",
        report
            .store
            .foreign_key_violations
            .map(|n| format!("{n} violation(s)"))
            .unwrap_or_else(|| "-".to_string())
    ));
    if report.store.pragmas.is_empty() {
        out.push_str("  pragmas             -\n");
    } else {
        let mut first = true;
        for (name, value) in &report.store.pragmas {
            out.push_str(&format!(
                "  {:<18}  {name}={value}\n",
                if first { "pragmas" } else { "" }
            ));
            first = false;
        }
    }
    for mismatch in &report.store.pragma_mismatches {
        out.push_str(&format!("  PRAGMA MISMATCH     {mismatch}\n"));
    }
    out.push_str(&format!(
        "  healthy             {}\n",
        yes_no(report.store.healthy)
    ));
    if let Some(err) = &report.store.error {
        out.push_str(&format!("  error               {err}\n"));
    }

    out.push_str("\ngit\n");
    out.push_str(&tool_lines(&report.git));

    out.push_str("\nadapters\n");
    for adapter in &report.adapters {
        out.push_str(&tool_lines(adapter));
    }

    out.push_str("\nsocket\n");
    out.push_str(&format!(
        "  directory           {} ({}, mode {})\n",
        report
            .socket_dir
            .path
            .clone()
            .unwrap_or_else(|| "<no HOME>".to_string()),
        if report.socket_dir.exists {
            "exists"
        } else {
            "absent"
        },
        report
            .socket_dir
            .mode
            .clone()
            .unwrap_or_else(|| "-".to_string())
    ));
    out.push_str(&format!(
        "  socket              {} ({})\n",
        report
            .socket_dir
            .socket_path
            .clone()
            .unwrap_or_else(|| "-".to_string()),
        if report.socket_dir.socket_exists {
            "present"
        } else {
            "absent"
        }
    ));

    out.push_str(&format!(
        "\ndoctor: {}\n",
        if report.ok { "OK" } else { "NOT OK" }
    ));
    out
}

fn tool_lines(tool: &ToolReport) -> String {
    if tool.present {
        format!(
            "  {:<18}  present  {}  {}\n",
            tool.name,
            tool.version.clone().unwrap_or_else(|| "-".to_string()),
            tool.path.clone().unwrap_or_else(|| "-".to_string())
        )
    } else {
        format!("  {:<18}  absent\n", tool.name)
    }
}
