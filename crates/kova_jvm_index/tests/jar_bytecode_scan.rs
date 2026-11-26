//! Integration tests for `.class` parsing and `.jar`/`.jmod` scanning in `kova_jvm_index`.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "test harness: failures must abort the test with context"
)]

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use kova_jvm_index::{
    BytecodeScanOptions, JdkOnDemandIndex, JvmIndexError, JvmSourceOrigin, JvmSymbolKind,
    JvmVisibility, parse_class_bytes, scan_jar_archive, split_display_signature,
};
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

// Classfile access flags (JVMS §4.1)
const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_PROTECTED: u16 = 0x0004;
const ACC_STATIC: u16 = 0x0008;
const ACC_FINAL: u16 = 0x0010;
const ACC_SUPER: u16 = 0x0020;
const ACC_BRIDGE: u16 = 0x0040;
const ACC_INTERFACE: u16 = 0x0200;
const ACC_ABSTRACT: u16 = 0x0400;
const ACC_SYNTHETIC: u16 = 0x1000;
const ACC_ANNOTATION: u16 = 0x2000;
const ACC_ENUM: u16 = 0x4000;
const ACC_MODULE: u16 = 0x8000;

struct FieldDef<'a> {
    flags: u16,
    name: &'a str,
    descriptor: &'a str,
    signature: Option<&'a str>,
    deprecated: bool,
    synthetic_attr: bool,
}

struct MethodDef<'a> {
    flags: u16,
    name: &'a str,
    descriptor: &'a str,
    signature: Option<&'a str>,
    deprecated: bool,
    synthetic_attr: bool,
    line_number: Option<u16>,
}

struct InnerClassDef<'a> {
    inner_class: &'a str,
    outer_class: Option<&'a str>,
    inner_name: Option<&'a str>,
    flags: u16,
}

struct RecordCompDef<'a> {
    name: &'a str,
    descriptor: &'a str,
}

struct ClassBuilder<'a> {
    major_version: u16,
    access_flags: u16,
    this_class: &'a str,
    super_class: Option<&'a str>,
    interfaces: Vec<&'a str>,
    fields: Vec<FieldDef<'a>>,
    methods: Vec<MethodDef<'a>>,
    signature: Option<&'a str>,
    deprecated: bool,
    synthetic_attr: bool,
    inner_classes: Vec<InnerClassDef<'a>>,
    record_components: Vec<RecordCompDef<'a>>,
    permitted_subclasses: Vec<&'a str>,
    module_name: Option<&'a str>,
    cp_entries: Vec<CpEntry>,
    utf8_map: HashMap<String, u16>,
    class_map: HashMap<String, u16>,
}

enum CpEntry {
    Utf8(String),
    Class(u16),
    Module(u16),
}

impl<'a> ClassBuilder<'a> {
    fn new(this_class: &'a str) -> Self {
        Self {
            major_version: 65, // Java 21
            access_flags: ACC_PUBLIC | ACC_SUPER,
            this_class,
            super_class: Some("java/lang/Object"),
            interfaces: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            signature: None,
            deprecated: false,
            synthetic_attr: false,
            inner_classes: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            module_name: None,
            cp_entries: Vec::new(),
            utf8_map: HashMap::new(),
            class_map: HashMap::new(),
        }
    }

    fn utf8(&mut self, s: &str) -> u16 {
        if let Some(&idx) = self.utf8_map.get(s) {
            return idx;
        }
        self.cp_entries.push(CpEntry::Utf8(s.to_owned()));
        let idx = u16::try_from(self.cp_entries.len()).expect("constant pool overflow");
        self.utf8_map.insert(s.to_owned(), idx);
        idx
    }

    fn class_ref(&mut self, internal_name: &str) -> u16 {
        if let Some(&idx) = self.class_map.get(internal_name) {
            return idx;
        }
        let name_idx = self.utf8(internal_name);
        self.cp_entries.push(CpEntry::Class(name_idx));
        let idx = u16::try_from(self.cp_entries.len()).expect("constant pool overflow");
        self.class_map.insert(internal_name.to_owned(), idx);
        idx
    }

