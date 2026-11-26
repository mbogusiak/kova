//! KOVA-037 integration tests: a mock `jdt.ls` on `tokio::io::duplex` drives diagnostics,
//! completion with auto-imports, inlay hints, hover, *Quick Fixes* (`Alt+Enter`) with
//! `codeAction/resolve` and a server `workspace/applyEdit`, *Rename* (`Shift+F6`) and
//! *Go to Definition* with Vineflower decompilation (mock `java`) and the
//! `java/classFileContents` fallback.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kova_buffer::{Utf16Point, Utf16Range};
use kova_lsp_dap::LspDapError;
use kova_lsp_dap::lsp::{
    ClassArchiveLocator, ClientRuntimeConfig, CompletionTrigger, DefinitionResolver,
    DefinitionTarget, DiagnosticSeverity, FileOperationEdit, InlayHintKind, JdtlsClient, LspEvent,
    apply_code_action, code_actions, completion, hover, inlay_hints, prepare_rename,
    read_lsp_frame, rename_symbol, resolve_completion_item, write_lsp_frame,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};

const STEP: Duration = Duration::from_secs(5);
const A_URI: &str = "file:///p/src/A.java";
const B_URI: &str = "file:///p/src/B.java";

struct MockServer {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

impl MockServer {
    async fn recv(&mut self) -> Value {
        let frame = tokio::time::timeout(STEP, read_lsp_frame(&mut self.reader))
            .await
            .expect("mock server timed out waiting for a frame")
            .expect("valid frame")
            .expect("stream open");
        serde_json::from_slice(&frame).expect("valid JSON")
    }

    /// Receives the next request and checks its method.
    async fn expect_request(&mut self, method: &str) -> Value {
        let request = self.recv().await;
        assert_eq!(request["method"], method, "{request}");
        request
    }

    async fn send(&mut self, message: Value) {
        let bytes = serde_json::to_vec(&message).unwrap();
        write_lsp_frame(&mut self.writer, &bytes).await.unwrap();
    }

