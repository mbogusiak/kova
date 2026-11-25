//! Integration tests for lossless `toml_edit::DocumentMut` config sync
//! and optional `kova.toml` project overrides (`KOVA-006`).
#![allow(clippy::expect_used, reason = "integration test harness")]

use kova_config::{
    ConfigError, EffectiveConfig, KeymapPreset, KovaPaths, KovaSettings, ProjectBuildTool,
    ProjectConfig, ProjectConfigDocument, RunConfigKind, SettingUpdate, SettingsDocument,
};
use tempfile::TempDir;
use toml_edit::Value;

#[test]
fn omakase_defaults_match_specification() {
    let defaults = KovaSettings::default();
    defaults.validate().expect("default settings must be valid");

    assert_eq!(defaults.editor.font_family, "JetBrains Mono");
    assert_eq!(defaults.editor.tab_size, 4);
    assert!(!defaults.editor.vim_mode);
    assert_eq!(defaults.editor.keymap, KeymapPreset::JetBrains);
    assert_eq!(defaults.ui.theme, "kova-islands-dark");
    assert!(!defaults.ui.focus_mode);
    assert_eq!(defaults.jvm.jdtls_max_ram_mb, 2048);
    assert_eq!(defaults.jvm.auto_hibernate_minutes, 15);
    assert!(defaults.jvm.auto_decompile_class);
    assert!(defaults.jvm.lombok_enabled);
    assert!(defaults.terminal.inject_java_home);
}

#[test]
fn partial_dotfiles_inherit_omakase_defaults_per_section() {
    let raw = r"
[editor]
vim_mode = true
";
    let doc = SettingsDocument::from_toml_str(raw).expect("partial TOML parses");
    assert!(doc.settings().editor.vim_mode);
    assert_eq!(doc.settings().editor.tab_size, 4);
    assert_eq!(doc.settings().editor.font_family, "JetBrains Mono");
    assert_eq!(doc.settings().ui.theme, "kova-islands-dark");
    assert_eq!(doc.settings().jvm.jdtls_max_ram_mb, 2048);
}

#[test]
fn round_trip_preserves_comments_blank_lines_and_section_order() {
    let original = "\
# ============================================================
# My personal Omarchy Kova dotfiles (do not strip comments!)
# ============================================================

# JVM placed intentionally before [editor]
[jvm]
jdtls_max_ram_mb = 4096 # Keep plenty of heap for Spring Boot
auto_hibernate_minutes = 15

# UI preferences
[ui]
theme = \"kova-islands-dark\" # Dark mode by default

[editor]
# Use 4 spaces for Java
tab_size = 4 # Java standard
vim_mode = false
";

    let mut doc = SettingsDocument::from_toml_str(original).expect("parse original");
    doc.set_setting(SettingUpdate::EditorVimMode(true))
        .expect("toggle vim_mode");
    doc.set_setting(SettingUpdate::EditorTabSize(2))
        .expect("change tab_size");
    doc.set_setting(SettingUpdate::UiTheme("kova-light".to_owned()))
        .expect("switch theme");
    doc.set_setting(SettingUpdate::JvmAutoHibernateMinutes(30))
        .expect("update hibernate");

    let expected = "\
# ============================================================
# My personal Omarchy Kova dotfiles (do not strip comments!)
# ============================================================

# JVM placed intentionally before [editor]
[jvm]
jdtls_max_ram_mb = 4096 # Keep plenty of heap for Spring Boot
auto_hibernate_minutes = 30

# UI preferences
[ui]
theme = \"kova-light\" # Dark mode by default

[editor]
# Use 4 spaces for Java
tab_size = 2 # Java standard
vim_mode = true
";

    assert_eq!(doc.to_toml_string(), expected);

    // Re-parse edited output to confirm bidirectional consistency.
    let reparsed = SettingsDocument::from_toml_str(&doc.to_toml_string()).expect("reparse");
    assert!(reparsed.settings().editor.vim_mode);
    assert_eq!(reparsed.settings().editor.tab_size, 2);
    assert_eq!(reparsed.settings().ui.theme, "kova-light");
    assert_eq!(reparsed.settings().jvm.auto_hibernate_minutes, 30);
    assert_eq!(reparsed.settings().jvm.jdtls_max_ram_mb, 4096);
}

