package dev.kova.plugin

import kotlin.wasm.ExperimentalWasmInterop
import kotlin.wasm.WasmExport
import kotlin.wasm.WasmImport
import kotlin.wasm.unsafe.Pointer
import kotlin.wasm.unsafe.UnsafeWasmMemoryApi
import kotlin.wasm.unsafe.withScopedMemoryAllocator

/**
 * ABI version spoken by [WitBridge].
 */
const val KOVA_WIT_ABI_VERSION: Int = 1

/**
 * Transport abstraction for host imports so unit tests can run in standard `wasmWasi`
 * (Node/WASI) without requiring custom host imports at instantiation time.
 */
interface HostTransport {
    fun editorSnapshotJson(): String? = null
    fun workspaceRoot(): String? = null
    fun log(level: Int, message: String) {}
    fun requestRerender(toolWindowId: String) {}
}

/**
 * Default in-memory/WASI fallback transport for standalone execution and unit tests.
 */
class InMemoryHostTransport(
    var snapshot: EditorSnapshot? = null,
    var root: String? = null,
) : HostTransport {
    val logs: MutableList<Pair<Int, String>> = mutableListOf()
    val rerenderRequests: MutableList<String> = mutableListOf()

    override fun editorSnapshotJson(): String? = snapshot?.let(WitCodec::encodeEditorSnapshot)
    override fun workspaceRoot(): String? = root ?: snapshot?.workspaceRoot
    override fun log(level: Int, message: String) {
        logs += level to message
    }

    override fun requestRerender(toolWindowId: String) {
        rerenderRequests += toolWindowId
    }
}

@OptIn(UnsafeWasmMemoryApi::class, ExperimentalWasmInterop::class)
@WasmImport("kova:plugin/host", "log")
private external fun rawHostLog(level: Int, ptr: Int, len: Int)

@OptIn(UnsafeWasmMemoryApi::class, ExperimentalWasmInterop::class)
@WasmImport("kova:plugin/host", "request-rerender")
private external fun rawHostRequestRerender(ptr: Int, len: Int)

@OptIn(UnsafeWasmMemoryApi::class, ExperimentalWasmInterop::class)
@WasmImport("kova:plugin/host", "get-workspace-root")
private external fun rawHostGetWorkspaceRoot(): Long

@OptIn(UnsafeWasmMemoryApi::class, ExperimentalWasmInterop::class)
@WasmImport("kova:plugin/host", "get-editor-snapshot")
private external fun rawHostGetEditorSnapshot(): Long

/**
 * Live transport backed by `@WasmImport("kova:plugin/host", ...)` functions provided by
 * `kova_plugin_host`.
 */
@OptIn(UnsafeWasmMemoryApi::class)
class WasmHostTransport : HostTransport {
    override fun editorSnapshotJson(): String? {
        val packed = rawHostGetEditorSnapshot()
        return WitBridge.readAndFreePackedStringOrNull(packed)
    }

    override fun workspaceRoot(): String? {
        val packed = rawHostGetWorkspaceRoot()
        return WitBridge.readAndFreePackedStringOrNull(packed)
    }

    override fun log(level: Int, message: String) {
        WitBridge.withUtf8Bytes(message) { ptr, len ->
            rawHostLog(level, ptr, len)
        }
    }

    override fun requestRerender(toolWindowId: String) {
        WitBridge.withUtf8Bytes(toolWindowId) { ptr, len ->
            rawHostRequestRerender(ptr, len)
        }
    }
}

/**
 * [HostContext] implementation backed by a [HostTransport].
 */
class BridgeHostContext(
    private val transport: HostTransport,
) : HostContext {
    override fun editorSnapshot(): EditorSnapshot? {
        val json = transport.editorSnapshotJson() ?: return null
        if (json.isBlank()) return null
        return WitCodec.decodeEditorSnapshot(json)
    }

    override fun workspaceRoot(): String? = transport.workspaceRoot()

    override fun logInfo(message: String) {
        transport.log(level = 1, message = message)
    }

    override fun logWarn(message: String) {
        transport.log(level = 2, message = message)
    }

    override fun logError(message: String) {
        transport.log(level = 3, message = message)
    }

    override fun requestRerender(toolWindowId: String) {
        transport.requestRerender(toolWindowId)
    }
}

/**
 * Central WIT / Canonical-ABI bridge connecting a Kotlin [KovaPlugin] implementation to
 * `kova_plugin_host`.
 *
 * Plugins call [register] in their `main()` entry point (or test harness) so that all
 * `@WasmExport` entry points (`kova_plugin_init`, `kova_plugin_render_ui`,
 * `kova_plugin_on_ui_event`, `kova_plugin_on_command`, `kova_plugin_on_event`,
 * `kova_plugin_shutdown`) delegate to the registered plugin instance.
 */
@OptIn(UnsafeWasmMemoryApi::class)
object WitBridge {
    private var registeredPlugin: KovaPlugin? = null
    private var transport: HostTransport = InMemoryHostTransport()
    private var lastError: String? = null
    private var exportAnchor: Int = 0

