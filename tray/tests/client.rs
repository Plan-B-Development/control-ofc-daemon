//! The hand-written HTTP/1.1 client, against a real Unix socket serving canned
//! bytes.
//!
//! No daemon, no hardware, no bus. The one wedge here (a server that accepts and
//! never answers) **self-releases** on a bounded timer: a wedge that did not
//! would turn a would-be red test into a hung CI job, which this project has
//! already paid for once.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use control_ofc_tray::client::{ClientError, DaemonApi, DaemonClient};

/// How the fake daemon answers.
enum Behaviour {
    Respond(Vec<u8>),
    /// Accept, read, then answer nothing until the bounded release fires.
    Silent,
    /// Answer one byte at a time, slower than the response completes but faster
    /// than any single-read timeout. This is the case a per-syscall timeout
    /// cannot bound, because every byte restarts its clock.
    Trickle,
}

struct TestServer {
    path: PathBuf,
    _dir: tempfile::TempDir,
    requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn spawn(behaviour: Behaviour) -> TestServer {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        // Bounded: serve a handful of connections then exit, so the thread does
        // not outlive the test as a permanently blocked accept().
        for conn in listener.incoming().take(4) {
            let Ok(mut stream) = conn else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            recorded
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            match &behaviour {
                Behaviour::Respond(bytes) => {
                    let _ = stream.write_all(bytes);
                }
                Behaviour::Silent => {
                    // Self-releasing: well past any client deadline in these
                    // tests, but finite, so a broken client fails rather than
                    // hanging the suite.
                    std::thread::sleep(Duration::from_secs(2));
                }
                Behaviour::Trickle => {
                    // One byte per 60 ms against a 300 ms deadline: each
                    // individual read completes comfortably, so only a TOTAL
                    // deadline can stop this. Bounded at ~3 s so a regression
                    // fails the test instead of hanging the suite.
                    for _ in 0..50 {
                        if stream.write_all(b"H").is_err() {
                            break;
                        }
                        let _ = stream.flush();
                        std::thread::sleep(Duration::from_millis(60));
                    }
                }
            }
        }
    });

    TestServer {
        path,
        _dir: dir,
        requests,
    }
}

fn http(status_line: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

const STATUS_BODY: &str = r#"{"api_version":1,"daemon_version":"2.43.6","overall_status":"ok",
        "thermal_state":"normal","active_profile_id":"balanced","active_profile_name":"Balanced"}"#;

#[test]
fn a_normal_status_response_parses() {
    let server = spawn(Behaviour::Respond(http("200 OK", STATUS_BODY)));
    let client = DaemonClient::new(&server.path);

    let status = client.status().expect("should parse");
    assert_eq!(status.daemon_version, "2.43.6");
    assert_eq!(status.thermal_state, "normal");
    assert_eq!(status.active_profile_id.as_deref(), Some("balanced"));
    assert!(!status.thermal_is_abnormal());

    let request = &server.requests()[0];
    assert!(
        request.starts_with("GET /status HTTP/1.1\r\n"),
        "{request:?}"
    );
    assert!(
        request.to_lowercase().contains("connection: close"),
        "the reader relies on EOF framing, so this header is load-bearing: {request:?}"
    );
}

#[test]
fn an_absent_thermal_state_reads_as_normal_not_as_abnormal() {
    // A daemon predating the field must not make the tray shout about a thermal
    // event forever. serde's default for String would be "", which is != "normal".
    let server = spawn(Behaviour::Respond(http(
        "200 OK",
        r#"{"api_version":1,"daemon_version":"1.0.0"}"#,
    )));
    let client = DaemonClient::new(&server.path);

    let status = client.status().expect("should parse");
    assert_eq!(status.thermal_state, "normal");
    assert!(
        !status.thermal_is_abnormal(),
        "a missing field must not render as a thermal warning"
    );
}

#[test]
fn profiles_are_unwrapped_from_the_envelope() {
    let server = spawn(Behaviour::Respond(http(
        "200 OK",
        r#"{"api_version":1,"profiles":[{"id":"quiet","name":"Quiet","description":"d"},
            {"id":"balanced","name":"Balanced","description":""}]}"#,
    )));
    let client = DaemonClient::new(&server.path);

    let profiles = client.profiles().expect("should parse");
    assert_eq!(profiles.len(), 2);
    assert_eq!(profiles[0].id, "quiet");
    assert_eq!(profiles[1].name, "Balanced");
}

#[test]
fn an_error_envelope_becomes_a_typed_daemon_error() {
    let server = spawn(Behaviour::Respond(http(
        "404 Not Found",
        r#"{"error":{"code":"not_found","message":"profile 'nope' not found","retryable":false,"source":"api"}}"#,
    )));
    let client = DaemonClient::new(&server.path);

    match client.activate_profile("nope") {
        Err(ClientError::Daemon {
            status,
            code,
            message,
        }) => {
            assert_eq!(status, 404);
            assert_eq!(code, "not_found");
            assert!(message.contains("not found"), "{message}");
        }
        other => panic!("expected a typed daemon error, got {other:?}"),
    }
}

