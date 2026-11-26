//! Domain model for JVM types, members, visibility, source origins, and FQCN helpers.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Classification of an indexed JVM type or member symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JvmSymbolKind {
    /// Standard concrete or abstract class.
    Class,
    /// Java/Kotlin interface.
    Interface,
    /// Java/Kotlin enum type.
    Enum,
    /// Java 16+ record class.
    Record,
    /// Java `@interface` / Kotlin `annotation class`.
    Annotation,
    /// Kotlin `object` or `companion object` singleton.
    Object,
    /// Class or interface method.
    Method,
    /// Constructor (`<init>`).
    Constructor,
    /// Class or interface field / enum constant / record component backing field.
    Field,
}

impl JvmSymbolKind {
    /// Returns `true` if this kind represents a top-level or nested JVM type declaration.
    #[must_use]
    pub const fn is_type(self) -> bool {
        matches!(
            self,
            Self::Class
                | Self::Interface
                | Self::Enum
                | Self::Record
                | Self::Annotation
                | Self::Object
        )
    }

    /// Returns `true` if this kind represents a member of a type (method, constructor, or field).
    #[must_use]
    pub const fn is_member(self) -> bool {
        matches!(self, Self::Method | Self::Constructor | Self::Field)
    }
}

/// Access visibility of a JVM type or member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum JvmVisibility {
    /// `public` access.
    Public,
    /// `protected` access.
    Protected,
    /// Package-private (default Java visibility when no modifier is present).
    #[default]
    PackagePrivate,
    /// `private` access.
    Private,
    /// Kotlin `internal` module visibility.
    Internal,
}

/// Normalized modifier flags for a JVM type or member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "JVM modifier flags map directly to independent boolean predicates specified by KOVA-030"
)]
pub struct JvmModifiers {
    /// Effective access visibility (resolved via `InnerClasses` for nested types).
    pub visibility: JvmVisibility,
    /// Whether the `static` modifier is set.
    pub is_static: bool,
    /// Whether the `final` modifier is set.
    pub is_final: bool,
    /// Whether the `abstract` modifier is set.
    pub is_abstract: bool,
    /// Whether the symbol is marked `ACC_SYNTHETIC` or carries a `Synthetic` attribute.
    pub is_synthetic: bool,
    /// Whether the symbol carries a `Deprecated` attribute.
    pub is_deprecated: bool,
    /// Whether the type is `sealed` (carries a `PermittedSubclasses` attribute).
    pub is_sealed: bool,
}

/// Origin of an indexed JVM symbol, used for Go-to-Definition and Vineflower routing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JvmSourceOrigin {
    /// Project source file (`.java`, `.kt`, `.kts`) relative to the workspace root.
    ProjectSource {
        /// Path relative to `project_root`.
        rel_path: PathBuf,
        /// 0-based line (row) of the declaration, as reported by the source scanner.
        line: u32,
        /// 0-based column of the declaration, as reported by the source scanner.
        column: u32,
    },
    /// Compiled `.class` entry inside a `.jar` or `.zip` library archive.
    JarEntry {
        /// Filesystem path to the `.jar` archive.
        jar_path: PathBuf,
        /// Archive-relative entry name (e.g. `"org/springframework/stereotype/Service.class"`).
        entry_name: String,
    },
    /// Compiled `.class` entry inside a JDK `.jmod` module archive.
    JmodEntry {
        /// Filesystem path to the `.jmod` archive.
        jmod_path: PathBuf,
        /// Archive-relative entry name (e.g. `"classes/java/util/ArrayList.class"`).
        entry_name: String,
    },
    /// Standalone compiled `.class` file on disk (e.g. `target/classes/...`).
    ClassFile {
        /// Filesystem path to the `.class` file.
        path: PathBuf,
    },
}

