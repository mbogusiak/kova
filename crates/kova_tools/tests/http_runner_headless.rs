//! Headless integration tests for the `.http` runner (`KOVA-042`): multi-request parsing,
//! variable substitution from `kova.toml` / `http-client.env.json`, and execution against a
//! local loopback server (no external network).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests abort with context on failure"
)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use kova_buffer::Buffer;
use kova_syntax::{LanguageRegistry, SyntaxEngine, SyntaxEngineConfig, detect_entry_points};
use kova_theme::SyntaxLanguage;
use kova_tools::http_runner::{
    HttpBodyKind, HttpEnvironments, HttpExecutor, HttpExecutorConfig, HttpFile, HttpRunnerError,
    HttpStatusClass, body_preview, prepare_request_at_line,
};

const SAMPLE: &str = "\
@apiVersion = v2

### List users
GET {{baseUrl}}/{{apiVersion}}/users?limit=10
Accept: application/json
Authorization: Bearer {{token}}

###
# @name createUser
POST {{baseUrl}}/{{apiVersion}}/users HTTP/1.1
Content-Type: application/json

{
  \"name\": \"{{userName}}\"
}

### Delete
DELETE {{baseUrl}}/users/42
";

#[test]
fn parses_multiple_requests_from_one_file() {
    let file = HttpFile::parse(SAMPLE);
    assert_eq!(file.requests.len(), 3);

    let list = &file.requests[0];
    assert_eq!(list.name.as_deref(), Some("List users"));
    assert_eq!(list.method, "GET");
    assert_eq!(list.url, "{{baseUrl}}/{{apiVersion}}/users?limit=10");
    assert_eq!(list.headers.len(), 2);
    assert_eq!(list.request_line, 3);

    let create = &file.requests[1];
    assert_eq!(create.name.as_deref(), Some("createUser"));
    assert_eq!(create.method, "POST");
    assert_eq!(create.http_version.as_deref(), Some("HTTP/1.1"));
    assert_eq!(
        create.body.as_deref(),
        Some("{\n  \"name\": \"{{userName}}\"\n}")
    );
    assert_eq!(create.line_range, 9..=14);

    let delete = &file.requests[2];
    assert_eq!(delete.method, "DELETE");
    assert!(delete.body.is_none());
    assert_eq!(
        file.variables.get("apiVersion").map(String::as_str),
        Some("v2")
    );
}

#[test]
fn request_lines_agree_with_syntax_gutter_entry_points() {
    let registry = LanguageRegistry::builtin().expect("registry");
    let engine =
        SyntaxEngine::new(registry.clone(), SyntaxEngineConfig::default()).expect("engine");
    let snapshot = Buffer::from_text(SAMPLE).snapshot();
    let tree = engine
        .parse_sync(SyntaxLanguage::Http, &snapshot, None)
        .expect("parse");
    let gutter_lines: Vec<usize> = detect_entry_points(&tree, &snapshot)
        .iter()
        .map(|ep| ep.line)
        .collect();

    let file = HttpFile::parse(SAMPLE);
    let parsed_lines: Vec<usize> = file.requests.iter().map(|r| r.request_line).collect();
    assert_eq!(gutter_lines, parsed_lines);
    for line in gutter_lines {
        assert_eq!(
            file.request_at_line(line).map(|r| r.request_line),
            Some(line)
        );
    }
}

