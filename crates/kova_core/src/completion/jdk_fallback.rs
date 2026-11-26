//! Deterministic built-in JDK fallback catalog (`KOVA-052` §FR-2).
//!
//! Guarantees instant (`< 1 ms`) completion for core `java.lang`, `java.util`, `java.io`,
//! `java.nio.file`, `java.time`, and `java.util.stream` classes even when no local JDK is
//! installed or `.jmod` archives have not been loaded yet.

use crate::completion::{CompletionProposal, CompletionProposalKind, CompletionSource};
use crate::navigation::resolver::ImportEntry;

/// One built-in fallback member of a standard JDK class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JdkFallbackMember {
    /// Member identifier (`requireNonNull`, `out`, `length`, ...).
    pub name: &'static str,
    /// Proposal kind (`Method` or `Field`).
    pub kind: CompletionProposalKind,
    /// Parameter signature (`Some("(T obj)")` for methods, `None` for fields).
    pub params_signature: Option<&'static str>,
    /// Return type or field type.
    pub return_type: &'static str,
    /// Whether this member is `static`.
    pub is_static: bool,
    /// Whether this member is `@Deprecated`.
    pub is_deprecated: bool,
}

/// Built-in metadata for one standard JDK class or interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JdkFallbackClass {
    /// Canonical FQCN (e.g. `"java.util.Objects"`).
    pub fqcn: &'static str,
    /// Simple class name (e.g. `"Objects"`).
    pub simple_name: &'static str,
    /// Declared members on this class.
    pub members: &'static [JdkFallbackMember],
    /// Superclass FQCNs / implemented interfaces to inherit from.
    pub supers: &'static [&'static str],
}

const fn m(
    name: &'static str,
    params: &'static str,
    ret: &'static str,
    is_static: bool,
) -> JdkFallbackMember {
    JdkFallbackMember {
        name,
        kind: CompletionProposalKind::Method,
        params_signature: Some(params),
        return_type: ret,
        is_static,
        is_deprecated: false,
    }
}

const fn f(name: &'static str, ty: &'static str, is_static: bool) -> JdkFallbackMember {
    JdkFallbackMember {
        name,
        kind: CompletionProposalKind::Field,
        params_signature: None,
        return_type: ty,
        is_static,
        is_deprecated: false,
    }
}

const OBJECT_MEMBERS: &[JdkFallbackMember] = &[
    m("toString", "()", "String", false),
    m("equals", "(Object obj)", "boolean", false),
    m("hashCode", "()", "int", false),
    m("getClass", "()", "Class<?>", false),
    m("notify", "()", "void", false),
    m("notifyAll", "()", "void", false),
    m("wait", "()", "void", false),
    m("wait", "(long timeoutMillis)", "void", false),
];

const OBJECTS_MEMBERS: &[JdkFallbackMember] = &[
    m("requireNonNull", "(T obj)", "T", true),
    m("requireNonNull", "(T obj, String message)", "T", true),
    m(
        "requireNonNull",
        "(T obj, Supplier<String> messageSupplier)",
        "T",
        true,
    ),
    m("requireNonNullElse", "(T obj, T defaultObj)", "T", true),
    m(
        "requireNonNullElseGet",
        "(T obj, Supplier<? extends T> supplier)",
        "T",
        true,
    ),
    m("isNull", "(Object obj)", "boolean", true),
    m("nonNull", "(Object obj)", "boolean", true),
    m("equals", "(Object a, Object b)", "boolean", true),
    m("deepEquals", "(Object a, Object b)", "boolean", true),
    m("hashCode", "(Object o)", "int", true),
    m("hash", "(Object... values)", "int", true),
    m("toString", "(Object o)", "String", true),
    m("toString", "(Object o, String nullDefault)", "String", true),
    m("toIdentityString", "(Object o)", "String", true),
    m(
        "compare",
        "(T a, T b, Comparator<? super T> c)",
        "int",
        true,
    ),
    m("checkIndex", "(int index, int length)", "int", true),
    m(
        "checkFromToIndex",
        "(int fromIndex, int toIndex, int length)",
        "int",
        true,
    ),
    m(
        "checkFromIndexSize",
        "(int fromIndex, int size, int length)",
        "int",
        true,
    ),
];

