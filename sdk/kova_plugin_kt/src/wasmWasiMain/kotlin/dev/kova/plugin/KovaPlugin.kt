package dev.kova.plugin

/**
 * Semantic style token mapped by `kova_ui` to the active `kova_theme` palette.
 *
 * Plugins must never specify raw HEX colors or pixel sizes (KOVA-045 / Headless UI contract).
 */
enum class StyleVariant(val wireName: String) {
    PRIMARY("primary"),
    MUTED("muted"),
    WARNING("warning"),
    ERROR("error"),
    ACCENT("accent");

    companion object {
        fun fromWireName(name: String): StyleVariant =
            entries.firstOrNull { it.wireName == name } ?: PRIMARY
    }
}

/**
 * Orientation for [WidgetNode.Split].
 */
enum class SplitOrientation(val wireName: String) {
    HORIZONTAL("horizontal"),
    VERTICAL("vertical");

    companion object {
        fun fromWireName(name: String): SplitOrientation =
            entries.firstOrNull { it.wireName == name } ?: HORIZONTAL
    }
}

/**
 * Docking location for a plugin Tool Window in IntelliJ New UI / Focus layout.
 */
enum class ToolWindowLocation(val wireName: String) {
    LEFT("left"),
    RIGHT("right"),
    BOTTOM("bottom");

    companion object {
        fun fromWireName(name: String): ToolWindowLocation =
            entries.firstOrNull { it.wireName == name } ?: RIGHT
    }
}

/**
 * Plugin metadata exposed to `kova_plugin_host` during registration.
 */
data class PluginInfo(
    val id: String,
    val name: String,
    val version: String,
    val description: String = "",
    val apiVersion: String = "0.1.0",
)

/**
 * Declaration of a dockable Tool Window provided by the plugin.
 */
data class ToolWindowSpec(
    val id: String,
    val title: String,
    val icon: String = "puzzle",
    val location: ToolWindowLocation = ToolWindowLocation.RIGHT,
)

/**
 * Declaration of a command registered in the Kova Command Palette (`Shift+Shift`).
 */
data class CommandSpec(
    val id: String,
    val title: String,
    val description: String = "",
)

/**
 * Read-only snapshot of the currently active editor buffer and workspace context.
 */
data class EditorSnapshot(
    val filePath: String? = null,
    val language: String? = null,
    val text: String = "",
    val cursorLine: Int = 0,
    val cursorColumn: Int = 0,
    val workspaceRoot: String? = null,
)

/**
 * Tab entry inside [WidgetNode.Tabs].
 */
data class TabItem(
    val id: String,
    val title: String,
    val content: WidgetNode,
)

/**
 * Hierarchical node inside [WidgetNode.Tree].
 */
data class TreeNode(
    val id: String,
    val label: String,
    val icon: String? = null,
    val expanded: Boolean = false,
    val style: StyleVariant = StyleVariant.PRIMARY,
    val children: List<TreeNode> = emptyList(),
)

/**
 * Column definition inside [WidgetNode.Table].
 */
data class TableColumn(
    val id: String,
    val title: String,
    val widthWeight: Float = 1.0f,
)

/**
 * Row data inside [WidgetNode.Table].
 */
data class TableRow(
    val id: String,
    val cells: List<String>,
    val style: StyleVariant = StyleVariant.PRIMARY,
)

/**
 * Entry inside [WidgetNode.ListWidget].
 */
data class ListItem(
    val id: String,
    val label: String,
    val secondary: String? = null,
    val style: StyleVariant = StyleVariant.PRIMARY,
)

/**
 * Option inside [WidgetNode.Select].
 */
data class SelectOption(
    val value: String,
    val label: String,
)

/**
 * Declarative UI widget tree rendered natively by GPUI (`kova_ui`) with zero WebView.
 *
 * Covers the 9 WIT base component types (`Split`, `Tabs`, `Tree`, `Table`, `ListWidget`,
 * `Form` + `TextInput`/`Select`/`Toggle`/`Button`, `CodeBlock`, `Markdown`, `Badge`).
 */
sealed interface WidgetNode {
    val id: String
    val style: StyleVariant