    fn module_ref(&mut self, name: &str) -> u16 {
        let name_idx = self.utf8(name);
        self.cp_entries.push(CpEntry::Module(name_idx));
        u16::try_from(self.cp_entries.len()).expect("constant pool overflow")
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Synthetic classfile builder writes all required JVMS constant pool and attribute tables"
    )]
    fn build(mut self) -> Vec<u8> {
        // Pre-intern all strings needed in sections so constant pool is finalized before encoding.
        let this_idx = self.class_ref(self.this_class);
        let super_idx = self.super_class.map_or(0, |s| self.class_ref(s));
        let iface_indices: Vec<u16> = self
            .interfaces
            .clone()
            .into_iter()
            .map(|i| self.class_ref(i))
            .collect();

        // Build fields bytes
        let mut fields_bytes = Vec::new();
        push_u2(
            &mut fields_bytes,
            u16::try_from(self.fields.len()).expect("too many fields"),
        );
        let fields = std::mem::take(&mut self.fields);
        for f in fields {
            push_u2(&mut fields_bytes, f.flags);
            let name_idx = self.utf8(f.name);
            let desc_idx = self.utf8(f.descriptor);
            push_u2(&mut fields_bytes, name_idx);
            push_u2(&mut fields_bytes, desc_idx);

            let mut attrs = Vec::new();
            if let Some(sig) = f.signature {
                let sig_name = self.utf8("Signature");
                let sig_val = self.utf8(sig);
                attrs.push((sig_name, sig_val.to_be_bytes().to_vec()));
            }
            if f.deprecated {
                let dep_name = self.utf8("Deprecated");
                attrs.push((dep_name, Vec::new()));
            }
            if f.synthetic_attr {
                let syn_name = self.utf8("Synthetic");
                attrs.push((syn_name, Vec::new()));
            }
            write_attrs(&mut fields_bytes, &attrs);
        }

        // Build methods bytes
        let mut methods_bytes = Vec::new();
        push_u2(
            &mut methods_bytes,
            u16::try_from(self.methods.len()).expect("too many methods"),
        );
        let methods = std::mem::take(&mut self.methods);
        for m in methods {
            push_u2(&mut methods_bytes, m.flags);
            let name_idx = self.utf8(m.name);
            let desc_idx = self.utf8(m.descriptor);
            push_u2(&mut methods_bytes, name_idx);
            push_u2(&mut methods_bytes, desc_idx);

            let mut attrs = Vec::new();
            if let Some(sig) = m.signature {
                let sig_name = self.utf8("Signature");
                let sig_val = self.utf8(sig);
                attrs.push((sig_name, sig_val.to_be_bytes().to_vec()));
            }
            if m.deprecated {
                let dep_name = self.utf8("Deprecated");
                attrs.push((dep_name, Vec::new()));
            }
            if m.synthetic_attr {
                let syn_name = self.utf8("Synthetic");
                attrs.push((syn_name, Vec::new()));
            }
            if let Some(line) = m.line_number {
                let code_name = self.utf8("Code");
                let lnt_name = self.utf8("LineNumberTable");
                let mut code_payload = Vec::new();
                push_u2(&mut code_payload, 1); // max_stack
                push_u2(&mut code_payload, 1); // max_locals
                push_u4(&mut code_payload, 1); // code_length
                code_payload.push(0xB1); // return opcode
                push_u2(&mut code_payload, 0); // exception_table_length

                let mut lnt_payload = Vec::new();
                push_u2(&mut lnt_payload, 1); // line_number_table_length
                push_u2(&mut lnt_payload, 0); // start_pc
                push_u2(&mut lnt_payload, line);
                write_attrs(&mut code_payload, &[(lnt_name, lnt_payload)]);

                attrs.push((code_name, code_payload));
            }
            write_attrs(&mut methods_bytes, &attrs);
        }

        // Build class attributes
        let mut class_attrs = Vec::new();
        if let Some(sig) = self.signature {
            let attr_name = self.utf8("Signature");
            let sig_idx = self.utf8(sig);
            class_attrs.push((attr_name, sig_idx.to_be_bytes().to_vec()));
        }
        if self.deprecated {
            let attr_name = self.utf8("Deprecated");
            class_attrs.push((attr_name, Vec::new()));
        }
        if self.synthetic_attr {
            let attr_name = self.utf8("Synthetic");
            class_attrs.push((attr_name, Vec::new()));
        }
        if !self.inner_classes.is_empty() {
            let attr_name = self.utf8("InnerClasses");
            let inners = std::mem::take(&mut self.inner_classes);
            let mut payload = Vec::new();
            push_u2(
                &mut payload,
                u16::try_from(inners.len()).expect("too many inner classes"),
            );
            for inner in inners {
                let inner_idx = self.class_ref(inner.inner_class);
                let outer_idx = inner.outer_class.map_or(0, |o| self.class_ref(o));
                let inner_name_idx = inner.inner_name.map_or(0, |n| self.utf8(n));
                push_u2(&mut payload, inner_idx);
                push_u2(&mut payload, outer_idx);
                push_u2(&mut payload, inner_name_idx);
                push_u2(&mut payload, inner.flags);
            }
            class_attrs.push((attr_name, payload));
        }
        if !self.record_components.is_empty() {
            let attr_name = self.utf8("Record");
            let comps = std::mem::take(&mut self.record_components);
            let mut payload = Vec::new();
            push_u2(
                &mut payload,
                u16::try_from(comps.len()).expect("too many record components"),
            );
            for comp in comps {
                let name_idx = self.utf8(comp.name);
                let desc_idx = self.utf8(comp.descriptor);
                push_u2(&mut payload, name_idx);
                push_u2(&mut payload, desc_idx);
                push_u2(&mut payload, 0); // attributes_count
            }
            class_attrs.push((attr_name, payload));
        }
        if !self.permitted_subclasses.is_empty() {
            let attr_name = self.utf8("PermittedSubclasses");
            let subs = std::mem::take(&mut self.permitted_subclasses);
            let mut payload = Vec::new();
            push_u2(
                &mut payload,
                u16::try_from(subs.len()).expect("too many permitted subclasses"),
            );
            for sub in subs {
                let sub_idx = self.class_ref(sub);
                push_u2(&mut payload, sub_idx);
            }
            class_attrs.push((attr_name, payload));
        }
        if let Some(mod_name) = self.module_name {
            let attr_name = self.utf8("Module");
            let mod_idx = self.module_ref(mod_name);
            let mut payload = Vec::new();
            push_u2(&mut payload, mod_idx); // module_name_index
            push_u2(&mut payload, 0); // module_flags
            push_u2(&mut payload, 0); // module_version_index
            push_u2(&mut payload, 0); // requires_count
            push_u2(&mut payload, 0); // exports_count
            push_u2(&mut payload, 0); // opens_count
            push_u2(&mut payload, 0); // uses_count
            push_u2(&mut payload, 0); // provides_count
            class_attrs.push((attr_name, payload));
        }

        // Assemble full classfile
        let mut out = Vec::new();
        push_u4(&mut out, 0xCAFE_BABE);
        push_u2(&mut out, 0); // minor_version
        push_u2(&mut out, self.major_version);
        push_u2(
            &mut out,
            u16::try_from(self.cp_entries.len() + 1).expect("cp count overflow"),
        );
        for entry in &self.cp_entries {
            match entry {
                CpEntry::Utf8(s) => {
                    out.push(1);
                    push_u2(&mut out, u16::try_from(s.len()).expect("utf8 too long"));
                    out.extend_from_slice(s.as_bytes());
                }
                CpEntry::Class(idx) => {
                    out.push(7);
                    push_u2(&mut out, *idx);
                }
                CpEntry::Module(idx) => {
                    out.push(19);
                    push_u2(&mut out, *idx);
                }
            }
        }

        push_u2(&mut out, self.access_flags);
        push_u2(&mut out, this_idx);
        push_u2(&mut out, super_idx);
        push_u2(
            &mut out,
            u16::try_from(iface_indices.len()).expect("too many interfaces"),
        );
        for iface_idx in iface_indices {
            push_u2(&mut out, iface_idx);
        }
        out.extend_from_slice(&fields_bytes);
        out.extend_from_slice(&methods_bytes);
        write_attrs(&mut out, &class_attrs);
        out
    }
}

