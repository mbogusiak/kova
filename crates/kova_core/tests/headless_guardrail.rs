//! Headless Core architecture guardrail (KOVA-003).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "test harness: failures must abort the test with context"
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command;

const UI_CRATES: [&str; 2] = ["kova_app", "kova_ui"];

/// Optional reference plugins (KOVA-048) and the SDK example plugin (KOVA-046):
/// WASM components outside the core.
const PLUGIN_CRATES: [&str; 5] = [
    "hello_kova",
    "kova-ai",
    "kova-database",
    "kova-kotlin-lsp",
    "kova-scala-metals",
];

const EXPECTED_MEMBERS: [&str; 18] = [
    "hello_kova",
    "kova-ai",
    "kova-database",
    "kova-kotlin-lsp",
    "kova-scala-metals",
    "kova_app",
    "kova_buffer",
    "kova_config",
    "kova_core",
    "kova_jvm_index",
    "kova_lsp_dap",
    "kova_plugin_host",
    "kova_plugin_rs",
    "kova_syntax",
    "kova_theme",
    "kova_toolchain",
    "kova_tools",
    "kova_ui",
];

const EXPECTED_HEADLESS: [&str; 16] = [
    "hello_kova",
    "kova-ai",
    "kova-database",
    "kova-kotlin-lsp",
    "kova-scala-metals",
    "kova_buffer",
    "kova_config",
    "kova_core",
    "kova_jvm_index",
    "kova_lsp_dap",
    "kova_plugin_host",
    "kova_plugin_rs",
    "kova_syntax",
    "kova_theme",
    "kova_toolchain",
    "kova_tools",
];

const FORBIDDEN_FAMILIES: [&str; 8] = [
    "kova_ui", "kova_app", "gpui", "winit", "metal", "cocoa", "wayland", "x11",
];

const FORBIDDEN_EXTRA: [&str; 2] = ["x11rb", "x11rb_protocol"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DepKind {
    Normal,
    Build,
    Dev,
}

#[derive(Debug)]
struct Graph {
    members: Vec<String>,
    names: HashMap<String, String>,
    edges: HashMap<String, Vec<(String, DepKind)>>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    root: String,
    first_kind: DepKind,
    path: Vec<String>,
}

fn normalize(name: &str) -> String {
    name.replace('-', "_")
}

fn is_forbidden(name: &str) -> bool {
    let norm = normalize(name);
    if FORBIDDEN_EXTRA.contains(&norm.as_str()) {
        return true;
    }
    FORBIDDEN_FAMILIES.iter().any(|family| {
        norm == *family
            || norm
                .strip_prefix(family)
                .is_some_and(|rest| rest.starts_with('_'))
    })
}

fn load_workspace_metadata() -> serde_json::Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let output = Command::new(&cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--all-features",
            "--locked",
            "--manifest-path",
            manifest_path,
        ])
        .output()
        .expect("failed to execute cargo metadata");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "cargo metadata failed with status {:?}:\n{stderr}",
            output.status.code()
        );
    }

    serde_json::from_slice(&output.stdout).expect("cargo metadata produced invalid JSON")
}

fn parse_kind(kind_val: &serde_json::Value) -> Result<DepKind, String> {
    match kind_val {
        serde_json::Value::Null => Ok(DepKind::Normal),
        serde_json::Value::String(s) if s == "build" => Ok(DepKind::Build),
        serde_json::Value::String(s) if s == "dev" => Ok(DepKind::Dev),
        other => Err(format!(
            "unsupported cargo metadata schema: unknown dep kind {other}"
        )),
    }
}

