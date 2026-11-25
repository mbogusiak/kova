//! Integration tests for KOVA-024: JVM (`main`, `@Test*`, `@Nested`) and `.http` entry-point detection.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert deterministic entry-point detection and invariants"
)]

use kova_buffer::{Buffer, BufferSnapshot};
use kova_syntax::{
    EntryPoint, EntryPointKind, GutterIconRole, LanguageRegistry, SyntaxEngine, SyntaxEngineConfig,
    detect_entry_points,
};
use kova_theme::SyntaxLanguage;

fn parse_and_detect(language: SyntaxLanguage, source: &str) -> (BufferSnapshot, Vec<EntryPoint>) {
    let registry = LanguageRegistry::builtin().expect("builtin registry initializes");
    let engine = SyntaxEngine::new(registry.clone(), SyntaxEngineConfig::default())
        .expect("SyntaxEngine initializes");
    let buffer = Buffer::from_text(source);
    let snapshot = buffer.snapshot();
    let tree = engine
        .parse_sync(language, &snapshot, None)
        .expect("parse succeeds");
    let entry_points = detect_entry_points(&tree, &snapshot);
    assert_invariants(&entry_points, &snapshot, source);
    (snapshot, entry_points)
}

fn assert_invariants(entry_points: &[EntryPoint], snapshot: &BufferSnapshot, source: &str) {
    let len_bytes = snapshot.len_bytes();
    for ep in entry_points {
        assert!(
            ep.start_line <= ep.line && ep.line <= ep.end_line,
            "invalid line ordering in {ep:?}"
        );
        assert!(
            ep.byte_range.start <= ep.byte_range.end && ep.byte_range.end <= len_bytes,
            "byte_range out of bounds in {ep:?} (len={len_bytes})"
        );
        assert!(
            source.is_char_boundary(ep.byte_range.start)
                && source.is_char_boundary(ep.byte_range.end),
            "byte_range not on UTF-8 char boundary in {ep:?}"
        );
    }

    for pair in entry_points.windows(2) {
        let left = (pair[0].line, pair[0].byte_range.start);
        let right = (pair[1].line, pair[1].byte_range.start);
        assert!(
            left <= right,
            "entry points not sorted ascending by (line, byte_range.start): {:?} > {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn java_entry_points_detects_main_variants_and_jep445() {
    let source = r#"package com.example.app;

public class Application {
    public static void main(String[] args) {
        System.out.println("classic");
    }

    static public void main(String... args) {
        System.out.println("varargs");
    }

    static void main(java.lang.String[] args) {
        System.out.println("qualified");
    }

    void main() {
        System.out.println("JEP 445");
    }
}
"#;
    let (_snap, eps) = parse_and_detect(SyntaxLanguage::Java, source);
    assert_eq!(eps.len(), 4);
    for ep in &eps {
        assert_eq!(ep.kind, EntryPointKind::JavaMain);
        assert_eq!(ep.icon_role, GutterIconRole::Run);
        assert_eq!(ep.package_name.as_deref(), Some("com.example.app"));
        assert_eq!(ep.class_name.as_deref(), Some("Application"));
        assert_eq!(ep.method_name.as_deref(), Some("main"));
        assert_eq!(ep.qualified_target(), "com.example.app.Application");
        assert_eq!(ep.label, "Run 'Application.main()'");
    }
    let lines: Vec<usize> = eps.iter().map(|e| e.line).collect();
    assert_eq!(lines, vec![3, 7, 11, 15]);
}

#[test]
fn java_entry_points_detects_tests_nested_classes_and_multiline_annotations() {
    let source = r#"package com.example.orders;

import org.junit.jupiter.api.Nested;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;

public class OrderServiceTest {
    public static class Helper {
        void assist() {}
    }

    /**
     * Ensures valid order placement.
     */
    @Test
    void shouldPlaceOrder() {}

    @ParameterizedTest
    @MethodSource(
        "orderInputs"
    )
    void shouldAcceptParameterized(String input) {}

    @RepeatedTest(3)
    void shouldRepeat() {}

    @TestFactory
    java.util.List<Object> dynamicTests() { return java.util.List.of(); }

    @org.junit.jupiter.api.Test
    void qualifiedAnnotation() {}

    @Nested
    class WhenEmpty {
        @Test
        void rejectsEmptyCart() {}
    }
}
"#;
    let (_snap, eps) = parse_and_detect(SyntaxLanguage::Java, source);

    let test_classes: Vec<&EntryPoint> = eps
        .iter()
        .filter(|e| e.kind == EntryPointKind::JvmTestClass)
        .collect();
    assert_eq!(
        test_classes.len(),
        2,
        "expected outer and @Nested test classes: {test_classes:?}"
    );

    assert_eq!(test_classes[0].icon_role, GutterIconRole::RunAll);
    assert_eq!(
        test_classes[0].class_name.as_deref(),
        Some("OrderServiceTest")
    );
    assert_eq!(test_classes[0].line, 6);
    assert_eq!(
        test_classes[0].qualified_target(),
        "com.example.orders.OrderServiceTest"
    );

    assert_eq!(test_classes[1].icon_role, GutterIconRole::RunAll);
    assert_eq!(
        test_classes[1].class_name.as_deref(),
        Some("OrderServiceTest$WhenEmpty")
    );
    assert_eq!(test_classes[1].line, 33);
    assert_eq!(
        test_classes[1].qualified_target(),
        "com.example.orders.OrderServiceTest$WhenEmpty"
    );

    let methods: Vec<&EntryPoint> = eps
        .iter()
        .filter(|e| e.kind == EntryPointKind::JvmTestMethod)
        .collect();
    assert_eq!(methods.len(), 6);

    // Line numbers must anchor on the method signature/name line, NOT above Javadoc or multi-line annotations.
    assert_eq!(methods[0].method_name.as_deref(), Some("shouldPlaceOrder"));
    assert_eq!(methods[0].line, 15);
    assert_eq!(
        methods[0].label,
        "Run 'OrderServiceTest.shouldPlaceOrder()'"
    );

    assert_eq!(
        methods[1].method_name.as_deref(),
        Some("shouldAcceptParameterized")
    );
    assert_eq!(methods[1].line, 21);
    assert!(methods[1].start_line <= 17);

    assert_eq!(methods[2].method_name.as_deref(), Some("shouldRepeat"));
    assert_eq!(methods[2].line, 24);

    assert_eq!(methods[3].method_name.as_deref(), Some("dynamicTests"));
    assert_eq!(methods[3].line, 27);

    assert_eq!(
        methods[4].method_name.as_deref(),
        Some("qualifiedAnnotation")
    );
    assert_eq!(methods[4].line, 30);

    assert_eq!(methods[5].method_name.as_deref(), Some("rejectsEmptyCart"));
    assert_eq!(
        methods[5].class_name.as_deref(),
        Some("OrderServiceTest$WhenEmpty")
    );
    assert_eq!(
        methods[5].id,
        "com.example.orders.OrderServiceTest$WhenEmpty#rejectsEmptyCart"
    );
    assert_eq!(methods[5].line, 35);
}

#[test]
fn java_entry_points_rejects_false_positives() {
    let source = r#"package com.example.neg;

public abstract class NotRunnable {
    // @Test
    /* @ParameterizedTest */
    String marker = "@Test public static void main(String[] args)";

    int main() { return 0; }

    void main(int argc) {}

    void main(String a, String b) {}

    void main(String[][] matrix) {}

    abstract void main(String[] args);

    @Test
    abstract void abstractTest();
}

interface Contract {
    void main(String[] args);

    @Test
    void interfaceTest();
}
"#;
    let (_snap, eps) = parse_and_detect(SyntaxLanguage::Java, source);
    assert!(eps.is_empty(), "unexpected false positives: {eps:?}");
}

#[test]
fn kotlin_entry_points_detects_top_level_and_jvm_static_main() {
    let source = r#"package com.example.kt

fun main() {
    println("zero-arg")
}

fun main(args: Array<String>) {
    println(args.size)
}

object App {
    @JvmStatic
    fun main(args: kotlin.Array<kotlin.String>): Unit {
        println("object")
    }
}

class Server {
    companion object {
        @kotlin.jvm.JvmStatic
        fun main() {
            println("companion")
        }
    }
}
"#;
    let (_snap, eps) = parse_and_detect(SyntaxLanguage::Kotlin, source);
    assert_eq!(eps.len(), 4, "got: {eps:?}");
    for ep in &eps {
        assert_eq!(ep.kind, EntryPointKind::KotlinMain);
        assert_eq!(ep.icon_role, GutterIconRole::Run);
        assert_eq!(ep.package_name.as_deref(), Some("com.example.kt"));
        assert_eq!(ep.method_name.as_deref(), Some("main"));
    }

    assert_eq!(eps[0].class_name, None);
    assert_eq!(eps[0].id, "com.example.kt#main");
    assert_eq!(eps[0].line, 2);

    assert_eq!(eps[1].class_name, None);
    assert_eq!(eps[1].line, 6);

    assert_eq!(eps[2].class_name.as_deref(), Some("App"));
    assert_eq!(eps[2].qualified_target(), "com.example.kt.App");
    assert_eq!(eps[2].line, 12);

    assert_eq!(eps[3].class_name.as_deref(), Some("Server"));
    assert_eq!(eps[3].qualified_target(), "com.example.kt.Server");
    assert_eq!(eps[3].line, 20);
}

#[test]
fn kotlin_entry_points_detects_tests_and_nested_classes() {
    let source = r#"package com.example.kt.tests

import kotlin.test.Test

class UserSpec {
    @Test
    fun shouldCreateUser() {}

    @ParameterizedTest
    @ValueSource(
        strings = ["a", "b"]
    )
    fun shouldValidateInputs(v: String) {}

    @kotlin.test.Test
    fun qualifiedKotlinTest() {}

    @Nested
    inner class NestedSpec {
        @Test
        fun nestedCase() {}
    }
}
"#;
    let (_snap, eps) = parse_and_detect(SyntaxLanguage::Kotlin, source);

    let classes: Vec<&EntryPoint> = eps
        .iter()
        .filter(|e| e.kind == EntryPointKind::JvmTestClass)
        .collect();
    assert_eq!(
        classes.len(),
        2,
        "expected outer and @Nested test classes: {classes:?}"
    );
    assert_eq!(classes[0].class_name.as_deref(), Some("UserSpec"));
    assert_eq!(classes[0].line, 4);
    assert_eq!(
        classes[1].class_name.as_deref(),
        Some("UserSpec$NestedSpec")
    );
    assert_eq!(classes[1].line, 18);

    let methods: Vec<&EntryPoint> = eps
        .iter()
        .filter(|e| e.kind == EntryPointKind::JvmTestMethod)
        .collect();
    assert_eq!(methods.len(), 4);
    assert_eq!(methods[0].line, 6);
    assert_eq!(methods[1].line, 12);
    assert_eq!(methods[2].line, 15);
    assert_eq!(methods[3].line, 20);
    assert_eq!(
        methods[3].id,
        "com.example.kt.tests.UserSpec$NestedSpec#nestedCase"
    );
}

#[test]
fn kotlin_entry_points_rejects_instance_main_and_false_positives() {
    let source = r#"package com.example.kt.neg

class Service {
    fun main() {
        // Instance method without @JvmStatic is not runnable
    }

    fun main(args: Array<String>) {}
}

object NonStaticObject {
    fun main(args: Array<String>) {}
}

fun main(x: Int) {}

fun main(): Int = 42

fun String.main() {}

fun outerHelper() {
    fun main() {}
    val text = "@Test fun fake() {}"
    // @Test
}

interface Contract {
    @Test
    fun abstractTest()
}
"#;
    let (_snap, eps) = parse_and_detect(SyntaxLanguage::Kotlin, source);
    assert!(eps.is_empty(), "unexpected Kotlin false positives: {eps:?}");
}

#[test]
fn http_entry_points_detects_methods_urls_names_and_indices() {
    let source = r#"### List all orders
# @name ListOrders
@host = https://api.kova.dev

GET {{host}}/v1/orders?page=1
Authorization: Bearer {{token}}

### Create new order
POST {{host}}/v1/orders HTTP/1.1
Content-Type: application/json

{"item": "espresso"}

###
# @name ListOrders
PUT https://api.kova.dev/v1/orders/42

### Delete item
DELETE https://api.kova.dev/v1/orders/42

### Patch item
PATCH https://api.kova.dev/v1/orders/42
"#;
    let (_snap, eps) = parse_and_detect(SyntaxLanguage::Http, source);
    assert_eq!(eps.len(), 5, "got: {eps:?}");

    assert_eq!(eps[0].id, "http#0");
    assert_eq!(eps[0].kind, EntryPointKind::HttpRequest);
    assert_eq!(eps[0].icon_role, GutterIconRole::Run);
    assert_eq!(eps[0].request_index, Some(0));
    assert_eq!(eps[0].http_method.as_deref(), Some("GET"));
    assert_eq!(
        eps[0].http_url.as_deref(),
        Some("{{host}}/v1/orders?page=1")
    );
    assert_eq!(
        eps[0].label,
        "Send 'ListOrders: GET {{host}}/v1/orders?page=1'"
    );
    assert_eq!(eps[0].line, 4);

    assert_eq!(eps[1].id, "http#1");
    assert_eq!(eps[1].request_index, Some(1));
    assert_eq!(eps[1].http_method.as_deref(), Some("POST"));
    assert_eq!(
        eps[1].label,
        "Send 'Create new order: POST {{host}}/v1/orders'"
    );
    assert_eq!(eps[1].line, 8);

    // Duplicate `# @name ListOrders` keeps distinct `id` (`http#2`).
    assert_eq!(eps[2].id, "http#2");
    assert_eq!(eps[2].request_index, Some(2));
    assert_eq!(eps[2].http_method.as_deref(), Some("PUT"));
    assert_eq!(
        eps[2].label,
        "Send 'ListOrders: PUT https://api.kova.dev/v1/orders/42'"
    );
    assert_eq!(eps[2].line, 15);

    assert_eq!(eps[3].id, "http#3");
    assert_eq!(eps[3].http_method.as_deref(), Some("DELETE"));
    assert_eq!(eps[3].line, 18);

    assert_eq!(eps[4].id, "http#4");
    assert_eq!(eps[4].http_method.as_deref(), Some("PATCH"));
    assert_eq!(eps[4].line, 21);
}

#[test]
fn entry_points_resilience_toml_json_empty_and_syntax_errors_utf8() {
    let (_snap, toml_eps) = parse_and_detect(SyntaxLanguage::Toml, "[editor]\ntab_size = 4\n");
    assert_eq!(toml_eps, []);

    let (_snap, json_eps) = parse_and_detect(SyntaxLanguage::Json, "{\"main\": \"@Test\"}\n");
    assert_eq!(json_eps, []);

    // Broken first method in Java (missing closing brace/parens), valid second @Test with UTF-8 + CRLF.
    let broken_java = "package dev.kova;\r\n\r\npublic class BrokenTest {\r\n  // zażółć 🚀\r\n  public void broken( {\r\n    int x = ;\r\n  }\r\n  @Test\r\n  void stillDetected_zażółć() {\r\n    String s = \"🚀\";\r\n  }\r\n}\r\n";
    let (_snap, java_eps) = parse_and_detect(SyntaxLanguage::Java, broken_java);
    assert!(
        java_eps
            .iter()
            .any(|e| e.kind == EntryPointKind::JvmTestMethod
                && e.method_name.as_deref() == Some("stillDetected_zażółć")),
        "expected valid @Test after syntax error in Java: {java_eps:?}"
    );

    // Broken first function in Kotlin, valid second @Test with UTF-8.
    let broken_kotlin = "package dev.kova\n\nclass BrokenKtTest {\n    fun broken() {\n        val a = \n    }\n\n    @Test\n    fun validAfterError() {\n        val msg = \"zażółć 🚀\"\n    }\n}\n";
    let (_snap, kt_eps) = parse_and_detect(SyntaxLanguage::Kotlin, broken_kotlin);
    assert!(
        kt_eps
            .iter()
            .any(|e| e.kind == EntryPointKind::JvmTestMethod
                && e.method_name.as_deref() == Some("validAfterError")),
        "expected valid @Test after syntax error in Kotlin: {kt_eps:?}"
    );

    // HTTP with UTF-8 query and CRLF + trailing malformed section.
    let broken_http = "### Żółty test 🚀\r\nGET https://example.com/search?q=zażółć\r\n\r\n### Incomplete\r\nINVALID\r\n";
    let (_snap, http_eps) = parse_and_detect(SyntaxLanguage::Http, broken_http);
    assert_eq!(http_eps.len(), 1);
    assert_eq!(http_eps[0].http_method.as_deref(), Some("GET"));
    assert_eq!(
        http_eps[0].http_url.as_deref(),
        Some("https://example.com/search?q=zażółć")
    );
}
