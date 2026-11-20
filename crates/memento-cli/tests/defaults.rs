use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;

use serde_json::{Value, json};

fn read_request(stream: &TcpStream) -> (String, Value) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let first = line.trim().to_owned();
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
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (first, value)
}

fn respond(stream: &mut TcpStream, body: &Value) {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

#[test]
fn defaults_use_cwd_and_digest_selects_latest_active_session() {
    let project = tempfile::tempdir().unwrap();
    let project_key = project
        .path()
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let expected_project = project_key.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (line, body) = read_request(&stream);
        assert!(line.starts_with("POST /v1/search "));
        assert_eq!(body["project"], expected_project);
        assert_eq!(body["scope"], "project");
        assert_eq!(body["harness"], "");
        assert_eq!(body["session"], "");
        respond(&mut stream, &json!({"hits":[]}));

        let (mut stream, _) = listener.accept().unwrap();
        let (line, _) = read_request(&stream);
        let target = line.split_whitespace().nth(1).unwrap();
        let query = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(query.path(), "/v1/sessions");
        assert_eq!(
            query.query_pairs().collect::<Vec<_>>(),
            vec![("project".into(), expected_project.clone().into())]
        );
        let record = |harness, session, active, deleted| {
            json!({
                "harness":harness,"project":expected_project,"session":session,
                "parent":null,"parent_harness":null,"title":null,"created":null,
                "last_active_at":active,"deleted_at":deleted
            })
        };
        respond(
            &mut stream,
            &json!({"sessions":[
                record("opencode","older","2026-11-18T10:00:00Z",Value::Null),
                record("codex","latest","2026-11-19T10:00:00Z",Value::Null),
                record("agy","deleted","2026-11-20T10:00:00Z",json!("2026-11-20T11:00:00Z"))
            ]}),
        );
        let (mut stream, _) = listener.accept().unwrap();
        let (line, _) = read_request(&stream);
        let target = line.split_whitespace().nth(1).unwrap();
        let query = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(query.path(), "/v1/stats");
        assert_eq!(
            query.query_pairs().collect::<Vec<_>>(),
            vec![
                ("harness".into(), "codex".into()),
                ("project".into(), expected_project.clone().into()),
                ("session".into(), "latest".into()),
            ]
        );
        respond(&mut stream, &json!({"total":1,"by_kind":{"decision":1}}));

        let (mut stream, _) = listener.accept().unwrap();
        let (line, _) = read_request(&stream);
        let target = line.split_whitespace().nth(1).unwrap();
        let query = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(query.path(), "/v1/digest");
        let pairs = query.query_pairs().collect::<Vec<_>>();
        assert!(pairs.contains(&("harness".into(), "codex".into())));
        assert!(pairs.contains(&("session".into(), "latest".into())));
        assert!(pairs.contains(&("project".into(), expected_project.clone().into())));
        respond(&mut stream, &json!({"text":"chosen digest","segments":1}));

        let (mut stream, _) = listener.accept().unwrap();
        let (line, _) = read_request(&stream);
        let target = line.split_whitespace().nth(1).unwrap();
        let query = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(query.path(), "/v1/stats");
        assert_eq!(
            query.query_pairs().collect::<Vec<_>>(),
            vec![("project".into(), expected_project.into())]
        );
        respond(&mut stream, &json!({"total":3,"by_kind":{"decision":3}}));
    });
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_memento"))
            .args(["--url", &url])
            .args(args)
            .current_dir(project.path())
            .env_remove("MEMENTO_TOKEN")
            .output()
            .unwrap()
    };
    assert!(run(&["search", "retry limit"]).status.success());
    let digest = run(&["digest"]);
    assert!(
        digest.status.success(),
        "{}",
        String::from_utf8_lossy(&digest.stderr)
    );
    assert_eq!(digest.stdout, b"chosen digest\n");
    assert!(String::from_utf8_lossy(&digest.stderr).contains("codex/latest"));
    let stats = run(&["stats"]);
    assert!(stats.status.success());
    assert!(String::from_utf8_lossy(&stats.stdout).contains("total  3"));
    server.join().unwrap();
}
