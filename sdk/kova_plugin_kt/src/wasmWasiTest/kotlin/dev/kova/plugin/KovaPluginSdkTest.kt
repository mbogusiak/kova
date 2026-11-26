package dev.kova.plugin

import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

private class TestPlugin : KovaPlugin {
    var clicks = 0
    var lastCommand: String? = null

    override fun info() = PluginInfo(
        id = "test-plugin",
        name = "Test Plugin",
        version = "0.1.0",
    )

    override fun toolWindows() = listOf(
        ToolWindowSpec(id = "test-win", title = "Test Window", location = ToolWindowLocation.BOTTOM),
    )

    override fun commands() = listOf(
        CommandSpec(id = "test.run", title = "Run Test"),
    )

    override fun init(ctx: HostContext) {
        ctx.logInfo("initialized")
    }

    override fun renderUi(toolWindowId: String, ctx: HostContext): WidgetNode = kovaUi {
        split(
            id = "root",
            orientation = SplitOrientation.HORIZONTAL,
            ratio = 0.4f,
            first = {
                form(id = "f", submitLabel = "Apply") {
                    textInput(id = "q", label = "Query", value = "SELECT 1")
                    select(id = "s", label = "Mode", selectedValue = "fast") {
                        option("fast", "Fast")
                        option("full", "Full")
                    }
                    toggle(id = "t", label = "Verbose", checked = true)
                    button(id = "btn", label = "Run ($clicks)", style = StyleVariant.ACCENT)
                    badge(id = "b", text = "OK", style = StyleVariant.PRIMARY)
                }
            },
            second = {
                tabs(id = "tabs", activeIndex = 0) {
                    tab(id = "t1", title = "Code") {
                        codeBlock(id = "cb", code = "val x = 1", language = "kotlin")
                    }
                    tab(id = "t2", title = "Tree") {
                        tree(id = "tr", selectedId = "n1") {
                            node(id = "n1", label = "Root", expanded = true) {
                                node(id = "n2", label = "Child", style = StyleVariant.WARNING)
                            }
                        }
                    }
                    tab(id = "t3", title = "Table") {
                        table(id = "tbl", selectedRow = 0) {
                            column(id = "c1", title = "Col1", widthWeight = 2.0f)
                            row(id = "r1", "Val1", style = StyleVariant.ERROR)
                        }
                    }
                    tab(id = "t4", title = "List") {
                        list(id = "lst", selectedId = "i1") {
                            item(id = "i1", label = "Item 1", secondary = "Sub", style = StyleVariant.MUTED)
                        }
                    }
                    tab(id = "t5", title = "Docs") {
                        markdown(id = "md", content = "# Heading")
                    }
                }
            },
        )
    }

    override fun onUiEvent(event: UiEvent, ctx: HostContext): WidgetNode? {
        if (event is UiEvent.Click && event.widgetId == "btn") {
            clicks += 1
        }
        return renderUi("test-win", ctx)
    }

    override fun onCommand(commandId: String, args: List<String>, ctx: HostContext) {
        lastCommand = "$commandId:${args.joinToString(",")}"
        ctx.requestRerender("test-win")
    }
}

class KovaPluginSdkTest {
    @AfterTest
    fun tearDown() {
        WitBridge.reset()
    }

    @Test
    fun widgetTreeRoundTripsThroughWitCodec() {
        val plugin = TestPlugin()
        val transport = InMemoryHostTransport(
            snapshot = EditorSnapshot(
                filePath = "src/Main.kt",
                language = "kotlin",
                text = "fun main() {}",
                cursorLine = 1,
                cursorColumn = 5,
                workspaceRoot = "/tmp/ws",
            ),
        )
        WitBridge.register(plugin, transport)

        assertEquals(0, kova_plugin_init())
        val renderedJson = WitBridge.renderUiJson("test-win")
        val decoded = WitCodec.decodeWidgetNode(renderedJson)

        val split = assertIs<WidgetNode.Split>(decoded)
        assertEquals("root", split.id)
        val form = assertIs<WidgetNode.Form>(split.first)
        assertEquals(5, form.fields.size)
        val tabs = assertIs<WidgetNode.Tabs>(split.second)
        assertEquals(5, tabs.tabs.size)
        assertIs<WidgetNode.CodeBlock>(tabs.tabs[0].content)
        assertIs<WidgetNode.Tree>(tabs.tabs[1].content)
        assertIs<WidgetNode.Table>(tabs.tabs[2].content)
        assertIs<WidgetNode.ListWidget>(tabs.tabs[3].content)
        assertIs<WidgetNode.Markdown>(tabs.tabs[4].content)
    }

