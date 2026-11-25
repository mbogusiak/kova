//! Completeness, TOML round-trip, Tree-sitter capture resolution, and registry tests.

use kova_theme::{
    AetherTheme, BuiltinTheme, PluginVariant, SyntaxLanguage, SyntaxTokenKind, ThemeLoadError,
    ThemePolarity, ThemeRegistry, ThemeRegistryError, resolve_capture,
};

#[test]
fn builtin_presets_load_and_roundtrip_losslessly() {
    let dark = AetherTheme::builtin(BuiltinTheme::KovaIslandsDark);
    assert_eq!(dark.metadata.id, "kova-islands-dark");
    assert_eq!(dark.metadata.polarity, ThemePolarity::Dark);
    assert_eq!(dark.metadata.schema_version, 1);
    assert!(dark.tokens.syntax.comment.italic);
    assert!(dark.tokens.syntax.comment_doc.italic);
    assert!(dark.tokens.syntax.http_method.bold);

    let light = AetherTheme::builtin(BuiltinTheme::KovaLight);
    assert_eq!(light.metadata.id, "kova-light");
    assert_eq!(light.metadata.polarity, ThemePolarity::Light);
    assert_eq!(light.metadata.schema_version, 1);

    for theme in [&dark, &light] {
        let serialized = theme.to_toml_string().expect("theme must serialize");
        let reparsed = AetherTheme::from_toml_str(&serialized).expect("round-trip TOML must parse");
        assert_eq!(&reparsed, theme);
    }
}

#[test]
fn rejects_incomplete_unknown_and_invalid_toml_themes() {
    let dark_toml = BuiltinTheme::KovaIslandsDark.toml_source();

    // Missing required field (`plugins.warning.border`)
    let missing_field = dark_toml.replace("border = \"#CA8A04\"\n", "");
    assert!(matches!(
        AetherTheme::from_toml_str(&missing_field),
        Err(ThemeLoadError::Parse(_))
    ));

    // Unknown field rejected by `deny_unknown_fields`
    let unknown_field = format!("{dark_toml}\nextra_unknown = \"#FFFFFF\"\n");
    assert!(matches!(
        AetherTheme::from_toml_str(&unknown_field),
        Err(ThemeLoadError::Parse(_))
    ));

    // Malformed hex value
    let bad_hex = dark_toml.replace("#0D1017", "#ZZZZZZ");
    assert!(matches!(
        AetherTheme::from_toml_str(&bad_hex),
        Err(ThemeLoadError::Parse(_))
    ));

    // Unsupported schema_version
    let bad_schema = dark_toml.replace("schema_version = 1", "schema_version = 99");
    assert!(matches!(
        AetherTheme::from_toml_str(&bad_schema),
        Err(ThemeLoadError::UnsupportedSchemaVersion {
            found: 99,
            supported: 1
        })
    ));
}

#[test]
fn terminal_ansi_and_plugin_variants_are_complete() {
    for preset in BuiltinTheme::ALL {
        let theme = AetherTheme::builtin(preset);
        for idx in 0..=15 {
            assert!(
                theme.tokens.terminal.ansi(idx).is_some(),
                "ANSI index {idx} must resolve"
            );
        }
        assert!(theme.tokens.terminal.ansi(16).is_none());

        for variant in PluginVariant::ALL {
            let via_theme = theme.plugin_variant(variant);
            let via_map = theme.tokens.plugins.get(variant);
            assert_eq!(via_theme, via_map);
        }
    }
}

#[test]
fn tree_sitter_captures_resolve_for_java_and_kotlin() {
    let expected = [
        ("@keyword", SyntaxTokenKind::Keyword),
        ("keyword.modifier", SyntaxTokenKind::Keyword),
        ("@keyword.control", SyntaxTokenKind::KeywordControl),
        (
            "@keyword.control.conditional",
            SyntaxTokenKind::KeywordControl,
        ),
        ("@type", SyntaxTokenKind::Type),
        ("@type.builtin", SyntaxTokenKind::TypeBuiltin),
        ("@annotation", SyntaxTokenKind::Annotation),
        ("@attribute", SyntaxTokenKind::Annotation),
        ("@function.method.call", SyntaxTokenKind::Function),
        ("@variable.parameter", SyntaxTokenKind::Parameter),
        ("@variable.member", SyntaxTokenKind::Property),
        ("@constant.builtin", SyntaxTokenKind::Constant),
        ("@string.escape", SyntaxTokenKind::StringEscape),
        ("@number.float", SyntaxTokenKind::Number),
        ("@boolean", SyntaxTokenKind::Boolean),
        ("@operator", SyntaxTokenKind::Operator),
        ("@punctuation.bracket", SyntaxTokenKind::Punctuation),
        ("@comment", SyntaxTokenKind::Comment),
        ("@comment.documentation", SyntaxTokenKind::CommentDoc),
    ];

    for lang in [SyntaxLanguage::Java, SyntaxLanguage::Kotlin] {
        for (capture, kind) in expected {
            assert_eq!(resolve_capture(lang, capture), Some(kind));
        }
    }
    assert_eq!(resolve_capture(SyntaxLanguage::Java, ""), None);
    assert_eq!(
        resolve_capture(SyntaxLanguage::Java, "@unknown.token"),
        None
    );
}

