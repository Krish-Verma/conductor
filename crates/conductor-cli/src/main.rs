//! The `conductor` binary.
//!
//! S1 shipped one command: `doctor`. S5 adds `task run|show|list` — §7.1's core
//! verb and the two commands that read what it did. S7 adds `policy explain`,
//! §7.1's "why was this denied — the 2 a.m. command". The rest of §7.1's
//! thirteen arrive with the slices that implement them.

mod doctor;
mod policy;
mod task;

use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};

/// Exit codes, master plan §7.2.
///
/// ```text
/// 0   success
/// 1   generic failure
/// 2   no project / not initialized / store unhealthy
/// 3   action required — approval or review pending    ← scriptable "human needed"
/// 4   policy denied
/// 5   verification failed
/// 64  usage error        (EX_USAGE)
/// 70  internal error     (EX_SOFTWARE)
/// ```
mod exit {
    /// The command did what was asked.
    pub const SUCCESS: u8 = 0;
    /// Generic failure.
    pub const FAILURE: u8 = 1;
    /// No project, not initialized, or the store is unhealthy.
    pub const NOT_INITIALIZED: u8 = 2;
    /// A human is needed. §7.2 gives this its own slot precisely so a wrapper
    /// script can tell it from a failure.
    pub const ACTION_REQUIRED: u8 = 3;
    /// Verification failed.
    pub const VERIFICATION_FAILED: u8 = 5;
    /// Usage error (`EX_USAGE`).
    pub const USAGE: u8 = 64;
    /// Internal error (`EX_SOFTWARE`).
    pub const INTERNAL: u8 = 70;

    // `4 policy denied` is still deliberately absent after S7. S7 *decides*;
    // nothing in it enforces, and `policy explain` succeeds when it explains a
    // deny — returning 4 there would break `set -e` scripts using the command
    // for exactly its stated purpose. S9 owns enforcement and is the slice that
    // can return this code without it being a constant nothing produces.
}

#[derive(Debug, Parser)]
#[command(
    name = "conductor",
    version,
    about = "Local-first execution control plane for coding agents",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Report on the store, git, adapters and the control-socket directory.
    Doctor(doctor::DoctorArgs),
    /// Run, inspect and list tasks.
    Task {
        #[command(subcommand)]
        command: task::TaskCommand,
        #[command(flatten)]
        shared: task::StoreArgs,
    },
    /// Explain a policy decision.
    Policy {
        #[command(subcommand)]
        command: policy::PolicyCommand,
        #[command(flatten)]
        shared: task::StoreArgs,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            // `--help` and `--version` are successful requests, not misuse.
            return match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ExitCode::SUCCESS,
                _ => ExitCode::from(exit::USAGE),
            };
        }
    };

    match cli.command {
        Commands::Doctor(args) => run_doctor(&args),
        Commands::Task { command, shared } => task::run(&command, &shared),
        Commands::Policy { command, shared } => policy::run(&command, &shared),
    }
}

fn run_doctor(args: &doctor::DoctorArgs) -> ExitCode {
    let report = doctor::build(args);

    if args.json {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(err) => {
                eprintln!("internal error: {err}");
                return ExitCode::from(exit::INTERNAL);
            }
        }
    } else {
        print!("{}", doctor::render(&report));
    }

    ExitCode::from(report.exit_code)
}