    /**
     * Registers [plugin] as the active plugin instance in this WASI module.
     */
    fun register(
        plugin: KovaPlugin,
        hostTransport: HostTransport = InMemoryHostTransport(),
    ) {
        registeredPlugin = plugin
        transport = hostTransport
        lastError = null
        // Keep @WasmExport entry points reachable so Kotlin/Wasm links them into executable modules.
        exportAnchor = keepExportsLinked()
    }

    private fun keepExportsLinked(): Int {
        var acc = kova_abi_version()
        if (acc == -1) {
            val p = kova_alloc(1)
            val p2 = cabi_realloc(p, 1, 1, 2)
            kova_free(p2, 2)
            acc += kova_plugin_init()
            acc += kova_plugin_shutdown()
            acc += kova_plugin_metadata().toInt()
            acc += kova_plugin_render_ui(0, 0).toInt()
            acc += kova_plugin_on_ui_event(0, 0).toInt()
            acc += kova_plugin_on_command(0, 0)
            acc += kova_plugin_on_event(0, 0)
            acc += kova_plugin_last_error().toInt()
        }
        return acc
    }

    /**
     * Clears the registered plugin (primarily for unit tests).
     */
    fun reset() {
        registeredPlugin = null
        transport = InMemoryHostTransport()
        lastError = null
    }

    internal fun currentPlugin(): KovaPlugin? = registeredPlugin

    internal fun currentContext(): HostContext = BridgeHostContext(transport)

    internal fun setError(message: String?) {
        lastError = message
    }

    internal fun currentError(): String? = lastError

    fun initPlugin(): Int {
        val plugin = registeredPlugin ?: return recordFailure("No KovaPlugin registered")
        return try {
            lastError = null
            plugin.init(currentContext())
            0
        } catch (t: Throwable) {
            recordFailure("init failed: ${t.message ?: t::class.simpleName}")
        }
    }

    fun shutdownPlugin(): Int {
        val plugin = registeredPlugin ?: return 0
        return try {
            plugin.shutdown()
            0
        } catch (t: Throwable) {
            recordFailure("shutdown failed: ${t.message ?: t::class.simpleName}")
        }
    }

    fun metadataJson(): String {
        val plugin = registeredPlugin ?: error("No KovaPlugin registered")
        return WitCodec.encodePluginMetadata(
            info = plugin.info(),
            toolWindows = plugin.toolWindows(),
            commands = plugin.commands(),
        )
    }

    fun renderUiJson(toolWindowId: String): String {
        val plugin = registeredPlugin ?: error("No KovaPlugin registered")
        val tree = plugin.renderUi(toolWindowId, currentContext())
        return WitCodec.encodeWidgetNode(tree)
    }

    fun onUiEventJson(eventJson: String): String? {
        val plugin = registeredPlugin ?: error("No KovaPlugin registered")
        val event = WitCodec.decodeUiEvent(eventJson)
        val updated = plugin.onUiEvent(event, currentContext()) ?: return null
        return WitCodec.encodeWidgetNode(updated)
    }

    fun onCommandJson(commandJson: String): Int {
        val plugin = registeredPlugin ?: return recordFailure("No KovaPlugin registered")
        return try {
            val (id, args) = WitCodec.decodeCommandInvocation(commandJson)
            plugin.onCommand(id, args, currentContext())
            0
        } catch (t: Throwable) {
            recordFailure("onCommand failed: ${t.message ?: t::class.simpleName}")
        }
    }

    fun onEventJson(eventJson: String): Int {
        val plugin = registeredPlugin ?: return recordFailure("No KovaPlugin registered")
        return try {
            val event = WitCodec.decodePluginEvent(eventJson)
            plugin.onEvent(event, currentContext())
            0
        } catch (t: Throwable) {
            recordFailure("onEvent failed: ${t.message ?: t::class.simpleName}")
        }
    }

    private fun recordFailure(message: String): Int {
        lastError = message
        runCatching { transport.log(3, message) }
        return 1
    }

    internal inline fun <R> withUtf8Bytes(value: String, block: (ptr: Int, len: Int) -> R): R {
        val bytes = value.encodeToByteArray()
        if (bytes.isEmpty()) {
            return block(0, 0)
        }
        val ptr = LinearBufferPool.write(bytes)
        return try {
            block(ptr, bytes.size)
        } finally {
            LinearBufferPool.free(ptr)
        }
    }

    internal fun readString(ptr: Int, len: Int): String {
        if (len <= 0 || ptr == 0) return ""
        val bytes = ByteArray(len)
        val base = Pointer(ptr.toUInt())
        for (i in 0 until len) {
            bytes[i] = (base + i).loadByte()
        }
        return bytes.decodeToString()
    }

    internal fun readPackedStringOrNull(packed: Long): String? {
        if (packed == 0L) return null
        val ptr = (packed ushr 32).toInt()
        val len = (packed and 0xFFFF_FFFFL).toInt()
        if (ptr == 0 || len < 0) return null
        return readString(ptr, len)
    }

    internal fun readAndFreePackedStringOrNull(packed: Long): String? {
        if (packed == 0L) return null
        val ptr = (packed ushr 32).toInt()
        val len = (packed and 0xFFFF_FFFFL).toInt()
        if (ptr == 0 || len < 0) return null
        return try {
            readString(ptr, len)
        } finally {
            LinearBufferPool.free(ptr)
        }
    }