    async fn respond(&mut self, request: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
            .await;
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    async fn server_request(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        let response = self.recv().await;
        assert_eq!(response["id"], id, "response to {method}");
        response
    }
}

async fn connect() -> (JdtlsClient, MockServer) {
    let (client, server) = tokio::io::duplex(256 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let (client_read, client_write) = tokio::io::split(client);
    let mut server = MockServer {
        reader: BufReader::new(server_read),
        writer: server_write,
    };
    let mut config = ClientRuntimeConfig::new(std::env::temp_dir().join("kova-037"));
    config.initialize_timeout = STEP;
    config.request_timeout = STEP;
    let handshake = async {
        let init = server.expect_request("initialize").await;
        server
            .respond(&init, json!({ "capabilities": { "textDocumentSync": 2 } }))
            .await;
        let initialized = server.recv().await;
        assert_eq!(initialized["method"], "initialized");
    };
    let (client, ()) = tokio::join!(
        JdtlsClient::start_with_streams(client_read, client_write, config),
        handshake
    );
    (client.unwrap(), server)
}

fn range(sr: u32, sc: u32, er: u32, ec: u32) -> Value {
    json!({ "start": { "line": sr, "character": sc }, "end": { "line": er, "character": ec } })
}

fn text_edit(sr: u32, sc: u32, er: u32, ec: u32, text: &str) -> Value {
    json!({ "range": range(sr, sc, er, ec), "newText": text })
}

fn utf16(sr: usize, sc: usize, er: usize, ec: usize) -> Utf16Range {
    Utf16Range::new(Utf16Point::new(sr, sc), Utf16Point::new(er, ec))
}

async fn publish_and_wait(client: &JdtlsClient, server: &mut MockServer, params: Value) {
    let mut events = client.subscribe_events();
    server
        .notify("textDocument/publishDiagnostics", params)
        .await;
    tokio::time::timeout(STEP, async {
        loop {
            if let Ok(LspEvent::DiagnosticsReceived { .. }) = events.recv().await {
                break;
            }
        }
    })
    .await
    .expect("diagnostics event");
}

fn unresolved_symbol() -> Value {
    json!({
        "range": range(4, 8, 4, 12),
        "severity": 1,
        "code": "16777218",
        "source": "Java",
        "message": "List cannot be resolved to a type",
    })
}

// ---------------------------------------------------------------------------
// AC-1: diagnostics store and snapshots
// ---------------------------------------------------------------------------

#[tokio::test]
async fn diagnostics_store_tracks_publications_and_round_trips_snapshots() {
    let (client, mut server) = connect().await;
    publish_and_wait(
        &client,
        &mut server,
        json!({
            "uri": A_URI,
            "version": 3,
            "diagnostics": [
                unresolved_symbol(),
                { "range": range(9, 4, 9, 9), "severity": 2, "message": "unused", "tags": [1] },
            ],
        }),
    )
    .await;
    publish_and_wait(
        &client,
        &mut server,
        json!({ "uri": B_URI, "diagnostics": [{ "range": range(0, 0, 0, 1), "severity": 1, "message": "x" }] }),
    )
    .await;
    // A stale publication (older version) is ignored.
    publish_and_wait(
        &client,
        &mut server,
        json!({ "uri": A_URI, "version": 2, "diagnostics": [] }),
    )
    .await;

    let store = client.diagnostics();
    let summary = store.summary();
    assert_eq!(summary.total_errors, 2);
    assert_eq!(summary.total_warnings, 1);
    assert_eq!(summary.files_with_diagnostics, 2);

    // The cursor right after `List` still offers the quick fix.
    let at_cursor = store.diagnostics_at(A_URI, Utf16Point::new(4, 12));
    assert_eq!(at_cursor.len(), 1);
    assert_eq!(at_cursor[0].severity, DiagnosticSeverity::Error);
    assert_eq!(at_cursor[0].code.as_deref(), Some("16777218"));
    assert_eq!(
        store.diagnostics_at(A_URI, Utf16Point::new(5, 0)),
        Vec::new()
    );

    let snapshot = store.export_snapshot(Path::new("/p"));
    assert_eq!(snapshot.files.len(), 2);
    assert_eq!(snapshot.files[0].uri, A_URI);
    let json = serde_json::to_string(&snapshot).unwrap();
    store.clear_all();
    assert_eq!(store.summary().files_with_diagnostics, 0);
    store.restore_snapshot(serde_json::from_str(&json).unwrap());
    assert_eq!(store.summary(), summary);
    assert_eq!(
        store.export_snapshot(Path::new("/p")).files,
        snapshot
            .files
            .iter()
            .map(|f| {
                let mut f = f.clone();
                f.version = None;
                f
            })
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// AC-2: completion with auto-imports, inlay hints and hover
// ---------------------------------------------------------------------------

#[tokio::test]
async fn completion_maps_auto_imports_and_inlay_hints_cover_line_range() {
    let (client, mut server) = connect().await;
    let item = json!({
        "label": "List<E> - java.util",
        "kind": 8,
        "sortText": "001",
        "insertTextFormat": 2,
        "textEdit": { "range": range(4, 8, 4, 10), "newText": "List<${1:E}>" },
        "data": { "id": 7 },
    });
    let (result, ()) = tokio::join!(
        completion(
            &client,
            A_URI,
            Utf16Point::new(4, 10),
            CompletionTrigger::TriggerCharacter('.')
        ),
        async {
            let request = server.expect_request("textDocument/completion").await;
            assert_eq!(
                request["params"]["position"],
                json!({ "line": 4, "character": 10 })
            );
            assert_eq!(request["params"]["context"]["triggerKind"], json!(2));
            assert_eq!(request["params"]["context"]["triggerCharacter"], json!("."));
            server
                .respond(&request, json!({ "isIncomplete": true, "items": [item] }))
                .await;
        }
    );
    let result = result.unwrap();
    assert!(result.is_incomplete);
    let candidate = &result.candidates[0];
    assert!(candidate.is_snippet);
    assert_eq!(
        candidate.primary_edit.as_ref().map(|e| e.range),
        Some(utf16(4, 8, 4, 10))
    );
    assert_eq!(candidate.additional_text_edits, Vec::new());

    let (resolved, ()) = tokio::join!(resolve_completion_item(&client, candidate), async {
        let request = server.expect_request("completionItem/resolve").await;
        assert_eq!(request["params"]["data"], json!({ "id": 7 }));
        let mut resolved = request["params"].clone();
        resolved["additionalTextEdits"] =
            json!([text_edit(1, 0, 1, 0, "import java.util.List;\n")]);
        resolved["documentation"] =
            json!({ "kind": "markdown", "value": "An ordered collection." });
        server.respond(&request, resolved).await;
    });
    let resolved = resolved.unwrap();
    assert_eq!(resolved.additional_text_edits.len(), 1);
    assert_eq!(
        resolved.additional_text_edits[0].new_text,
        "import java.util.List;\n"
    );
    assert_eq!(resolved.additional_text_edits[0].range, utf16(1, 0, 1, 0));
    assert_eq!(
        resolved.documentation.as_deref(),
        Some("An ordered collection.")
    );

    let (hints, ()) = tokio::join!(inlay_hints(&client, A_URI, 10, 12), async {
        let request = server.expect_request("textDocument/inlayHint").await;
        assert_eq!(request["params"]["range"], range(10, 0, 13, 0));
        server
            .respond(
                &request,
                json!([
                    { "position": { "line": 10, "character": 20 }, "label": "name:", "kind": 2, "paddingRight": true },
                    { "position": { "line": 11, "character": 7 }, "label": [{ "value": ": " }, { "value": "Map<String, Integer>" }], "kind": 1 },
                ]),
            )
            .await;
    });
    let hints = hints.unwrap();
    assert_eq!(hints.len(), 2);
    assert_eq!(hints[0].kind, InlayHintKind::Parameter);
    assert!(hints[0].padding_right);
    assert_eq!(hints[1].kind, InlayHintKind::Type);
    assert_eq!(hints[1].label, ": Map<String, Integer>");
    assert!(matches!(
        inlay_hints(&client, A_URI, 5, 4).await,
        Err(LspDapError::InvalidArgument { .. })
    ));

    let (info, ()) = tokio::join!(hover(&client, A_URI, Utf16Point::new(4, 9)), async {
        let request = server.expect_request("textDocument/hover").await;
        server
            .respond(
                &request,
                json!({ "contents": [{ "language": "java", "value": "java.util.List<E>" }, "Javadoc"], "range": range(4, 8, 4, 12) }),
            )
            .await;
    });
    let info = info.unwrap().expect("hover");
    assert_eq!(info.contents, "```java\njava.util.List<E>\n```\n\nJavadoc");
    assert_eq!(info.range, Some(utf16(4, 8, 4, 12)));
}

// ---------------------------------------------------------------------------
// AC-3: Alt+Enter quick fixes and Shift+F6 rename
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quick_fix_resolves_executes_and_collects_server_apply_edit() {
    let (client, mut server) = connect().await;
    publish_and_wait(
        &client,
        &mut server,
        json!({ "uri": A_URI, "diagnostics": [unresolved_symbol()] }),
    )
    .await;

    let (choices, ()) = tokio::join!(
        code_actions(&client, client.diagnostics(), A_URI, utf16(4, 10, 4, 10)),
        async {
            let request = server.expect_request("textDocument/codeAction").await;
            let sent = &request["params"]["context"]["diagnostics"];
            assert_eq!(sent.as_array().map(Vec::len), Some(1));
            assert_eq!(sent[0]["message"], "List cannot be resolved to a type");
            server
                .respond(
                    &request,
                    json!([
                        { "title": "Organize imports", "command": "java.action.organizeImports", "arguments": [A_URI] },
                        {
                            "title": "Import 'List' (java.util)",
                            "kind": "quickfix",
                            "isPreferred": true,
                            "data": { "rid": 1 },
                        },
                    ]),
                )
                .await;
        }
    );
    let choices = choices.unwrap();
    assert_eq!(choices.len(), 2);
    assert!(choices[0].is_preferred, "preferred quick fix comes first");
    assert_eq!(choices[0].kind.as_deref(), Some("quickfix"));
    assert!(choices[1].is_command_only);

    let preferred = choices[0].clone();
    let (outcome, ()) = tokio::join!(apply_code_action(&client, preferred), async {
        let resolve = server.expect_request("codeAction/resolve").await;
        assert_eq!(resolve["params"]["data"], json!({ "rid": 1 }));
        let mut resolved = resolve["params"].clone();
        resolved["edit"] = json!({
            "documentChanges": [
                { "textDocument": { "uri": A_URI, "version": 4 },
                  "edits": [text_edit(4, 8, 4, 12, "List"), text_edit(1, 0, 1, 0, "import java.util.List;\n")] },
                { "textDocument": { "uri": B_URI, "version": 1 }, "edits": [text_edit(0, 0, 0, 0, "// touched\n")] },
            ]
        });
        resolved["command"] =
            json!({ "title": "Format", "command": "java.edit.format", "arguments": [A_URI] });
        server.respond(&resolve, resolved).await;

        let execute = server.expect_request("workspace/executeCommand").await;
        assert_eq!(execute["params"]["command"], "java.edit.format");
        // jdt.ls applies the command's changes through a server request and waits for it.
        let apply = server
            .server_request(
                json!("apply-1"),
                "workspace/applyEdit",
                json!({ "label": "Format", "edit": { "changes": { A_URI: [text_edit(9, 0, 9, 4, "")] } } }),
            )
            .await;
        assert_eq!(apply["result"]["applied"], json!(true));
        server.respond(&execute, Value::Null).await;
    });
    let outcome = outcome.unwrap();
    assert_eq!(
        outcome.executed_command.as_deref(),
        Some("java.edit.format")
    );
    let edit = &outcome.edit;
    assert_eq!(edit.text_edits.len(), 2);
    let a = edit.edits_for(Path::new("/p/src/A.java")).expect("A edits");
    assert_eq!(a.expected_version, Some(4));
    let starts: Vec<_> = a.edits.iter().map(|e| e.range.start.row).collect();
    assert_eq!(starts, [9, 4, 1], "edits sorted from the end of the file");
    assert!(edit.edits_for(Path::new("/p/src/B.java")).is_some());

    // Outside apply_code_action, server edits are refused (nothing would apply them).
    let idle = server
        .server_request(
            json!("apply-2"),
            "workspace/applyEdit",
            json!({ "edit": { "changes": { A_URI: [text_edit(0, 0, 0, 0, "x")] } } }),
        )
        .await;
    assert_eq!(idle["result"]["applied"], json!(false));
}

#[tokio::test]
async fn rename_spans_files_and_refuses_read_only_documents() {
    let (client, mut server) = connect().await;
    let (prepared, ()) = tokio::join!(
        prepare_rename(&client, A_URI, Utf16Point::new(2, 15)),
        async {
            let request = server.expect_request("textDocument/prepareRename").await;
            server
                .respond(
                    &request,
                    json!({ "range": range(2, 13, 2, 18), "placeholder": "total" }),
                )
                .await;
        }
    );
    let prepared = prepared.unwrap().expect("renamable");
    assert_eq!(prepared.range, Some(utf16(2, 13, 2, 18)));
    assert_eq!(prepared.placeholder.as_deref(), Some("total"));

    let (edit, ()) = tokio::join!(
        rename_symbol(&client, A_URI, Utf16Point::new(2, 15), "grandTotal"),
        async {
            let request = server.expect_request("textDocument/rename").await;
            assert_eq!(request["params"]["newName"], "grandTotal");
            server
                .respond(
                    &request,
                    json!({
                        "documentChanges": [
                            { "textDocument": { "uri": B_URI, "version": 2 },
                              "edits": [text_edit(7, 4, 7, 9, "grandTotal"), text_edit(3, 4, 3, 9, "grandTotal")] },
                            { "textDocument": { "uri": A_URI, "version": 5 },
                              "edits": [text_edit(2, 13, 2, 18, "grandTotal"), text_edit(8, 1, 8, 6, "grandTotal")] },
                            { "kind": "rename", "oldUri": "file:///p/src/Old.java", "newUri": "file:///p/src/New.java" },
                        ]
                    }),
                )
                .await;
        }
    );
    let edit = edit.unwrap();
    let paths: Vec<_> = edit
        .text_edits
        .iter()
        .map(|f| f.file_path.clone())
        .collect();
    assert_eq!(
        paths,
        [
            PathBuf::from("/p/src/A.java"),
            PathBuf::from("/p/src/B.java")
        ]
    );
    for file in &edit.text_edits {
        let rows: Vec<_> = file.edits.iter().map(|e| e.range.start.row).collect();
        let mut sorted = rows.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(
            rows,
            sorted,
            "descending order in {}",
            file.file_path.display()
        );
    }
    assert!(matches!(
        edit.file_ops.as_slice(),
        [FileOperationEdit::RenameFile { new_path, .. }] if new_path == Path::new("/p/src/New.java")
    ));

    // Read-only and invalid requests never reach the server.
    let jdt = "jdt://contents/rt.jar/java.lang/String.class";
    assert!(matches!(
        rename_symbol(&client, jdt, Utf16Point::new(0, 0), "Str").await,
        Err(LspDapError::ReadOnlyDocument { .. })
    ));
    assert_eq!(
        prepare_rename(&client, jdt, Utf16Point::new(0, 0)).await,
        Ok(None)
    );
    assert!(matches!(
        rename_symbol(&client, A_URI, Utf16Point::new(0, 0), "bad name").await,
        Err(LspDapError::InvalidArgument { .. })
    ));
    assert!(matches!(
        rename_symbol(&client, A_URI, Utf16Point::new(0, 0), "").await,
        Err(LspDapError::InvalidArgument { .. })
    ));
    assert_eq!(client.session().pending_requests(), 0);
}

// ---------------------------------------------------------------------------
// AC-4: Go to Definition with Vineflower and the classFileContents fallback
// ---------------------------------------------------------------------------

/// Mock `java`: treats the last two args as input/output dirs and writes one `.java` per
/// top-level class (same contract as the KOVA-035 tests).
#[cfg(unix)]
const MOCK_VINEFLOWER: &str = r#"#!/bin/sh
echo run >> "$(dirname "$0")/invocations"
in_dir=""; out_dir=""
for a in "$@"; do in_dir="$out_dir"; out_dir="$a"; done
cd "$in_dir" || exit 3
find . -name '*.class' ! -name '*$*' | while read -r f; do
  rel="${f#./}"; dir=$(dirname "$rel"); base=$(basename "$rel" .class)
  mkdir -p "$out_dir/$dir"
  {
    if [ "$dir" != "." ]; then echo "package $(echo "$dir" | tr / .);"; fi
    echo ""
    echo "public class $base {"
    echo "}"
  } > "$out_dir/$dir/$base.java"
done
"#;

#[cfg(unix)]
fn fake_jdk(home: &Path) -> kova_toolchain::JdkInstallation {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let java_bin = bin.join("java");
    fs::write(&java_bin, MOCK_VINEFLOWER).unwrap();
    fs::set_permissions(&java_bin, fs::Permissions::from_mode(0o755)).unwrap();
    kova_toolchain::JdkInstallation {
        home_dir: home.to_path_buf(),
        java_bin,
        jmods_dir: None,
        version: kova_toolchain::JavaVersion::new(21, 0, 5, Some(11)),
        vendor: kova_toolchain::JdkVendor::Temurin,
        arch: "aarch64".to_owned(),
        source: kova_toolchain::JdkSource::Explicit,
    }
}

fn write_jar(path: &Path, entries: &[&str]) {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for name in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"\xca\xfe\xba\xbe").unwrap();
    }
    fs::write(path, zip.finish().unwrap().into_inner()).unwrap();
}

fn listing(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

async fn definition_round(
    client: &JdtlsClient,
    server: &mut MockServer,
    resolver: &DefinitionResolver,
    location: Value,
) -> Result<Vec<DefinitionTarget>, LspDapError> {
    let (targets, ()) = tokio::join!(
        resolver.go_to_definition(client, A_URI, Utf16Point::new(6, 12)),
        async {
            let request = server.expect_request("textDocument/definition").await;
            assert_eq!(
                request["params"]["position"],
                json!({ "line": 6, "character": 12 })
            );
            server.respond(&request, location).await;
        }
    );
    targets
}

#[cfg(unix)]
#[tokio::test]
async fn go_to_definition_decompiles_jar_classes_then_serves_the_cache() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(project.join("libs")).unwrap();
    let jar = project.join("libs").join("kova lib.jar");
    write_jar(
        &jar,
        &[
            "dev/kova/Lib.class",
            "dev/kova/Lib$Inner.class",
            "dev/kova/Other.class",
        ],
    );
    let before = listing(&project);

    let jdk = fake_jdk(&tmp.path().join("jdk"));
    let invocations = jdk.home_dir.join("bin").join("invocations");
    let vineflower_jar = tmp.path().join("vineflower.jar");
    fs::write(&vineflower_jar, b"vf").unwrap();
    let cache_root = tmp.path().join("cache").join("kova").join("decompiled");
    let decompiler =
        kova_toolchain::VineflowerDecompiler::new(vineflower_jar, jdk, cache_root.clone());
    let settings = kova_config::JvmSettings::default();
    assert!(settings.auto_decompile_class);
    let resolver = DefinitionResolver::from_settings(&settings, decompiler)
        .with_locator(Arc::new(JarLocator(jar.clone())));

    let (client, mut server) = connect().await;
    let jar_uri = format!(
        "jar:{}!/dev/kova/Lib.class",
        url::Url::from_file_path(&jar).unwrap()
    );
    let location = json!({ "uri": jar_uri, "range": range(40, 2, 40, 5) });

    let first = definition_round(&client, &mut server, &resolver, location.clone())
        .await
        .unwrap();
    let [
        DefinitionTarget::DecompiledClass {
            fqcn,
            archive_path,
            cache_file,
            source_text,
            range: target_range,
            from_cache,
            read_only,
        },
    ] = first.as_slice()
    else {
        panic!("unexpected {first:?}");
    };
    assert_eq!(fqcn, "dev.kova.Lib");
    assert_eq!(archive_path.as_deref(), Some(jar.as_path()));
    assert!(cache_file.starts_with(&cache_root));
    assert!(cache_file.ends_with("dev/kova/Lib.java"));
    assert!(source_text.contains("public class Lib {"));
    // jdt.ls' bytecode range is meaningless in Vineflower output: the declaration is used.
    assert_eq!(*target_range, utf16(2, 13, 2, 16));
    assert!(!from_cache);
    assert!(read_only);
    assert_eq!(fs::read_to_string(&invocations).unwrap().lines().count(), 1);

    let second = definition_round(&client, &mut server, &resolver, location)
        .await
        .unwrap();
    assert!(matches!(
        second.as_slice(),
        [DefinitionTarget::DecompiledClass { from_cache: true, read_only: true, cache_file: c, .. }]
            if c == cache_file
    ));
    assert_eq!(
        fs::read_to_string(&invocations).unwrap().lines().count(),
        1,
        "cache hit must not start the JVM"
    );
    assert_eq!(
        listing(&project),
        before,
        "nothing written into the project"
    );

    // jdt:// whose JDT memento query names the physical jar: Vineflower, no classFileContents.
    let escaped_jar = jar
        .display()
        .to_string()
        .replace('/', "%5C/")
        .replace(' ', "%20");
    let jdt_lib = format!(
        "jdt://contents/kova%20lib.jar/dev.kova/Lib.class?=demo/{escaped_jar}%3Cdev.kova(Lib.class"
    );
    let via_memento = definition_round(
        &client,
        &mut server,
        &resolver,
        json!({ "uri": jdt_lib, "range": range(0, 0, 0, 0) }),
    )
    .await
    .unwrap();
    assert!(matches!(
        via_memento.as_slice(),
        [DefinitionTarget::DecompiledClass { archive_path: Some(a), from_cache: true, read_only: true, .. }]
            if a == &jar
    ));

    // jdt:// with an unreachable archive: the ClassArchiveLocator finds the jar (cache miss).
    let jdt_other = "jdt://contents/other.jar/dev.kova/Other.class?=demo/%5C/missing%5C/other.jar%3Cdev.kova(Other.class";
    let via_locator = definition_round(
        &client,
        &mut server,
        &resolver,
        json!({ "uri": jdt_other, "range": range(0, 0, 0, 0) }),
    )
    .await
    .unwrap();
    assert!(matches!(
        via_locator.as_slice(),
        [DefinitionTarget::DecompiledClass { fqcn, from_cache: false, cache_file, .. }]
            if fqcn == "dev.kova.Other" && cache_file.starts_with(&cache_root)
    ));
    assert_eq!(fs::read_to_string(&invocations).unwrap().lines().count(), 2);

    // jdt:// attached sources (.java): served by jdt.ls, Vineflower is never started.
    let jdt_sources = "jdt://contents/kova%20lib.jar/dev.kova/Lib.java";
    let (sources, ()) = tokio::join!(
        resolver.go_to_definition(&client, A_URI, Utf16Point::new(6, 12)),
        async {
            let request = server.expect_request("textDocument/definition").await;
            server
                .respond(
                    &request,
                    json!({ "uri": jdt_sources, "range": range(5, 13, 5, 16) }),
                )
                .await;
            let contents = server.expect_request("java/classFileContents").await;
            server
                .respond(
                    &contents,
                    json!(
                        "package dev.kova;
/** docs */
public class Lib {}
"
                    ),
                )
                .await;
        }
    );
    let sources = sources.unwrap();
    assert!(matches!(
        sources.as_slice(),
        [DefinitionTarget::SourceFile { path, read_only: true, range: r }]
            if path.starts_with(&cache_root) && *r == utf16(5, 13, 5, 16)
    ));
    assert_eq!(fs::read_to_string(&invocations).unwrap().lines().count(), 2);
    assert_eq!(listing(&project), before);
}

/// Test locator resolving every FQCN to an entry of one jar.
struct JarLocator(PathBuf);

impl ClassArchiveLocator for JarLocator {
    fn resolve_fqcn_archive(&self, fqcn: &str) -> Option<kova_toolchain::DecompileTarget> {
        Some(kova_toolchain::DecompileTarget::ArchiveEntry {
            archive_path: self.0.clone(),
            entry_name: format!("{}.class", fqcn.replace('.', "/")),
        })
    }
}

#[tokio::test]
async fn jdt_uri_without_archive_falls_back_to_class_file_contents() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join("decompiled");
    let resolver = DefinitionResolver::new(&cache_root, true);
    let (client, mut server) = connect().await;
    let jdt = "jdt://contents/guava-33.0.jar/com.google.common.base/Strings.class?=demo/%5C/missing%5C/guava-33.0.jar%3Ccom.google.common.base(Strings.class";
    let location = json!([{ "uri": jdt, "range": range(30, 21, 30, 28) }]);

    let (first, ()) = tokio::join!(
        resolver.go_to_definition(&client, A_URI, Utf16Point::new(6, 12)),
        async {
            let request = server.expect_request("textDocument/definition").await;
            server.respond(&request, location.clone()).await;
            let contents = server.expect_request("java/classFileContents").await;
            assert_eq!(contents["params"]["uri"], jdt);
            server
                .respond(
                    &contents,
                    json!("package com.google.common.base;\npublic final class Strings {}\n"),
                )
                .await;
        }
    );
    let first = first.unwrap();
    let [
        DefinitionTarget::DecompiledClass {
            fqcn,
            cache_file,
            range: target_range,
            from_cache: false,
            read_only: true,
            ..
        },
    ] = first.as_slice()
    else {
        panic!("unexpected {first:?}");
    };
    assert_eq!(fqcn, "com.google.common.base.Strings");
    assert!(cache_file.starts_with(&cache_root));
    // jdt.ls' own range is valid in its own class contents.
    assert_eq!(*target_range, utf16(30, 21, 30, 28));

    // Second jump: served from the cache, no `java/classFileContents` round-trip.
    let second = definition_round(&client, &mut server, &resolver, location)
        .await
        .unwrap();
    assert!(matches!(
        second.as_slice(),
        [DefinitionTarget::DecompiledClass {
            from_cache: true,
            ..
        }]
    ));

    // A project file resolves to an editable source file.
    let local = definition_round(
        &client,
        &mut server,
        &resolver,
        json!({ "uri": B_URI, "range": range(3, 4, 3, 9) }),
    )
    .await
    .unwrap();
    assert_eq!(
        local,
        [DefinitionTarget::SourceFile {
            path: PathBuf::from("/p/src/B.java"),
            range: utf16(3, 4, 3, 9),
            read_only: false,
        }]
    );

    // With auto-decompilation disabled, a bare jar entry is a controlled error.
    let disabled = DefinitionResolver::new(&cache_root, false);
    let error = definition_round(
        &client,
        &mut server,
        &disabled,
        json!({ "uri": "jar:file:///nowhere/x.jar!/a/B.class", "range": range(0, 0, 0, 0) }),
    )
    .await;
    assert!(matches!(
        error,
        Err(LspDapError::DecompilationFailed { .. })
    ));
}
