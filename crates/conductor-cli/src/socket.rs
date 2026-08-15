//! The control socket — master plan §7.3, and §4.3's honesty about what it is.
//!
//! > Unix socket at `$HOME/.conductor/conductor.sock`, mode `0600`,
//! > line-delimited JSON-RPC. **No TCP port in v1** — a loopback port is
//! > reachable by any process on the machine, including the agent; a socket in
//! > `$HOME/.conductor/` is not writable by a sandboxed agent (M6) and not
//! > connectable (M10). Hand-rolled JSON-RPC framing; no async RPC framework.
//!
//! # What the mode does and does not buy
//!
//! §4.3 opens with the refusal this module is built around: *"A `0600` unix
//! socket does not distinguish a human from a same-user subprocess, and removing
//! an environment variable is obscurity."* So `0600` is **not** the boundary.
//! The boundary, when there is one, is the kernel refusing the agent's
//! `connect(2)` — measured as `control_surface: Hard`, reported by
//! [`conductor_run::approval::nonce::Tier`], and never asserted here. What the
//! mode does buy is the *other* users on the machine, and the property that the
//! socket cannot be squatted or replaced by anything that cannot already write
//! `$HOME/.conductor/`.
//!
//! # Publication is window-free, by `rename(2)` and not by `chmod(2)`
//!
//! The obvious sequence — bind at the published path, then `chmod` it to
//! `0600` — leaves a window in which the socket exists at `0777 & ~umask`,
//! typically `0755`. A window whose length is "however long the scheduler
//! feels like" is not a mode guarantee, and a test that checks the mode *after*
//! startup cannot see it.
//!
//! Two techniques close it. Setting the process umask around the `bind` needs
//! `libc::umask`, which would add a dependency to this crate for one call and
//! would race any other thread in the process — the umask is process-wide, and
//! a control socket is not worth making the whole binary single-threaded at
//! startup. **So this module uses the other one, and uses both halves of it:**
//!
//! 1. The containing directory is created `0700` **at creation**
//!    ([`std::os::unix::fs::DirBuilderExt::mode`], applied by `mkdir(2)` itself,
//!    so there is no moment at which the directory is more permissive). A
//!    pre-existing directory with a laxer mode is tightened *before* anything is
//!    bound inside it.
//! 2. The socket is bound at a **private temporary name** inside that directory,
//!    chmod'ed to `0600` while nothing can name it, and then `rename(2)`d onto
//!    `conductor.sock`. `rename` is atomic: at the published path the socket
//!    exists at `0600` from the first instant a client could refer to it, and
//!    never at any other mode.
//!
//! The `umask` is therefore irrelevant to correctness here, which is the point —
//! relying on an inherited umask would make the guarantee a property of whoever
//! started the process.
//!
//! # Stale sockets, and the socket vanishing underneath a running server
//!
//! A machine that lost power comes back with a socket file whose server is gone.
//! Treating that as fatal would mean a crash makes approvals permanently
//! unreachable, which is the opposite of §4.7's "restart converges with no human
//! input". Treating it as "replace whatever is there" would let a second process
//! silently steal the name from a **live** server, leaving the first accepting
//! on an inode no client can reach. So [`ControlSocket::publish`] distinguishes
//! the two the only way that is not a guess: it tries to **connect**. A refused
//! connect means nothing is listening and the name is free; a successful one
//! means it is not.
//!
//! The mirror image is [`ControlSocket::still_published`]. Identity, not
//! existence: the published inode is remembered at publication and re-stat'ed
//! afterwards, so a socket that was unlinked *and* a socket that was replaced by
//! somebody else's both read as "no longer published". Existence alone would
//! call the second case healthy.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// `$HOME/.conductor` — §7.3.
pub const SOCKET_DIR: &str = ".conductor";
/// `conductor.sock` — §7.3.
pub const SOCKET_FILE: &str = "conductor.sock";
/// The published mode of the socket — §7.3.
pub const SOCKET_MODE: u32 = 0o600;
/// The mode of the directory the socket lives in.
///
/// §4.3 tier A rests on the agent being unable to write `$HOME/.conductor/`,
/// "so it cannot squat or replace it". `0700` is the file-mode half of that;
/// the enforced half is the sandbox profile, measured, and owned by S9.
pub const SOCKET_DIR_MODE: u32 = 0o700;