    internal fun writeResultString(value: String): Long {
        val bytes = value.encodeToByteArray()
        if (bytes.isEmpty()) {
            val ptr = LinearBufferPool.alloc(1)
            return packPtrLen(ptr, 0)
        }
        val ptr = LinearBufferPool.write(bytes)
        return packPtrLen(ptr, bytes.size)
    }

    internal fun packPtrLen(ptr: Int, len: Int): Long =
        ((ptr.toLong() and 0xFFFF_FFFFL) shl 32) or (len.toLong() and 0xFFFF_FFFFL)
}

/**
 * Linear-memory allocator for ABI transfers across WASM function boundaries.
 *
 * Reads and writes directly in WASM linear memory via [Pointer] so the host
 * (`kova_plugin_host` / Wasmtime / WASI) can inspect and populate buffers through
 * the exported `memory`.
 */
@OptIn(UnsafeWasmMemoryApi::class)
internal object LinearBufferPool {
    private data class FreeBlock(val ptr: Int, val capacity: Int)

    private const val INITIAL_BASE_OFFSET: Int = 4096

    private val activeBlocks = mutableMapOf<Int, Int>()
    private val freeBlocks = mutableListOf<FreeBlock>()
    private var nextOffset: Int = INITIAL_BASE_OFFSET
    private var committedBytes: Int = 0

    private fun ensureCapacity(requiredEnd: Int) {
        if (requiredEnd <= committedBytes) return
        withScopedMemoryAllocator { allocator ->
            allocator.allocate(requiredEnd)
        }
        committedBytes = requiredEnd
    }

    fun alloc(size: Int): Int {
        val alignedSize = ((size.coerceAtLeast(1) + 7) and 7.inv())
        val reuseIdx = freeBlocks.indexOfFirst { it.capacity >= alignedSize }
        if (reuseIdx >= 0) {
            val block = freeBlocks.removeAt(reuseIdx)
            activeBlocks[block.ptr] = block.capacity
            return block.ptr
        }
        val ptr = nextOffset
        val end = ptr + alignedSize
        ensureCapacity(end)
        nextOffset = end
        activeBlocks[ptr] = alignedSize
        return ptr
    }

    fun realloc(oldPtr: Int, oldSize: Int, newSize: Int): Int {
        val next = alloc(newSize)
        if (oldPtr != 0 && oldSize > 0) {
            val copyLen = minOf(oldSize, newSize)
            val src = Pointer(oldPtr.toUInt())
            val dst = Pointer(next.toUInt())
            for (i in 0 until copyLen) {
                (dst + i).storeByte((src + i).loadByte())
            }
            free(oldPtr)
        }
        return next
    }

    fun free(ptr: Int) {
        val capacity = activeBlocks.remove(ptr) ?: return
        freeBlocks += FreeBlock(ptr, capacity)
    }

    fun write(bytes: ByteArray): Int {
        val ptr = alloc(bytes.size)
        val dst = Pointer(ptr.toUInt())
        for (i in bytes.indices) {
            (dst + i).storeByte(bytes[i])
        }
        return ptr
    }
}

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_abi_version(): Int = KOVA_WIT_ABI_VERSION

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_alloc(size: Int): Int = LinearBufferPool.alloc(size)

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_free(ptr: Int, @Suppress("UNUSED_PARAMETER") size: Int) {
    LinearBufferPool.free(ptr)
}

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun cabi_realloc(
    oldPtr: Int,
    oldSize: Int,
    @Suppress("UNUSED_PARAMETER") align: Int,
    newSize: Int,
): Int = LinearBufferPool.realloc(oldPtr, oldSize, newSize)

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_plugin_init(): Int = WitBridge.initPlugin()

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_plugin_shutdown(): Int = WitBridge.shutdownPlugin()

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_plugin_metadata(): Long =
    runCatching { WitBridge.writeResultString(WitBridge.metadataJson()) }
        .getOrElse {
            WitBridge.setError(it.message ?: "metadata error")
            0L
        }

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_plugin_render_ui(toolWindowIdPtr: Int, toolWindowIdLen: Int): Long =
    runCatching {
        val id = WitBridge.readString(toolWindowIdPtr, toolWindowIdLen)
        WitBridge.writeResultString(WitBridge.renderUiJson(id))
    }.getOrElse {
        WitBridge.setError(it.message ?: "render_ui error")
        0L
    }

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_plugin_on_ui_event(eventPtr: Int, eventLen: Int): Long =
    runCatching {
        val json = WitBridge.readString(eventPtr, eventLen)
        val updated = WitBridge.onUiEventJson(json) ?: return@runCatching 0L
        WitBridge.writeResultString(updated)
    }.getOrElse {
        WitBridge.setError(it.message ?: "on_ui_event error")
        0L
    }

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_plugin_on_command(cmdPtr: Int, cmdLen: Int): Int =
    runCatching {
        val json = WitBridge.readString(cmdPtr, cmdLen)
        WitBridge.onCommandJson(json)
    }.getOrElse {
        WitBridge.setError(it.message ?: "on_command error")
        1
    }

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_plugin_on_event(eventPtr: Int, eventLen: Int): Int =
    runCatching {
        val json = WitBridge.readString(eventPtr, eventLen)
        WitBridge.onEventJson(json)
    }.getOrElse {
        WitBridge.setError(it.message ?: "on_event error")
        1
    }