const STRING_MEMBERS: &[JdkFallbackMember] = &[
    m("length", "()", "int", false),
    m("isEmpty", "()", "boolean", false),
    m("isBlank", "()", "boolean", false),
    m("charAt", "(int index)", "char", false),
    m("substring", "(int beginIndex)", "String", false),
    m(
        "substring",
        "(int beginIndex, int endIndex)",
        "String",
        false,
    ),
    m("contains", "(CharSequence s)", "boolean", false),
    m("startsWith", "(String prefix)", "boolean", false),
    m("endsWith", "(String suffix)", "boolean", false),
    m("indexOf", "(String str)", "int", false),
    m("lastIndexOf", "(String str)", "int", false),
    m(
        "replace",
        "(CharSequence target, CharSequence replacement)",
        "String",
        false,
    ),
    m(
        "replaceAll",
        "(String regex, String replacement)",
        "String",
        false,
    ),
    m("split", "(String regex)", "String[]", false),
    m("trim", "()", "String", false),
    m("strip", "()", "String", false),
    m("toLowerCase", "()", "String", false),
    m("toUpperCase", "()", "String", false),
    m("equals", "(Object anObject)", "boolean", false),
    m(
        "equalsIgnoreCase",
        "(String anotherString)",
        "boolean",
        false,
    ),
    m("compareTo", "(String anotherString)", "int", false),
    m("chars", "()", "IntStream", false),
    m("lines", "()", "Stream<String>", false),
    m("repeat", "(int count)", "String", false),
    m("formatted", "(Object... args)", "String", false),
    m("getBytes", "()", "byte[]", false),
    m("toCharArray", "()", "char[]", false),
    m("matches", "(String regex)", "boolean", false),
    m("valueOf", "(Object obj)", "String", true),
    m("format", "(String format, Object... args)", "String", true),
    m(
        "join",
        "(CharSequence delimiter, CharSequence... elements)",
        "String",
        true,
    ),
];

const STRING_BUILDER_MEMBERS: &[JdkFallbackMember] = &[
    m("append", "(String str)", "StringBuilder", false),
    m("append", "(Object obj)", "StringBuilder", false),
    m("insert", "(int offset, String str)", "StringBuilder", false),
    m("delete", "(int start, int end)", "StringBuilder", false),
    m("deleteCharAt", "(int index)", "StringBuilder", false),
    m(
        "replace",
        "(int start, int end, String str)",
        "StringBuilder",
        false,
    ),
    m("reverse", "()", "StringBuilder", false),
    m("length", "()", "int", false),
    m("charAt", "(int index)", "char", false),
    m("substring", "(int start)", "String", false),
    m("toString", "()", "String", false),
    m("setLength", "(int newLength)", "void", false),
    m("capacity", "()", "int", false),
];

const SYSTEM_MEMBERS: &[JdkFallbackMember] = &[
    f("out", "PrintStream", true),
    f("err", "PrintStream", true),
    f("in", "InputStream", true),
    m("currentTimeMillis", "()", "long", true),
    m("nanoTime", "()", "long", true),
    m("getenv", "(String name)", "String", true),
    m("getProperty", "(String key)", "String", true),
    m("setProperty", "(String key, String value)", "String", true),
    m("exit", "(int status)", "void", true),
    m(
        "arraycopy",
        "(Object src, int srcPos, Object dest, int destPos, int length)",
        "void",
        true,
    ),
    m("lineSeparator", "()", "String", true),
    m("identityHashCode", "(Object x)", "int", true),
    m("gc", "()", "void", true),
];

const PRINT_STREAM_MEMBERS: &[JdkFallbackMember] = &[
    m("println", "()", "void", false),
    m("println", "(String x)", "void", false),
    m("println", "(Object x)", "void", false),
    m("print", "(String s)", "void", false),
    m("print", "(Object obj)", "void", false),
    m(
        "printf",
        "(String format, Object... args)",
        "PrintStream",
        false,
    ),
    m(
        "format",
        "(String format, Object... args)",
        "PrintStream",
        false,
    ),
    m("append", "(CharSequence csq)", "PrintStream", false),
    m("flush", "()", "void", false),
    m("close", "()", "void", false),
    m("checkError", "()", "boolean", false),
];

const MATH_MEMBERS: &[JdkFallbackMember] = &[
    f("PI", "double", true),
    f("E", "double", true),
    m("abs", "(int a)", "int", true),
    m("abs", "(double a)", "double", true),
    m("max", "(int a, int b)", "int", true),
    m("min", "(int a, int b)", "int", true),
    m("pow", "(double a, double b)", "double", true),
    m("sqrt", "(double a)", "double", true),
    m("round", "(double a)", "long", true),
    m("floor", "(double a)", "double", true),
    m("ceil", "(double a)", "double", true),
    m("random", "()", "double", true),
    m("clamp", "(long value, int min, int max)", "int", true),
];

const INTEGER_MEMBERS: &[JdkFallbackMember] = &[
    f("MAX_VALUE", "int", true),
    f("MIN_VALUE", "int", true),
    m("parseInt", "(String s)", "int", true),
    m("valueOf", "(int i)", "Integer", true),
    m("valueOf", "(String s)", "Integer", true),
    m("toString", "(int i)", "String", true),
    m("compare", "(int x, int y)", "int", true),
    m("intValue", "()", "int", false),
    m("longValue", "()", "long", false),
    m("compareTo", "(Integer anotherInteger)", "int", false),
];