fn parse_graph(meta: &serde_json::Value) -> Result<Graph, String> {
    let members_arr = meta
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "unsupported cargo metadata schema: missing workspace_members".to_owned())?;
    let mut members = Vec::with_capacity(members_arr.len());
    for entry in members_arr {
        let id = entry.as_str().ok_or_else(|| {
            "unsupported cargo metadata schema: non-string workspace member".to_owned()
        })?;
        members.push(id.to_owned());
    }

    let packages_arr = meta
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "unsupported cargo metadata schema: missing packages".to_owned())?;
    let mut names = HashMap::with_capacity(packages_arr.len());
    for pkg in packages_arr {
        let id = pkg
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "unsupported cargo metadata schema: package missing id".to_owned())?;
        let name = pkg
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "unsupported cargo metadata schema: package missing name".to_owned())?;
        names.insert(id.to_owned(), name.to_owned());
    }

    let resolve_nodes = meta
        .get("resolve")
        .and_then(|r| r.get("nodes"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "unsupported cargo metadata schema: missing resolve".to_owned())?;

    let mut edges = HashMap::with_capacity(resolve_nodes.len());
    for node in resolve_nodes {
        let id = node
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                "unsupported cargo metadata schema: resolve node missing id".to_owned()
            })?;
        let deps_arr = node
            .get("deps")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                "unsupported cargo metadata schema: resolve node missing deps".to_owned()
            })?;

        let mut node_edges = Vec::new();
        for dep in deps_arr {
            let pkg_id = dep
                .get("pkg")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "unsupported cargo metadata schema: dep missing pkg".to_owned())?;
            let kinds_arr = dep
                .get("dep_kinds")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    "unsupported cargo metadata schema: dep missing dep_kinds".to_owned()
                })?;

            for dk in kinds_arr {
                let kind_val = dk.get("kind").unwrap_or(&serde_json::Value::Null);
                let kind = parse_kind(kind_val)?;
                node_edges.push((pkg_id.to_owned(), kind));
            }
        }

        node_edges.sort_by(|(id_a, kind_a), (id_b, kind_b)| {
            let name_a = names.get(id_a).map_or(id_a.as_str(), String::as_str);
            let name_b = names.get(id_b).map_or(id_b.as_str(), String::as_str);
            name_a.cmp(name_b).then_with(|| kind_a.cmp(kind_b))
        });
        node_edges.dedup();
        edges.insert(id.to_owned(), node_edges);
    }

    Ok(Graph {
        members,
        names,
        edges,
    })
}

fn headless_roots(graph: &Graph) -> Vec<String> {
    let mut roots: Vec<String> = graph
        .members
        .iter()
        .filter(|id| {
            let name = graph.names.get(*id).map_or("", String::as_str);
            !UI_CRATES.contains(&name)
        })
        .cloned()
        .collect();
    roots.sort_by(|a, b| {
        let na = graph.names.get(a).map_or(a.as_str(), String::as_str);
        let nb = graph.names.get(b).map_or(b.as_str(), String::as_str);
        na.cmp(nb)
    });
    roots
}

fn reconstruct_path(
    graph: &Graph,
    parent: &HashMap<String, String>,
    root_id: &str,
    last_non_forbidden: &str,
    forbidden_id: &str,
) -> Vec<String> {
    let mut reversed = Vec::new();
    reversed.push(
        graph
            .names
            .get(forbidden_id)
            .cloned()
            .unwrap_or_else(|| forbidden_id.to_owned()),
    );

    let mut cur = last_non_forbidden;
    loop {
        reversed.push(
            graph
                .names
                .get(cur)
                .cloned()
                .unwrap_or_else(|| cur.to_owned()),
        );
        if cur == root_id {
            break;
        }
        cur = parent.get(cur).map_or(root_id, String::as_str);
    }
    reversed.reverse();
    reversed
}