@OptIn(ExperimentalWasmInterop::class)
@WasmExport
fun kova_plugin_last_error(): Long {
    val err = WitBridge.currentError() ?: return 0L
    return WitBridge.writeResultString(err)
}

/**
 * Zero-dependency deterministic JSON codec matching the `kova:plugin@0.1.0` WIT schema.
 */
object WitCodec {
    fun encodePluginMetadata(
        info: PluginInfo,
        toolWindows: List<ToolWindowSpec>,
        commands: List<CommandSpec>,
    ): String = buildString {
        append('{')
        appendField("info"); appendPluginInfo(info); append(',')
        appendField("tool-windows"); appendArray(toolWindows) { appendToolWindowSpec(it) }; append(',')
        appendField("commands"); appendArray(commands) { appendCommandSpec(it) }
        append('}')
    }

    fun encodeEditorSnapshot(snapshot: EditorSnapshot): String = buildString {
        append('{')
        appendField("file-path"); appendNullableString(snapshot.filePath); append(',')
        appendField("language"); appendNullableString(snapshot.language); append(',')
        appendField("text"); appendJsonString(snapshot.text); append(',')
        appendField("cursor-line"); append(snapshot.cursorLine); append(',')
        appendField("cursor-column"); append(snapshot.cursorColumn); append(',')
        appendField("workspace-root"); appendNullableString(snapshot.workspaceRoot)
        append('}')
    }

    fun decodeEditorSnapshot(json: String): EditorSnapshot {
        val obj = JsonMini.parseObject(json)
        return EditorSnapshot(
            filePath = obj.optString("file-path"),
            language = obj.optString("language"),
            text = obj.optString("text") ?: "",
            cursorLine = obj.optInt("cursor-line") ?: 0,
            cursorColumn = obj.optInt("cursor-column") ?: 0,
            workspaceRoot = obj.optString("workspace-root"),
        )
    }

    fun encodeWidgetNode(node: WidgetNode): String = buildString {
        appendWidgetNode(node)
    }

    fun decodeWidgetNode(json: String): WidgetNode =
        parseWidgetNode(JsonMini.parseObject(json))

