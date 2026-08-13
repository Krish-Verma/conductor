//! The containment probe's payload — master plan §4.2, slice S2.5.
//!
//! One process, one operation, one line of output. It exists because a shell
//! cannot be trusted as an instrument: `sh -c 'echo x > /somewhere'` exits
//! non-zero both when the kernel refused the write and when the shell was never
//! started, and S0's first containment round was invalidated twice by exactly
//! that kind of ambiguity (ADR-0002).
//!
//! # Contract
//!
//! Prints exactly one line to stdout:
//!
//! ```text
//! RESULT ok -                     the operation succeeded
//! RESULT blocked <ErrorKind>      the operation was refused
//! RESULT error <detail>           the operation could not be attempted
//! ```
//!
//! and exits `0`, `3` or `4` respectively — except `exit-code <n>`, which exits
//! `n` so the launcher's exit-code propagation can be checked (M15). A usage
//! error exits `64` and prints **no** `RESULT` line.
//!
//! The absence of a `RESULT` line therefore means the payload never ran, which
//! the harness reads as a broken case and never as a denial.
//!
//! It never prints file contents: `read-expect` compares against a value it was
//! given and reports only whether they matched.

use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

/// The operation succeeded.
const EXIT_OK: u8 = 0;
/// The operation was refused.
const EXIT_BLOCKED: u8 = 3;
/// The operation could not be attempted, so it says nothing about containment.
const EXIT_ERROR: u8 = 4;
/// Usage error (`EX_USAGE`, master plan §7.2).
const EXIT_USAGE: u8 = 64;

/// What one attempt produced.
enum Outcome {
    Ok,
    Blocked(String),
    Error(String),
}

impl Outcome {
    fn emit(&self) -> ExitCode {
        match self {
            Outcome::Ok => {
                println!("RESULT ok -");
                ExitCode::from(EXIT_OK)
            }
            Outcome::Blocked(detail) => {
                println!("RESULT blocked {detail}");
                ExitCode::from(EXIT_BLOCKED)
            }
            Outcome::Error(detail) => {
                println!("RESULT error {detail}");
                ExitCode::from(EXIT_ERROR)
            }
        }
    }
}