#[test]
fn a_missing_socket_reports_unavailable_rather_than_panicking() {
    let client = DaemonClient::new("/nonexistent/control-ofc.sock");
    match client.status() {
        Err(ClientError::Unavailable(_)) => {}
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn a_wedged_daemon_times_out_instead_of_freezing_the_menu() {
    // The menu is built synchronously inside menu_about_to_show, so an
    // unbounded read here would freeze the Plasma panel, not just the tray.
    let server = spawn(Behaviour::Silent);
    let client = DaemonClient::with_timeout(&server.path, Duration::from_millis(100));

    let started = Instant::now();
    let result = client.status();
    let elapsed = started.elapsed();

    match result {
        Err(ClientError::Unavailable(why)) => assert!(
            why.contains("timed out"),
            "the deadline must be reported as a timeout, not a generic error: {why}"
        ),
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(1),
        "must return on its own deadline (100ms), not on the server's 2s release; took {elapsed:?}"
    );
}

#[test]
fn a_truncated_body_is_rejected_rather_than_parsed() {
    // content-length claims more than was sent.
    let mut raw = b"HTTP/1.1 200 OK\r\ncontent-length: 500\r\nconnection: close\r\n\r\n".to_vec();
    raw.extend_from_slice(br#"{"daemon_version":"2.4"#);
    let server = spawn(Behaviour::Respond(raw));
    let client = DaemonClient::new(&server.path);

    match client.status() {
        Err(ClientError::Protocol(why)) => assert!(why.contains("truncated"), "{why}"),
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[test]
fn chunked_encoding_is_refused_rather_than_mis_framed() {
    let raw =
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec();
    let server = spawn(Behaviour::Respond(raw));
    let client = DaemonClient::new(&server.path);

    match client.status() {
        Err(ClientError::Protocol(why)) => assert!(why.contains("chunked"), "{why}"),
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[test]
fn a_connection_closed_without_a_response_is_unavailable_not_a_parse_error() {
    let server = spawn(Behaviour::Respond(Vec::new()));
    let client = DaemonClient::new(&server.path);

    match client.status() {
        Err(ClientError::Unavailable(_)) => {}
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn activate_sends_the_documented_request_shape() {
    let server = spawn(Behaviour::Respond(http(
        "200 OK",
        r#"{"api_version":1,"activated":true,"profile_id":"quiet","profile_name":"Quiet"}"#,
    )));
    let client = DaemonClient::new(&server.path);
    client.activate_profile("quiet").expect("should succeed");

    let request = &server.requests()[0];
    assert!(
        request.starts_with("POST /profile/activate HTTP/1.1\r\n"),
        "{request:?}"
    );
    assert!(
        request.contains(r#"{"profile_id":"quiet"}"#),
        "the daemon reads profile_id from the body: {request:?}"
    );
    assert!(
        request.to_lowercase().contains("content-length: 22"),
        "a POST must declare its body length: {request:?}"
    );
}

#[test]
fn a_profile_id_containing_json_metacharacters_cannot_break_the_body() {
    // Ids come from the daemon's own listing, but building the body by hand
    // would still be a latent injection; this pins that serde does the escaping.
    let server = spawn(Behaviour::Respond(http("200 OK", "{}")));
    let client = DaemonClient::new(&server.path);
    let _ = client.activate_profile(r#"ev"il\"#);

    let request = &server.requests()[0];
    assert!(
        request.contains(r#"{"profile_id":"ev\"il\\"}"#),
        "quotes and backslashes must be escaped, not emitted raw: {request:?}"
    );
}

#[test]
fn deactivate_posts_with_an_explicit_empty_body() {
    let server = spawn(Behaviour::Respond(http(
        "200 OK",
        r#"{"api_version":1,"deactivated":true,"previous_profile_id":null,"previous_profile_name":null}"#,
    )));
    let client = DaemonClient::new(&server.path);
    client.deactivate_profile().expect("should succeed");

    let request = &server.requests()[0];
    assert!(
        request.starts_with("POST /profile/deactivate HTTP/1.1\r\n"),
        "{request:?}"
    );
    assert!(
        request.to_lowercase().contains("content-length: 0"),
        "an unambiguous bodyless POST declares zero length: {request:?}"
    );
}

#[test]
fn a_trickling_daemon_cannot_outlast_the_deadline() {
    // A per-syscall read timeout (SO_RCVTIMEO) restarts on every byte received,
    // so a peer answering slowly-but-steadily is never "timed out" by it. Only
    // a deadline measured across the whole request bounds this. Without one the
    // ksni service thread — the same thread that dispatches "Quit tray" — is
    // held for as long as the peer keeps dribbling.
    let server = spawn(Behaviour::Trickle);
    let client = DaemonClient::with_timeout(&server.path, Duration::from_millis(300));

    let started = Instant::now();
    let result = client.status();
    let elapsed = started.elapsed();

    assert!(
        result.is_err(),
        "a trickling peer never completes a response; this must not return Ok"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "the request must end on its own deadline (300 ms), not on the peer's \
         ~3 s self-release; took {elapsed:?}"
    );
}
