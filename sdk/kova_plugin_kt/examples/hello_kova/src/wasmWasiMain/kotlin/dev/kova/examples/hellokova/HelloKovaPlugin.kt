package dev.kova.examples.hellokova

import dev.kova.plugin.CommandSpec
import dev.kova.plugin.HostContext
import dev.kova.plugin.KovaPlugin
import dev.kova.plugin.PluginEvent
import dev.kova.plugin.PluginInfo
import dev.kova.plugin.SplitOrientation
import dev.kova.plugin.StyleVariant
import dev.kova.plugin.ToolWindowLocation
import dev.kova.plugin.ToolWindowSpec
import dev.kova.plugin.UiEvent
import dev.kova.plugin.WasmHostTransport
import dev.kova.plugin.WidgetNode
import dev.kova.plugin.WitBridge
import dev.kova.plugin.kovaUi

/**
 * Sample Kotlin/Wasm plugin demonstrating Tool Window registration, declarative GPUI UI,
 * editor context inspection, and interactive [UiEvent] handling.
 */
class HelloKovaPlugin : KovaPlugin {
    private var clickCount: Int = 0
    private var filterText: String = ""
    private var selectedEnvironment: String = "dev"
    private var showLineNumbers: Boolean = true
    private var activeTabIndex: Int = 0
    private var selectedTreeNode: String? = "src"
    private var selectedTableRow: Int? = 0
    private var lastCommandMessage: String = "Ready"

    override fun info(): PluginInfo = PluginInfo(
        id = "hello-kova",
        name = "Hello Kova (Kotlin/Wasm)",
        version = "0.1.0",
        description = "Starter Kotlin/Wasm Tool Window plugin for Kova IDE",
        apiVersion = "0.1.0",
    )

    override fun toolWindows(): List<ToolWindowSpec> = listOf(
        ToolWindowSpec(
            id = TOOL_WINDOW_ID,
            title = "Hello Kotlin",
            icon = "kotlin",
            location = ToolWindowLocation.RIGHT,
        ),
    )

    override fun commands(): List<CommandSpec> = listOf(
        CommandSpec(
            id = "hello-kova.reset",
            title = "Hello Kova: Reset Counter",
            description = "Resets the interactive button counter in the Hello Kotlin Tool Window",
        ),
    )

    override fun init(ctx: HostContext) {
        ctx.logInfo("HelloKovaPlugin initialized")
    }