#[test]
fn tree_sitter_captures_resolve_for_http_toml_and_json() {
    assert_eq!(
        resolve_capture(SyntaxLanguage::Http, "@keyword.http.method"),
        Some(SyntaxTokenKind::HttpMethod)
    );
    assert_eq!(
        resolve_capture(SyntaxLanguage::Http, "method.http"),
        Some(SyntaxTokenKind::HttpMethod)
    );
    assert_eq!(
        resolve_capture(SyntaxLanguage::Http, "@string.special.url"),
        Some(SyntaxTokenKind::HttpUrl)
    );
    assert_eq!(
        resolve_capture(SyntaxLanguage::Http, "@property.http.header"),
        Some(SyntaxTokenKind::HttpHeader)
    );
    assert_eq!(
        resolve_capture(SyntaxLanguage::Http, "@number.http.status"),
        Some(SyntaxTokenKind::HttpStatus)
    );
    assert_eq!(
        resolve_capture(SyntaxLanguage::Http, "@comment"),
        Some(SyntaxTokenKind::Comment)
    );

    for lang in [SyntaxLanguage::Toml, SyntaxLanguage::Json] {
        for (capture, kind) in [
            ("@property", SyntaxTokenKind::Property),
            ("@label", SyntaxTokenKind::Property),
            ("@string", SyntaxTokenKind::String),
            ("@string.escape", SyntaxTokenKind::StringEscape),
            ("@number", SyntaxTokenKind::Number),
            ("@boolean", SyntaxTokenKind::Boolean),
            ("@punctuation.delimiter", SyntaxTokenKind::Punctuation),
        ] {
            assert_eq!(resolve_capture(lang, capture), Some(kind));
        }
    }
}

#[test]
fn registry_manages_builtin_and_custom_themes() {
    let mut registry = ThemeRegistry::new();
    assert_eq!(registry.len(), 2);
    assert!(!registry.is_empty());
    assert_eq!(registry.active_id(), "kova-islands-dark");
    assert_eq!(registry.active().metadata.name, "Kova Islands Dark");

    let switched = registry
        .set_active("kova-light")
        .expect("kova-light is registered");
    assert_eq!(switched.metadata.id, "kova-light");
    assert_eq!(registry.active_id(), "kova-light");

    assert!(matches!(
        registry.set_active("non-existent"),
        Err(ThemeRegistryError::UnknownTheme { .. })
    ));

    let custom_toml = BuiltinTheme::KovaIslandsDark
        .toml_source()
        .replace("id = \"kova-islands-dark\"", "id = \"omarchy-custom\"")
        .replace("name = \"Kova Islands Dark\"", "name = \"Omarchy Custom\"");
    let custom_id = registry
        .register_toml(&custom_toml)
        .expect("custom TOML must register");
    assert_eq!(custom_id, "omarchy-custom");
    assert_eq!(registry.len(), 3);
    assert!(registry.get("omarchy-custom").is_some());

    registry
        .set_active("omarchy-custom")
        .expect("custom theme should activate");
    assert_eq!(registry.active_arc().metadata.id, "omarchy-custom");

    // Built-in themes cannot be unregistered.
    assert!(!registry.unregister("kova-islands-dark"));
    assert!(!registry.unregister("kova-light"));

    // Unregistering the active custom theme safely falls back to kova-islands-dark.
    let fallback = registry.unregister_custom("omarchy-custom");
    assert_eq!(fallback.as_deref(), Some("kova-islands-dark"));
    assert_eq!(registry.active_id(), "kova-islands-dark");
    assert_eq!(registry.active_arc().metadata.id, "kova-islands-dark");
    assert!(registry.get("omarchy-custom").is_none());
    assert_eq!(registry.len(), 2);
}
