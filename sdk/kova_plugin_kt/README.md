# Kova Plugin SDK for Kotlin/Wasm (`kova_plugin_kt`)

Official Kotlin Multiplatform (`wasmWasi`) SDK and starter template for writing **Kova IDE** plugins in Kotlin/Wasm without WebView.

Plugins declare native GPUI widget trees (`Split`, `Tabs`, `Tree`, `Table`, `ListWidget`, `Form`, `TextInput`, `Select`, `Toggle`, `Button`, `CodeBlock`, `Markdown`, `Badge`) styled exclusively with semantic `StyleVariant` tokens (`PRIMARY`, `MUTED`, `WARNING`, `ERROR`, `ACCENT`) mapped by `kova_ui` to the active `kova_theme`.

---

## Requirements

- **JDK 17+** (tested with OpenJDK 25)
- **Gradle 9.4+** (provided via `./gradlew`)
- **Kova IDE** (`kova_plugin_host` with Wasmtime + WASI + WasmGC support)

---

## Quick Start

Build the SDK, run all `wasmWasi` tests, compile the `hello-kova` starter plugin, and package the `.wasm` + `plugin.toml` bundle:

```bash
cd sdk/kova_plugin_kt
./gradlew build
```

The packaged plugin directory is generated at:

```text
sdk/kova_plugin_kt/examples/hello_kova/build/kova-plugin-package/hello-kova/
├── hello-kova.wasm
└── plugin.toml
```

To install the starter plugin directly into your local Kova plugins directory (`~/.local/share/kova/plugins/hello-kova/`):

```bash
./gradlew :examples:hello_kova:installKovaPlugin
```

---

## Project Layout

- `src/wasmWasiMain/kotlin/dev/kova/plugin/KovaPlugin.kt` — `KovaPlugin` interface, `WidgetNode`, `StyleVariant`, `UiEvent`, `PluginEvent`, `EditorSnapshot`, `HostContext`, and the `@KovaUiDsl` (`kovaUi { ... }`) DSL builder.
- `src/wasmWasiMain/kotlin/dev/kova/plugin/WitBridge.kt` — WASI linear-memory ABI bridge (`kova_abi_version`, `kova_alloc`, `kova_free`, `cabi_realloc`, `kova_plugin_*` exports, `kova:plugin/host` imports, and `WitCodec` JSON wire codec matching `kova:plugin@0.1.0`).
- `examples/hello_kova/` — complete starter plugin (`HelloKovaPlugin.kt`, `plugin.toml`, `build.gradle.kts`) registering a dockable Tool Window, Command Palette action, and interactive `UiEvent` handling.

---

## Writing a Plugin

1. Implement `KovaPlugin`:

```kotlin
package dev.example.myplugin

import dev.kova.plugin.*

class MyKovaPlugin : KovaPlugin {
    private var counter = 0

    override fun info() = PluginInfo(
        id = "my-kova-plugin",
        name = "My Kova Plugin",
        version = "0.1.0",
    )

    override fun toolWindows() = listOf(
        ToolWindowSpec(id = "my-panel", title = "My Panel", location = ToolWindowLocation.RIGHT),
    )

    override fun renderUi(toolWindowId: String, ctx: HostContext): WidgetNode = kovaUi {
        form(id = "root") {
            badge(id = "status", text = "Count: $counter", style = StyleVariant.ACCENT)
            button(id = "inc", label = "Increment", style = StyleVariant.ACCENT)
        }
    }

    override fun onUiEvent(event: UiEvent, ctx: HostContext): WidgetNode? {
        if (event is UiEvent.Click && event.widgetId == "inc") {
            counter += 1
        }
        return renderUi("my-panel", ctx)
    }
}

fun main() {
    WitBridge.register(MyKovaPlugin(), WasmHostTransport())
}
```

2. Declare permissions explicitly in `plugin.toml` (capability-based WASI sandbox — no filesystem, network, or process spawning access is granted by default):

```toml
id = "my-kova-plugin"
name = "My Kova Plugin"
version = "0.1.0"
entrypoint = "my-kova-plugin.wasm"
api_version = "0.1.0"

[permissions]
fs_read = []
fs_write = false
network = []
process_spawn = false
editor_read = true
editor_write = false
```