const LONG_MEMBERS: &[JdkFallbackMember] = &[
    f("MAX_VALUE", "long", true),
    f("MIN_VALUE", "long", true),
    m("parseLong", "(String s)", "long", true),
    m("valueOf", "(long l)", "Long", true),
    m("valueOf", "(String s)", "Long", true),
    m("toString", "(long i)", "String", true),
    m("compare", "(long x, long y)", "int", true),
    m("longValue", "()", "long", false),
    m("intValue", "()", "int", false),
    m("compareTo", "(Long anotherLong)", "int", false),
];

const DOUBLE_MEMBERS: &[JdkFallbackMember] = &[
    f("MAX_VALUE", "double", true),
    f("MIN_VALUE", "double", true),
    f("NaN", "double", true),
    m("parseDouble", "(String s)", "double", true),
    m("valueOf", "(double d)", "Double", true),
    m("isNaN", "(double v)", "boolean", true),
    m("compare", "(double d1, double d2)", "int", true),
    m("doubleValue", "()", "double", false),
    m("compareTo", "(Double anotherDouble)", "int", false),
];

const BOOLEAN_MEMBERS: &[JdkFallbackMember] = &[
    f("TRUE", "Boolean", true),
    f("FALSE", "Boolean", true),
    m("parseBoolean", "(String s)", "boolean", true),
    m("valueOf", "(boolean b)", "Boolean", true),
    m("valueOf", "(String s)", "Boolean", true),
    m("booleanValue", "()", "boolean", false),
    m("compare", "(boolean x, boolean y)", "int", true),
];

const THREAD_MEMBERS: &[JdkFallbackMember] = &[
    m("start", "()", "void", false),
    m("run", "()", "void", false),
    m("join", "()", "void", false),
    m("interrupt", "()", "void", false),
    m("isInterrupted", "()", "boolean", false),
    m("getName", "()", "String", false),
    m("setName", "(String name)", "void", false),
    m("currentThread", "()", "Thread", true),
    m("sleep", "(long millis)", "void", true),
    m("ofVirtual", "()", "Thread.Builder.OfVirtual", true),
    m("ofPlatform", "()", "Thread.Builder.OfPlatform", true),
    m("yield", "()", "void", true),
];

const THROWABLE_MEMBERS: &[JdkFallbackMember] = &[
    m("getMessage", "()", "String", false),
    m("getLocalizedMessage", "()", "String", false),
    m("getCause", "()", "Throwable", false),
    m("printStackTrace", "()", "void", false),
    m("getStackTrace", "()", "StackTraceElement[]", false),
    m("addSuppressed", "(Throwable exception)", "void", false),
    m("getSuppressed", "()", "Throwable[]", false),
    m("initCause", "(Throwable cause)", "Throwable", false),
];

const COLLECTION_MEMBERS: &[JdkFallbackMember] = &[
    m("size", "()", "int", false),
    m("isEmpty", "()", "boolean", false),
    m("contains", "(Object o)", "boolean", false),
    m("add", "(E e)", "boolean", false),
    m("remove", "(Object o)", "boolean", false),
    m("addAll", "(Collection<? extends E> c)", "boolean", false),
    m("clear", "()", "void", false),
    m("stream", "()", "Stream<E>", false),
    m("iterator", "()", "Iterator<E>", false),
    m("forEach", "(Consumer<? super E> action)", "void", false),
    m("toArray", "()", "Object[]", false),
];

const LIST_MEMBERS: &[JdkFallbackMember] = &[
    m("get", "(int index)", "E", false),
    m("set", "(int index, E element)", "E", false),
    m("add", "(int index, E element)", "void", false),
    m("remove", "(int index)", "E", false),
    m("indexOf", "(Object o)", "int", false),
    m("subList", "(int fromIndex, int toIndex)", "List<E>", false),
    m("of", "()", "List<E>", true),
    m("of", "(E e1)", "List<E>", true),
    m("of", "(E... elements)", "List<E>", true),
    m("copyOf", "(Collection<? extends E> coll)", "List<E>", true),
];

const SET_MEMBERS: &[JdkFallbackMember] = &[
    m("of", "()", "Set<E>", true),
    m("of", "(E e1)", "Set<E>", true),
    m("of", "(E... elements)", "Set<E>", true),
    m("copyOf", "(Collection<? extends E> coll)", "Set<E>", true),
];

