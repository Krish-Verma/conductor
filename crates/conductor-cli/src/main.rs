//! The `conductor` binary.
//!
//! S1 ships one command: `doctor`. The remaining twelve commands of master plan
//! §7.1 arrive with the slices that implement them.

mod doctor;

use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};

/// Exit codes, master plan §7.2.
mod exit {
    /// Usage error (`EX_USAGE`).
    pub const USAGE: u8 = 64;
    /// Internal error (`EX_SOFTWARE`).
    pub const INTERNAL: u8 = 70;
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
