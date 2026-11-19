#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for condition");
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_request(stream: &std::net::TcpStream) -> Value {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("POST /v1/ingest "));
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn write_hook_input(child: &mut std::process::Child, transcript: &Path, project: &Path) {
    let input = json!({
        "session_id": "test-session",
        "transcript_path": transcript,
        "cwd": project,
        "hook_event_name": "Stop"
    });
    write!(child.stdin.take().unwrap(), "{input}").unwrap();
}

#[test]
fn detached_stop_returns_before_ingest_and_clears_marker() {
    let temp = tempfile::tempdir().unwrap();
    let transcript = temp.path().join("transcript.jsonl");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude.jsonl"),
        &transcript,
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let marker = temp
        .path()
        .join("state/memento/pending/claude-code-test-session");
    let marker_before_ingest = marker.clone();
    let (request_sender, request_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let body = read_request(&stream);
        request_sender
            .send((marker_before_ingest.exists(), body))
            .unwrap();
        release_receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        let response = r#"{"segments":3,"skipped":1}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .unwrap();
    });
    let mut hook = Command::new(env!("CARGO_BIN_EXE_memento"))
        .args(["--url", &url, "hook", "claude-code"])
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("MEMENTO_JEV", "0")
        .env_remove("MEMENTO_HOOK_SYNC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    write_hook_input(&mut hook, &transcript, temp.path());
    wait_until(|| hook.try_wait().unwrap().is_some());
    assert!(started.elapsed() < Duration::from_secs(5));
    let output = hook.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    let (marker_existed, body) = request_receiver
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(marker_existed);
    assert_eq!(body["session"], "test-session");
    assert!(marker.exists());
    release_sender.send(()).unwrap();
    server.join().unwrap();
    wait_until(|| !marker.exists());
    assert!(temp.path().join("state/memento/cursors.json").exists());
}

#[test]
fn failed_detached_import_clears_marker() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("state/memento/pending/codex-test-session");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(&marker, b"1").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_memento"))
        .args(["hook-import", "codex", "--transcript"])
        .arg(temp.path().join("missing.jsonl"))
        .args(["--session", "test-session"])
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!marker.exists());
}

#[test]
fn continue_waits_before_contacting_daemon_and_ignores_stale_marker() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp
        .path()
        .join("state/memento/pending/claude-code-test-session");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(&marker, now.to_string()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut child = Command::new(env!("CARGO_BIN_EXE_memento"))
        .args([
            "--url",
            &url,
            "continue",
            "--from",
            "claude-code/test-session",
            "--project",
        ])
        .arg(temp.path())
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    fs::remove_file(&marker).unwrap();
    let mut connection = None;
    wait_until(|| match listener.accept() {
        Ok((stream, _)) => {
            connection = Some(stream);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("cannot accept continuation: {error}"),
    });
    drop(connection);
    wait_until(|| child.try_wait().unwrap().is_some());

    fs::write(&marker, b"1").unwrap();
    let mut stale = Command::new(env!("CARGO_BIN_EXE_memento"))
        .args([
            "--url",
            &url,
            "continue",
            "--from",
            "claude-code/test-session",
            "--project",
        ])
        .arg(temp.path())
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut connection = None;
    wait_until(|| match listener.accept() {
        Ok((stream, _)) => {
            connection = Some(stream);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("cannot accept continuation: {error}"),
    });
    drop(connection);
    wait_until(|| stale.try_wait().unwrap().is_some());
    assert!(!marker.exists());
}