const MAP_MEMBERS: &[JdkFallbackMember] = &[
    m("size", "()", "int", false),
    m("isEmpty", "()", "boolean", false),
    m("containsKey", "(Object key)", "boolean", false),
    m("containsValue", "(Object value)", "boolean", false),
    m("get", "(Object key)", "V", false),
    m("getOrDefault", "(Object key, V defaultValue)", "V", false),
    m("put", "(K key, V value)", "V", false),
    m("putIfAbsent", "(K key, V value)", "V", false),
    m("remove", "(Object key)", "V", false),
    m("clear", "()", "void", false),
    m("keySet", "()", "Set<K>", false),
    m("values", "()", "Collection<V>", false),
    m("entrySet", "()", "Set<Map.Entry<K, V>>", false),
    m(
        "forEach",
        "(BiConsumer<? super K, ? super V> action)",
        "void",
        false,
    ),
    m("of", "()", "Map<K, V>", true),
    m("of", "(K k1, V v1)", "Map<K, V>", true),
    m(
        "copyOf",
        "(Map<? extends K, ? extends V> map)",
        "Map<K, V>",
        true,
    ),
];

const COLLECTIONS_MEMBERS: &[JdkFallbackMember] = &[
    m("emptyList", "()", "List<T>", true),
    m("emptySet", "()", "Set<T>", true),
    m("emptyMap", "()", "Map<K, V>", true),
    m("singletonList", "(T o)", "List<T>", true),
    m(
        "unmodifiableList",
        "(List<? extends T> list)",
        "List<T>",
        true,
    ),
    m("unmodifiableSet", "(Set<? extends T> s)", "Set<T>", true),
    m(
        "unmodifiableMap",
        "(Map<? extends K, ? extends V> m)",
        "Map<K, V>",
        true,
    ),
    m("sort", "(List<T> list)", "void", true),
    m("reverse", "(List<?> list)", "void", true),
    m("shuffle", "(List<?> list)", "void", true),
    m("min", "(Collection<? extends T> coll)", "T", true),
    m("max", "(Collection<? extends T> coll)", "T", true),
    m("frequency", "(Collection<?> c, Object o)", "int", true),
];

const ARRAYS_MEMBERS: &[JdkFallbackMember] = &[
    m("asList", "(T... a)", "List<T>", true),
    m("sort", "(int[] a)", "void", true),
    m("sort", "(T[] a)", "void", true),
    m("binarySearch", "(int[] a, int key)", "int", true),
    m("copyOf", "(T[] original, int newLength)", "T[]", true),
    m(
        "copyOfRange",
        "(T[] original, int from, int to)",
        "T[]",
        true,
    ),
    m("equals", "(Object[] a, Object[] a2)", "boolean", true),
    m("deepEquals", "(Object[] a1, Object[] a2)", "boolean", true),
    m("fill", "(int[] a, int val)", "void", true),
    m("stream", "(T[] array)", "Stream<T>", true),
    m("toString", "(Object[] a)", "String", true),
    m("deepToString", "(Object[] a)", "String", true),
];

const OPTIONAL_MEMBERS: &[JdkFallbackMember] = &[
    m("of", "(T value)", "Optional<T>", true),
    m("ofNullable", "(T value)", "Optional<T>", true),
    m("empty", "()", "Optional<T>", true),
    m("isPresent", "()", "boolean", false),
    m("isEmpty", "()", "boolean", false),
    m("get", "()", "T", false),
    m("orElse", "(T other)", "T", false),
    m("orElseGet", "(Supplier<? extends T> supplier)", "T", false),
    m("orElseThrow", "()", "T", false),
    m("ifPresent", "(Consumer<? super T> action)", "void", false),
    m(
        "ifPresentOrElse",
        "(Consumer<? super T> action, Runnable emptyAction)",
        "void",
        false,
    ),
    m(
        "map",
        "(Function<? super T, ? extends U> mapper)",
        "Optional<U>",
        false,
    ),
    m(
        "flatMap",
        "(Function<? super T, ? extends Optional<? extends U>> mapper)",
        "Optional<U>",
        false,
    ),
    m(
        "filter",
        "(Predicate<? super T> predicate)",
        "Optional<T>",
        false,
    ),
    m("stream", "()", "Stream<T>", false),
];