    @Test
    fun uiEventAndCommandDispatchThroughBridge() {
        val plugin = TestPlugin()
        val transport = InMemoryHostTransport(root = "/workspace/demo")
        WitBridge.register(plugin, transport)

        val clickJson = WitCodec.encodeUiEvent(UiEvent.Click("btn"))
        val updatedJson = WitBridge.onUiEventJson(clickJson)
        assertNotNull(updatedJson)
        val updated = assertIs<WidgetNode.Split>(WitCodec.decodeWidgetNode(updatedJson))
        val form = assertIs<WidgetNode.Form>(updated.first)
        val button = assertIs<WidgetNode.Button>(form.fields[3])
        assertEquals("Run (1)", button.label)

        assertEquals(0, WitBridge.onCommandJson("""{"id":"test.run","args":["--fast"]}"""))
        assertEquals("test.run:--fast", plugin.lastCommand)
        assertEquals(listOf("test-win"), transport.rerenderRequests)
    }

    @Test
    fun metadataSerializationIncludesInfoToolWindowsAndCommands() {
        WitBridge.register(TestPlugin(), InMemoryHostTransport())
        val metadata = WitBridge.metadataJson()
        assertTrue(metadata.contains(""""id":"test-plugin""""))
        assertTrue(metadata.contains(""""id":"test-win""""))
        assertTrue(metadata.contains(""""id":"test.run""""))
    }

    @Test
    fun allUiEventsAndPluginEventsRoundTrip() {
        val events = listOf(
            UiEvent.Click("b1"),
            UiEvent.TextChanged("t1", "line1\n\"quoted\"\\path"),
            UiEvent.SelectionChanged("s1", "opt-a", 0),
            UiEvent.Toggled("tg1", true),
            UiEvent.TabChanged("tabs", 2),
            UiEvent.TreeNodeSelected("tr", "node-1"),
            UiEvent.TreeNodeToggled("tr", "node-1", expanded = true),
            UiEvent.TableRowSelected("tbl", 42),
            UiEvent.ListItemSelected("lst", "item-3", 3),
            UiEvent.FormSubmitted("frm", mapOf("k" to "v")),
        )
        for (ev in events) {
            val encoded = WitCodec.encodeUiEvent(ev)
            assertEquals(ev, WitCodec.decodeUiEvent(encoded))
        }

        val ws = WitCodec.decodePluginEvent("""{"kind":"workspace-opened","workspace-root":"/app"}""")
        assertIs<PluginEvent.WorkspaceOpened>(ws)
        assertEquals("/app", ws.workspaceRoot)

        val saved = WitCodec.decodePluginEvent("""{"kind":"buffer-saved","file-path":"Main.kt"}""")
        assertIs<PluginEvent.BufferSaved>(saved)

        val active = WitCodec.decodePluginEvent("""{"kind":"active-editor-changed","file-path":"A.java","language":"java"}""")
        assertIs<PluginEvent.ActiveEditorChanged>(active)
        assertEquals("java", active.language)
    }

    @Test
    fun linearMemoryExportsRoundTripStrings() {
        WitBridge.register(TestPlugin(), InMemoryHostTransport())
        val packed = kova_plugin_metadata()
        val decoded = WitBridge.readAndFreePackedStringOrNull(packed)
        assertNotNull(decoded)
        assertTrue(decoded.contains(""""id":"test-plugin""""))

        val ptr = kova_alloc(16)
        assertTrue(ptr >= 4096)
        val grown = cabi_realloc(ptr, 16, 1, 64)
        assertTrue(grown >= 4096)
        kova_free(grown, 64)
    }
}