    fun encodeUiEvent(event: UiEvent): String = buildString {
        append('{')
        when (event) {
            is UiEvent.Click -> {
                appendField("kind"); appendJsonString("click"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId)
            }
            is UiEvent.TextChanged -> {
                appendField("kind"); appendJsonString("text-changed"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("value"); appendJsonString(event.value)
            }
            is UiEvent.SelectionChanged -> {
                appendField("kind"); appendJsonString("selection-changed"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("selected-value"); appendJsonString(event.selectedValue); append(',')
                appendField("selected-index"); append(event.selectedIndex?.toString() ?: "null")
            }
            is UiEvent.Toggled -> {
                appendField("kind"); appendJsonString("toggled"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("checked"); append(event.checked)
            }
            is UiEvent.TabChanged -> {
                appendField("kind"); appendJsonString("tab-changed"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("active-index"); append(event.activeIndex)
            }
            is UiEvent.TreeNodeSelected -> {
                appendField("kind"); appendJsonString("tree-node-selected"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("node-id"); appendJsonString(event.nodeId)
            }
            is UiEvent.TreeNodeToggled -> {
                appendField("kind"); appendJsonString("tree-node-toggled"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("node-id"); appendJsonString(event.nodeId); append(',')
                appendField("expanded"); append(event.expanded)
            }
            is UiEvent.TableRowSelected -> {
                appendField("kind"); appendJsonString("table-row-selected"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("row-index"); append(event.rowIndex)
            }
            is UiEvent.ListItemSelected -> {
                appendField("kind"); appendJsonString("list-item-selected"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("item-id"); appendJsonString(event.itemId); append(',')
                appendField("index"); append(event.index)
            }
            is UiEvent.FormSubmitted -> {
                appendField("kind"); appendJsonString("form-submitted"); append(',')
                appendField("widget-id"); appendJsonString(event.widgetId); append(',')
                appendField("values"); appendStringMap(event.values)
            }
        }
        append('}')
    }

    fun decodeUiEvent(json: String): UiEvent {
        val obj = JsonMini.parseObject(json)
        val kind = obj.reqString("kind")
        val widgetId = obj.reqString("widget-id")
        return when (kind) {
            "click" -> UiEvent.Click(widgetId)
            "text-changed" -> UiEvent.TextChanged(widgetId, obj.reqString("value"))
            "selection-changed" -> UiEvent.SelectionChanged(
                widgetId = widgetId,
                selectedValue = obj.reqString("selected-value"),
                selectedIndex = obj.optInt("selected-index"),
            )
            "toggled" -> UiEvent.Toggled(widgetId, obj.reqBoolean("checked"))
            "tab-changed" -> UiEvent.TabChanged(widgetId, obj.reqInt("active-index"))
            "tree-node-selected" -> UiEvent.TreeNodeSelected(widgetId, obj.reqString("node-id"))
            "tree-node-toggled" -> UiEvent.TreeNodeToggled(
                widgetId = widgetId,
                nodeId = obj.reqString("node-id"),
                expanded = obj.reqBoolean("expanded"),
            )
            "table-row-selected" -> UiEvent.TableRowSelected(widgetId, obj.reqInt("row-index"))
            "list-item-selected" -> UiEvent.ListItemSelected(
                widgetId = widgetId,
                itemId = obj.reqString("item-id"),
                index = obj.reqInt("index"),
            )
            "form-submitted" -> UiEvent.FormSubmitted(
                widgetId = widgetId,
                values = obj.optStringMap("values"),
            )
            else -> error("Unknown UiEvent kind: $kind")
        }
    }

    fun decodeCommandInvocation(json: String): Pair<String, List<String>> {
        val obj = JsonMini.parseObject(json)
        val id = obj.reqString("id")
        val args = obj.optStringList("args")
        return id to args
    }

    fun decodePluginEvent(json: String): PluginEvent {
        val obj = JsonMini.parseObject(json)
        return when (val kind = obj.reqString("kind")) {
            "workspace-opened" -> PluginEvent.WorkspaceOpened(obj.reqString("workspace-root"))
            "buffer-saved" -> PluginEvent.BufferSaved(obj.reqString("file-path"))
            "active-editor-changed" -> PluginEvent.ActiveEditorChanged(
                filePath = obj.optString("file-path"),
                language = obj.optString("language"),
            )
            else -> PluginEvent.Custom(kind, obj.optString("payload") ?: "")
        }
    }

    private fun StringBuilder.appendPluginInfo(info: PluginInfo) {
        append('{')
        appendField("id"); appendJsonString(info.id); append(',')
        appendField("name"); appendJsonString(info.name); append(',')
        appendField("version"); appendJsonString(info.version); append(',')
        appendField("description"); appendJsonString(info.description); append(',')
        appendField("api-version"); appendJsonString(info.apiVersion)
        append('}')
    }

    private fun StringBuilder.appendToolWindowSpec(spec: ToolWindowSpec) {
        append('{')
        appendField("id"); appendJsonString(spec.id); append(',')
        appendField("title"); appendJsonString(spec.title); append(',')
        appendField("icon"); appendJsonString(spec.icon); append(',')
        appendField("location"); appendJsonString(spec.location.wireName)
        append('}')
    }

    private fun StringBuilder.appendCommandSpec(spec: CommandSpec) {
        append('{')
        appendField("id"); appendJsonString(spec.id); append(',')
        appendField("title"); appendJsonString(spec.title); append(',')
        appendField("description"); appendJsonString(spec.description)
        append('}')
    }

    private fun StringBuilder.appendWidgetNode(node: WidgetNode) {
        append('{')
        appendField("id"); appendJsonString(node.id); append(',')
        appendField("style-variant"); appendJsonString(node.style.wireName); append(',')
        when (node) {
            is WidgetNode.Split -> {
                appendField("type"); appendJsonString("split"); append(',')
                appendField("orientation"); appendJsonString(node.orientation.wireName); append(',')
                appendField("ratio"); append(if (node.ratio.isFinite()) node.ratio else 0.5f); append(',')
                appendField("first"); appendWidgetNode(node.first); append(',')
                appendField("second"); appendWidgetNode(node.second)
            }
            is WidgetNode.Tabs -> {
                appendField("type"); appendJsonString("tabs"); append(',')
                appendField("active-index"); append(node.activeIndex); append(',')
                appendField("tabs"); appendArray(node.tabs) { tab ->
                    append('{')
                    appendField("id"); appendJsonString(tab.id); append(',')
                    appendField("title"); appendJsonString(tab.title); append(',')
                    appendField("content"); appendWidgetNode(tab.content)
                    append('}')
                }
            }
            is WidgetNode.Tree -> {
                appendField("type"); appendJsonString("tree"); append(',')
                appendField("selected-id"); appendNullableString(node.selectedId); append(',')
                appendField("nodes"); appendArray(node.nodes) { appendTreeNode(it) }
            }
            is WidgetNode.Table -> {
                appendField("type"); appendJsonString("table"); append(',')
                appendField("selected-row"); append(node.selectedRow?.toString() ?: "null"); append(',')
                appendField("columns"); appendArray(node.columns) { col ->
                    append('{')
                    appendField("id"); appendJsonString(col.id); append(',')
                    appendField("title"); appendJsonString(col.title); append(',')
                    appendField("width-weight"); append(if (col.widthWeight.isFinite()) col.widthWeight else 1.0f)
                    append('}')
                }; append(',')
                appendField("rows"); appendArray(node.rows) { row ->
                    append('{')
                    appendField("id"); appendJsonString(row.id); append(',')
                    appendField("style-variant"); appendJsonString(row.style.wireName); append(',')
                    appendField("cells"); appendArray(row.cells) { appendJsonString(it) }
                    append('}')
                }
            }
            is WidgetNode.ListWidget -> {
                appendField("type"); appendJsonString("list"); append(',')
                appendField("selected-id"); appendNullableString(node.selectedId); append(',')
                appendField("items"); appendArray(node.items) { item ->
                    append('{')
                    appendField("id"); appendJsonString(item.id); append(',')
                    appendField("label"); appendJsonString(item.label); append(',')
                    appendField("secondary"); appendNullableString(item.secondary); append(',')
                    appendField("style-variant"); appendJsonString(item.style.wireName)
                    append('}')
                }
            }
            is WidgetNode.Form -> {
                appendField("type"); appendJsonString("form"); append(',')
                appendField("submit-label"); appendNullableString(node.submitLabel); append(',')
                appendField("fields"); appendArray(node.fields) { appendWidgetNode(it) }
            }
            is WidgetNode.TextInput -> {
                appendField("type"); appendJsonString("text-input"); append(',')
                appendField("label"); appendJsonString(node.label); append(',')
                appendField("value"); appendJsonString(node.value); append(',')
                appendField("placeholder"); appendJsonString(node.placeholder)
            }
            is WidgetNode.Select -> {
                appendField("type"); appendJsonString("select"); append(',')
                appendField("label"); appendJsonString(node.label); append(',')
                appendField("selected-value"); appendNullableString(node.selectedValue); append(',')
                appendField("options"); appendArray(node.options) { opt ->
                    append('{')
                    appendField("value"); appendJsonString(opt.value); append(',')
                    appendField("label"); appendJsonString(opt.label)
                    append('}')
                }
            }
            is WidgetNode.Toggle -> {
                appendField("type"); appendJsonString("toggle"); append(',')
                appendField("label"); appendJsonString(node.label); append(',')
                appendField("checked"); append(node.checked)
            }
            is WidgetNode.Button -> {
                appendField("type"); appendJsonString("button"); append(',')
                appendField("label"); appendJsonString(node.label); append(',')
                appendField("disabled"); append(node.disabled)
            }
            is WidgetNode.CodeBlock -> {
                appendField("type"); appendJsonString("code-block"); append(',')
                appendField("code"); appendJsonString(node.code); append(',')
                appendField("language"); appendJsonString(node.language); append(',')
                appendField("show-line-numbers"); append(node.showLineNumbers)
            }
            is WidgetNode.Markdown -> {
                appendField("type"); appendJsonString("markdown"); append(',')
                appendField("content"); appendJsonString(node.content)
            }
            is WidgetNode.Badge -> {
                appendField("type"); appendJsonString("badge"); append(',')
                appendField("text"); appendJsonString(node.text)
            }
        }
        append('}')
    }

    private fun StringBuilder.appendTreeNode(node: TreeNode) {
        append('{')
        appendField("id"); appendJsonString(node.id); append(',')
        appendField("label"); appendJsonString(node.label); append(',')
        appendField("icon"); appendNullableString(node.icon); append(',')
        appendField("expanded"); append(node.expanded); append(',')
        appendField("style-variant"); appendJsonString(node.style.wireName); append(',')
        appendField("children"); appendArray(node.children) { appendTreeNode(it) }
        append('}')
    }

    private fun parseWidgetNode(obj: JsonObject): WidgetNode {
        val id = obj.reqString("id")
        val style = StyleVariant.fromWireName(obj.optString("style-variant") ?: "primary")
        return when (val type = obj.reqString("type")) {
            "split" -> WidgetNode.Split(
                id = id,
                orientation = SplitOrientation.fromWireName(obj.optString("orientation") ?: "horizontal"),
                ratio = obj.optFloat("ratio") ?: 0.5f,
                first = parseWidgetNode(obj.reqObject("first")),
                second = parseWidgetNode(obj.reqObject("second")),
                style = style,
            )
            "tabs" -> WidgetNode.Tabs(
                id = id,
                tabs = obj.reqArray("tabs").map { item ->
                    val t = item.asObject()
                    TabItem(
                        id = t.reqString("id"),
                        title = t.reqString("title"),
                        content = parseWidgetNode(t.reqObject("content")),
                    )
                },
                activeIndex = obj.optInt("active-index") ?: 0,
                style = style,
            )
            "tree" -> WidgetNode.Tree(
                id = id,
                nodes = obj.reqArray("nodes").map { parseTreeNode(it.asObject()) },
                selectedId = obj.optString("selected-id"),
                style = style,
            )
            "table" -> WidgetNode.Table(
                id = id,
                columns = obj.reqArray("columns").map { c ->
                    val co = c.asObject()
                    TableColumn(
                        id = co.reqString("id"),
                        title = co.reqString("title"),
                        widthWeight = co.optFloat("width-weight") ?: 1.0f,
                    )
                },
                rows = obj.reqArray("rows").map { r ->
                    val ro = r.asObject()
                    TableRow(
                        id = ro.reqString("id"),
                        cells = ro.reqArray("cells").map { it.asString() },
                        style = StyleVariant.fromWireName(ro.optString("style-variant") ?: "primary"),
                    )
                },
                selectedRow = obj.optInt("selected-row"),
                style = style,
            )
            "list" -> WidgetNode.ListWidget(
                id = id,
                items = obj.reqArray("items").map { i ->
                    val io = i.asObject()
                    ListItem(
                        id = io.reqString("id"),
                        label = io.reqString("label"),
                        secondary = io.optString("secondary"),
                        style = StyleVariant.fromWireName(io.optString("style-variant") ?: "primary"),
                    )
                },
                selectedId = obj.optString("selected-id"),
                style = style,
            )
            "form" -> WidgetNode.Form(
                id = id,
                fields = obj.reqArray("fields").map { parseWidgetNode(it.asObject()) },
                submitLabel = obj.optString("submit-label"),
                style = style,
            )
            "text-input" -> WidgetNode.TextInput(
                id = id,
                label = obj.reqString("label"),
                value = obj.optString("value") ?: "",
                placeholder = obj.optString("placeholder") ?: "",
                style = style,
            )
            "select" -> WidgetNode.Select(
                id = id,
                label = obj.reqString("label"),
                options = obj.reqArray("options").map { o ->
                    val oo = o.asObject()
                    SelectOption(oo.reqString("value"), oo.reqString("label"))
                },
                selectedValue = obj.optString("selected-value"),
                style = style,
            )
            "toggle" -> WidgetNode.Toggle(
                id = id,
                label = obj.reqString("label"),
                checked = obj.optBoolean("checked") ?: false,
                style = style,
            )
            "button" -> WidgetNode.Button(
                id = id,
                label = obj.reqString("label"),
                style = style,
                disabled = obj.optBoolean("disabled") ?: false,
            )
            "code-block" -> WidgetNode.CodeBlock(
                id = id,
                code = obj.reqString("code"),
                language = obj.optString("language") ?: "text",
                showLineNumbers = obj.optBoolean("show-line-numbers") ?: true,
                style = style,
            )
            "markdown" -> WidgetNode.Markdown(
                id = id,
                content = obj.reqString("content"),
                style = style,
            )
            "badge" -> WidgetNode.Badge(
                id = id,
                text = obj.reqString("text"),
                style = style,
            )
            else -> error("Unknown WidgetNode type: $type")
        }
    }

    private fun parseTreeNode(obj: JsonObject): TreeNode = TreeNode(
        id = obj.reqString("id"),
        label = obj.reqString("label"),
        icon = obj.optString("icon"),
        expanded = obj.optBoolean("expanded") ?: false,
        style = StyleVariant.fromWireName(obj.optString("style-variant") ?: "primary"),
        children = obj.optArray("children")?.map { parseTreeNode(it.asObject()) } ?: emptyList(),
    )

    private fun StringBuilder.appendField(name: String) {
        appendJsonString(name)
        append(':')
    }

    private fun StringBuilder.appendNullableString(value: String?) {
        if (value == null) append("null") else appendJsonString(value)
    }

    private inline fun <T> StringBuilder.appendArray(
        items: List<T>,
        crossinline render: StringBuilder.(T) -> Unit,
    ) {
        append('[')
        items.forEachIndexed { index, item ->
            if (index > 0) append(',')
            render(item)
        }
        append(']')
    }

    private fun StringBuilder.appendStringMap(map: Map<String, String>) {
        append('{')
        var first = true
        for ((k, v) in map) {
            if (!first) append(',')
            first = false
            appendJsonString(k)
            append(':')
            appendJsonString(v)
        }
        append('}')
    }

    private fun StringBuilder.appendJsonString(value: String) {
        append('"')
        for (ch in value) {
            when (ch) {
                '"' -> append("\\\"")
                '\\' -> append("\\\\")
                '\b' -> append("\\b")
                '\u000C' -> append("\\f")
                '\n' -> append("\\n")
                '\r' -> append("\\r")
                '\t' -> append("\\t")
                else -> {
                    if (ch.code < 0x20) {
                        val hex = ch.code.toString(16).padStart(4, '0')
                        append("\\u").append(hex)
                    } else {
                        append(ch)
                    }
                }
            }
        }
        append('"')
    }
}

private sealed interface JsonValue {
    data object Null : JsonValue
    data class Bool(val value: Boolean) : JsonValue
    data class Num(val raw: String) : JsonValue
    data class Str(val value: String) : JsonValue
    data class Arr(val items: List<JsonValue>) : JsonValue
    data class Obj(val entries: Map<String, JsonValue>) : JsonValue

    fun asObject(): JsonObject =
        (this as? Obj)?.let(::JsonObject) ?: error("Expected JSON object, got $this")

    fun asString(): String =
        (this as? Str)?.value ?: error("Expected JSON string, got $this")
}

private class JsonObject(private val obj: JsonValue.Obj) {
    fun reqString(key: String): String =
        optString(key) ?: error("Missing required string field '$key'")

    fun optString(key: String): String? = when (val v = obj.entries[key]) {
        null, JsonValue.Null -> null
        is JsonValue.Str -> v.value
        else -> error("Field '$key' is not a string")
    }

    fun reqInt(key: String): Int =
        optInt(key) ?: error("Missing required int field '$key'")

    fun optInt(key: String): Int? = when (val v = obj.entries[key]) {
        null, JsonValue.Null -> null
        is JsonValue.Num -> v.raw.toDouble().toInt()
        else -> error("Field '$key' is not a number")
    }

    fun optFloat(key: String): Float? = when (val v = obj.entries[key]) {
        null, JsonValue.Null -> null
        is JsonValue.Num -> v.raw.toFloat()
        else -> error("Field '$key' is not a number")
    }

    fun reqBoolean(key: String): Boolean =
        optBoolean(key) ?: error("Missing required boolean field '$key'")

    fun optBoolean(key: String): Boolean? = when (val v = obj.entries[key]) {
        null, JsonValue.Null -> null
        is JsonValue.Bool -> v.value
        else -> error("Field '$key' is not a boolean")
    }

    fun reqObject(key: String): JsonObject =
        (obj.entries[key] as? JsonValue.Obj)?.let(::JsonObject)
            ?: error("Missing required object field '$key'")

    fun reqArray(key: String): List<JsonValue> =
        optArray(key) ?: error("Missing required array field '$key'")

    fun optArray(key: String): List<JsonValue>? = when (val v = obj.entries[key]) {
        null, JsonValue.Null -> null
        is JsonValue.Arr -> v.items
        else -> error("Field '$key' is not an array")
    }

    fun optStringList(key: String): List<String> =
        optArray(key)?.map { it.asString() } ?: emptyList()

    fun optStringMap(key: String): Map<String, String> {
        val v = obj.entries[key] ?: return emptyMap()
        if (v == JsonValue.Null) return emptyMap()
        val sub = (v as? JsonValue.Obj) ?: error("Field '$key' is not an object")
        return sub.entries.mapValues { (_, valNode) -> valNode.asString() }
    }
}

private class JsonMini(private val input: String) {
    private var pos: Int = 0

    fun parse(): JsonValue {
        skipWs()
        val value = parseValue()
        skipWs()
        if (pos != input.length) {
            error("Unexpected trailing JSON content at index $pos")
        }
        return value
    }

    private fun parseValue(): JsonValue {
        skipWs()
        if (pos >= input.length) error("Unexpected end of JSON")
        return when (val ch = input[pos]) {
            '{' -> parseObjectValue()
            '[' -> parseArrayValue()
            '"' -> JsonValue.Str(parseString())
            't' -> {
                expect("true")
                JsonValue.Bool(true)
            }
            'f' -> {
                expect("false")
                JsonValue.Bool(false)
            }
            'n' -> {
                expect("null")
                JsonValue.Null
            }
            '-', in '0'..'9' -> parseNumber()
            else -> error("Unexpected JSON character '$ch' at index $pos")
        }
    }

    private fun parseObjectValue(): JsonValue.Obj {
        pos++ // '{'
        skipWs()
        val map = linkedMapOf<String, JsonValue>()
        if (peek() == '}') {
            pos++
            return JsonValue.Obj(map)
        }
        while (true) {
            skipWs()
            if (peek() != '"') error("Expected string key at index $pos")
            val key = parseString()
            skipWs()
            if (peek() != ':') error("Expected ':' at index $pos")
            pos++
            val value = parseValue()
            map[key] = value
            skipWs()
            when (peek()) {
                ',' -> pos++
                '}' -> {
                    pos++
                    return JsonValue.Obj(map)
                }
                else -> error("Expected ',' or '}' at index $pos")
            }
        }
    }

    private fun parseArrayValue(): JsonValue.Arr {
        pos++ // '['
        skipWs()
        val items = mutableListOf<JsonValue>()
        if (peek() == ']') {
            pos++
            return JsonValue.Arr(items)
        }
        while (true) {
            items += parseValue()
            skipWs()
            when (peek()) {
                ',' -> pos++
                ']' -> {
                    pos++
                    return JsonValue.Arr(items)
                }
                else -> error("Expected ',' or ']' at index $pos")
            }
        }
    }

    private fun parseString(): String {
        pos++ // '"'
        val sb = StringBuilder()
        while (pos < input.length) {
            val ch = input[pos++]
            when (ch) {
                '"' -> return sb.toString()
                '\\' -> {
                    if (pos >= input.length) error("Unterminated escape sequence")
                    when (val esc = input[pos++]) {
                        '"' -> sb.append('"')
                        '\\' -> sb.append('\\')
                        '/' -> sb.append('/')
                        'b' -> sb.append('\b')
                        'f' -> sb.append('\u000C')
                        'n' -> sb.append('\n')
                        'r' -> sb.append('\r')
                        't' -> sb.append('\t')
                        'u' -> {
                            if (pos + 4 > input.length) error("Invalid \\u escape")
                            val code = input.substring(pos, pos + 4).toInt(16)
                            sb.append(code.toChar())
                            pos += 4
                        }
                        else -> error("Unknown escape '\\$esc'")
                    }
                }
                else -> sb.append(ch)
            }
        }
        error("Unterminated JSON string")
    }

    private fun parseNumber(): JsonValue.Num {
        val start = pos
        if (peek() == '-') pos++
        while (pos < input.length && input[pos] in '0'..'9') pos++
        if (pos < input.length && input[pos] == '.') {
            pos++
            while (pos < input.length && input[pos] in '0'..'9') pos++
        }
        if (pos < input.length && (input[pos] == 'e' || input[pos] == 'E')) {
            pos++
            if (pos < input.length && (input[pos] == '+' || input[pos] == '-')) pos++
            while (pos < input.length && input[pos] in '0'..'9') pos++
        }
        return JsonValue.Num(input.substring(start, pos))
    }

    private fun expect(literal: String) {
        if (!input.startsWith(literal, pos)) {
            error("Expected '$literal' at index $pos")
        }
        pos += literal.length
    }

    private fun peek(): Char? = if (pos < input.length) input[pos] else null

    private fun skipWs() {
        while (pos < input.length && input[pos].isWhitespace()) {
            pos++
        }
    }

    companion object {
        fun parseObject(json: String): JsonObject =
            JsonMini(json).parse().asObject()
    }
}