#[test]
fn substitutes_variables_from_project_environment_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("kova.toml"),
        "schema_version = 1\n\n[http]\nenvironment = \"dev\"\n\n[http.environments.dev]\nbaseUrl = \"http://toml.invalid\"\nuserName = \"Ada\"\n",
    )
    .expect("write kova.toml");
    std::fs::write(
        dir.path().join("http-client.env.json"),
        r#"{"dev": {"baseUrl": "http://localhost:8080"}, "prod": {"baseUrl": "https://api.kova.dev"}}"#,
    )
    .expect("write env");
    std::fs::write(
        dir.path().join("http-client.private.env.json"),
        r#"{"dev": {"token": "s3cr3t"}}"#,
    )
    .expect("write private env");

    let envs = HttpEnvironments::load_from_dir(dir.path()).expect("load envs");
    assert_eq!(envs.names().collect::<Vec<_>>(), ["dev", "prod"]);
    let file = HttpFile::parse(SAMPLE);

    let (_, list) = prepare_request_at_line(&file, 3, &envs, None).expect("resolve list");
    assert_eq!(list.url, "http://localhost:8080/v2/users?limit=10");
    assert_eq!(list.headers[1].value, "Bearer s3cr3t");

    let (spec, create) = prepare_request_at_line(&file, 12, &envs, None).expect("resolve create");
    assert_eq!(spec.index, 1);
    assert_eq!(create.body.as_deref(), Some("{\n  \"name\": \"Ada\"\n}"));

    let (_, prod) = prepare_request_at_line(&file, 17, &envs, Some("prod")).expect("prod");
    assert_eq!(prod.url, "https://api.kova.dev/users/42");

    assert_eq!(
        prepare_request_at_line(&file, 3, &envs, Some("prod")).map(|_| ()),
        Err(HttpRunnerError::UnresolvedVariable {
            name: "token".to_owned()
        })
    );
    assert_eq!(
        prepare_request_at_line(&file, 0, &envs, None).map(|_| ()),
        Err(HttpRunnerError::NoRequestAtLine { line: 0 })
    );
}

#[test]
fn missing_environment_files_are_optional() {
    let dir = tempfile::tempdir().expect("tempdir");
    let envs = HttpEnvironments::load_from_dir(dir.path()).expect("load");
    assert!(envs.environments.is_empty());
}

// ---------------------------------------------------------------------------
// Loopback HTTP server
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn respond(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        write!(out, "{k}: {v}\r\n").ok();
    }
    out.push_str("\r\n");
    out.push_str(body);
    stream.write_all(out.as_bytes()).ok();
}

fn handle(mut stream: TcpStream) {
    let Some(req) = read_request(&mut stream) else {
        return;
    };
    match req.path.as_str() {
        "/json" => respond(
            &mut stream,
            "200 OK",
            &[("Content-Type", "application/json"), ("X-Kova", "1")],
            r#"{"id":7,"tags":["a","b"]}"#,
        ),
        "/echo" => {
            let token = req
                .headers
                .iter()
                .find(|(k, _)| k == "x-token")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let body = format!("{} {} {}", req.method, token, req.body);
            respond(
                &mut stream,
                "201 Created",
                &[("Content-Type", "text/plain")],
                &body,
            );
        }
        "/redirect" => respond(&mut stream, "302 Found", &[("Location", "/json")], ""),
        "/slow" => {
            std::thread::sleep(Duration::from_secs(3));
            respond(&mut stream, "200 OK", &[], "late");
        }
        _ => respond(
            &mut stream,
            "404 Not Found",
            &[("Content-Type", "text/plain")],
            "nope",
        ),
    }
}

fn start_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || handle(stream));
        }
    });
    format!("http://{addr}")
}

fn resolve(text: &str, base: &str) -> kova_tools::http_runner::ResolvedHttpRequest {
    let mut envs = HttpEnvironments::default();
    envs.environments
        .entry("local".to_owned())
        .or_default()
        .insert("baseUrl".to_owned(), base.to_owned());
    let file = HttpFile::parse(text);
    let line = file.requests[0].request_line;
    prepare_request_at_line(&file, line, &envs, None)
        .expect("resolve")
        .1
}

fn executor(timeout: Duration) -> HttpExecutor {
    HttpExecutor::new(HttpExecutorConfig {
        timeout,
        ..HttpExecutorConfig::default()
    })
    .expect("executor")
}