/// Indexed metadata for a JVM class, interface, enum, record, annotation, or Kotlin object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JvmTypeEntry {
    /// Canonical dot-separated qualified name (e.g. `"com.example.OrderService.LineItem"`).
    pub fqcn: String,
    /// JVM binary name with `$` preserved for nested types (e.g. `"com.example.OrderService$LineItem"`).
    pub binary_name: String,
    /// Unqualified simple name (e.g. `"LineItem"`).
    pub simple_name: String,
    /// Package name, or `""` for the default package (e.g. `"com.example"`).
    pub package_name: String,
    /// Kind of JVM type.
    pub kind: JvmSymbolKind,
    /// Access visibility and modifier flags.
    pub modifiers: JvmModifiers,
    /// Canonical FQCN of the superclass, if any (`None` for `java.lang.Object`).
    pub super_fqcn: Option<String>,
    /// Canonical FQCNs of directly implemented or extended interfaces.
    pub interfaces: Vec<String>,
    /// Where this type was discovered.
    pub origin: JvmSourceOrigin,
}

/// Indexed metadata for a method, constructor, or field belonging to a [`JvmTypeEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JvmMemberEntry {
    /// Canonical FQCN of the declaring [`JvmTypeEntry`].
    pub owner_fqcn: String,
    /// Display name of the member (or class simple name for constructors).
    pub name: String,
    /// Member kind (`Method`, `Constructor`, or `Field`).
    pub kind: JvmSymbolKind,
    /// Access visibility and modifier flags.
    pub modifiers: JvmModifiers,
    /// Raw JVM descriptor or source-level signature string.
    pub descriptor: String,
    /// Human-readable decoded signature (e.g. `"(String, int): List<String>"` or `"long"`).
    pub display_signature: String,
    /// Optional 1-based declaration line number (from source AST or `LineNumberTable`).
    pub line: Option<u32>,
}

/// Result of parsing a single `.class` file into its type entry and members.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParsedClassUnit {
    /// Indexed type descriptor.
    pub ty: JvmTypeEntry,
    /// Indexed non-synthetic members declared on `ty`.
    pub members: Vec<JvmMemberEntry>,
}

/// Converts a JVM internal name (e.g. `"com/example/OrderService$LineItem"`) into a
/// canonical dot-separated FQCN (e.g. `"com.example.OrderService.LineItem"`).
#[must_use]
pub fn internal_name_to_fqcn(internal_name: &str) -> String {
    internal_name.replace(['/', '$'], ".")
}

/// Converts a JVM internal name (e.g. `"java/util/Map$Entry"`) into a JVM binary name
/// preserving `$` for nested classes (e.g. `"java.util.Map$Entry"`).
#[must_use]
pub fn internal_name_to_binary(internal_name: &str) -> String {
    internal_name.replace('/', ".")
}

/// Splits a JVM binary or qualified name (e.g. `"com.example.Order$Item"`) into
/// `(package_name, simple_name)` (e.g. `("com.example", "Item")`).
///
/// For types in the default package (no `.`), `package_name` is `""`.
/// For nested types separated by `$`, `simple_name` is the innermost non-empty segment.
#[must_use]
pub fn split_package_and_simple(name: &str) -> (String, String) {
    let (package, class_part) = match name.rsplit_once('.') {
        Some((pkg, cls)) => (pkg, cls),
        None => ("", name),
    };
    let simple = class_part
        .rsplit('$')
        .find(|segment| !segment.is_empty())
        .unwrap_or(class_part);
    (package.to_owned(), simple.to_owned())
}

/// Alias for [`split_package_and_simple`] matching the specification helper name.
#[must_use]
pub fn split_package_and_simple_name(name: &str) -> (String, String) {
    split_package_and_simple(name)
}

