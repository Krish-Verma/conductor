//! The probe payload itself.
//!
//! The payload is the instrument's tip. Everything the harness concludes rests
//! on it distinguishing three things a shell conflates: *the operation
//! succeeded*, *the operation was refused*, and *I could not even try*.

use std::path::Path;
use std::process::{Command, Output};

const PAYLOAD: &str = env!("CARGO_BIN_EXE_conductor-probe-action");

fn run(args: &[&str]) -> Output {
    Command::new(PAYLOAD)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn {PAYLOAD}: {e}"))
}

fn result_line(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| line.starts_with("RESULT "))
        .unwrap_or_else(|| {
            panic!(
                "no RESULT line. stdout: {:?} stderr: {:?}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
        .to_string()
}

#[test]
fn a_permitted_write_reports_ok_and_exits_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("written.txt");

    let out = run(&["write", target.to_str().expect("utf8")]);

    assert_eq!(result_line(&out), "RESULT ok -");
    assert_eq!(out.status.code(), Some(0));
    assert!(target.exists(), "the write must actually have happened");
}

#[test]
fn a_refused_write_reports_blocked_with_the_error_kind() {
    // A real EPERM, produced without a sandbox: a directory with no write
    // permission. If the payload reported this as `error` the harness would
    // treat a genuine denial as a broken case, and vice versa.
    let dir = tempfile::tempdir().expect("tempdir");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).expect("mkdir");
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o500))
        .expect("chmod");

    let out = run(&["write", locked.join("nope.txt").to_str().expect("utf8")]);

    assert_eq!(result_line(&out), "RESULT blocked PermissionDenied");
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn a_write_through_two_nested_shells_still_reports_the_inner_result() {
    // M8's shape. The harness must see the inner attempt's verdict, not the
    // shell's opinion of it.
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("nested.txt");

    let out = run(&["write-nested", target.to_str().expect("utf8")]);

    assert_eq!(result_line(&out), "RESULT ok -");
    assert_eq!(out.status.code(), Some(0));
    assert!(target.exists());
}

#[test]
fn reading_a_planted_secret_is_verified_against_its_content_without_printing_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let secret = dir.path().join("planted.txt");
    std::fs::write(&secret, "synthetic-token-abc").expect("plant");

    let out = run(&[
        "read-expect",
        secret.to_str().expect("utf8"),
        "synthetic-token-abc",
    ]);

    assert_eq!(result_line(&out), "RESULT ok -");
    let printed =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        !printed.contains("synthetic-token-abc"),
        "the payload must never echo what it read: {printed:?}"
    );
}

#[test]
fn a_read_that_returns_the_wrong_bytes_is_an_error_not_a_success() {
    let dir = tempfile::tempdir().expect("tempdir");
    let secret = dir.path().join("planted.txt");
    std::fs::write(&secret, "something-else").expect("plant");

    let out = run(&["read-expect", secret.to_str().expect("utf8"), "expected"]);

    assert!(
        result_line(&out).starts_with("RESULT error"),
        "got {:?}",
        result_line(&out)
    );
}

#[test]
fn connecting_to_a_socket_that_is_not_there_is_an_error_not_a_denial() {
    // The round-1 failure mode: an AF_UNIX connect that fails for a reason
    // unrelated to containment must never be reported as `blocked`.
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("c.sock");

    let out = run(&["unix-connect", absent.to_str().expect("utf8")]);

    assert!(
        result_line(&out).starts_with("RESULT error"),
        "got {:?}",
        result_line(&out)
    );
}

#[test]
fn connecting_to_a_live_socket_reports_ok() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("c.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
    std::thread::spawn(move || {
        use std::io::Write;
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.write_all(b"HELLO\n");
        }
    });

    let out = run(&["unix-connect", path.to_str().expect("utf8")]);

    assert_eq!(result_line(&out), "RESULT ok -");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn exit_code_is_propagated_verbatim_so_the_launcher_can_be_checked() {
    // M15. The liveness case depends on this.
    let out = run(&["exit-code", "42"]);

    assert_eq!(result_line(&out), "RESULT ok -");
    assert_eq!(out.status.code(), Some(42));
}

#[test]
fn an_unknown_action_is_a_usage_error_and_prints_no_result_line() {
    // A payload that printed `RESULT ok` for an action it did not perform would
    // be the worst possible bug in this harness.
    let out = run(&["do-something-undefined"]);

    assert_eq!(out.status.code(), Some(64));
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("RESULT "),
        "stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_probe_binary_that_never_runs_produces_no_result_line() {
    // Establishes the harness's core discriminator: an unstarted payload is
    // silent, so "no RESULT line" can only ever mean "broken", never "denied".
    let out = Command::new(Path::new("/nonexistent/conductor-probe-action"))
        .arg("exit-code")
        .arg("0")
        .output();

    assert!(out.is_err(), "expected the spawn itself to fail");
}