/// How long a client waits for the server to answer one call.
///
/// Bounded rather than infinite: a wedged server must produce an error a script
/// can act on, not a hang a human has to notice.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the server waits for a connected client to say something.
///
/// The accept loop is single-threaded (§7.3: "no async RPC framework"), so a
/// client that connects and then says nothing would otherwise hold the socket
/// against every other operator. S14 owns concurrency; until then the answer is
/// a timeout, not a thread pool.
const SERVER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the accept loop looks up from `accept` to check its own socket is
/// still published.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Why a control-socket operation could not be completed.
#[derive(Debug)]
pub enum SocketError {
    /// `$HOME` is not set, so §7.3's path cannot be formed.
    NoHome,
    /// A filesystem operation failed, naming the path it failed on.
    Io {
        /// What was being attempted.
        doing: String,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// Another process is already serving this socket.
    AlreadyServing {
        /// The published path.
        path: PathBuf,
    },
    /// Nothing is listening. The caller is a client and there is no server.
    NotListening {
        /// The path that was tried.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The socket this server published is no longer the socket at that path.
    NoLongerPublished {
        /// The path that was published.
        path: PathBuf,
    },
    /// A line could not be parsed as JSON-RPC.
    Protocol {
        /// What was wrong.
        detail: String,
    },
    /// The server answered with an error object.
    Rpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The message.
        message: String,
    },
}

impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketError::NoHome => write!(
                f,
                "HOME is not set, so §7.3's $HOME/{SOCKET_DIR}/{SOCKET_FILE} cannot be formed"
            ),
            SocketError::Io {
                doing,
                path,
                source,
            } => write!(f, "{doing} {}: {source}", path.display()),
            SocketError::AlreadyServing { path } => write!(
                f,
                "{} is already being served by a live process; refusing to take \
                 the name, because the process that lost it would keep accepting \
                 on a socket no client can reach",
                path.display()
            ),
            SocketError::NotListening { path, source } => write!(
                f,
                "no control socket is listening at {} ({source}); granting is a \
                 mutating operation and §4.3 gives it exactly one route",
                path.display()
            ),
            SocketError::NoLongerPublished { path } => write!(
                f,
                "the control socket at {} is no longer the one this process \
                 published; it was unlinked or replaced, and every client that \
                 tries to reach it now reaches something else or nothing",
                path.display()
            ),
            SocketError::Protocol { detail } => write!(f, "control socket protocol: {detail}"),
            SocketError::Rpc { code, message } => write!(f, "{message} (rpc code {code})"),
        }
    }
}

impl std::error::Error for SocketError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SocketError::Io { source, .. } | SocketError::NotListening { source, .. } => {
                Some(source)
            }
            _ => None,
        }
    }
}

/// An `io::Error` mapper that remembers what was being attempted and where.
///
/// Every filesystem call in this module names its path in the error, because a
/// permissions failure on `$HOME/.conductor/` and one on the socket inside it
/// send an operator to two different places.
fn io<'a>(doing: &'a str, path: &'a Path) -> impl FnOnce(std::io::Error) -> SocketError + use<'a> {
    move |source| SocketError::Io {
        doing: doing.to_string(),
        path: path.to_path_buf(),
        source,
    }
}

// ---------------------------------------------------------------------------
// the wire format — §7.3's "hand-rolled JSON-RPC framing"
// ---------------------------------------------------------------------------

/// One request line.
///
/// `params` is untyped here on purpose: the framing layer must not need to know
/// the verbs, or adding a verb would mean editing the transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    /// Always `"2.0"`.
    #[serde(default = "two_point_oh")]
    pub jsonrpc: String,
    /// Correlates the answer with the question.
    pub id: u64,
    /// e.g. `approval.approve`.
    pub method: String,
    /// The verb's arguments.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// One response line. Exactly one of `result` and `error` is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    /// Always `"2.0"`.
    #[serde(default = "two_point_oh")]
    pub jsonrpc: String,
    /// The `id` of the request being answered.
    pub id: u64,
    /// The verb's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Why there is no answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    /// JSON-RPC's code. [`rpc_code`] holds the ones this server uses.
    pub code: i64,
    /// What went wrong, in a sentence a human can act on.
    pub message: String,
}