fn find_violations(graph: &Graph, roots: &[String]) -> Vec<Violation> {
    let mut violations = Vec::new();

    for root in roots {
        let root_name = graph
            .names
            .get(root)
            .cloned()
            .unwrap_or_else(|| root.clone());
        let mut queue: VecDeque<(String, usize, DepKind)> = VecDeque::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut parent: HashMap<String, String> = HashMap::new();

        visited.insert(root.clone());
        queue.push_back((root.clone(), 0, DepKind::Normal));

        while let Some((node_id, depth, branch_first_kind)) = queue.pop_front() {
            let Some(neighbors) = graph.edges.get(&node_id) else {
                continue;
            };

            for &(ref dep_id, edge_kind) in neighbors {
                if depth > 0 && edge_kind == DepKind::Dev {
                    continue;
                }

                let first_kind = if depth == 0 {
                    edge_kind
                } else {
                    branch_first_kind
                };

                let dep_name = graph
                    .names
                    .get(dep_id)
                    .map_or(dep_id.as_str(), String::as_str);

                if !visited.insert(dep_id.clone()) {
                    continue;
                }

                if is_forbidden(dep_name) {
                    let path = reconstruct_path(graph, &parent, root, &node_id, dep_id);
                    violations.push(Violation {
                        root: root_name.clone(),
                        first_kind,
                        path,
                    });
                    continue;
                }

                parent.insert(dep_id.clone(), node_id.clone());
                queue.push_back((dep_id.clone(), depth + 1, first_kind));
            }
        }
    }

    violations.sort();
    violations.dedup();
    violations
}

fn render_report(violations: &[Violation]) -> String {
    let plural = if violations.len() == 1 { "" } else { "s" };
    let mut out = format!(
        "Headless Core violated ({} path{plural}). Crates outside kova_app/kova_ui must not reach UI/windowing crates:\n",
        violations.len(),
    );
    for v in violations {
        out.push_str("  ");
        for (idx, segment) in v.path.iter().enumerate() {
            if idx > 0 {
                let arrow = if idx == 1 {
                    match v.first_kind {
                        DepKind::Normal => " -> ",
                        DepKind::Build => " -[build]-> ",
                        DepKind::Dev => " -[dev]-> ",
                    }
                } else {
                    " -> "
                };
                out.push_str(arrow);
            }
            out.push_str(segment);
        }
        out.push('\n');
    }
    out.push_str("See jvm_rust_ide_plan.md §2 and specs KOVA-003.");
    out
}

fn pkg_id(name: &str) -> String {
    format!("path+file:///fx/{name}#0.1.0")
}

fn fixture(members: &[&str], edges: &[(&str, &str, Option<&str>)]) -> serde_json::Value {
    let mut all_names: Vec<String> = members.iter().map(|s| (*s).to_owned()).collect();
    for (from, to, _) in edges {
        all_names.push((*from).to_owned());
        all_names.push((*to).to_owned());
    }
    all_names.sort();
    all_names.dedup();

    let member_ids: Vec<serde_json::Value> = members
        .iter()
        .map(|name| serde_json::Value::String(pkg_id(name)))
        .collect();

    let packages: Vec<serde_json::Value> = all_names
        .iter()
        .map(|name| {
            serde_json::json!({
                "id": pkg_id(name),
                "name": name,
            })
        })
        .collect();

    let mut grouped: HashMap<&str, Vec<(&str, Option<&str>)>> = HashMap::new();
    for &(from, to, kind) in edges {
        grouped.entry(from).or_default().push((to, kind));
    }

    let nodes: Vec<serde_json::Value> = all_names
        .iter()
        .map(|name| {
            let mut per_target: HashMap<&str, Vec<Option<&str>>> = HashMap::new();
            if let Some(list) = grouped.get(name.as_str()) {
                for &(to, kind) in list {
                    per_target.entry(to).or_default().push(kind);
                }
            }
            let deps: Vec<serde_json::Value> = per_target
                .into_iter()
                .map(|(to, kinds)| {
                    let dep_kinds: Vec<serde_json::Value> = kinds
                        .into_iter()
                        .map(|k| match k {
                            None => serde_json::json!({ "kind": null }),
                            Some(s) => serde_json::json!({ "kind": s }),
                        })
                        .collect();
                    serde_json::json!({
                        "pkg": pkg_id(to),
                        "dep_kinds": dep_kinds,
                    })
                })
                .collect();

            serde_json::json!({
                "id": pkg_id(name),
                "deps": deps,
            })
        })
        .collect();

    serde_json::json!({
        "workspace_members": member_ids,
        "packages": packages,
        "resolve": {
            "nodes": nodes,
        },
    })
}