#[test]
fn adding_new_section_and_unsetting_key_preserves_existing_comments() {
    let raw = "\
# Top header
# Keep comment above JVM section
[jvm]
preferred_jdk = \"temurin-21\"
# Keep comment above sibling key
lombok_enabled = true
";

    let mut doc = SettingsDocument::from_toml_str(raw).expect("parse");
    doc.set_setting(SettingUpdate::TerminalFontSize(14.0))
        .expect("create [terminal] section");
    assert!(doc.to_toml_string().contains("# Top header"));
    assert!(
        doc.to_toml_string()
            .contains("# Keep comment above JVM section")
    );
    assert!((doc.settings().terminal.font_size - 14.0).abs() < f64::EPSILON);

    doc.set_setting(SettingUpdate::JvmPreferredJdk(None))
        .expect("unset preferred_jdk");
    assert_eq!(doc.settings().jvm.preferred_jdk, None);
    assert!(
        doc.to_toml_string()
            .contains("# Keep comment above JVM section")
    );
    assert!(
        doc.to_toml_string()
            .contains("# Keep comment above sibling key")
    );
    assert!(doc.to_toml_string().contains("[jvm]"));

    // Unsetting the last key in a section keeps the table header & its comments intact.
    assert!(
        doc.unset_dotted_key("jvm.lombok_enabled")
            .expect("unset last key")
    );
    assert!(
        doc.to_toml_string()
            .contains("# Keep comment above JVM section")
    );
    assert!(doc.to_toml_string().contains("[jvm]"));
}

#[test]
fn validation_error_rolls_back_document_state_cleanly() {
    let raw = "[editor]\ntab_size = 4 # keep valid\n";
    let mut doc = SettingsDocument::from_toml_str(raw).expect("parse");

    let err = doc
        .set_setting(SettingUpdate::EditorTabSize(0))
        .expect_err("tab_size=0 must fail validation");
    assert!(matches!(
        err,
        ConfigError::Validation {
            field: "editor.tab_size",
            ..
        }
    ));
    assert_eq!(doc.settings().editor.tab_size, 4);
    assert_eq!(doc.to_toml_string(), raw);

    let ram_err = doc
        .set_dotted_value("jvm.jdtls_max_ram_mb", Value::from(64))
        .expect_err("RAM < 256 must fail validation");
    assert!(matches!(
        ram_err,
        ConfigError::Validation {
            field: "jvm.jdtls_max_ram_mb",
            ..
        }
    ));
    assert_eq!(doc.to_toml_string(), raw);
}

#[test]
fn atomic_save_and_load_leave_no_temp_files() {
    let dir = TempDir::new().expect("temp dir");
    let paths = KovaPaths::from_roots(
        dir.path().join("config/kova"),
        dir.path().join("local/share/kova"),
        dir.path().join("cache/kova"),
    )
    .expect("paths");

    // Missing file yields default template without writing to disk.
    let mut doc =
        SettingsDocument::load_or_default(&paths.settings_file()).expect("load default template");
    assert!(!paths.settings_file().exists());

    doc.set_setting(SettingUpdate::EditorVimMode(true))
        .expect("set vim_mode");
    doc.save_to_file(&paths.settings_file())
        .expect("atomic save");
    assert!(paths.settings_file().is_file());

    // Only settings.toml should exist inside config/kova (no leftover temp files).
    let entries: Vec<_> = std::fs::read_dir(paths.config_dir())
        .expect("read config dir")
        .map(|e| e.expect("dir entry").file_name())
        .collect();
    assert_eq!(entries, vec!["settings.toml"]);

    let loaded =
        SettingsDocument::load_or_default(&paths.settings_file()).expect("reload saved file");
    assert!(loaded.settings().editor.vim_mode);
    assert_eq!(loaded.to_toml_string(), doc.to_toml_string());
}

#[test]
fn optional_project_kova_toml_and_effective_merge_preserve_user_settings() {
    let project_dir = TempDir::new().expect("project temp dir");

    // Missing kova.toml returns None and creates zero files in project_dir.
    assert_eq!(
        ProjectConfig::load_optional(project_dir.path()).expect("load optional"),
        None
    );
    assert_eq!(
        std::fs::read_dir(project_dir.path())
            .expect("read project dir")
            .count(),
        0
    );

    let kova_toml = r#"# Team shared Kova config
schema_version = 1
jdk = "temurin-21"
build_tool = "gradle"

[editor]
tab_size = 2

[jvm]
jdtls_max_ram_mb = 4096

[[run_configurations]]
name = "OrderServiceApplication"
kind = "spring_boot"
main_class = "com.example.OrderServiceApplication"
vm_options = ["-Dspring.profiles.active=dev"]
"#;

    let mut proj_doc = ProjectConfigDocument::from_toml_str(kova_toml).expect("parse kova.toml");
    proj_doc
        .set_dotted_value("editor.format_on_save", Value::from(true))
        .expect("update kova.toml");
    assert!(
        proj_doc
            .to_toml_string()
            .contains("# Team shared Kova config")
    );

    proj_doc
        .save_to_project(project_dir.path())
        .expect("explicit save kova.toml");

    let loaded_proj = ProjectConfig::load_optional(project_dir.path())
        .expect("reload kova.toml")
        .expect("kova.toml should exist");
    assert_eq!(loaded_proj.build_tool, Some(ProjectBuildTool::Gradle));
    assert_eq!(loaded_proj.run_configurations.len(), 1);
    assert_eq!(
        loaded_proj.run_configurations[0].kind,
        RunConfigKind::SpringBoot
    );

    let user = KovaSettings::default();
    let effective = EffectiveConfig::merge(&user, Some(&loaded_proj));

    // Project overrides take effect in EffectiveConfig...
    assert_eq!(effective.settings.editor.tab_size, 2);
    assert!(effective.settings.editor.format_on_save);
    assert_eq!(effective.settings.jvm.jdtls_max_ram_mb, 4096);
    assert_eq!(
        effective.settings.jvm.preferred_jdk.as_deref(),
        Some("temurin-21")
    );
    assert_eq!(effective.run_configurations.len(), 1);

    // ...without mutating the user's own KovaSettings!
    assert_eq!(user.editor.tab_size, 4);
    assert!(!user.editor.format_on_save);
    assert_eq!(user.jvm.jdtls_max_ram_mb, 2048);
    assert_eq!(user.jvm.preferred_jdk, None);
}