    override fun renderUi(toolWindowId: String, ctx: HostContext): WidgetNode {
        val snapshot = ctx.editorSnapshot()
        val activeFile = snapshot?.filePath ?: "No active file"
        val activeLang = snapshot?.language ?: "kotlin"
        val snippet = snapshot?.text?.takeIf { it.isNotBlank() }
            ?: """
            fun greet(name: String): String {
                return "Hello from Kotlin/Wasm, ${'$'}name!"
            }
            """.trimIndent()

        return kovaUi {
            split(
                id = "hello-root-split",
                orientation = SplitOrientation.VERTICAL,
                ratio = 0.45f,
                first = {
                    form(id = "hello-controls-form") {
                        badge(
                            id = "hello-status-badge",
                            text = "Clicks: $clickCount | Env: $selectedEnvironment | $lastCommandMessage",
                            style = if (clickCount > 0) StyleVariant.ACCENT else StyleVariant.MUTED,
                        )
                        markdown(
                            id = "hello-summary-md",
                            content = "## Hello Kova (Kotlin/Wasm)\nActive file: `$activeFile` ($activeLang)",
                        )
                        textInput(
                            id = "hello-filter-input",
                            label = "Filter",
                            value = filterText,
                            placeholder = "Type symbol filter...",
                        )
                        select(
                            id = "hello-env-select",
                            label = "Target",
                            selectedValue = selectedEnvironment,
                        ) {
                            option("dev", "Development")
                            option("staging", "Staging")
                            option("prod", "Production")
                        }
                        toggle(
                            id = "hello-lines-toggle",
                            label = "Show line numbers",
                            checked = showLineNumbers,
                        )
                        button(
                            id = "hello-increment-btn",
                            label = "Increment Counter ($clickCount)",
                            style = StyleVariant.ACCENT,
                        )
                    }
                },
                second = {
                    tabs(id = "hello-tabs", activeIndex = activeTabIndex) {
                        tab(id = "tab-preview", title = "Preview") {
                            codeBlock(
                                id = "hello-code-preview",
                                code = snippet,
                                language = activeLang,
                                showLineNumbers = showLineNumbers,
                            )
                        }
                        tab(id = "tab-structure", title = "Structure") {
                            tree(id = "hello-tree", selectedId = selectedTreeNode) {
                                node(id = "src", label = "src/wasmWasiMain", expanded = true) {
                                    node(id = "plugin", label = "HelloKovaPlugin.kt", style = StyleVariant.ACCENT)
                                    node(id = "manifest", label = "plugin.toml", style = StyleVariant.MUTED)
                                }
                            }
                        }
                        tab(id = "tab-metrics", title = "Metrics") {
                            table(id = "hello-table", selectedRow = selectedTableRow) {
                                column(id = "metric", title = "Metric", widthWeight = 1.5f)
                                column(id = "value", title = "Value", widthWeight = 1.0f)
                                row(id = "row-clicks", "Button clicks", clickCount.toString(), style = StyleVariant.ACCENT)
                                row(id = "row-env", "Environment", selectedEnvironment)
                                row(id = "row-filter", "Filter", filterText.ifEmpty { "<empty>" }, style = StyleVariant.MUTED)
                            }
                        }
                        tab(id = "tab-events", title = "Events") {
                            list(id = "hello-events-list", selectedId = "ev-1") {
                                item(id = "ev-1", label = "Plugin initialized", secondary = "kova:plugin@0.1.0")
                                item(id = "ev-2", label = "Last command", secondary = lastCommandMessage)
                            }
                        }
                    }
                },
            )
        }
    }

    override fun onUiEvent(event: UiEvent, ctx: HostContext): WidgetNode? {
        when (event) {
            is UiEvent.Click -> {
                if (event.widgetId == "hello-increment-btn") {
                    clickCount += 1
                    ctx.logInfo("Button clicked; count=$clickCount")
                }
            }
            is UiEvent.TextChanged -> {
                if (event.widgetId == "hello-filter-input") {
                    filterText = event.value
                }
            }
            is UiEvent.SelectionChanged -> {
                if (event.widgetId == "hello-env-select") {
                    selectedEnvironment = event.selectedValue
                }
            }
            is UiEvent.Toggled -> {
                if (event.widgetId == "hello-lines-toggle") {
                    showLineNumbers = event.checked
                }
            }
            is UiEvent.TabChanged -> {
                if (event.widgetId == "hello-tabs") {
                    activeTabIndex = event.activeIndex
                }
            }
            is UiEvent.TreeNodeSelected -> {
                selectedTreeNode = event.nodeId
            }
            is UiEvent.TableRowSelected -> {
                selectedTableRow = event.rowIndex
            }
            else -> Unit
        }
        return renderUi(TOOL_WINDOW_ID, ctx)
    }

    override fun onCommand(commandId: String, args: List<String>, ctx: HostContext) {
        if (commandId == "hello-kova.reset") {
            clickCount = 0
            lastCommandMessage = "Reset via command"
            ctx.requestRerender(TOOL_WINDOW_ID)
        }
    }

    override fun onEvent(event: PluginEvent, ctx: HostContext) {
        when (event) {
            is PluginEvent.BufferSaved -> {
                lastCommandMessage = "Saved ${event.filePath}"
                ctx.requestRerender(TOOL_WINDOW_ID)
            }
            is PluginEvent.WorkspaceOpened -> {
                lastCommandMessage = "Workspace: ${event.workspaceRoot}"
            }
            else -> Unit
        }
    }

    companion object {
        const val TOOL_WINDOW_ID: String = "hello-kova-panel"
    }
}

fun main() {
    WitBridge.register(HelloKovaPlugin(), WasmHostTransport())
}