    data class Split(
        override val id: String,
        val orientation: SplitOrientation = SplitOrientation.HORIZONTAL,
        val ratio: Float = 0.5f,
        val first: WidgetNode,
        val second: WidgetNode,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Tabs(
        override val id: String,
        val tabs: List<TabItem>,
        val activeIndex: Int = 0,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Tree(
        override val id: String,
        val nodes: List<TreeNode>,
        val selectedId: String? = null,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Table(
        override val id: String,
        val columns: List<TableColumn>,
        val rows: List<TableRow>,
        val selectedRow: Int? = null,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class ListWidget(
        override val id: String,
        val items: List<ListItem>,
        val selectedId: String? = null,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Form(
        override val id: String,
        val fields: List<WidgetNode>,
        val submitLabel: String? = null,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class TextInput(
        override val id: String,
        val label: String,
        val value: String = "",
        val placeholder: String = "",
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Select(
        override val id: String,
        val label: String,
        val options: List<SelectOption>,
        val selectedValue: String? = null,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Toggle(
        override val id: String,
        val label: String,
        val checked: Boolean = false,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Button(
        override val id: String,
        val label: String,
        override val style: StyleVariant = StyleVariant.ACCENT,
        val disabled: Boolean = false,
    ) : WidgetNode

    data class CodeBlock(
        override val id: String,
        val code: String,
        val language: String = "text",
        val showLineNumbers: Boolean = true,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Markdown(
        override val id: String,
        val content: String,
        override val style: StyleVariant = StyleVariant.PRIMARY,
    ) : WidgetNode

    data class Badge(
        override val id: String,
        val text: String,
        override val style: StyleVariant = StyleVariant.ACCENT,
    ) : WidgetNode
}

/**
 * Interactive UI event dispatched from `kova_ui` through `kova_plugin_host` to the plugin.
 */
sealed interface UiEvent {
    val widgetId: String

    data class Click(
        override val widgetId: String,
    ) : UiEvent

    data class TextChanged(
        override val widgetId: String,
        val value: String,
    ) : UiEvent

    data class SelectionChanged(
        override val widgetId: String,
        val selectedValue: String,
        val selectedIndex: Int? = null,
    ) : UiEvent

    data class Toggled(
        override val widgetId: String,
        val checked: Boolean,
    ) : UiEvent

    data class TabChanged(
        override val widgetId: String,
        val activeIndex: Int,
    ) : UiEvent

    data class TreeNodeSelected(
        override val widgetId: String,
        val nodeId: String,
    ) : UiEvent

    data class TreeNodeToggled(
        override val widgetId: String,
        val nodeId: String,
        val expanded: Boolean,
    ) : UiEvent

    data class TableRowSelected(
        override val widgetId: String,
        val rowIndex: Int,
    ) : UiEvent

    data class ListItemSelected(
        override val widgetId: String,
        val itemId: String,
        val index: Int,
    ) : UiEvent

    data class FormSubmitted(
        override val widgetId: String,
        val values: Map<String, String> = emptyMap(),
    ) : UiEvent
}

/**
 * Lifecycle or workspace event emitted by `kova_plugin_host`.
 */
sealed interface PluginEvent {
    data class WorkspaceOpened(val workspaceRoot: String) : PluginEvent
    data class BufferSaved(val filePath: String) : PluginEvent
    data class ActiveEditorChanged(val filePath: String?, val language: String?) : PluginEvent
    data class Custom(val kind: String, val payload: String) : PluginEvent
}

/**
 * Capability-restricted bridge back into the Kova host.
 */
interface HostContext {
    fun editorSnapshot(): EditorSnapshot?
    fun workspaceRoot(): String?
    fun logInfo(message: String)
    fun logWarn(message: String)
    fun logError(message: String)
    fun requestRerender(toolWindowId: String)
}

/**
 * Primary contract implemented by Kotlin/Wasm plugins for Kova IDE.
 */
interface KovaPlugin {
    fun info(): PluginInfo
    fun toolWindows(): List<ToolWindowSpec> = emptyList()
    fun commands(): List<CommandSpec> = emptyList()
    fun init(ctx: HostContext) {}
    fun shutdown() {}
    fun renderUi(toolWindowId: String, ctx: HostContext): WidgetNode
    fun onUiEvent(event: UiEvent, ctx: HostContext): WidgetNode? = null
    fun onCommand(commandId: String, args: List<String>, ctx: HostContext) {}
    fun onEvent(event: PluginEvent, ctx: HostContext) {}
}

@DslMarker
annotation class KovaUiDsl

/**
 * Entry point for building a declarative [WidgetNode] tree using Kotlin DSL.
 */
inline fun kovaUi(block: KovaUiScope.() -> WidgetNode): WidgetNode =
    KovaUiScope.block()

@KovaUiDsl
object KovaUiScope {
    fun split(
        id: String,
        orientation: SplitOrientation = SplitOrientation.HORIZONTAL,
        ratio: Float = 0.5f,
        style: StyleVariant = StyleVariant.PRIMARY,
        first: KovaUiScope.() -> WidgetNode,
        second: KovaUiScope.() -> WidgetNode,
    ): WidgetNode.Split = WidgetNode.Split(
        id = id,
        orientation = orientation,
        ratio = ratio.coerceIn(0.05f, 0.95f),
        first = first(),
        second = second(),
        style = style,
    )

    fun tabs(
        id: String,
        activeIndex: Int = 0,
        style: StyleVariant = StyleVariant.PRIMARY,
        block: TabsBuilder.() -> Unit,
    ): WidgetNode.Tabs {
        val builder = TabsBuilder().apply(block)
        return WidgetNode.Tabs(
            id = id,
            tabs = builder.items.toList(),
            activeIndex = activeIndex,
            style = style,
        )
    }

    fun tree(
        id: String,
        selectedId: String? = null,
        style: StyleVariant = StyleVariant.PRIMARY,
        block: TreeBuilder.() -> Unit,
    ): WidgetNode.Tree {
        val builder = TreeBuilder().apply(block)
        return WidgetNode.Tree(
            id = id,
            nodes = builder.nodes.toList(),
            selectedId = selectedId,
            style = style,
        )
    }

    fun table(
        id: String,
        selectedRow: Int? = null,
        style: StyleVariant = StyleVariant.PRIMARY,
        block: TableBuilder.() -> Unit,
    ): WidgetNode.Table {
        val builder = TableBuilder().apply(block)
        return WidgetNode.Table(
            id = id,
            columns = builder.columns.toList(),
            rows = builder.rows.toList(),
            selectedRow = selectedRow,
            style = style,
        )
    }

    fun list(
        id: String,
        selectedId: String? = null,
        style: StyleVariant = StyleVariant.PRIMARY,
        block: ListBuilder.() -> Unit,
    ): WidgetNode.ListWidget {
        val builder = ListBuilder().apply(block)
        return WidgetNode.ListWidget(
            id = id,
            items = builder.items.toList(),
            selectedId = selectedId,
            style = style,
        )
    }

    fun form(
        id: String,
        submitLabel: String? = null,
        style: StyleVariant = StyleVariant.PRIMARY,
        block: FormBuilder.() -> Unit,
    ): WidgetNode.Form {
        val builder = FormBuilder().apply(block)
        return WidgetNode.Form(
            id = id,
            fields = builder.fields.toList(),
            submitLabel = submitLabel,
            style = style,
        )
    }

    fun textInput(
        id: String,
        label: String,
        value: String = "",
        placeholder: String = "",
        style: StyleVariant = StyleVariant.PRIMARY,
    ): WidgetNode.TextInput = WidgetNode.TextInput(
        id = id,
        label = label,
        value = value,
        placeholder = placeholder,
        style = style,
    )

    fun select(
        id: String,
        label: String,
        selectedValue: String? = null,
        style: StyleVariant = StyleVariant.PRIMARY,
        block: SelectBuilder.() -> Unit,
    ): WidgetNode.Select {
        val builder = SelectBuilder().apply(block)
        return WidgetNode.Select(
            id = id,
            label = label,
            options = builder.options.toList(),
            selectedValue = selectedValue,
            style = style,
        )
    }

    fun toggle(
        id: String,
        label: String,
        checked: Boolean = false,
        style: StyleVariant = StyleVariant.PRIMARY,
    ): WidgetNode.Toggle = WidgetNode.Toggle(
        id = id,
        label = label,
        checked = checked,
        style = style,
    )

    fun button(
        id: String,
        label: String,
        style: StyleVariant = StyleVariant.ACCENT,
        disabled: Boolean = false,
    ): WidgetNode.Button = WidgetNode.Button(
        id = id,
        label = label,
        style = style,
        disabled = disabled,
    )

    fun codeBlock(
        id: String,
        code: String,
        language: String = "text",
        showLineNumbers: Boolean = true,
        style: StyleVariant = StyleVariant.PRIMARY,
    ): WidgetNode.CodeBlock = WidgetNode.CodeBlock(
        id = id,
        code = code,
        language = language,
        showLineNumbers = showLineNumbers,
        style = style,
    )

    fun markdown(
        id: String,
        content: String,
        style: StyleVariant = StyleVariant.PRIMARY,
    ): WidgetNode.Markdown = WidgetNode.Markdown(
        id = id,
        content = content,
        style = style,
    )

    fun badge(
        id: String,
        text: String,
        style: StyleVariant = StyleVariant.ACCENT,
    ): WidgetNode.Badge = WidgetNode.Badge(
        id = id,
        text = text,
        style = style,
    )
}

@KovaUiDsl
class TabsBuilder {
    internal val items = mutableListOf<TabItem>()

    fun tab(id: String, title: String, content: KovaUiScope.() -> WidgetNode) {
        items += TabItem(id = id, title = title, content = KovaUiScope.content())
    }
}

@KovaUiDsl
class TreeBuilder {
    internal val nodes = mutableListOf<TreeNode>()

    fun node(
        id: String,
        label: String,
        icon: String? = null,
        expanded: Boolean = false,
        style: StyleVariant = StyleVariant.PRIMARY,
        children: TreeBuilder.() -> Unit = {},
    ) {
        val childBuilder = TreeBuilder().apply(children)
        nodes += TreeNode(
            id = id,
            label = label,
            icon = icon,
            expanded = expanded,
            style = style,
            children = childBuilder.nodes.toList(),
        )
    }
}

@KovaUiDsl
class TableBuilder {
    internal val columns = mutableListOf<TableColumn>()
    internal val rows = mutableListOf<TableRow>()

    fun column(id: String, title: String, widthWeight: Float = 1.0f) {
        columns += TableColumn(id = id, title = title, widthWeight = widthWeight)
    }

    fun row(id: String, vararg cells: String, style: StyleVariant = StyleVariant.PRIMARY) {
        rows += TableRow(id = id, cells = cells.toList(), style = style)
    }

    fun row(id: String, cells: List<String>, style: StyleVariant = StyleVariant.PRIMARY) {
        rows += TableRow(id = id, cells = cells, style = style)
    }
}

@KovaUiDsl
class ListBuilder {
    internal val items = mutableListOf<ListItem>()

    fun item(
        id: String,
        label: String,
        secondary: String? = null,
        style: StyleVariant = StyleVariant.PRIMARY,
    ) {
        items += ListItem(id = id, label = label, secondary = secondary, style = style)
    }
}

@KovaUiDsl
class FormBuilder {
    internal val fields = mutableListOf<WidgetNode>()

    fun field(node: WidgetNode) {
        fields += node
    }

    fun textInput(
        id: String,
        label: String,
        value: String = "",
        placeholder: String = "",
        style: StyleVariant = StyleVariant.PRIMARY,
    ) {
        fields += WidgetNode.TextInput(id, label, value, placeholder, style)
    }

    fun select(
        id: String,
        label: String,
        selectedValue: String? = null,
        style: StyleVariant = StyleVariant.PRIMARY,
        block: SelectBuilder.() -> Unit,
    ) {
        fields += KovaUiScope.select(id, label, selectedValue, style, block)
    }

    fun toggle(
        id: String,
        label: String,
        checked: Boolean = false,
        style: StyleVariant = StyleVariant.PRIMARY,
    ) {
        fields += WidgetNode.Toggle(id, label, checked, style)
    }

    fun button(
        id: String,
        label: String,
        style: StyleVariant = StyleVariant.ACCENT,
        disabled: Boolean = false,
    ) {
        fields += WidgetNode.Button(id, label, style, disabled)
    }

    fun badge(
        id: String,
        text: String,
        style: StyleVariant = StyleVariant.ACCENT,
    ) {
        fields += WidgetNode.Badge(id, text, style)
    }

    fun markdown(
        id: String,
        content: String,
        style: StyleVariant = StyleVariant.PRIMARY,
    ) {
        fields += WidgetNode.Markdown(id, content, style)
    }
}

@KovaUiDsl
class SelectBuilder {
    internal val options = mutableListOf<SelectOption>()

    fun option(value: String, label: String = value) {
        options += SelectOption(value = value, label = label)
    }
}
