# kova_plugin_rs — Rust SDK for Kova plugins

`kova_plugin_rs` is the official Rust SDK for writing Kova IDE plugins. A plugin is a WebAssembly component that runs in the `kova_plugin_host` sandbox. It never draws pixels: it returns a tree of semantic widgets, and Kova renders that tree natively with its own theme.

The SDK compiles the canonical contract `crates/kova_plugin_host/wit/kova-plugin.wit` (`kova:plugin@0.1.0`) with `wit-bindgen`. It does not keep its own copy, so an SDK build always matches the host.

| Item | What it does |
| :--- | :--- |
| `KovaPlugin` | Stateful trait. `render_ui` is the only required method. `commands`, `tool_windows`, `init`, `on_ui_event`, `on_command`, `on_event` and `shutdown` have defaults. |
| `export_plugin!(MyPlugin)` | Exports the type as the component entrypoint. It accepts a `KovaPlugin` or a hand-written `Guest`. |
| `ui` | One constructor per widget kind: `split`, `tabs`, `tree`, `table`, `list`, `form`, `text_input`, `select`, `toggle`, `button`, `code_block`, `markdown`, `badge`. Also `Element` (fluent, nested), `UiBuilder` (arena) and `validate`. |
| `StyleVariant` | Semantic style: `Primary`, `Muted`, `Warning`, `Error`, `Accent`. The host maps it to theme tokens. |
| `host` | Calls into the IDE: `log`, `get_editor_snapshot`, `apply_editor_edit`, `register_command`, `register_tool_window`. |
| `prelude` | `use kova_plugin_rs::prelude::*;` imports everything above. |

## Requirements

- The Rust toolchain from `rust-toolchain.toml` (Rust 1.85 or newer).
- The WASI Preview 2 target:

  ```sh
  rustup target add wasm32-wasip2
  ```

  `wasm32-wasip2` produces a Component Model `.wasm` directly, so you do not need `wasm-tools componentize`.

## A minimal plugin

`Cargo.toml`:

```toml
[package]
name    = "my_plugin"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib", "rlib"]   # cdylib: the component; rlib: native unit tests

[dependencies]
kova_plugin_rs = { path = "../kova/sdk/kova_plugin_rs" }
```

`src/lib.rs`:

```rust
use kova_plugin_rs::prelude::*;

#[derive(Default)]
struct MyPlugin {
    clicks: u32,
}

impl KovaPlugin for MyPlugin {
    fn tool_windows(&self) -> Vec<ToolWindowDescriptor> {
        vec![tool_window("my-plugin.panel", "My Plugin", SemanticIcon::Plugin)]
    }

    fn render_ui(&mut self, _panel_id: &str) -> Result<WidgetTree, String> {
        Ok(ui::form(Some("Clicks"))
            .id("my-plugin.root")
            .child(ui::badge(self.clicks.to_string()).id("my-plugin.count").style(StyleVariant::Accent))
            .child(ui::button("Click me", false).id("my-plugin.click"))
            .build())
    }

    fn on_ui_event(&mut self, panel_id: &str, event: UiEvent) -> Result<Option<WidgetTree>, String> {
        if event.widget_id != "my-plugin.click" {
            return Ok(None); // nothing changed, no re-render
        }
        self.clicks += 1;
        self.render_ui(panel_id).map(Some)
    }
}

export_plugin!(MyPlugin);
```

How it works:

- The SDK creates the instance with `Default` on the first call and keeps it between calls. `shutdown` drops it.
- Before `init`, the SDK registers everything returned by `commands()` and `tool_windows()`.
- Prefix command, Tool Window and widget ids with your plugin id (`my-plugin.`). Widget ids must be unique within a tree.
- Natively, `export_plugin!` expands to nothing and host calls are not available. You can still unit test the logic by calling the trait methods directly and checking trees with `ui::validate`. Keep `host::*` calls out of the code paths you test natively.

`plugin.toml` sits next to the `.wasm`:

```toml
id          = "my-plugin"
name        = "My Plugin"
version     = "0.1.0"
entrypoint  = "my_plugin.wasm"
api_version = "0.1.0"

[permissions]
editor_read   = false   # true to call host::get_editor_snapshot
editor_write  = false   # true to call host::apply_editor_edit
network       = []
fs_read       = false
fs_write      = false
process_spawn = false
```

## Build

```sh
cargo build --release --target wasm32-wasip2
# -> target/wasm32-wasip2/release/my_plugin.wasm
```

## Install

Kova loads every subdirectory of `~/.local/share/kova/plugins/` that has a `plugin.toml`:

```sh
mkdir -p ~/.local/share/kova/plugins/my-plugin
cp plugin.toml ~/.local/share/kova/plugins/my-plugin/
cp target/wasm32-wasip2/release/my_plugin.wasm ~/.local/share/kova/plugins/my-plugin/
```

Restart Kova, or reload the plugin from the plugin manager. A plugin that fails or traps is marked as faulted and does not affect other plugins.

## The `hello_kova` example

[`examples/hello_kova`](examples/hello_kova) is a workspace member that uses every widget kind, all `KovaPlugin` hooks and both commands. Build it and install it from the repository root:

```sh
cargo build --release --target wasm32-wasip2 -p hello_kova
mkdir -p ~/.local/share/kova/plugins/hello-kova
cp sdk/kova_plugin_rs/examples/hello_kova/plugin.toml ~/.local/share/kova/plugins/hello-kova/
cp "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/hello_kova.wasm" ~/.local/share/kova/plugins/hello-kova/
```

`cargo test -p kova_plugin_rs` runs `tests/hello_kova_host.rs`. It builds the example for `wasm32-wasip2`, loads it through `kova_plugin_host::PluginManager`, and checks registration, rendering, UI events, commands and reload.
