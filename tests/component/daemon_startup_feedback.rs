#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The daemon's JSON-RPC handshake, sent as one line on its socket.
const RPC_INITIALIZE: &str =
    r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocol_version":1},"id":1}"#;

fn rpc_initialize_ok(socket: &Path) -> bool {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .is_err()
        || writeln!(stream, "{RPC_INITIALIZE}").is_err()
    {
        return false;
    }
    let mut response = [0_u8; 4096];
    let Ok(read) = stream.read(&mut response) else {
        return false;
    };
    String::from_utf8_lossy(&response[..read]).contains("\"result\"")
}

fn http_status(port: u16, request: &[u8]) -> Option<u16> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    stream.write_all(request).ok()?;

    let mut response = [0_u8; 512];
    let read = stream.read(&mut response).ok()?;
    let status_line = std::str::from_utf8(&response[..read])
        .ok()?
        .lines()
        .next()?;
    status_line.split_whitespace().nth(1)?.parse().ok()
}

#[test]
fn daemon_surfaces_retired_wati_config_without_leaking_tokens() {
    let cases = [
        (
            "current",
            r#"schema_version = 4

[gateway]
require_pairing = false

[channels.wati.exact_head_smoke]
enabled = true
api_token = "WATI_CURRENT_PLACEHOLDER_MUST_NOT_APPEAR"
api_url = "https://example.invalid"
allowed_numbers = ["1234567890"]
"#,
            "channels.wati",
            "WATI_CURRENT_PLACEHOLDER_MUST_NOT_APPEAR",
        ),
        (
            "legacy",
            r#"[gateway]
require_pairing = false

[channels_config.wati]
enabled = true
api_token = "WATI_LEGACY_PLACEHOLDER_MUST_NOT_APPEAR"
api_url = "https://example.invalid"
allowed_numbers = ["1234567890"]
"#,
            // Unversioned, so the V1 migration first renames `channels_config`
            // to `channels`; the retirement notice names the migrated path.
            "channels.wati",
            "WATI_LEGACY_PLACEHOLDER_MUST_NOT_APPEAR",
        ),
    ];

    for (case, raw_config, expected_path, placeholder) in cases {
        let config_dir = tempfile::tempdir().unwrap();
        std::fs::write(config_dir.path().join("config.toml"), raw_config).unwrap();
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        let mut child = Command::new(env!("CARGO_BIN_EXE_zeroclaw"))
            .arg("--config-dir")
            .arg(config_dir.path())
            .arg("daemon")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--allow-degraded-security")
            .env("LC_ALL", "C")
            .env("TERM", "dumb")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // With the gateway, readiness is its HTTP health; without it, the socket.
        let socket = config_dir.path().join("data").join("daemon.sock");
        let health_request =
            b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut health_status = None;
        let mut ready = false;
        let mut early_exit = None;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                early_exit = Some(status);
                break;
            }
            ready = if cfg!(feature = "gateway") {
                health_status = http_status(port, health_request);
                health_status == Some(200)
            } else {
                rpc_initialize_ok(&socket)
            };
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        if early_exit.is_some() || !ready {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "{case} daemon failed before readiness: exit={early_exit:?}, health={health_status:?}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        let post_status = cfg!(feature = "gateway").then(|| {
            let body = r#"{"text":"hello","waId":"1234567890","fromMe":false}"#;
            let request = format!(
                "POST /wati HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            http_status(port, request.as_bytes())
        });

        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if let Some(post_status) = post_status {
            assert_eq!(health_status, Some(200), "{case}: daemon health");
            assert!(
                matches!(post_status, Some(404 | 405)),
                "{case}: retired POST /wati must be unavailable, got {post_status:?}"
            );
        }
        assert!(
            stderr.contains(&format!("dropped retired config key `{expected_path}`")),
            "{case}: stderr must name retired path {expected_path}: {stderr}"
        );
        assert!(
            !stdout.contains(placeholder) && !stderr.contains(placeholder),
            "{case}: placeholder WATI token leaked to process output"
        );
    }
}

#[test]
fn daemon_guidance_does_not_advertise_unhandled_sigusr1() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let main_source = std::fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(
        !main_source.contains("SIGUSR1"),
        "daemon guidance must not advertise the unsupported SIGUSR1 reload path"
    );
}