/// The JSON-RPC codes this server produces.
pub mod rpc_code {
    /// The line was not valid JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// No such verb.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The verb exists; its parameters do not make sense.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The verb ran and refused. Application-defined range.
    pub const REFUSED: i64 = -32000;
}

fn two_point_oh() -> String {
    "2.0".to_string()
}

impl RpcResponse {
    /// An answer.
    pub fn ok(id: u64, result: serde_json::Value) -> RpcResponse {
        RpcResponse {
            jsonrpc: two_point_oh(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// A refusal.
    pub fn failed(id: u64, code: i64, message: impl Into<String>) -> RpcResponse {
        RpcResponse {
            jsonrpc: two_point_oh(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/// §7.3's `$HOME/.conductor/conductor.sock`.
pub fn default_socket_path() -> Result<PathBuf, SocketError> {
    let home = std::env::var_os("HOME").ok_or(SocketError::NoHome)?;
    Ok(PathBuf::from(home).join(SOCKET_DIR).join(SOCKET_FILE))
}

/// Create the socket's directory at `0700`, or tighten one that already exists.
///
/// The mode is passed to `mkdir(2)`, so a directory this call creates is never
/// briefly more permissive. A directory that was already there is a different
/// question — it may predate this code, or have been created by a `doctor` run,
/// or by a human — and the answer is to tighten it before anything is bound
/// inside, rather than to trust it.
fn prepare_directory(dir: &Path) -> Result<(), SocketError> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(SOCKET_DIR_MODE)
            .create(dir)
            .map_err(io("create", dir))?;
    }
    let current = std::fs::metadata(dir).map_err(io("stat", dir))?.mode() & 0o7777;
    if current != SOCKET_DIR_MODE {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(SOCKET_DIR_MODE))
            .map_err(io("chmod", dir))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the server
// ---------------------------------------------------------------------------

/// Why the accept loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeEnd {
    /// The socket this process published is no longer at the published path.
    NoLongerPublished,
    /// A handler asked to stop.
    HandlerStopped,
}

/// What a handler wants to happen after answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterCall {
    /// Keep serving.
    Continue,
    /// Stop the accept loop.
    Stop,
}

/// A published control socket.
///
/// Unlinks its own path on drop, but only if the path is still *its* inode —
/// deleting a socket some other process published would be the theft this type
/// refuses to commit at startup.
#[derive(Debug)]
pub struct ControlSocket {
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl ControlSocket {
    /// Bind and publish a control socket at `path`.
    ///
    /// See the module docs for why this is `bind` → `chmod` → `rename` and not
    /// `bind` → `chmod`, and for how a stale socket is told from a live one.
    pub fn publish(path: &Path) -> Result<ControlSocket, SocketError> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        prepare_directory(dir)?;

        // A live server owns the name. A dead one does not, and the only way to
        // tell without guessing is to try to talk to it.
        if path.exists() && UnixStream::connect(path).is_ok() {
            return Err(SocketError::AlreadyServing {
                path: path.to_path_buf(),
            });
        }

        // Private, per-process, and inside the 0700 directory: nothing that
        // cannot already enter the directory can name it, whatever its mode is
        // between `bind` and `chmod`.
        let staging = dir.join(format!(
            ".{}.{}.staging",
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| SOCKET_FILE.to_string()),
            std::process::id()
        ));
        let _ = std::fs::remove_file(&staging);
        let listener = UnixListener::bind(&staging).map_err(io("bind", &staging))?;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(SOCKET_MODE)).map_err(
            |e| {
                let _ = std::fs::remove_file(&staging);
                io("chmod", &staging)(e)
            },
        )?;
        // Atomic. The published name never refers to a socket at any other mode.
        std::fs::rename(&staging, path).map_err(|e| {
            let _ = std::fs::remove_file(&staging);
            io("publish", path)(e)
        })?;

        let metadata = std::fs::metadata(path).map_err(io("stat", path))?;
        Ok(ControlSocket {
            listener,
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    /// Where it is published.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The mode the published socket actually has.
    pub fn mode(&self) -> Result<u32, SocketError> {
        Ok(std::fs::metadata(&self.path)
            .map_err(io("stat", &self.path))?
            .mode()
            & 0o7777)
    }

    /// Whether the path still names **this** socket.
    ///
    /// Identity, not existence — see the module docs.
    pub fn still_published(&self) -> bool {
        match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata.dev() == self.device && metadata.ino() == self.inode,
            Err(_) => false,
        }
    }

    /// Accept connections and answer them until the socket stops being ours or
    /// a handler says to stop.
    ///
    /// Non-blocking `accept` plus a short sleep, rather than a blocking one: a
    /// blocking `accept` on an unlinked socket waits forever for a client that
    /// can no longer name it, and the slice's failure-injection list names
    /// "socket file deleted" precisely because that is the shape of the bug.
    pub fn serve<H>(&self, mut handler: H) -> Result<ServeEnd, SocketError>
    where
        H: FnMut(&RpcRequest) -> (RpcResponse, AfterCall),
    {
        self.listener
            .set_nonblocking(true)
            .map_err(io("set_nonblocking", &self.path))?;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.answer(stream, &mut handler) == AfterCall::Stop {
                        return Ok(ServeEnd::HandlerStopped);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if !self.still_published() {
                        return Ok(ServeEnd::NoLongerPublished);
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(err) => return Err(io("accept", &self.path)(err)),
            }
        }
    }

    /// Answer every line one client sends, then close.
    ///
    /// **No error here ends the accept loop.** One client that hung up between
    /// `connect` and `accept`, or that closed mid-line, or whose socket refused
    /// a timeout option, is a fact about that client. Propagating it would let
    /// any process on the machine stop the control socket by connecting and
    /// immediately disconnecting — a denial of service against approvals, which
    /// is the one surface that must stay reachable when things are going wrong.
    /// So the connection is abandoned and the loop goes back to `accept`.
    fn answer<H>(&self, stream: UnixStream, handler: &mut H) -> AfterCall
    where
        H: FnMut(&RpcRequest) -> (RpcResponse, AfterCall),
    {
        // POSIX does not have the accepted socket inherit the listener's
        // non-blocking flag, but it does not forbid it either; setting it
        // explicitly means the read timeout below is the only thing that ends a
        // silent client, on every platform. Both are best-effort: a peer that
        // has already gone away makes these fail on some kernels, and that peer
        // is not going to send anything anyway.
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(SERVER_READ_TIMEOUT));
        let Ok(mut writer) = stream.try_clone() else {
            return AfterCall::Continue;
        };
        let reader = BufReader::new(stream);
        let mut after = AfterCall::Continue;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<RpcRequest>(&line) {
                Ok(request) => {
                    let (response, next) = handler(&request);
                    after = next;
                    response
                }
                Err(err) => RpcResponse::failed(0, rpc_code::PARSE_ERROR, err.to_string()),
            };
            let Ok(encoded) = serde_json::to_string(&response) else {
                break;
            };
            if writeln!(writer, "{encoded}").is_err() || writer.flush().is_err() {
                break;
            }
            if after == AfterCall::Stop {
                break;
            }
        }
        after
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        if self.still_published() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// the client
// ---------------------------------------------------------------------------

/// Ask the control socket one question and read one answer.
///
/// There is no fallback to the store. A client that could fall back would be a
/// client that grants approvals without a socket, and then the socket would be
/// a formality rather than a route (§4.3).
pub fn call(
    path: &Path,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, SocketError> {
    let stream = UnixStream::connect(path).map_err(|source| SocketError::NotListening {
        path: path.to_path_buf(),
        source,
    })?;
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .map_err(io("set_read_timeout", path))?;
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .map_err(io("set_write_timeout", path))?;

    let request = RpcRequest {
        jsonrpc: two_point_oh(),
        id: 1,
        method: method.to_string(),
        params,
    };
    let encoded = serde_json::to_string(&request).map_err(|err| SocketError::Protocol {
        detail: format!("could not encode a request: {err}"),
    })?;

    let mut writer = stream.try_clone().map_err(io("dup", path))?;
    writeln!(writer, "{encoded}").map_err(io("write", path))?;
    writer.flush().map_err(io("flush", path))?;

    let mut line = String::new();
    let read = BufReader::new(stream)
        .read_line(&mut line)
        .map_err(io("read", path))?;
    if read == 0 {
        return Err(SocketError::Protocol {
            detail: "the server closed the connection without answering".to_string(),
        });
    }
    let response: RpcResponse =
        serde_json::from_str(&line).map_err(|err| SocketError::Protocol {
            detail: format!("the answer was not a JSON-RPC response ({err}): {line}"),
        })?;
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (_, Some(error)) => Err(SocketError::Rpc {
            code: error.code,
            message: error.message,
        }),
        (None, None) => Err(SocketError::Protocol {
            detail: "the answer carried neither a result nor an error".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(SOCKET_DIR).join(SOCKET_FILE)
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
            .mode()
            & 0o7777
    }

    #[test]
    fn the_published_socket_is_0600_in_a_0700_directory() {
        // **The literals are the assertion, not `SOCKET_MODE`.** Found by
        // mutation: an earlier version of this test compared the socket's mode
        // against the constant, so setting `SOCKET_MODE = 0o666` left it green
        // — it was asserting that the code agreed with itself. §7.3 names
        // `0600`, so `0600` is what appears here, and the constant is checked
        // against the same literal separately.
        let dir = temp();
        let path = socket_in(&dir);
        let socket = ControlSocket::publish(&path).expect("publish");
        assert_eq!(socket.mode().expect("mode"), 0o600, "§7.3 says mode 0600");
        assert_eq!(
            mode_of(path.parent().expect("parent")),
            0o700,
            "the socket directory must be owner-only"
        );
        assert_eq!(SOCKET_MODE, 0o600, "§7.3's constant must say 0600 too");
        assert_eq!(SOCKET_DIR_MODE, 0o700);
    }

    #[test]
    fn a_directory_that_was_already_lax_is_tightened_before_anything_is_bound() {
        // Not hypothetical: the directory may predate this code, or have been
        // created by hand. Binding into a 0755 directory would put the socket
        // somewhere every user on the machine can reach for as long as the
        // directory stays that way.
        let dir = temp();
        let path = socket_in(&dir);
        let parent = path.parent().expect("parent").to_path_buf();
        std::fs::create_dir_all(&parent).expect("mkdir");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(mode_of(&parent), 0o755, "the fixture must start lax");

        let _socket = ControlSocket::publish(&path).expect("publish");
        assert_eq!(mode_of(&parent), 0o700);
    }

    #[test]
    fn publishing_leaves_no_staging_socket_behind() {
        // The staging name is an implementation detail; a leftover would be a
        // second socket in the directory, bound to the same listener, that
        // nothing ever unlinks.
        let dir = temp();
        let path = socket_in(&dir);
        let _socket = ControlSocket::publish(&path).expect("publish");
        let leftovers: Vec<String> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != SOCKET_FILE)
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_not() {
        let dir = temp();
        let path = socket_in(&dir);

        // Stale: a file at the path that nothing is listening on.
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"left over from a process that died").expect("write");
        let first = ControlSocket::publish(&path).expect("a stale socket must not be fatal");
        assert_eq!(
            first.mode().expect("mode"),
            0o600,
            "replacing a stale socket must not publish a laxer one"
        );

        // Live: the same path, now genuinely served.
        let err = ControlSocket::publish(&path).expect_err("a live socket must not be stolen");
        assert!(
            matches!(err, SocketError::AlreadyServing { .. }),
            "unexpected error: {err}"
        );
        // POSITIVE CONTROL: once the live server is gone the name is free
        // again, so the refusal above is about the server and not about the
        // path being permanently poisoned.
        drop(first);
        ControlSocket::publish(&path).expect("a released socket must be re-publishable");
    }

    #[test]
    fn an_unlinked_socket_is_detected_and_so_is_a_replaced_one() {
        // Existence is not identity. A server that only asked "does the path
        // exist?" would call the second case below healthy while every client
        // reached somebody else.
        let dir = temp();
        let path = socket_in(&dir);
        let socket = ControlSocket::publish(&path).expect("publish");
        assert!(socket.still_published(), "it must start published");

        std::fs::remove_file(&path).expect("unlink");
        assert!(!socket.still_published(), "an unlinked socket is not ours");

        let usurper = ControlSocket::publish(&path).expect("republish");
        assert!(
            !socket.still_published(),
            "a replaced socket is not ours either"
        );
        assert!(usurper.still_published());
    }

    #[test]
    fn a_client_reaches_the_server_and_a_missing_server_is_named_as_such() {
        let dir = temp();
        let path = socket_in(&dir);
        let socket = ControlSocket::publish(&path).expect("publish");
        let served = path.clone();

        let server = std::thread::spawn(move || {
            socket.serve(|request| {
                let response = match request.method.as_str() {
                    "echo" => RpcResponse::ok(request.id, request.params.clone()),
                    other => RpcResponse::failed(
                        request.id,
                        rpc_code::METHOD_NOT_FOUND,
                        format!("no such method {other}"),
                    ),
                };
                (response, AfterCall::Stop)
            })
        });

        let answer = call(&served, "echo", serde_json::json!({"hello": "world"})).expect("call");
        assert_eq!(answer["hello"], "world");
        let end = server.join().expect("join").expect("serve");
        assert_eq!(end, ServeEnd::HandlerStopped);

        // The socket is unlinked on drop, so the client now has nothing to
        // reach — and says so rather than hanging.
        let err = call(&served, "echo", serde_json::Value::Null)
            .expect_err("there is no server any more");
        assert!(
            matches!(err, SocketError::NotListening { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_server_whose_socket_is_unlinked_stops_instead_of_serving_into_the_void() {
        let dir = temp();
        let path = socket_in(&dir);
        let socket = ControlSocket::publish(&path).expect("publish");
        let watched = path.clone();

        let server = std::thread::spawn(move || {
            socket.serve(|request| {
                (
                    RpcResponse::ok(request.id, serde_json::Value::Null),
                    AfterCall::Continue,
                )
            })
        });
        // Give the loop a moment to reach its first `accept`, then take the
        // name away underneath it.
        std::thread::sleep(POLL_INTERVAL * 4);
        std::fs::remove_file(&watched).expect("unlink");

        let end = server.join().expect("join").expect("serve");
        assert_eq!(end, ServeEnd::NoLongerPublished);
    }

    #[test]
    fn a_client_that_hangs_up_without_saying_anything_does_not_stop_the_server() {
        // Found the hard way: the first version propagated every per-connection
        // error out of the accept loop, so a `connect` followed immediately by a
        // `close` killed the server. That is a denial of service against the one
        // surface §4.3 needs reachable when things are going wrong, available to
        // any process that can reach the socket at all — which under tier C is
        // every process running as the operator.
        let dir = temp();
        let path = socket_in(&dir);
        let socket = ControlSocket::publish(&path).expect("publish");
        let served = path.clone();

        let server = std::thread::spawn(move || {
            socket.serve(|request| {
                let stop = if request.method == "stop" {
                    AfterCall::Stop
                } else {
                    AfterCall::Continue
                };
                (RpcResponse::ok(request.id, json_true()), stop)
            })
        });

        for _ in 0..5 {
            drop(UnixStream::connect(&served).expect("connect"));
        }
        // POSITIVE CONTROL: the server is still answering after all of that.
        let answer = call(&served, "ping", serde_json::Value::Null).expect("still serving");
        assert_eq!(answer, json_true());

        let _ = call(&served, "stop", serde_json::Value::Null);
        let end = server.join().expect("join").expect("serve");
        assert_eq!(end, ServeEnd::HandlerStopped);
    }

    fn json_true() -> serde_json::Value {
        serde_json::Value::Bool(true)
    }

    #[test]
    fn a_line_that_is_not_json_gets_a_parse_error_and_not_a_dropped_connection() {
        // §7.3's framing is hand-rolled, so the failure mode of a bad line is
        // this module's problem. Closing the connection silently would make a
        // client-side bug look like a dead server.
        let dir = temp();
        let path = socket_in(&dir);
        let socket = ControlSocket::publish(&path).expect("publish");
        let served = path.clone();

        let server = std::thread::spawn(move || {
            socket.serve(|request| {
                (
                    RpcResponse::ok(request.id, serde_json::Value::Null),
                    AfterCall::Stop,
                )
            })
        });

        let mut stream = UnixStream::connect(&served).expect("connect");
        stream
            .set_read_timeout(Some(CLIENT_TIMEOUT))
            .expect("timeout");
        writeln!(stream, "this is not json").expect("write");
        stream.flush().expect("flush");
        let mut line = String::new();
        BufReader::new(stream.try_clone().expect("dup"))
            .read_line(&mut line)
            .expect("read");
        let response: RpcResponse = serde_json::from_str(&line).expect("a response");
        assert_eq!(
            response.error.expect("an error object").code,
            rpc_code::PARSE_ERROR
        );

        // Let the server finish: a well-formed line ends its loop.
        let _ = call(&served, "anything", serde_json::Value::Null);
        let _ = server.join().expect("join");
    }
}