#[test]
fn keymap_preset_deserializes_and_round_trips_jetbrains_vim_custom() {
    for (raw_value, expected_preset) in [
        ("jetbrains", KeymapPreset::JetBrains),
        ("vim", KeymapPreset::Vim),
        ("custom", KeymapPreset::Custom),
    ] {
        let raw = format!("[editor]\nkeymap = \"{raw_value}\"\n");
        let doc = SettingsDocument::from_toml_str(&raw).expect("parse keymap preset");
        assert_eq!(doc.settings().editor.keymap, expected_preset);
    }

    let mut doc = SettingsDocument::default_template().expect("default template");
    assert_eq!(doc.settings().editor.keymap, KeymapPreset::JetBrains);
    assert!(doc.to_toml_string().contains("keymap = \"jetbrains\""));

    for preset in [
        KeymapPreset::Vim,
        KeymapPreset::Custom,
        KeymapPreset::JetBrains,
    ] {
        doc.set_setting(SettingUpdate::EditorKeymap(preset))
            .expect("update keymap");
        assert_eq!(doc.settings().editor.keymap, preset);

        let reparsed =
            SettingsDocument::from_toml_str(&doc.to_toml_string()).expect("reparse keymap");
        assert_eq!(reparsed.settings().editor.keymap, preset);
    }
    assert!(doc.to_toml_string().contains("keymap = \"jetbrains\""));
}

#[test]
fn unknown_keys_detects_extra_keys_and_handles_inline_tables() {
    let raw = r#"
extra_top_level = true
ui = { theme = "kova-light", custom_ui_flag = 42 }

[editor]
vim_mode = true
experimental_flag = "on"

[editor.nested]
foo = 1

[custom_plugin]
enabled = true
"#;

    let doc = SettingsDocument::from_toml_str(raw).expect("parse with unknown keys");
    let unknown = doc.unknown_keys();
    assert_eq!(
        unknown,
        vec![
            "extra_top_level",
            "ui.custom_ui_flag",
            "editor.experimental_flag",
            "editor.nested",
            "custom_plugin",
        ]
    );

    let inline_known_only = "editor = { vim_mode = true, keymap = \"jetbrains\" }\n";
    let inline_doc =
        SettingsDocument::from_toml_str(inline_known_only).expect("parse inline editor table");
    assert_eq!(inline_doc.unknown_keys(), Vec::<String>::new());
}

#[test]
fn non_finite_floats_nan_and_infinity_are_rejected_and_rolled_back() {
    let nan_toml = "[editor]\nfont_size = nan\n";
    let err = SettingsDocument::from_toml_str(nan_toml).expect_err("NaN must fail validation");
    assert!(matches!(
        err,
        ConfigError::Validation {
            field: "editor.font_size",
            ..
        }
    ));

    let inf_toml = "[editor]\nline_height = inf\n";
    let err = SettingsDocument::from_toml_str(inf_toml).expect_err("Inf must fail validation");
    assert!(matches!(
        err,
        ConfigError::Validation {
            field: "editor.line_height",
            ..
        }
    ));

    let raw = "[editor]\nfont_size = 13.0 # keep valid\n";
    let mut doc = SettingsDocument::from_toml_str(raw).expect("parse valid float");

    let nan_err = doc
        .set_setting(SettingUpdate::EditorFontSize(f64::NAN))
        .expect_err("f64::NAN update must fail");
    assert!(matches!(
        nan_err,
        ConfigError::Validation {
            field: "editor.font_size",
            ..
        }
    ));
    assert!((doc.settings().editor.font_size - 13.0).abs() < f64::EPSILON);
    assert_eq!(doc.to_toml_string(), raw);

    let inf_err = doc
        .set_dotted_value("ui.sidebar_width", Value::from(f64::INFINITY))
        .expect_err("f64::INFINITY update must fail");
    assert!(matches!(
        inf_err,
        ConfigError::Validation {
            field: "ui.sidebar_width",
            ..
        }
    ));
    assert_eq!(doc.to_toml_string(), raw);
}
