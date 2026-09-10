//! Blocking HTTP/1.1 client for the daemon's Unix-socket API.
//!
//! Deliberately hand-written rather than pulling in an HTTP stack. The tray
//! issues at most four request shapes, always over a local Unix socket, always
//! with `Connection: close`; a measured round trip is 0.13 ms. `hyper` +
//! `hyper-util` would add a large dependency tree to the *shipped* binary for
//! no behaviour the tray needs. (The daemon carries them as dev-dependencies
//! only, for its own integration tests.)
//!
//! Everything here is blocking and synchronous. It is called from ksni's
//! service thread — from `menu_about_to_show` and from menu-item callbacks — so
//! every call is bounded by [`DEFAULT_TIMEOUT`]: a wedged daemon must not freeze
//! the Plasma panel indefinitely.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Where the daemon listens. Mirrors `daemon/src/config.rs::default_socket_path`.
pub const DEFAULT_SOCKET_PATH: &str = "/run/control-ofc/control-ofc.sock";

/// Total deadline for one request — connect, write and read together.
///
/// This is a real wall-clock bound, not a per-syscall one. It has to be: the
/// menu is built synchronously inside `menu_about_to_show` and menu-item
/// callbacks are synchronous too, so an unbounded call holds ksni's service
/// thread, which is also the thread that dispatches "Quit tray".
///
/// It bounds a **single call**, not a whole interaction. Worst case against a
/// wedged daemon: a menu open costs two calls (~600 ms) and a profile switch
/// three (~900 ms — the POST plus the re-read). The panel is unresponsive for
/// that long and then recovers; it cannot hang. 300 ms is ~2300x the measured
/// 0.13 ms round trip, so a merely slow daemon never reaches it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(300);

/// Hard ceiling on a response body.
///
/// The daemon is trusted (it runs as root and we read its own socket), so this
/// is not a security boundary — it is a bound on how wrong things can go if the
/// socket is ever something other than what we think. `/status` measures ~1 KB
/// and `/profiles` scales with the profile count; 1 MiB is far above any real
/// value while still being a bound rather than an unbounded `read_to_end`.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

fn default_thermal_state() -> String {
    // Absent field must NOT read as "abnormal", or a daemon that predates
    // `thermal_state` would permanently show the thermal warning line.
    "normal".to_string()
}

/// The subset of `GET /status` the tray renders.
///
/// Unknown fields are ignored by serde's default behaviour, so daemon-side
/// additions never break the tray.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub daemon_version: String,
    #[serde(default = "default_thermal_state")]
    pub thermal_state: String,
    #[serde(default)]
    pub active_profile_id: Option<String>,
    #[serde(default)]
    pub active_profile_name: Option<String>,
}

impl Status {
    /// True when the daemon reports anything other than routine operation.
    ///
    /// Deliberately "not normal" rather than a match on the known abnormal
    /// tokens: a daemon that gains a new thermal state must surface it, not
    /// silently fall through as if nothing were happening.
    pub fn thermal_is_abnormal(&self) -> bool {
        self.thermal_state != "normal"
    }
}