const STREAM_MEMBERS: &[JdkFallbackMember] = &[
    m(
        "filter",
        "(Predicate<? super T> predicate)",
        "Stream<T>",
        false,
    ),
    m(
        "map",
        "(Function<? super T, ? extends R> mapper)",
        "Stream<R>",
        false,
    ),
    m(
        "flatMap",
        "(Function<? super T, ? extends Stream<? extends R>> mapper)",
        "Stream<R>",
        false,
    ),
    m("distinct", "()", "Stream<T>", false),
    m("sorted", "()", "Stream<T>", false),
    m("peek", "(Consumer<? super T> action)", "Stream<T>", false),
    m("limit", "(long maxSize)", "Stream<T>", false),
    m("skip", "(long n)", "Stream<T>", false),
    m("forEach", "(Consumer<? super T> action)", "void", false),
    m(
        "collect",
        "(Collector<? super T, A, R> collector)",
        "R",
        false,
    ),
    m("toList", "()", "List<T>", false),
    m("toArray", "()", "Object[]", false),
    m(
        "reduce",
        "(T identity, BinaryOperator<T> accumulator)",
        "T",
        false,
    ),
    m("count", "()", "long", false),
    m(
        "anyMatch",
        "(Predicate<? super T> predicate)",
        "boolean",
        false,
    ),
    m(
        "allMatch",
        "(Predicate<? super T> predicate)",
        "boolean",
        false,
    ),
    m(
        "noneMatch",
        "(Predicate<? super T> predicate)",
        "boolean",
        false,
    ),
    m("findFirst", "()", "Optional<T>", false),
    m("findAny", "()", "Optional<T>", false),
    m("of", "(T... values)", "Stream<T>", true),
    m("empty", "()", "Stream<T>", true),
    m(
        "concat",
        "(Stream<? extends T> a, Stream<? extends T> b)",
        "Stream<T>",
        true,
    ),
];

const COLLECTORS_MEMBERS: &[JdkFallbackMember] = &[
    m("toList", "()", "Collector<T, ?, List<T>>", true),
    m("toSet", "()", "Collector<T, ?, Set<T>>", true),
    m("toUnmodifiableList", "()", "Collector<T, ?, List<T>>", true),
    m(
        "toMap",
        "(Function<? super T, ? extends K> keyMapper, Function<? super T, ? extends U> valueMapper)",
        "Collector<T, ?, Map<K, U>>",
        true,
    ),
    m(
        "joining",
        "(CharSequence delimiter)",
        "Collector<CharSequence, ?, String>",
        true,
    ),
    m(
        "groupingBy",
        "(Function<? super T, ? extends K> classifier)",
        "Collector<T, ?, Map<K, List<T>>>",
        true,
    ),
    m("counting", "()", "Collector<T, ?, Long>", true),
];

const FILES_MEMBERS: &[JdkFallbackMember] = &[
    m("readString", "(Path path)", "String", true),
    m(
        "writeString",
        "(Path path, CharSequence csq, OpenOption... options)",
        "Path",
        true,
    ),
    m("readAllBytes", "(Path path)", "byte[]", true),
    m("readAllLines", "(Path path)", "List<String>", true),
    m(
        "exists",
        "(Path path, LinkOption... options)",
        "boolean",
        true,
    ),
    m(
        "notExists",
        "(Path path, LinkOption... options)",
        "boolean",
        true,
    ),
    m(
        "createFile",
        "(Path path, FileAttribute<?>... attrs)",
        "Path",
        true,
    ),
    m(
        "createDirectory",
        "(Path dir, FileAttribute<?>... attrs)",
        "Path",
        true,
    ),
    m(
        "createDirectories",
        "(Path dir, FileAttribute<?>... attrs)",
        "Path",
        true,
    ),
    m("delete", "(Path path)", "void", true),
    m("deleteIfExists", "(Path path)", "boolean", true),
    m(
        "copy",
        "(Path source, Path target, CopyOption... options)",
        "Path",
        true,
    ),
    m(
        "move",
        "(Path source, Path target, CopyOption... options)",
        "Path",
        true,
    ),
    m("list", "(Path dir)", "Stream<Path>", true),
    m(
        "walk",
        "(Path start, FileVisitOption... options)",
        "Stream<Path>",
        true,
    ),
    m(
        "isRegularFile",
        "(Path path, LinkOption... options)",
        "boolean",
        true,
    ),
    m(
        "isDirectory",
        "(Path path, LinkOption... options)",
        "boolean",
        true,
    ),
    m("size", "(Path path)", "long", true),
];

const PATHS_MEMBERS: &[JdkFallbackMember] = &[
    m("get", "(String first, String... more)", "Path", true),
    m("get", "(URI uri)", "Path", true),
];

const PATH_MEMBERS: &[JdkFallbackMember] = &[
    m("of", "(String first, String... more)", "Path", true),
    m("resolve", "(String other)", "Path", false),
    m("resolve", "(Path other)", "Path", false),
    m("resolveSibling", "(String other)", "Path", false),
    m("getParent", "()", "Path", false),
    m("getFileName", "()", "Path", false),
    m("toAbsolutePath", "()", "Path", false),
    m("normalize", "()", "Path", false),
    m("toFile", "()", "File", false),
];

const LOCAL_DATE_MEMBERS: &[JdkFallbackMember] = &[
    m("now", "()", "LocalDate", true),
    m(
        "of",
        "(int year, int month, int dayOfMonth)",
        "LocalDate",
        true,
    ),
    m("parse", "(CharSequence text)", "LocalDate", true),
    m("plusDays", "(long daysToAdd)", "LocalDate", false),
    m("minusDays", "(long daysToSubtract)", "LocalDate", false),
    m("isBefore", "(ChronoLocalDate other)", "boolean", false),
    m("isAfter", "(ChronoLocalDate other)", "boolean", false),
    m("format", "(DateTimeFormatter formatter)", "String", false),
];

