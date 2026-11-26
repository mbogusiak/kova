package dev.kova.examples.hellokova

import dev.kova.plugin.EditorSnapshot
import dev.kova.plugin.InMemoryHostTransport
import dev.kova.plugin.PluginEvent
import dev.kova.plugin.StyleVariant
import dev.kova.plugin.UiEvent
import dev.kova.plugin.WidgetNode
import dev.kova.plugin.WitBridge
import dev.kova.plugin.WitCodec
import dev.kova.plugin.kova_plugin_init
import dev.kova.plugin.kova_plugin_shutdown
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

class HelloKovaPluginTest {
    @AfterTest
    fun tearDown() {
        WitBridge.reset()
    }

    @Test
    fun rendersInitialToolWindowAndHandlesEvents() {
        val transport = InMemoryHostTransport(
            snapshot = EditorSnapshot(
                filePath = "src/Main.kt",
                language = "kotlin",
                text = "fun main() { println(\"hi\") }",
                workspaceRoot = "/tmp/ws",
            ),
        )
        val plugin = HelloKovaPlugin()
        WitBridge.register(plugin, transport)

        assertEquals(0, kova_plugin_init())
        assertTrue(transport.logs.any { it.second.contains("HelloKovaPlugin initialized") })

        val initial = WitCodec.decodeWidgetNode(WitBridge.renderUiJson(HelloKovaPlugin.TOOL_WINDOW_ID))
        val split = assertIs<WidgetNode.Split>(initial)
        val form = assertIs<WidgetNode.Form>(split.first)
        val tabs = assertIs<WidgetNode.Tabs>(split.second)
        assertEquals(4, tabs.tabs.size)
        val codeBlock = assertIs<WidgetNode.CodeBlock>(tabs.tabs[0].content)
        assertEquals("kotlin", codeBlock.language)
        assertTrue(codeBlock.code.contains("println"))

        val clickRes = WitBridge.onUiEventJson(
            WitCodec.encodeUiEvent(UiEvent.Click("hello-increment-btn")),
        )!!
        val updatedSplit = assertIs<WidgetNode.Split>(WitCodec.decodeWidgetNode(clickRes))
        val updatedForm = assertIs<WidgetNode.Form>(updatedSplit.first)
        val badge = assertIs<WidgetNode.Badge>(updatedForm.fields[0])
        assertEquals(StyleVariant.ACCENT, badge.style)
        assertTrue(badge.text.contains("Clicks: 1"))

        val envRes = WitBridge.onUiEventJson(
            WitCodec.encodeUiEvent(UiEvent.SelectionChanged("hello-env-select", "prod", 2)),
        )!!
        val envSplit = assertIs<WidgetNode.Split>(WitCodec.decodeWidgetNode(envRes))
        val envBadge = assertIs<WidgetNode.Badge>((envSplit.first as WidgetNode.Form).fields[0])
        assertTrue(envBadge.text.contains("Env: prod"))

        assertEquals(0, WitBridge.onCommandJson("""{"id":"hello-kova.reset","args":[]}"""))
        assertEquals(listOf(HelloKovaPlugin.TOOL_WINDOW_ID), transport.rerenderRequests)

        assertEquals(
            0,
            WitBridge.onEventJson("""{"kind":"buffer-saved","file-path":"src/Main.kt"}"""),
        )
        assertEquals(2, transport.rerenderRequests.size)
        assertEquals(0, kova_plugin_shutdown())
    }
}