fn push_u2(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn push_u4(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn write_attrs(out: &mut Vec<u8>, attrs: &[(u16, Vec<u8>)]) {
    push_u2(
        out,
        u16::try_from(attrs.len()).expect("too many attributes"),
    );
    for (name_idx, payload) in attrs {
        push_u2(out, *name_idx);
        push_u4(out, u32::try_from(payload.len()).expect("attr too large"));
        out.extend_from_slice(payload);
    }
}

fn build_sample_service_class(version: u16, extra_method: Option<&str>) -> Vec<u8> {
    let mut builder = ClassBuilder::new("com/example/orders/OrderService");
    builder.major_version = version;
    builder.interfaces = vec!["java/io/Serializable"];
    builder.deprecated = true;
    builder.fields.push(FieldDef {
        flags: ACC_PRIVATE | ACC_FINAL,
        name: "maxRetries",
        descriptor: "I",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
    });
    builder.fields.push(FieldDef {
        flags: ACC_PUBLIC | ACC_FINAL,
        name: "tags",
        descriptor: "Ljava/util/List;",
        signature: Some("Ljava/util/List<Ljava/lang/String;>;"),
        deprecated: false,
        synthetic_attr: false,
    });
    builder.fields.push(FieldDef {
        flags: ACC_PRIVATE | ACC_SYNTHETIC,
        name: "this$0",
        descriptor: "Ljava/lang/Object;",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
    });
    builder.methods.push(MethodDef {
        flags: ACC_PUBLIC,
        name: "<init>",
        descriptor: "(J)V",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: Some(18),
    });
    builder.methods.push(MethodDef {
        flags: ACC_STATIC,
        name: "<clinit>",
        descriptor: "()V",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    builder.methods.push(MethodDef {
        flags: ACC_PUBLIC,
        name: "findOrders",
        descriptor: "(Ljava/lang/String;I)Ljava/util/List;",
        signature: Some(
            "<T:Ljava/lang/Comparable<TT;>;>(Ljava/lang/String;I)Ljava/util/List<TT;>;",
        ),
        deprecated: true,
        synthetic_attr: false,
        line_number: Some(42),
    });
    builder.methods.push(MethodDef {
        flags: ACC_PUBLIC | ACC_BRIDGE | ACC_SYNTHETIC,
        name: "findOrders",
        descriptor: "(Ljava/lang/Object;I)Ljava/lang/Object;",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    // Broken Signature attribute must fall back cleanly to base descriptor.
    builder.methods.push(MethodDef {
        flags: ACC_PUBLIC,
        name: "brokenSigMethod",
        descriptor: "(I)Ljava/lang/String;",
        signature: Some("Ljava/util/List<>;"),
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    if let Some(name) = extra_method {
        builder.methods.push(MethodDef {
            flags: ACC_PUBLIC,
            name,
            descriptor: "()V",
            signature: None,
            deprecated: false,
            synthetic_attr: false,
            line_number: Some(99),
        });
    }
    builder.build()
}

fn write_zip_file(path: &std::path::Path, jmod_header: bool, entries: &[(&str, &[u8])]) {
    let mut file = std::fs::File::create(path).unwrap();
    if jmod_header {
        file.write_all(&[0x4A, 0x4D, 0x01, 0x00]).unwrap();
    }
    let mut writer = ZipWriter::new(file);
    let zip_opts = SimpleFileOptions::default();
    for (name, bytes) in entries {
        writer.start_file(*name, zip_opts).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "End-to-end integration test covering all JVM symbol kinds and member visibility"
)]
fn parses_all_jvm_symbol_kinds_inner_classes_and_members() {
    let opts = BytecodeScanOptions::default();

    // 1. Standard class with generic methods, constructor, fallback signature, and private field filter
    let service_bytes = build_sample_service_class(65, None);
    let unit = parse_class_bytes(
        &service_bytes,
        JvmSourceOrigin::ClassFile {
            path: PathBuf::from("OrderService.class"),
        },
        &opts,
    )
    .unwrap()
    .expect("public class must be indexed");

    assert_eq!(unit.ty.fqcn, "com.example.orders.OrderService");
    assert_eq!(unit.ty.binary_name, "com.example.orders.OrderService");
    assert_eq!(unit.ty.simple_name, "OrderService");
    assert_eq!(unit.ty.package_name, "com.example.orders");
    assert_eq!(unit.ty.kind, JvmSymbolKind::Class);
    assert!(unit.ty.modifiers.is_deprecated);
    assert_eq!(unit.ty.super_fqcn.as_deref(), Some("java.lang.Object"));
    assert_eq!(unit.ty.interfaces, vec!["java.io.Serializable"]);

    // Private `maxRetries`, synthetic `this$0`, `<clinit>`, and bridge method are filtered out.
    assert_eq!(unit.members.len(), 4);
    assert_eq!(unit.members[0].name, "tags");
    assert_eq!(unit.members[0].kind, JvmSymbolKind::Field);
    assert_eq!(unit.members[0].display_signature, "List<String>");

    assert_eq!(unit.members[1].name, "OrderService");
    assert_eq!(unit.members[1].kind, JvmSymbolKind::Constructor);
    assert_eq!(unit.members[1].display_signature, "(long)");
    assert_eq!(unit.members[1].line, Some(18));

    assert_eq!(unit.members[2].name, "findOrders");
    assert_eq!(unit.members[2].kind, JvmSymbolKind::Method);
    assert_eq!(
        unit.members[2].display_signature,
        "<T extends Comparable<T>>(String, int): List<T>"
    );
    assert!(unit.members[2].modifiers.is_deprecated);
    assert_eq!(unit.members[2].line, Some(42));

    assert_eq!(unit.members[3].name, "brokenSigMethod");
    assert_eq!(unit.members[3].display_signature, "(int): String");

    // With include_private_members = true, `maxRetries` is retained while synthetic `this$0` stays skipped.
    let with_private = BytecodeScanOptions {
        include_private_members: true,
        ..BytecodeScanOptions::default()
    };
    let unit_priv = parse_class_bytes(
        &service_bytes,
        JvmSourceOrigin::ClassFile {
            path: PathBuf::from("OrderService.class"),
        },
        &with_private,
    )
    .unwrap()
    .unwrap();
    assert!(unit_priv.members.iter().any(|m| m.name == "maxRetries"));
    assert!(!unit_priv.members.iter().any(|m| m.name == "this$0"));

    // 2. Annotation (has both ACC_ANNOTATION and ACC_INTERFACE)
    let mut ann = ClassBuilder::new("com/example/MyMarker");
    ann.access_flags = ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT | ACC_ANNOTATION;
    let ann_unit = parse_class_bytes(
        &ann.build(),
        JvmSourceOrigin::ClassFile {
            path: PathBuf::from("MyMarker.class"),
        },
        &opts,
    )
    .unwrap()
    .unwrap();
    assert_eq!(ann_unit.ty.kind, JvmSymbolKind::Annotation);

    // 3. Interface (sealed with PermittedSubclasses)
    let mut iface = ClassBuilder::new("com/example/Shape");
    iface.access_flags = ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT;
    iface.permitted_subclasses = vec!["com/example/Circle"];
    let iface_unit = parse_class_bytes(
        &iface.build(),
        JvmSourceOrigin::ClassFile {
            path: PathBuf::from("Shape.class"),
        },
        &opts,
    )
    .unwrap()
    .unwrap();
    assert_eq!(iface_unit.ty.kind, JvmSymbolKind::Interface);
    assert!(iface_unit.ty.modifiers.is_sealed);

    // 4. Enum (filters synthetic $VALUES)
    let mut en = ClassBuilder::new("com/example/Color");
    en.access_flags = ACC_PUBLIC | ACC_FINAL | ACC_SUPER | ACC_ENUM;
    en.super_class = Some("java/lang/Enum");
    en.fields.push(FieldDef {
        flags: ACC_PUBLIC | ACC_STATIC | ACC_FINAL | ACC_ENUM,
        name: "RED",
        descriptor: "Lcom/example/Color;",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
    });
    en.fields.push(FieldDef {
        flags: ACC_PRIVATE | ACC_STATIC | ACC_FINAL,
        name: "$VALUES",
        descriptor: "[Lcom/example/Color;",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
    });
    let en_unit = parse_class_bytes(
        &en.build(),
        JvmSourceOrigin::ClassFile {
            path: PathBuf::from("Color.class"),
        },
        &with_private,
    )
    .unwrap()
    .unwrap();
    assert_eq!(en_unit.ty.kind, JvmSymbolKind::Enum);
    assert_eq!(en_unit.members.len(), 1);
    assert_eq!(en_unit.members[0].name, "RED");

    // 5. Java 23 (major 67) Record
    let mut rec = ClassBuilder::new("com/example/Money");
    rec.major_version = 67;
    rec.access_flags = ACC_PUBLIC | ACC_FINAL | ACC_SUPER;
    rec.super_class = Some("java/lang/Record");
    rec.record_components.push(RecordCompDef {
        name: "cents",
        descriptor: "J",
    });
    let rec_unit = parse_class_bytes(
        &rec.build(),
        JvmSourceOrigin::ClassFile {
            path: PathBuf::from("Money.class"),
        },
        &opts,
    )
    .unwrap()
    .unwrap();
    assert_eq!(rec_unit.ty.kind, JvmSymbolKind::Record);

    // 6. Named nested class with InnerClasses visibility & static override
    let mut inner = ClassBuilder::new("com/example/Outer$ProtectedStatic");
    // Outer classfile flag is package-private without ACC_STATIC, while InnerClasses carries real flags.
    inner.access_flags = ACC_SUPER;
    inner.inner_classes.push(InnerClassDef {
        inner_class: "com/example/Outer$ProtectedStatic",
        outer_class: Some("com/example/Outer"),
        inner_name: Some("ProtectedStatic"),
        flags: ACC_PROTECTED | ACC_STATIC,
    });
    let inner_unit = parse_class_bytes(
        &inner.build(),
        JvmSourceOrigin::ClassFile {
            path: PathBuf::from("Outer$ProtectedStatic.class"),
        },
        &opts,
    )
    .unwrap()
    .unwrap();
    assert_eq!(inner_unit.ty.fqcn, "com.example.Outer.ProtectedStatic");
    assert_eq!(
        inner_unit.ty.binary_name,
        "com.example.Outer$ProtectedStatic"
    );
    assert_eq!(inner_unit.ty.simple_name, "ProtectedStatic");
    assert_eq!(inner_unit.ty.modifiers.visibility, JvmVisibility::Protected);
    assert!(inner_unit.ty.modifiers.is_static);

    // 7. Anonymous ($1), inner_name=None, synthetic, and module-info must be filtered out
    let anonymous_nested = ClassBuilder::new("com/example/Outer$1");
    assert!(
        parse_class_bytes(
            &anonymous_nested.build(),
            JvmSourceOrigin::ClassFile {
                path: PathBuf::from("Outer$1.class"),
            },
            &opts,
        )
        .unwrap()
        .is_none()
    );

    let mut anon_inner = ClassBuilder::new("com/example/Outer$LocalAnon");
    anon_inner.inner_classes.push(InnerClassDef {
        inner_class: "com/example/Outer$LocalAnon",
        outer_class: None,
        inner_name: None,
        flags: 0,
    });
    assert!(
        parse_class_bytes(
            &anon_inner.build(),
            JvmSourceOrigin::ClassFile {
                path: PathBuf::from("Outer$LocalAnon.class"),
            },
            &opts,
        )
        .unwrap()
        .is_none()
    );

    let mut mod_info = ClassBuilder::new("module-info");
    mod_info.access_flags = ACC_MODULE;
    mod_info.super_class = None;
    mod_info.module_name = Some("com.example.app");
    assert!(
        parse_class_bytes(
            &mod_info.build(),
            JvmSourceOrigin::ClassFile {
                path: PathBuf::from("module-info.class"),
            },
            &opts,
        )
        .unwrap()
        .is_none()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "End-to-end integration test covering Multi-Release JAR deduplication and JMOD scanning"
)]
fn scans_jar_with_multi_release_dedup_and_jmod_archives() {
    let dir = TempDir::new().unwrap();
    let jar_path = dir.path().join("sample-mr.jar");

    let base_service = build_sample_service_class(52, None);
    let mr17_service = build_sample_service_class(61, Some("java17Only"));
    let mr21_service = build_sample_service_class(65, Some("java21VirtualThreads"));
    let mr25_service = build_sample_service_class(69, Some("java25Future"));
    let inner_class = {
        let mut b = ClassBuilder::new("com/example/orders/OrderService$LineItem");
        b.inner_classes.push(InnerClassDef {
            inner_class: "com/example/orders/OrderService$LineItem",
            outer_class: Some("com/example/orders/OrderService"),
            inner_name: Some("LineItem"),
            flags: ACC_PUBLIC | ACC_STATIC,
        });
        b.build()
    };
    let anon_class = ClassBuilder::new("com/example/orders/OrderService$1").build();
    let mut mod_info = ClassBuilder::new("module-info");
    mod_info.access_flags = ACC_MODULE;
    mod_info.super_class = None;
    mod_info.module_name = Some("com.example.orders");
    let mod_bytes = mod_info.build();

    write_zip_file(
        &jar_path,
        false,
        &[
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\nMulti-Release: true\n",
            ),
            ("META-INF/versions/9/module-info.class", &mod_bytes),
            ("com/example/orders/OrderService.class", &base_service),
            (
                "META-INF/versions/17/com/example/orders/OrderService.class",
                &mr17_service,
            ),
            (
                "META-INF/versions/21/com/example/orders/OrderService.class",
                &mr21_service,
            ),
            (
                "META-INF/versions/25/com/example/orders/OrderService.class",
                &mr25_service,
            ),
            (
                "com/example/orders/OrderService$LineItem.class",
                &inner_class,
            ),
            ("com/example/orders/OrderService$1.class", &anon_class),
        ],
    );

    // Default target_java_version = Some(21): picks version 21 override, skips 25, deduplicates binary_name.
    let result = scan_jar_archive(&jar_path, &BytecodeScanOptions::default()).unwrap();
    assert_eq!(result.units.len(), 2);
    assert_eq!(
        result.units[0].ty.binary_name,
        "com.example.orders.OrderService"
    );
    assert!(
        result.units[0]
            .members
            .iter()
            .any(|m| m.name == "java21VirtualThreads")
    );
    assert!(
        !result.units[0]
            .members
            .iter()
            .any(|m| m.name == "java25Future")
    );
    assert_eq!(
        result.units[0].ty.origin,
        JvmSourceOrigin::JarEntry {
            jar_path: jar_path.clone(),
            entry_name: "META-INF/versions/21/com/example/orders/OrderService.class".to_owned(),
        }
    );
    assert_eq!(
        result.units[1].ty.binary_name,
        "com.example.orders.OrderService$LineItem"
    );

    // Disabling Multi-Release (target_java_version = None) falls back to root entry.
    let base_only = scan_jar_archive(
        &jar_path,
        &BytecodeScanOptions {
            target_java_version: None,
            ..BytecodeScanOptions::default()
        },
    )
    .unwrap();
    assert_eq!(base_only.units.len(), 2);
    assert!(
        !base_only.units[0]
            .members
            .iter()
            .any(|m| m.name == "java21VirtualThreads")
    );

    // Test .jmod archive with real JM\x01\x00 magic prefix and non-class directories (native/, legal/)
    let jmod_path = dir.path().join("java.base.jmod");
    let array_list = ClassBuilder::new("java/util/ArrayList").build();
    write_zip_file(
        &jmod_path,
        true,
        &[
            ("classes/module-info.class", &mod_bytes),
            ("classes/java/util/ArrayList.class", &array_list),
            ("native/libjava.dylib", b"not-a-class"),
            ("legal/LICENSE", b"GPLv2"),
        ],
    );

    let jmod_res = scan_jar_archive(&jmod_path, &BytecodeScanOptions::default()).unwrap();
    assert_eq!(jmod_res.units.len(), 1);
    assert_eq!(jmod_res.units[0].ty.fqcn, "java.util.ArrayList");
    assert_eq!(
        jmod_res.units[0].ty.origin,
        JvmSourceOrigin::JmodEntry {
            jmod_path,
            entry_name: "classes/java/util/ArrayList.class".to_owned(),
        }
    );
    assert_eq!(jmod_res.stats.total_entries, 4);
    assert_eq!(jmod_res.stats.class_entries, 2);
    assert_eq!(jmod_res.stats.indexed_types, 1);
    assert_eq!(jmod_res.stats.skipped_entries, 3);
}

#[test]
fn handles_corrupt_classfiles_oversized_entries_and_strict_mode() {
    // Direct parse_class_bytes negative cases (AC-4)
    assert!(matches!(
        parse_class_bytes(
            &[],
            JvmSourceOrigin::ClassFile {
                path: PathBuf::from("empty.class"),
            },
            &BytecodeScanOptions::default(),
        ),
        Err(JvmIndexError::InvalidClassFile { .. })
    ));
    assert!(matches!(
        parse_class_bytes(
            &[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x41],
            JvmSourceOrigin::ClassFile {
                path: PathBuf::from("bad_magic.class"),
            },
            &BytecodeScanOptions::default(),
        ),
        Err(JvmIndexError::InvalidClassFile { .. })
    ));
    let valid_bytes = build_sample_service_class(65, None);
    let truncated = &valid_bytes[..valid_bytes.len() / 2];
    assert!(matches!(
        parse_class_bytes(
            truncated,
            JvmSourceOrigin::ClassFile {
                path: PathBuf::from("truncated.class"),
            },
            &BytecodeScanOptions::default(),
        ),
        Err(JvmIndexError::InvalidClassFile { .. })
    ));

    // Archive with one valid class and one corrupt class
    let dir = TempDir::new().unwrap();
    let mixed_jar = dir.path().join("mixed.jar");
    let good = ClassBuilder::new("com/example/Good").build();
    write_zip_file(
        &mixed_jar,
        false,
        &[
            ("com/example/Good.class", &good),
            ("com/example/Broken.class", b"\xCA\xFE\xBA\xBE\x00\x00"),
        ],
    );

    let lenient = scan_jar_archive(&mixed_jar, &BytecodeScanOptions::default()).unwrap();
    assert_eq!(lenient.units.len(), 1);
    assert_eq!(lenient.units[0].ty.fqcn, "com.example.Good");
    assert_eq!(lenient.stats.warnings.len(), 1);
    assert_eq!(lenient.stats.skipped_entries, 1);

    let strict_err = scan_jar_archive(
        &mixed_jar,
        &BytecodeScanOptions {
            strict: true,
            ..BytecodeScanOptions::default()
        },
    )
    .expect_err("strict mode must fail on corrupt class entry");
    assert!(matches!(strict_err, JvmIndexError::InvalidClassFile { .. }));

    // Zip-bomb / max_class_bytes protection
    let bounded_jar = dir.path().join("bounded.jar");
    write_zip_file(
        &bounded_jar,
        false,
        &[("com/example/Big.class", &valid_bytes)],
    );
    let tiny_limit_opts = BytecodeScanOptions {
        max_class_bytes: 64,
        strict: false,
        ..BytecodeScanOptions::default()
    };
    let bounded_lenient = scan_jar_archive(&bounded_jar, &tiny_limit_opts).unwrap();
    assert_eq!(bounded_lenient.units.len(), 0);
    assert_eq!(bounded_lenient.stats.warnings.len(), 1);

    let bounded_strict = scan_jar_archive(
        &bounded_jar,
        &BytecodeScanOptions {
            max_class_bytes: 64,
            strict: true,
            ..BytecodeScanOptions::default()
        },
    )
    .expect_err("strict mode must reject oversized class entry");
    assert!(matches!(
        bounded_strict,
        JvmIndexError::ArchiveEntryRead { .. }
    ));

    // Non-existent and invalid archives
    let missing = scan_jar_archive(
        &dir.path().join("missing.jar"),
        &BytecodeScanOptions::default(),
    )
    .expect_err("missing file must return Io error");
    assert!(matches!(missing, JvmIndexError::Io { .. }));

    let invalid_zip = dir.path().join("not-a-zip.jar");
    std::fs::write(&invalid_zip, b"not a zip archive").unwrap();
    let bad_archive =
        scan_jar_archive(&invalid_zip, &BytecodeScanOptions::default()).expect_err("bad zip");
    assert!(matches!(bad_archive, JvmIndexError::InvalidArchive { .. }));
}

#[test]
fn scans_two_thousand_synthetic_classes_within_nfr3_budget() {
    let dir = TempDir::new().unwrap();
    let large_jar = dir.path().join("two-thousand.jar");

    let mut owned_entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(2_000);
    for idx in 0..2_000 {
        let internal = format!("com/example/generated/Type{idx:04}");
        let mut builder = ClassBuilder::new(&internal);
        builder.methods.push(MethodDef {
            flags: ACC_PUBLIC,
            name: "compute",
            descriptor: "(Ljava/lang/String;I)I",
            signature: None,
            deprecated: false,
            synthetic_attr: false,
            line_number: Some(10),
        });
        owned_entries.push((format!("{internal}.class"), builder.build()));
    }

    let refs: Vec<(&str, &[u8])> = owned_entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    write_zip_file(&large_jar, false, &refs);

    let start = Instant::now();
    let result = scan_jar_archive(&large_jar, &BytecodeScanOptions::default()).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(result.units.len(), 2_000);
    assert_eq!(result.stats.indexed_types, 2_000);
    assert_eq!(result.stats.indexed_members, 2_000);
    assert_eq!(result.stats.warnings.len(), 0);
    // Generous ceiling in debug builds while ensuring sub-second single-thread throughput.
    assert!(
        elapsed.as_millis() < 1_000,
        "scanning 2 000 classes took unexpectedly long: {elapsed:?}"
    );
}

fn build_mock_java_base_jmod(jmods_dir: &std::path::Path) {
    let mut obj_builder = ClassBuilder::new("java/lang/Object");
    obj_builder.super_class = None;
    obj_builder.methods.push(MethodDef {
        flags: ACC_PUBLIC,
        name: "toString",
        descriptor: "()Ljava/lang/String;",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    obj_builder.methods.push(MethodDef {
        flags: ACC_PUBLIC,
        name: "hashCode",
        descriptor: "()I",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    let obj_bytes = obj_builder.build();

    let mut objects_builder = ClassBuilder::new("java/util/Objects");
    objects_builder.methods.push(MethodDef {
        flags: ACC_PUBLIC | ACC_STATIC,
        name: "requireNonNull",
        descriptor: "(Ljava/lang/Object;Ljava/lang/String;)Ljava/lang/Object;",
        signature: Some("<T:Ljava/lang/Object;>(TT;Ljava/lang/String;)TT;"),
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    objects_builder.methods.push(MethodDef {
        flags: ACC_PUBLIC | ACC_STATIC,
        name: "isNull",
        descriptor: "(Ljava/lang/Object;)Z",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    objects_builder.methods.push(MethodDef {
        flags: ACC_PRIVATE,
        name: "<init>",
        descriptor: "()V",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    let objects_bytes = objects_builder.build();

    let mut system_builder = ClassBuilder::new("java/lang/System");
    system_builder.fields.push(FieldDef {
        flags: ACC_PUBLIC | ACC_STATIC | ACC_FINAL,
        name: "out",
        descriptor: "Ljava/io/PrintStream;",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
    });
    let system_bytes = system_builder.build();

    let mut string_builder = ClassBuilder::new("java/lang/String");
    string_builder.methods.push(MethodDef {
        flags: ACC_PUBLIC,
        name: "<init>",
        descriptor: "()V",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    string_builder.methods.push(MethodDef {
        flags: ACC_PUBLIC,
        name: "length",
        descriptor: "()I",
        signature: None,
        deprecated: false,
        synthetic_attr: false,
        line_number: None,
    });
    let string_bytes = string_builder.build();

    write_zip_file(
        &jmods_dir.join("java.base.jmod"),
        true,
        &[
            ("classes/java/lang/Object.class", &obj_bytes),
            ("classes/java/util/Objects.class", &objects_bytes),
            ("classes/java/lang/System.class", &system_bytes),
            ("classes/java/lang/String.class", &string_bytes),
        ],
    );
}

#[test]
fn jdk_on_demand_index_reads_jmod_entries_inherits_object_and_caches() {
    let dir = TempDir::new().unwrap();
    let jmods_dir = dir.path().join("jmods");
    std::fs::create_dir_all(&jmods_dir).unwrap();
    build_mock_java_base_jmod(&jmods_dir);

    let index = JdkOnDemandIndex::from_jdk_home(dir.path());
    assert!(index.has_archives());

    // 1. Static lookup on java.util.Objects
    let t0 = Instant::now();
    let members = index.collect_members("java.util.Objects", true, None);
    let first_elapsed = t0.elapsed();
    assert!(
        first_elapsed.as_millis() < 15,
        "first jmod lookup took {first_elapsed:?}"
    );
    let req = members
        .iter()
        .find(|m| m.name == "requireNonNull")
        .expect("requireNonNull must be present");
    assert_eq!(req.return_type.as_deref(), Some("T"));
    assert!(req.params_signature.as_deref().unwrap().contains("String"));
    assert!(members.iter().any(|m| m.name == "isNull"));

    // 2. Cached hit
    let t1 = Instant::now();
    let cached = index.lookup_class("java.util.Objects").expect("cached");
    assert_eq!(cached.ty.fqcn, "java.util.Objects");
    assert!(t1.elapsed().as_millis() < 2);

    // 3. Instance lookup on java.lang.String includes Object methods and skips Constructor
    let str_members = index.collect_members("java.lang.String", false, None);
    let str_names: Vec<&str> = str_members.iter().map(|m| m.name.as_str()).collect();
    assert!(str_names.contains(&"length"));
    assert!(str_names.contains(&"toString"));
    assert!(str_names.contains(&"hashCode"));
    assert!(!str_names.contains(&"String"));

    // 4. Field type resolution on java.lang.System.out
    assert_eq!(
        index.resolve_field_type("java.lang.System", "out", None),
        Some("java.io.PrintStream".to_owned())
    );
    assert_eq!(
        split_display_signature(JvmSymbolKind::Method, "(Object): boolean"),
        (Some("(Object)".to_owned()), Some("boolean".to_owned()))
    );
}