const LOCAL_DATE_TIME_MEMBERS: &[JdkFallbackMember] = &[
    m("now", "()", "LocalDateTime", true),
    m(
        "of",
        "(int year, int month, int dayOfMonth, int hour, int minute)",
        "LocalDateTime",
        true,
    ),
    m("parse", "(CharSequence text)", "LocalDateTime", true),
    m("plusHours", "(long hours)", "LocalDateTime", false),
    m("minusHours", "(long hours)", "LocalDateTime", false),
    m("toLocalDate", "()", "LocalDate", false),
    m("format", "(DateTimeFormatter formatter)", "String", false),
];

const INSTANT_MEMBERS: &[JdkFallbackMember] = &[
    m("now", "()", "Instant", true),
    m("ofEpochMilli", "(long epochMilli)", "Instant", true),
    m("ofEpochSecond", "(long epochSecond)", "Instant", true),
    m("parse", "(CharSequence text)", "Instant", true),
    m("toEpochMilli", "()", "long", false),
    m("getEpochSecond", "()", "long", false),
    m("plusSeconds", "(long secondsToAdd)", "Instant", false),
    m("isBefore", "(Instant otherInstant)", "boolean", false),
    m("isAfter", "(Instant otherInstant)", "boolean", false),
];

const DURATION_MEMBERS: &[JdkFallbackMember] = &[
    m("ofMillis", "(long millis)", "Duration", true),
    m("ofSeconds", "(long seconds)", "Duration", true),
    m("ofMinutes", "(long minutes)", "Duration", true),
    m(
        "between",
        "(Temporal startInclusive, Temporal endExclusive)",
        "Duration",
        true,
    ),
    m("toMillis", "()", "long", false),
    m("toSeconds", "()", "long", false),
    m("isZero", "()", "boolean", false),
    m("isNegative", "()", "boolean", false),
];

