# Kova IDE

**Kova** is a native, ultra-lightweight, GPU-accelerated IDE for the JVM ecosystem (Java, Kotlin, Scala), built from the ground up in **Rust** with **GPUI**.

It combines the ergonomics and visual clarity of **IntelliJ IDEA (New UI)** with the uncompromising **Omarchy** philosophy: opinionated *Omakase* defaults, keyboard-first workflows, zero dot-folder clutter in your repositories, and a 100% telemetry- and bloat-free core.

---

## Vision & Core Principles

- **Hybrid JVM Intelligence:** Instant project navigation powered by native Rust indexing (`tree-sitter` + `.jar`/`.class` bytecode parsing via `cafebabe` + `nucleo` fuzzy matching), backed by lazily spawned `jdt.ls` (with automatic `lombok.jar` wiring and idle JVM hibernation).
- **IntelliJ × Omarchy UX:** Dockable *IntelliJ New UI* tool windows alongside a single-shortcut **Focus / Tiling** mode, JetBrains default keymap, and first-class modal **Vim** emulation.
- **JVM Omakase Out of the Box:** Native Git (`gix`), graphical JUnit runner, DAP/JDWP debugger, on-demand `.class` decompilation (**Vineflower**), and a built-in `.http` request runner.
- **Zero-WebView WASM Extensibility:** Sandboxed plugins (`wasmtime` + WIT) rendering native GPUI widgets (`Tree`, virtualized `Table`, `Form`, `CodeBlock`) with official SDKs for **Rust** and **Kotlin/Wasm**. AI, databases, and cloud tooling live strictly in optional WASM plugins—never bloating the core.

---

## Architecture: Headless Core + GPUI

Kova strictly separates deterministic IDE state from GPU rendering across a 17-crate Cargo workspace (11 headless core crates + 4 headless reference plugins + 2 GPU presentation crates):

- **Headless Core (`kova_core`, `kova_config`, `kova_theme`, `kova_buffer`, `kova_syntax`, `kova_jvm_index`, `kova_toolchain`, `kova_lsp_dap`, `kova_tools`, `kova_plugin_host`, `kova_plugin_rs` in `sdk/kova_plugin_rs`):** These 11 crates (and the 4 reference plugins plus the SDK example below: 16 headless crates outside `kova_ui` and `kova_app`) compile and pass integration tests without initializing a graphics context or window. This boundary is automatically enforced by [crates/kova_core/tests/headless_guardrail.rs](crates/kova_core/tests/headless_guardrail.rs). Text buffers use `ropey::Rope` with $O(1)$ `Arc` snapshots to feed `rayon` CPU workers, `tokio` async LSP/DAP/BSP bridges, semantic Aether theme tokens, and `wasmtime` plugins lock-free.
- **Reference plugins (`plugins/kova-ai`, `plugins/kova-database`, `plugins/kova-kotlin-lsp`, `plugins/kova-scala-metals`):** Optional WASM components built with `kova_plugin_rs` (`cargo build --target wasm32-wasip2 -p <plugin>`), each with its own `plugin.toml`. They keep AI, database clients and extra languages out of the core: the guardrail fails if any core or UI crate depends on a plugin crate, and `crates/kova_plugin_host/tests/reference_plugins.rs` loads every plugin into the sandbox and checks its widget tree.
- **SDK example (`sdk/kova_plugin_rs/examples/hello_kova`):** Minimal plugin written against the `KovaPlugin` trait of `kova_plugin_rs` that uses every widget kind. `sdk/kova_plugin_rs/tests/hello_kova_host.rs` builds it for `wasm32-wasip2` and loads it through `PluginManager`. See [sdk/kova_plugin_rs/README.md](sdk/kova_plugin_rs/README.md).
- **GPU Presentation (`kova_ui`, `kova_app`):** 120 FPS hardware-accelerated rendering via **GPUI** on **macOS (Metal)** and **Linux (Wayland/X11 via Vulkan)**.

See [jvm_rust_ide_plan.md](jvm_rust_ide_plan.md) for the complete architectural specification and roadmap.

---

## Performance Budgets

| Metric | Target Budget |
| :--- | :--- |
| **Cold window startup to edit-ready** | `< 120 ms` |
| **Kova process memory footprint** (Rust + GPUI + local indexes) | `< 150 MB` |
| **Keystroke input latency** (zero disk/LSP blocking on render thread) | `< 5 ms` |
| ***Go to Class* readiness after opening a project** | `< 1.5 s` |

Inactive background projects automatically hibernate their `jdt.ls` JVM process to reclaim system RAM and wake transparently upon return.

---

## Omarchy Configuration Layout

Kova never pollutes your project repositories with hidden `.idea/` directories. Configuration and state follow clean XDG / dotfile conventions (with comment-preserving two-way GUI sync via `toml_edit`):

```text
~/.config/kova/           # Human-readable, dotfile-friendly configuration
├── settings.toml         # Core preferences (synced two-way with Cmd+, GUI)
├── keymap.toml           # Keybinding overrides
└── themes/               # TOML color themes (e.g., kova-islands-dark.toml)

~/.local/share/kova/      # Managed toolchains & extensions
├── omakase/              # Managed JDKs, jdt.ls + lombok.jar, java-debug, Vineflower
└── plugins/              # Sandboxed .wasm extensions (Rust & Kotlin/Wasm)

~/.cache/kova/            # Ephemeral state outside project repositories
└── workspaces/<hash>/    # Open tabs, history, and binary .jar symbol caches
```

Projects remain completely clean; teams may optionally commit a single, explicit `kova.toml` file for shared run/build configurations.

---

## Development

Run the workspace quality gate before opening any PR:

```bash
./scripts/verify-workspace.sh
```

This single command verifies formatting (`cargo fmt`), strict linting (`clippy -D warnings`), all workspace tests, the `Headless Core` dependency guardrail (`crates/kova_core/tests/headless_guardrail.rs`), UI-excluded headless test execution, and documentation (`cargo doc`).

---

## License

Licensed under either of:

- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- **MIT License** ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