/// One entry of `GET /profiles`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProfileSummary {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize)]
struct ProfileListResponse {
    #[serde(default)]
    profiles: Vec<ProfileSummary>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// Why a daemon call did not produce an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// Could not reach the daemon at all: not listening, timed out, socket gone.
    /// The tray renders this as "daemon unavailable" rather than as an error.
    Unavailable(String),
    /// Reached something, but it did not speak the expected protocol.
    Protocol(String),
    /// The daemon answered with a non-2xx status and (usually) an error envelope.
    Daemon {
        status: u16,
        code: String,
        message: String,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unavailable(why) => write!(f, "daemon unavailable: {why}"),
            ClientError::Protocol(why) => write!(f, "malformed daemon response: {why}"),
            ClientError::Daemon {
                status,
                code,
                message,
            } => write!(f, "daemon returned {status} {code}: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// The daemon operations the tray needs.
///
/// A trait so the menu can be exercised against a scripted daemon in tests
/// without a socket, a bus, or a running service.
pub trait DaemonApi: Send {
    fn status(&self) -> Result<Status, ClientError>;
    fn profiles(&self) -> Result<Vec<ProfileSummary>, ClientError>;
    fn activate_profile(&self, profile_id: &str) -> Result<(), ClientError>;
    fn deactivate_profile(&self) -> Result<(), ClientError>;
}

/// Talks HTTP/1.1 to the daemon over its Unix socket.
pub struct DaemonClient {
    socket_path: PathBuf,
    timeout: Duration,
}

impl DaemonClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(socket_path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout,
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, ClientError> {
        // Run the exchange on a worker and wait on a bounded channel, so the
        // caller's deadline covers CONNECT as well as the transfer.
        //
        // `UnixStream::connect` cannot be bounded in place: a blocking AF_UNIX
        // connect does not fail when the listener's backlog is full, it sleeps
        // in the kernel until a slot frees. Against a daemon whose accept loop
        // has stalled, an in-place connect therefore blocks forever — on ksni's
        // service thread, which is also the thread that dispatches "Quit tray",
        // so the tray becomes unkillable from its own menu.
        //
        // Residual, accepted and recorded as `T1-g`: a worker parked in that
        // connect outlives its caller. It holds one fd and an 8 KiB stack, it
        // ends as soon as the daemon accepts or the socket errors, and menu
        // opens are user-paced. That is strictly better than a frozen panel.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let socket_path = self.socket_path.clone();
        let budget = self.timeout;
        let method = method.to_string();
        let path = path.to_string();
        let body = body.map(<[u8]>::to_vec);

        std::thread::spawn(move || {
            let outcome = Self::exchange(&socket_path, budget, &method, &path, body.as_deref());
            // The receiver is gone whenever the deadline fired first. Expected.
            let _ = tx.send(outcome);
        });

        rx.recv_timeout(budget)
            .unwrap_or_else(|_| Err(ClientError::Unavailable("request timed out".to_string())))
    }

    /// One request/response on the calling thread, bounded by `budget` in total.
    fn exchange(
        socket_path: &std::path::Path,
        budget: Duration,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, ClientError> {
        let deadline = Instant::now() + budget;

        let stream = UnixStream::connect(socket_path)
            .map_err(|e| ClientError::Unavailable(format!("connect: {e}")))?;
        // The write side still gets a per-syscall timeout: a daemon that accepts
        // and then stops reading would otherwise wedge the write rather than the
        // read.
        stream
            .set_write_timeout(Some(budget))
            .map_err(|e| ClientError::Unavailable(format!("set timeout: {e}")))?;

        let mut request = Vec::with_capacity(128 + body.map_or(0, <[u8]>::len));
        // `Connection: close` is what lets the reader below stop at EOF instead
        // of having to implement keep-alive framing.
        let head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        request.extend_from_slice(head.as_bytes());
        if let Some(payload) = body {
            let content = format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                payload.len()
            );
            request.extend_from_slice(content.as_bytes());
        }
        request.extend_from_slice(b"\r\n");
        if let Some(payload) = body {
            request.extend_from_slice(payload);
        }

        let mut writer = &stream;
        writer
            .write_all(&request)
            .and_then(|()| writer.flush())
            .map_err(|e| Self::io_error(&e, "write"))?;

        // Read against the REMAINING budget, not a fresh timeout per call.
        //
        // `set_read_timeout` is `SO_RCVTIMEO`, which applies to each `recv()`
        // individually — so `read_to_end` restarts the clock on every byte that
        // arrives, and a peer dribbling one byte just inside the timeout is
        // never timed out at all. Measured before this was fixed: a peer sending
        // 1 byte per 60 ms held a 300 ms "deadline" for 3.0 s, and would have
        // held it for as long as it kept dribbling. Pinned by
        // `a_trickling_daemon_cannot_outlast_the_deadline`.
        let mut raw = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ClientError::Unavailable("read timed out".to_string()));
            }
            // Any non-zero Duration is safe here: std rejects exactly zero and
            // clamps anything smaller than a microsecond up to 1 µs.
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|e| ClientError::Unavailable(format!("set timeout: {e}")))?;

            match (&stream).read(&mut chunk) {
                // EOF — `Connection: close` is what makes this the frame end.
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&chunk[..n]);
                    // Bounded at ingest rather than after the fact.
                    if raw.len() as u64 > MAX_RESPONSE_BYTES {
                        return Err(ClientError::Protocol(format!(
                            "response exceeded {MAX_RESPONSE_BYTES} bytes"
                        )));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Self::io_error(&e, "read")),
            }
        }

        Self::parse_response(&raw)
    }

    fn io_error(e: &std::io::Error, phase: &str) -> ClientError {
        match e.kind() {
            // A socket deadline surfaces as WouldBlock on Linux, TimedOut elsewhere.
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                ClientError::Unavailable(format!("{phase} timed out"))
            }
            _ => ClientError::Unavailable(format!("{phase}: {e}")),
        }
    }

    /// Split an HTTP/1.1 response into status + body.
    ///
    /// Associated rather than free so the parser is reachable from tests
    /// without opening a socket.
    fn parse_response(raw: &[u8]) -> Result<Vec<u8>, ClientError> {
        if raw.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(ClientError::Protocol(format!(
                "response exceeded {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        if raw.is_empty() {
            return Err(ClientError::Unavailable(
                "daemon closed the connection without responding".to_string(),
            ));
        }

        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| ClientError::Protocol("no header terminator".to_string()))?;
        let head = std::str::from_utf8(&raw[..split])
            .map_err(|_| ClientError::Protocol("non-UTF-8 headers".to_string()))?;
        let mut body = &raw[split + 4..];

        let mut lines = head.split("\r\n");
        let status_line = lines
            .next()
            .ok_or_else(|| ClientError::Protocol("empty status line".to_string()))?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| {
                ClientError::Protocol(format!("unparsable status line: {status_line:?}"))
            })?;

        let mut content_length: Option<usize> = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
                // axum always sets content-length for the fixed-size JSON bodies
                // the tray reads, so this cannot happen against a real daemon.
                // Refuse loudly rather than hand back a body with chunk framing
                // still embedded in it.
                return Err(ClientError::Protocol(
                    "chunked transfer-encoding is not supported".to_string(),
                ));
            }
            if name == "content-length" {
                content_length = value.parse::<usize>().ok();
            }
        }

        if let Some(len) = content_length {
            if len > body.len() {
                return Err(ClientError::Protocol(format!(
                    "truncated body: content-length {len}, got {}",
                    body.len()
                )));
            }
            body = &body[..len];
        }

        if (200..300).contains(&status) {
            return Ok(body.to_vec());
        }

        Err(match serde_json::from_slice::<ErrorEnvelope>(body) {
            Ok(envelope) => ClientError::Daemon {
                status,
                code: envelope.error.code,
                message: envelope.error.message,
            },
            Err(_) => ClientError::Daemon {
                status,
                code: String::new(),
                message: String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned(),
            },
        })
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, ClientError> {
        let body = self.request("GET", path, None)?;
        serde_json::from_slice(&body).map_err(|e| ClientError::Protocol(format!("{path}: {e}")))
    }
}

impl DaemonApi for DaemonClient {
    fn status(&self) -> Result<Status, ClientError> {
        self.get_json("/status")
    }

    fn profiles(&self) -> Result<Vec<ProfileSummary>, ClientError> {
        let list: ProfileListResponse = self.get_json("/profiles")?;
        Ok(list.profiles)
    }

    fn activate_profile(&self, profile_id: &str) -> Result<(), ClientError> {
        // Built through serde so an id containing a quote or backslash cannot
        // break out of the JSON document.
        let payload = serde_json::json!({ "profile_id": profile_id }).to_string();
        self.request("POST", "/profile/activate", Some(payload.as_bytes()))?;
        Ok(())
    }

    fn deactivate_profile(&self) -> Result<(), ClientError> {
        // The handler reads no body; an explicit `Content-Length: 0` keeps the
        // request unambiguous rather than relying on a bodyless POST being
        // accepted.
        self.request("POST", "/profile/deactivate", Some(b""))?;
        Ok(())
    }
}