/// Complete built-in catalog of core JDK classes.
pub const JDK_FALLBACK_CLASSES: &[JdkFallbackClass] = &[
    JdkFallbackClass {
        fqcn: "java.lang.Object",
        simple_name: "Object",
        members: OBJECT_MEMBERS,
        supers: &[],
    },
    JdkFallbackClass {
        fqcn: "java.util.Objects",
        simple_name: "Objects",
        members: OBJECTS_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.String",
        simple_name: "String",
        members: STRING_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.StringBuilder",
        simple_name: "StringBuilder",
        members: STRING_BUILDER_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.System",
        simple_name: "System",
        members: SYSTEM_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.io.PrintStream",
        simple_name: "PrintStream",
        members: PRINT_STREAM_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.Math",
        simple_name: "Math",
        members: MATH_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.Integer",
        simple_name: "Integer",
        members: INTEGER_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.Long",
        simple_name: "Long",
        members: LONG_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.Double",
        simple_name: "Double",
        members: DOUBLE_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.Boolean",
        simple_name: "Boolean",
        members: BOOLEAN_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.Thread",
        simple_name: "Thread",
        members: THREAD_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.Throwable",
        simple_name: "Throwable",
        members: THROWABLE_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.lang.Exception",
        simple_name: "Exception",
        members: &[],
        supers: &["java.lang.Throwable", "java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.Collection",
        simple_name: "Collection",
        members: COLLECTION_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.List",
        simple_name: "List",
        members: LIST_MEMBERS,
        supers: &["java.util.Collection", "java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.ArrayList",
        simple_name: "ArrayList",
        members: &[],
        supers: &["java.util.List", "java.util.Collection", "java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.Set",
        simple_name: "Set",
        members: SET_MEMBERS,
        supers: &["java.util.Collection", "java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.HashSet",
        simple_name: "HashSet",
        members: &[],
        supers: &["java.util.Set", "java.util.Collection", "java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.Map",
        simple_name: "Map",
        members: MAP_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.HashMap",
        simple_name: "HashMap",
        members: &[],
        supers: &["java.util.Map", "java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.Collections",
        simple_name: "Collections",
        members: COLLECTIONS_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.Arrays",
        simple_name: "Arrays",
        members: ARRAYS_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.Optional",
        simple_name: "Optional",
        members: OPTIONAL_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.stream.Stream",
        simple_name: "Stream",
        members: STREAM_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.util.stream.Collectors",
        simple_name: "Collectors",
        members: COLLECTORS_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.nio.file.Files",
        simple_name: "Files",
        members: FILES_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.nio.file.Paths",
        simple_name: "Paths",
        members: PATHS_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.nio.file.Path",
        simple_name: "Path",
        members: PATH_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.time.LocalDate",
        simple_name: "LocalDate",
        members: LOCAL_DATE_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.time.LocalDateTime",
        simple_name: "LocalDateTime",
        members: LOCAL_DATE_TIME_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.time.Instant",
        simple_name: "Instant",
        members: INSTANT_MEMBERS,
        supers: &["java.lang.Object"],
    },
    JdkFallbackClass {
        fqcn: "java.time.Duration",
        simple_name: "Duration",
        members: DURATION_MEMBERS,
        supers: &["java.lang.Object"],
    },
];

/// Well-known `java.lang` and standard library simple-name → FQCN mappings used even before an
/// explicit `import` statement is written.
const KNOWN_JDK_FQCNS: &[(&str, &str)] = &[
    ("Object", "java.lang.Object"),
    ("String", "java.lang.String"),
    ("StringBuilder", "java.lang.StringBuilder"),
    ("StringBuffer", "java.lang.StringBuffer"),
    ("CharSequence", "java.lang.CharSequence"),
    ("System", "java.lang.System"),
    ("Math", "java.lang.Math"),
    ("Integer", "java.lang.Integer"),
    ("Long", "java.lang.Long"),
    ("Double", "java.lang.Double"),
    ("Float", "java.lang.Float"),
    ("Boolean", "java.lang.Boolean"),
    ("Byte", "java.lang.Byte"),
    ("Short", "java.lang.Short"),
    ("Character", "java.lang.Character"),
    ("Thread", "java.lang.Thread"),
    ("Runnable", "java.lang.Runnable"),
    ("Throwable", "java.lang.Throwable"),
    ("Exception", "java.lang.Exception"),
    ("RuntimeException", "java.lang.RuntimeException"),
    (
        "IllegalArgumentException",
        "java.lang.IllegalArgumentException",
    ),
    ("IllegalStateException", "java.lang.IllegalStateException"),
    ("Class", "java.lang.Class"),
    ("Iterable", "java.lang.Iterable"),
    ("Comparable", "java.lang.Comparable"),
    ("Objects", "java.util.Objects"),
    ("List", "java.util.List"),
    ("ArrayList", "java.util.ArrayList"),
    ("LinkedList", "java.util.LinkedList"),
    ("Map", "java.util.Map"),
    ("HashMap", "java.util.HashMap"),
    ("LinkedHashMap", "java.util.LinkedHashMap"),
    ("TreeMap", "java.util.TreeMap"),
    ("Set", "java.util.Set"),
    ("HashSet", "java.util.HashSet"),
    ("TreeSet", "java.util.TreeSet"),
    ("Collection", "java.util.Collection"),
    ("Collections", "java.util.Collections"),
    ("Arrays", "java.util.Arrays"),
    ("Optional", "java.util.Optional"),
    ("UUID", "java.util.UUID"),
    ("Comparator", "java.util.Comparator"),
    ("Iterator", "java.util.Iterator"),
    ("Stream", "java.util.stream.Stream"),
    ("Collectors", "java.util.stream.Collectors"),
    ("IntStream", "java.util.stream.IntStream"),
    ("PrintStream", "java.io.PrintStream"),
    ("InputStream", "java.io.InputStream"),
    ("OutputStream", "java.io.OutputStream"),
    ("File", "java.io.File"),
    ("IOException", "java.io.IOException"),
    ("Files", "java.nio.file.Files"),
    ("Paths", "java.nio.file.Paths"),
    ("Path", "java.nio.file.Path"),
    ("LocalDate", "java.time.LocalDate"),
    ("LocalDateTime", "java.time.LocalDateTime"),
    ("Instant", "java.time.Instant"),
    ("Duration", "java.time.Duration"),
    ("ZonedDateTime", "java.time.ZonedDateTime"),
    ("BigDecimal", "java.math.BigDecimal"),
    ("BigInteger", "java.math.BigInteger"),
    (
        "CompletableFuture",
        "java.util.concurrent.CompletableFuture",
    ),
    ("Pattern", "java.util.regex.Pattern"),
    ("Matcher", "java.util.regex.Matcher"),
];

/// Resolves `name_or_fqcn` against explicit/wildcard imports, `java.lang.*`, and well-known
/// standard JDK classes.
#[must_use]
pub fn resolve_builtin_jdk_fqcn(name_or_fqcn: &str, imports: &[ImportEntry]) -> Option<String> {
    if name_or_fqcn.contains('.') {
        if name_or_fqcn.starts_with("java.") || name_or_fqcn.starts_with("javax.") {
            return Some(name_or_fqcn.to_owned());
        }
        return None;
    }

    for imp in imports {
        if imp.local_name() == Some(name_or_fqcn) {
            return (imp.path.starts_with("java.") || imp.path.starts_with("javax."))
                .then(|| imp.path.clone());
        }
    }

    for imp in imports {
        if imp.is_wildcard && (imp.path.starts_with("java.") || imp.path.starts_with("javax.")) {
            let pkg = imp.path.trim_end_matches(".*");
            let candidate = format!("{pkg}.{name_or_fqcn}");
            if KNOWN_JDK_FQCNS.iter().any(|(_, fqcn)| *fqcn == candidate)
                || JDK_FALLBACK_CLASSES.iter().any(|c| c.fqcn == candidate)
            {
                return Some(candidate);
            }
        }
    }

    KNOWN_JDK_FQCNS
        .iter()
        .find(|(simple, _)| *simple == name_or_fqcn)
        .map(|(_, fqcn)| (*fqcn).to_owned())
}

/// Looks up built-in fallback proposals for `fqcn` (or simple name), filtering by `static_only`.
#[must_use]
pub fn lookup_jdk_fallback(
    fqcn_or_simple: &str,
    static_only: bool,
    is_method_reference: bool,
) -> Vec<CompletionProposal> {
    let Some(root_class) = find_fallback_class(fqcn_or_simple) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut seen: Vec<(String, Option<String>)> = Vec::new();

    collect_from_fallback_class(
        root_class,
        static_only,
        is_method_reference,
        0,
        &mut seen,
        &mut out,
    );

    if !static_only {
        for &super_fqcn in root_class.supers {
            if let Some(super_cls) = find_fallback_class(super_fqcn) {
                let priority = if super_fqcn == "java.lang.Object" {
                    3
                } else {
                    2
                };
                collect_from_fallback_class(
                    super_cls,
                    false,
                    is_method_reference,
                    priority,
                    &mut seen,
                    &mut out,
                );
            }
        }
    }

    out
}

/// Resolves the return/field type of a member on a fallback JDK class (used for chains like `System.out.`).
#[must_use]
pub fn fallback_member_type(fqcn_or_simple: &str, member_name: &str) -> Option<String> {
    let cls = find_fallback_class(fqcn_or_simple)?;
    if let Some(mem) = cls.members.iter().find(|m| m.name == member_name) {
        let base = mem
            .return_type
            .split('<')
            .next()
            .unwrap_or(mem.return_type)
            .trim();
        return resolve_builtin_jdk_fqcn(base, &[]).or_else(|| Some(base.to_owned()));
    }
    for &sup in cls.supers {
        if let Some(found) = fallback_member_type(sup, member_name) {
            return Some(found);
        }
    }
    None
}

/// Returns well-known JDK class proposals for unqualified `Ctrl+Space` completion.
#[must_use]
pub fn builtin_jdk_type_proposals() -> Vec<CompletionProposal> {
    KNOWN_JDK_FQCNS
        .iter()
        .map(|(simple, fqcn)| {
            let pkg = fqcn.rsplit_once('.').map_or("", |(p, _)| p);
            CompletionProposal {
                label: (*simple).to_owned(),
                kind: CompletionProposalKind::Class,
                params_signature: None,
                return_type: None,
                detail_right: Some(pkg.to_owned()),
                insert_text: (*simple).to_owned(),
                is_callable: false,
                has_params: false,
                is_method_reference: false,
                additional_text_edits: Vec::new(),
                source: CompletionSource::JdkBuiltin,
                is_deprecated: false,
                sort_priority: 4,
            }
        })
        .collect()
}

fn find_fallback_class(fqcn_or_simple: &str) -> Option<&'static JdkFallbackClass> {
    JDK_FALLBACK_CLASSES
        .iter()
        .find(|c| c.fqcn == fqcn_or_simple || c.simple_name == fqcn_or_simple)
}

fn collect_from_fallback_class(
    cls: &JdkFallbackClass,
    static_only: bool,
    is_method_reference: bool,
    base_priority: u16,
    seen: &mut Vec<(String, Option<String>)>,
    out: &mut Vec<CompletionProposal>,
) {
    for member in cls.members {
        if member.is_static != static_only {
            continue;
        }
        let params = member.params_signature.map(str::to_owned);
        let key = (member.name.to_owned(), params.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);

        let is_callable = member.kind == CompletionProposalKind::Method;
        let has_params = params
            .as_deref()
            .is_some_and(|p| p.trim() != "()" && !p.trim().is_empty());

        out.push(CompletionProposal {
            label: member.name.to_owned(),
            kind: member.kind,
            params_signature: params,
            return_type: Some(member.return_type.to_owned()),
            detail_right: None,
            insert_text: member.name.to_owned(),
            is_callable,
            has_params,
            is_method_reference,
            additional_text_edits: Vec::new(),
            source: CompletionSource::JdkBuiltin,
            is_deprecated: member.is_deprecated,
            sort_priority: if static_only && base_priority == 0 {
                0
            } else if base_priority == 0 {
                1
            } else {
                base_priority
            },
        });
    }
}