/// Returns `true` if a JVM internal or binary class name denotes compiler-generated
/// noise that should be excluded from symbol indexing.
///
/// Rejects:
/// - empty names, `module-info`, and `package-info` (including nested packages),
/// - anonymous classes whose `$`-delimited segment is purely ASCII digits (`Foo$1`, `Outer$Inner$2`),
/// - synthetic lambda classes (`$$Lambda$...`, `$$Lambda/...`, `...$$Lambda`).
///
/// Preserves valid named nested classes such as `Outer$Inner`, `MyClass$Companion`,
/// `Foo$Bar`, and Scala/Kotlin singleton names ending in `$`.
#[must_use]
pub fn is_ignored_internal_name(internal_name: &str) -> bool {
    if internal_name.is_empty() {
        return true;
    }

    let class_part = internal_name
        .rsplit(['/', '.'])
        .next()
        .unwrap_or(internal_name);

    if class_part.is_empty() || class_part == "module-info" || class_part == "package-info" {
        return true;
    }

    if class_part.contains("$$Lambda$")
        || internal_name.contains("$$Lambda/")
        || class_part.ends_with("$$Lambda")
    {
        return true;
    }

    class_part
        .split('$')
        .skip(1)
        .any(|segment| !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Alias for [`is_ignored_internal_name`] matching the specification helper name.
#[must_use]
pub fn is_compiler_generated_class_name(name: &str) -> bool {
    is_ignored_internal_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_internal_names_to_fqcn_and_binary() {
        assert_eq!(
            internal_name_to_fqcn("com/example/orders/OrderService$LineItem"),
            "com.example.orders.OrderService.LineItem"
        );
        assert_eq!(
            internal_name_to_binary("com/example/orders/OrderService$LineItem"),
            "com.example.orders.OrderService$LineItem"
        );
        assert_eq!(internal_name_to_fqcn("TopLevel"), "TopLevel");
        assert_eq!(internal_name_to_binary("TopLevel"), "TopLevel");
    }

    #[test]
    fn splits_package_and_simple_name_including_default_package_and_nested() {
        assert_eq!(
            split_package_and_simple("com.example.Order$Item"),
            ("com.example".to_owned(), "Item".to_owned())
        );
        assert_eq!(
            split_package_and_simple_name("com.example.Order$Sub$Leaf"),
            ("com.example".to_owned(), "Leaf".to_owned())
        );
        assert_eq!(
            split_package_and_simple("DefaultPkgClass"),
            (String::new(), "DefaultPkgClass".to_owned())
        );
        assert_eq!(
            split_package_and_simple("Outer$Inner"),
            (String::new(), "Inner".to_owned())
        );
        assert_eq!(
            split_package_and_simple("com.example.ScalaCompanion$"),
            ("com.example".to_owned(), "ScalaCompanion".to_owned())
        );
    }

    #[test]
    fn filters_compiler_generated_and_preserves_named_nested_classes() {
        assert!(is_ignored_internal_name(""));
        assert!(is_ignored_internal_name("module-info"));
        assert!(is_ignored_internal_name("META-INF/versions/9/module-info"));
        assert!(is_ignored_internal_name("package-info"));
        assert!(is_ignored_internal_name("com/example/package-info"));
        assert!(is_ignored_internal_name("com/example/Foo$1"));
        assert!(is_ignored_internal_name("com/example/Outer$Inner$2"));
        assert!(is_ignored_internal_name("com/example/Foo$1$Nested"));
        assert!(is_ignored_internal_name("com/example/Foo$$Lambda$1"));
        assert!(is_ignored_internal_name(
            "com/example/Foo$$Lambda/0x0000000800c01000"
        ));
        assert!(is_compiler_generated_class_name("com.example.Foo$1"));

        assert!(!is_ignored_internal_name("com/example/Outer$Inner"));
        assert!(!is_ignored_internal_name("com/example/MyClass$Companion"));
        assert!(!is_ignored_internal_name("com/example/Foo$Bar"));
        assert!(!is_ignored_internal_name("com/example/ScalaObject$"));
        assert!(!is_ignored_internal_name("com/example/Foo$1Local"));
        assert!(!is_compiler_generated_class_name("com.example.Outer$Inner"));
    }

    #[test]
    fn symbol_kind_classification_helpers_work() {
        assert!(JvmSymbolKind::Class.is_type());
        assert!(JvmSymbolKind::Record.is_type());
        assert!(JvmSymbolKind::Object.is_type());
        assert!(!JvmSymbolKind::Class.is_member());

        assert!(JvmSymbolKind::Method.is_member());
        assert!(JvmSymbolKind::Constructor.is_member());
        assert!(JvmSymbolKind::Field.is_member());
        assert!(!JvmSymbolKind::Method.is_type());
    }
}