#[test]
fn executes_get_and_collects_metadata_with_pretty_json() {
    let base = start_server();
    let req = resolve("GET {{baseUrl}}/json\nAccept: application/json\n", &base);
    let resp = executor(Duration::from_secs(10))
        .spawn(req)
        .blocking_result()
        .expect("response");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.reason.as_deref(), Some("OK"));
    assert_eq!(
        HttpStatusClass::from_status(resp.status),
        HttpStatusClass::Success
    );
    assert_eq!(resp.size_bytes, 25);
    assert_eq!(resp.header("x-kova"), Some("1"));
    assert!(!resp.truncated);
    let preview = body_preview(&resp);
    assert_eq!(preview.kind, HttpBodyKind::Json);
    assert!(preview.pretty);
    assert_eq!(
        preview.text,
        "{\n  \"id\": 7,\n  \"tags\": [\n    \"a\",\n    \"b\"\n  ]\n}"
    );
}

#[test]
fn sends_method_headers_and_body() {
    let base = start_server();
    let req = resolve("POST {{baseUrl}}/echo\nX-Token: abc\n\npayload\n", &base);
    let resp = executor(Duration::from_secs(10))
        .spawn(req)
        .blocking_result()
        .expect("response");
    assert_eq!(resp.status, 201);
    assert_eq!(String::from_utf8_lossy(&resp.body), "POST abc payload");
}

#[test]
fn follows_redirects_and_reports_client_errors() {
    let base = start_server();
    let exec = executor(Duration::from_secs(10));
    let redirected = exec
        .spawn(resolve("GET {{baseUrl}}/redirect\n", &base))
        .blocking_result()
        .expect("redirect");
    assert_eq!(redirected.status, 200);
    assert!(redirected.final_url.ends_with("/json"));

    let missing = exec
        .spawn(resolve("GET {{baseUrl}}/missing\n", &base))
        .blocking_result()
        .expect("404");
    assert_eq!(
        HttpStatusClass::from_status(missing.status),
        HttpStatusClass::ClientError
    );
}

#[test]
fn no_redirect_policy_returns_3xx() {
    let base = start_server();
    let exec = HttpExecutor::new(HttpExecutorConfig {
        max_redirects: 0,
        ..HttpExecutorConfig::default()
    })
    .expect("executor");
    let resp = exec
        .spawn(resolve("GET {{baseUrl}}/redirect\n", &base))
        .blocking_result()
        .expect("302");
    assert_eq!(resp.status, 302);
}

#[test]
fn times_out_slow_requests() {
    let base = start_server();
    let result = executor(Duration::from_millis(200))
        .spawn(resolve("GET {{baseUrl}}/slow\n", &base))
        .blocking_result();
    assert_eq!(result, Err(HttpRunnerError::Timeout { timeout_ms: 200 }));
}

#[test]
fn cancels_in_flight_request() {
    let base = start_server();
    let handle = executor(Duration::from_secs(10)).spawn(resolve("GET {{baseUrl}}/slow\n", &base));
    let token = handle.cancel_token();
    std::thread::sleep(Duration::from_millis(100));
    token.cancel();
    let started = std::time::Instant::now();
    assert_eq!(handle.blocking_result(), Err(HttpRunnerError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn rejects_invalid_urls_before_sending() {
    let exec = executor(Duration::from_secs(1));
    let result = exec
        .spawn(resolve("GET {{baseUrl}}/x\n", "ftp://host"))
        .blocking_result();
    assert!(matches!(result, Err(HttpRunnerError::InvalidUrl { .. })));
    let result = exec
        .spawn(resolve("GET {{baseUrl}}\n", "not a url"))
        .blocking_result();
    assert!(matches!(result, Err(HttpRunnerError::InvalidUrl { .. })));
}

#[test]
fn reports_transport_errors() {
    // Bind then drop to get a port with nothing listening.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("port")
        .port();
    let result = executor(Duration::from_secs(5))
        .spawn(resolve(
            "GET {{baseUrl}}/\n",
            &format!("http://127.0.0.1:{port}"),
        ))
        .blocking_result();
    assert!(matches!(result, Err(HttpRunnerError::Transport { .. })));
}