/// Classify an I/O failure.
///
/// `PermissionDenied` is the kernel refusing — the thing being measured.
/// Anything else (no such file, connection refused, address in use) is a defect
/// in the probe's own setup and must not be mistaken for containment.
fn classify(err: &std::io::Error) -> Outcome {
    match err.kind() {
        std::io::ErrorKind::PermissionDenied => Outcome::Blocked("PermissionDenied".to_string()),
        other => Outcome::Error(format!("{other:?}")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    match argv.as_slice() {
        ["exit-code", code] => match code.parse::<u8>() {
            Ok(code) => {
                println!("RESULT ok -");
                ExitCode::from(code)
            }
            Err(_) => usage(&format!("exit-code needs a number 0-255, got {code:?}")),
        },
        ["write", path] => write(Path::new(path)).emit(),
        ["write-nested", path] => write_nested(Path::new(path)),
        ["read-expect", path, expected] => read_expect(Path::new(path), expected).emit(),
        ["tcp-connect", addr, timeout_ms] => match timeout_ms.parse::<u64>() {
            Ok(ms) => tcp_connect(addr, ms).emit(),
            Err(_) => usage("tcp-connect needs a timeout in milliseconds"),
        },
        ["dns-resolve", host] => dns_resolve(host).emit(),
        ["unix-connect", path] => unix_connect(Path::new(path)).emit(),
        ["sleep", ms] => match ms.parse::<u64>() {
            Ok(ms) => {
                std::thread::sleep(Duration::from_millis(ms));
                Outcome::Ok.emit()
            }
            Err(_) => usage("sleep needs a duration in milliseconds"),
        },
        _ => usage(&format!("unknown action: {}", args.join(" "))),
    }
}

fn usage(message: &str) -> ExitCode {
    eprintln!("conductor-probe-action: {message}");
    eprintln!(
        "usage: conductor-probe-action \
         (exit-code N | write PATH | write-nested PATH | read-expect PATH TOKEN | \
         tcp-connect ADDR TIMEOUT_MS | dns-resolve HOST | unix-connect PATH | sleep MS)"
    );
    ExitCode::from(EXIT_USAGE)
}

fn write(path: &Path) -> Outcome {
    match std::fs::write(path, b"conductor containment probe\n") {
        Ok(()) => Outcome::Ok,
        Err(err) => classify(&err),
    }
}

/// M8: does a child process inherit the restriction?
///
/// Two `sh` levels, exactly like the S0 measurement, with this same binary at
/// the bottom so the inner attempt's `RESULT` line and exit code reach the
/// harness unchanged.
fn write_nested(path: &Path) -> ExitCode {
    let Ok(exe) = std::env::current_exe() else {
        return Outcome::Error("cannot locate self".to_string()).emit();
    };
    let inner = format!(
        "{} write {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&path.to_string_lossy())
    );
    let outer = format!("/bin/sh -c {}", shell_quote(&inner));

    match Command::new("/bin/sh").arg("-c").arg(outer).status() {
        // stdout and stderr are inherited, so the inner RESULT line is already
        // on the harness's pipe. Only the exit code needs forwarding.
        Ok(status) => match status.code() {
            Some(code) => ExitCode::from(code as u8),
            None => Outcome::Error("nested shell died on a signal".to_string()).emit(),
        },
        Err(err) => classify(&err).emit(),
    }
}

/// Single-quote for `sh`, so a path containing spaces or quotes cannot change
/// the command being run.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// M12: can the sandboxed process read a credential it was not given?
///
/// The token is supplied on the command line and compared in memory. The
/// contents are never printed — the probe proves the read happened, it does not
/// disclose what was read.
fn read_expect(path: &Path, expected: &str) -> Outcome {
    match std::fs::read(path) {
        Ok(bytes) if bytes == expected.as_bytes() => Outcome::Ok,
        Ok(bytes) => Outcome::Error(format!(
            "read {} bytes but they are not the planted value; the probe is measuring \
             something other than what it planted",
            bytes.len()
        )),
        Err(err) => classify(&err),
    }
}

fn tcp_connect(addr: &str, timeout_ms: u64) -> Outcome {
    let mut resolved = match addr.to_socket_addrs() {
        Ok(addrs) => addrs,
        Err(err) => return Outcome::Error(format!("cannot parse {addr}: {err}")),
    };
    let Some(target) = resolved.next() else {
        return Outcome::Error(format!("{addr} resolved to nothing"));
    };
    match TcpStream::connect_timeout(&target, Duration::from_millis(timeout_ms)) {
        Ok(_) => Outcome::Ok,
        Err(err) => classify(&err),
    }
}

/// DNS denial does not surface as `PermissionDenied`, so any failure is
/// reported as blocked and the harness's positive control decides whether that
/// means containment or an offline machine.
fn dns_resolve(host: &str) -> Outcome {
    match (host, 0u16).to_socket_addrs() {
        Ok(mut addrs) => {
            if addrs.next().is_some() {
                Outcome::Ok
            } else {
                Outcome::Blocked("NoAddresses".to_string())
            }
        }
        Err(err) => Outcome::Blocked(format!("{:?}", err.kind())),
    }
}

/// M10: can the process reach Conductor's control surface?
fn unix_connect(path: &Path) -> Outcome {
    match UnixStream::connect(path) {
        Ok(mut stream) => {
            let mut buffer = [0u8; 64];
            match stream.read(&mut buffer) {
                Ok(0) => Outcome::Error("connected but the listener sent nothing".to_string()),
                Ok(_) => Outcome::Ok,
                Err(err) => classify(&err),
            }
        }
        Err(err) => classify(&err),
    }
}