#[test]
fn workspace_has_exactly_18_members() {
    let meta = load_workspace_metadata();
    let graph = parse_graph(&meta).expect("failed to parse workspace metadata");
    let mut member_names: Vec<&str> = graph
        .members
        .iter()
        .map(|id| graph.names.get(id).map_or("", String::as_str))
        .collect();
    member_names.sort_unstable();
    assert_eq!(member_names.len(), 18);
    assert_eq!(member_names, EXPECTED_MEMBERS);
}

#[test]
fn headless_set_is_exactly_the_expected_16() {
    let meta = load_workspace_metadata();
    let graph = parse_graph(&meta).expect("failed to parse workspace metadata");
    let roots = headless_roots(&graph);
    let mut root_names: Vec<&str> = roots
        .iter()
        .map(|id| graph.names.get(id).map_or("", String::as_str))
        .collect();
    root_names.sort_unstable();
    assert_eq!(root_names.len(), 16);
    assert_eq!(root_names, EXPECTED_HEADLESS);
}

/// Zero-Bloat Core (KOVA-048): no core, SDK or UI crate may depend on a
/// reference plugin, directly or transitively, through any dependency kind.
#[test]
fn core_and_ui_crates_never_depend_on_reference_plugins() {
    let meta = load_workspace_metadata();
    let graph = parse_graph(&meta).expect("failed to parse workspace metadata");
    let name_of = |id: &str| graph.names.get(id).map_or("", String::as_str).to_owned();

    let mut offenders = Vec::new();
    for member in &graph.members {
        let member_name = name_of(member);
        if PLUGIN_CRATES.contains(&member_name.as_str()) {
            continue;
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([member.clone()]);
        while let Some(current) = queue.pop_front() {
            for (dep, _) in graph.edges.get(&current).into_iter().flatten() {
                if !seen.insert(dep.clone()) {
                    continue;
                }
                let dep_name = name_of(dep);
                if PLUGIN_CRATES.contains(&dep_name.as_str()) {
                    offenders.push(format!("{member_name} -> {dep_name}"));
                }
                queue.push_back(dep.clone());
            }
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "core/UI crates must not depend on optional plugins:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn headless_crates_do_not_depend_on_ui_or_windowing() {
    let meta = load_workspace_metadata();
    let graph = parse_graph(&meta).expect("failed to parse workspace metadata");
    let roots = headless_roots(&graph);
    let violations = find_violations(&graph, &roots);
    assert!(violations.is_empty(), "\n{}", render_report(&violations));
}

#[test]
fn detects_direct_dependency() {
    let meta = fixture(&["kova_core"], &[("kova_core", "gpui", None)]);
    let graph = parse_graph(&meta).expect("valid fixture");
    let violations = find_violations(&graph, &headless_roots(&graph));
    assert_eq!(
        violations,
        vec![Violation {
            root: "kova_core".to_owned(),
            first_kind: DepKind::Normal,
            path: vec!["kova_core".to_owned(), "gpui".to_owned()],
        }]
    );
    assert!(render_report(&violations).contains("kova_core -> gpui"));
}

#[test]
fn detects_transitive_dependency() {
    let meta = fixture(
        &["kova_core", "kova_theme"],
        &[
            ("kova_core", "kova_theme", None),
            ("kova_theme", "foo", None),
            ("foo", "winit", None),
        ],
    );
    let graph = parse_graph(&meta).expect("valid fixture");
    let violations = find_violations(&graph, &headless_roots(&graph));
    assert_eq!(violations.len(), 2);
    assert_eq!(
        violations[0],
        Violation {
            root: "kova_core".to_owned(),
            first_kind: DepKind::Normal,
            path: vec![
                "kova_core".to_owned(),
                "kova_theme".to_owned(),
                "foo".to_owned(),
                "winit".to_owned(),
            ],
        }
    );
}

#[test]
fn detects_build_dependency() {
    let meta = fixture(
        &["kova_syntax"],
        &[("kova_syntax", "cocoa-foundation", Some("build"))],
    );
    let graph = parse_graph(&meta).expect("valid fixture");
    let violations = find_violations(&graph, &headless_roots(&graph));
    assert_eq!(
        violations,
        vec![Violation {
            root: "kova_syntax".to_owned(),
            first_kind: DepKind::Build,
            path: vec!["kova_syntax".to_owned(), "cocoa-foundation".to_owned()],
        }]
    );
    assert!(render_report(&violations).contains("kova_syntax -[build]-> cocoa-foundation"));
}

#[test]
fn detects_root_dev_dependency() {
    let meta = fixture(
        &["kova_buffer"],
        &[("kova_buffer", "wayland-client", Some("dev"))],
    );
    let graph = parse_graph(&meta).expect("valid fixture");
    let violations = find_violations(&graph, &headless_roots(&graph));
    assert_eq!(
        violations,
        vec![Violation {
            root: "kova_buffer".to_owned(),
            first_kind: DepKind::Dev,
            path: vec!["kova_buffer".to_owned(), "wayland-client".to_owned()],
        }]
    );
    assert!(render_report(&violations).contains("kova_buffer -[dev]-> wayland-client"));
}

#[test]
fn ignores_dev_dependency_of_transitive_dep() {
    let meta = fixture(
        &["kova_core"],
        &[("kova_core", "bar", None), ("bar", "x11", Some("dev"))],
    );
    let graph = parse_graph(&meta).expect("valid fixture");
    let violations = find_violations(&graph, &headless_roots(&graph));
    assert_eq!(violations, Vec::<Violation>::new());
}

#[test]
fn detects_ui_crate_dependency() {
    let meta = fixture(
        &["kova_config", "kova_ui"],
        &[("kova_config", "kova_ui", None)],
    );
    let graph = parse_graph(&meta).expect("valid fixture");
    let violations = find_violations(&graph, &headless_roots(&graph));
    assert_eq!(
        violations,
        vec![Violation {
            root: "kova_config".to_owned(),
            first_kind: DepKind::Normal,
            path: vec!["kova_config".to_owned(), "kova_ui".to_owned()],
        }]
    );
}

#[test]
fn ui_crates_are_exempt() {
    let meta = fixture(
        &["kova_ui", "kova_app"],
        &[("kova_ui", "gpui", None), ("kova_app", "winit", None)],
    );
    let graph = parse_graph(&meta).expect("valid fixture");
    let roots = headless_roots(&graph);
    assert_eq!(roots, Vec::<String>::new());
    let violations = find_violations(&graph, &roots);
    assert_eq!(violations, Vec::<Violation>::new());
}

#[test]
fn reports_shortest_path() {
    let meta = fixture(
        &["kova_core"],
        &[
            ("kova_core", "short_mid", None),
            ("short_mid", "metal", None),
            ("kova_core", "long_a", None),
            ("long_a", "long_b", None),
            ("long_b", "metal", None),
        ],
    );
    let graph = parse_graph(&meta).expect("valid fixture");
    let violations = find_violations(&graph, &headless_roots(&graph));
    assert_eq!(
        violations,
        vec![Violation {
            root: "kova_core".to_owned(),
            first_kind: DepKind::Normal,
            path: vec![
                "kova_core".to_owned(),
                "short_mid".to_owned(),
                "metal".to_owned(),
            ],
        }]
    );
}

#[test]
fn name_matching_rules() {
    let forbidden = [
        "gpui",
        "gpui_macros",
        "winit",
        "metal",
        "cocoa",
        "cocoa-foundation",
        "wayland-client",
        "wayland-sys",
        "wayland-backend",
        "x11",
        "x11-dl",
        "x11rb",
        "x11rb-protocol",
        "kova_ui",
        "kova_app",
        "kova_ui_widgets",
        "x11_like",
    ];
    for name in forbidden {
        assert!(is_forbidden(name), "expected `{name}` to be forbidden");
    }

    let allowed = [
        "metalink",
        "winit2",
        "kova_core",
        "serde_json",
        "ropey",
        "tokio",
    ];
    for name in allowed {
        assert!(!is_forbidden(name), "expected `{name}` to be allowed");
    }
}
