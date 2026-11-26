//! Syntax highlight tokens and capture resolution for Java, Kotlin, HTTP, TOML, JSON, YAML, XML, and Groovy.

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, Visitor},
    ser::SerializeStruct,
};

use crate::color::RgbaColor;

/// Typographical style for a single syntax highlight token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxStyle {
    /// Foreground color of the highlighted token.
    pub fg: RgbaColor,
    /// Whether the token is rendered in bold weight.
    pub bold: bool,
    /// Whether the token is rendered in italic style.
    pub italic: bool,
    /// Whether the token is rendered with an underline.
    pub underline: bool,
}

impl SyntaxStyle {
    /// Creates a plain syntax style with `bold`, `italic`, and `underline` disabled.
    #[must_use]
    pub const fn new(fg: RgbaColor) -> Self {
        Self {
            fg,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

impl Serialize for SyntaxStyle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Always emit as a map/table so `toml::to_string_pretty` never triggers
        // `ValueAfterTable` regardless of field ordering in `SyntaxTokens`.
        let field_count =
            1 + usize::from(self.bold) + usize::from(self.italic) + usize::from(self.underline);
        let mut map = serializer.serialize_struct("SyntaxStyle", field_count)?;
        map.serialize_field("fg", &self.fg)?;
        if self.bold {
            map.serialize_field("bold", &true)?;
        }
        if self.italic {
            map.serialize_field("italic", &true)?;
        }
        if self.underline {
            map.serialize_field("underline", &true)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SyntaxStyle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SyntaxStyleVisitor;

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SyntaxStyleTable {
            fg: RgbaColor,
            #[serde(default)]
            bold: bool,
            #[serde(default)]
            italic: bool,
            #[serde(default)]
            underline: bool,
        }

        impl<'de> Visitor<'de> for SyntaxStyleVisitor {
            type Value = SyntaxStyle;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a hex color string `#RRGGBB` or a syntax style table `{ fg, bold, italic, underline }`")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let fg = RgbaColor::from_hex(value).map_err(de::Error::custom)?;
                Ok(SyntaxStyle::new(fg))
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let table =
                    SyntaxStyleTable::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(SyntaxStyle {
                    fg: table.fg,
                    bold: table.bold,
                    italic: table.italic,
                    underline: table.underline,
                })
            }
        }

        deserializer.deserialize_any(SyntaxStyleVisitor)
    }
}

/// Canonical Aether syntax token categories (`[syntax]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyntaxTokens {
    /// Declaration and modifier keywords (`public`, `class`, `fun`, `val`).
    pub keyword: SyntaxStyle,
    /// Control-flow keywords (`if`, `else`, `return`, `when`, `try`).
    pub keyword_control: SyntaxStyle,
    /// User-defined and library type names (`String`, `Optional`, `OrderService`).
    #[serde(rename = "type")]
    pub ty: SyntaxStyle,
    /// Built-in primitive and special types (`int`, `long`, `boolean`, `void`, `Unit`).
    pub type_builtin: SyntaxStyle,
    /// JVM annotations and attributes (`@Service`, `@Test`, `@Override`).
    pub annotation: SyntaxStyle,
    /// Function and method declarations and calls.
    pub function: SyntaxStyle,
    /// Local variables and general identifiers.
    pub variable: SyntaxStyle,
    /// Method and lambda parameters.
    pub parameter: SyntaxStyle,
    /// Fields, Kotlin properties, and TOML/JSON object keys.
    pub property: SyntaxStyle,
    /// Constants and enum variants.
    pub constant: SyntaxStyle,
    /// String literals and text blocks.
    pub string: SyntaxStyle,
    /// Escape sequences and string interpolation delimiters (`\n`, `${expr}`).
    pub string_escape: SyntaxStyle,
    /// Numeric literals (`42`, `3.14`, `0xFF`).
    pub number: SyntaxStyle,
    /// Boolean and null literals (`true`, `false`, `null`).
    pub boolean: SyntaxStyle,
    /// Operators (`=`, `->`, `::`, `?.`, `+`).
    pub operator: SyntaxStyle,
    /// Brackets, delimiters, and separators.
    pub punctuation: SyntaxStyle,
    /// Line and block comments.
    pub comment: SyntaxStyle,
    /// Documentation comments (`/** Javadoc */`, `KDoc`).
    pub comment_doc: SyntaxStyle,
    /// HTTP request methods in `.http` files (`GET`, `POST`, `DELETE`).
    pub http_method: SyntaxStyle,
    /// HTTP request URLs and URIs in `.http` files.
    pub http_url: SyntaxStyle,
    /// HTTP header names in `.http` files.
    pub http_header: SyntaxStyle,
    /// HTTP status codes and protocol versions in `.http` files.
    pub http_status: SyntaxStyle,
}

impl SyntaxTokens {
    /// Returns the [`SyntaxStyle`] associated with `kind`.
    #[must_use]
    pub const fn style(&self, kind: SyntaxTokenKind) -> &SyntaxStyle {
        match kind {
            SyntaxTokenKind::Keyword => &self.keyword,
            SyntaxTokenKind::KeywordControl => &self.keyword_control,
            SyntaxTokenKind::Type => &self.ty,
            SyntaxTokenKind::TypeBuiltin => &self.type_builtin,
            SyntaxTokenKind::Annotation => &self.annotation,
            SyntaxTokenKind::Function => &self.function,
            SyntaxTokenKind::Variable => &self.variable,
            SyntaxTokenKind::Parameter => &self.parameter,
            SyntaxTokenKind::Property => &self.property,
            SyntaxTokenKind::Constant => &self.constant,
            SyntaxTokenKind::String => &self.string,
            SyntaxTokenKind::StringEscape => &self.string_escape,
            SyntaxTokenKind::Number => &self.number,
            SyntaxTokenKind::Boolean => &self.boolean,
            SyntaxTokenKind::Operator => &self.operator,
            SyntaxTokenKind::Punctuation => &self.punctuation,
            SyntaxTokenKind::Comment => &self.comment,
            SyntaxTokenKind::CommentDoc => &self.comment_doc,
            SyntaxTokenKind::HttpMethod => &self.http_method,
            SyntaxTokenKind::HttpUrl => &self.http_url,
            SyntaxTokenKind::HttpHeader => &self.http_header,
            SyntaxTokenKind::HttpStatus => &self.http_status,
        }
    }
}

/// Enumeration of all 22 semantic syntax token kinds supported by Aether.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxTokenKind {
    /// Declaration and modifier keywords.
    Keyword,
    /// Control-flow keywords.
    KeywordControl,
    /// User-defined and library type names.
    Type,
    /// Built-in primitive and unit types.
    TypeBuiltin,
    /// Annotations and attributes.
    Annotation,
    /// Functions and methods.
    Function,
    /// Variables and general identifiers.
    Variable,
    /// Function and lambda parameters.
    Parameter,
    /// Fields, properties, and TOML/JSON keys.
    Property,
    /// Constants and enum variants.
    Constant,
    /// String literals.
    String,
    /// Escape sequences and interpolation markers.
    StringEscape,
    /// Numeric literals.
    Number,
    /// Boolean and null literals.
    Boolean,
    /// Operators.
    Operator,
    /// Brackets and delimiters.
    Punctuation,
    /// Standard comments.
    Comment,
    /// Documentation comments (`Javadoc`, `KDoc`).
    CommentDoc,
    /// HTTP request method (`GET`, `POST`, etc.).
    HttpMethod,
    /// HTTP request URI/URL.
    HttpUrl,
    /// HTTP header name.
    HttpHeader,
    /// HTTP status code or protocol version.
    HttpStatus,
}

impl SyntaxTokenKind {
    /// All 22 semantic syntax token kinds in canonical order.
    pub const ALL: [Self; 22] = [
        Self::Keyword,
        Self::KeywordControl,
        Self::Type,
        Self::TypeBuiltin,
        Self::Annotation,
        Self::Function,
        Self::Variable,
        Self::Parameter,
        Self::Property,
        Self::Constant,
        Self::String,
        Self::StringEscape,
        Self::Number,
        Self::Boolean,
        Self::Operator,
        Self::Punctuation,
        Self::Comment,
        Self::CommentDoc,
        Self::HttpMethod,
        Self::HttpUrl,
        Self::HttpHeader,
        Self::HttpStatus,
    ];
}

/// Languages with first-class syntax support in `kova_syntax`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxLanguage {
    /// Java source files (`.java`).
    Java,
    /// Kotlin source and build scripts (`.kt`, `.kts`).
    Kotlin,
    /// Built-in REST request files (`.http`).
    Http,
    /// Configuration files (`settings.toml`, `kova.toml`, `Cargo.toml`).
    Toml,
    /// Structured JSON data files (`.json`).
    Json,
    /// YAML configuration and manifest files (`.yaml`, `.yml`).
    Yaml,
    /// XML, Maven POM, and XML Schema files (`.xml`, `.pom`, `.xsd`).
    Xml,
    /// Groovy sources and Gradle Groovy DSL build scripts (`.groovy`, `.gradle`).
    Groovy,
}

/// Resolves a Tree-sitter capture name (with or without leading `@`) into an Aether [`SyntaxTokenKind`].
///
/// Exact language-specific and shared captures are checked first; if none matches,
/// trailing dot-separated segments are stripped iteratively (e.g.,
/// `@keyword.control.conditional` -> `keyword.control` -> `KeywordControl`) so
/// specialized grammar captures gracefully inherit parent styling.
#[must_use]
pub fn resolve_capture(language: SyntaxLanguage, capture_name: &str) -> Option<SyntaxTokenKind> {
    let normalized = capture_name.strip_prefix('@').unwrap_or(capture_name);
    if normalized.is_empty() {
        return None;
    }

    let mut candidate = normalized;
    loop {
        if let Some(kind) = resolve_language_specific(language, candidate) {
            return Some(kind);
        }
        if let Some(kind) = resolve_shared_capture(candidate) {
            return Some(kind);
        }
        match candidate.rsplit_once('.') {
            Some((prefix, _)) => candidate = prefix,
            None => return None,
        }
    }
}

fn resolve_language_specific(language: SyntaxLanguage, capture: &str) -> Option<SyntaxTokenKind> {
    match (language, capture) {
        (SyntaxLanguage::Http, "keyword.http.method" | "method.http") => {
            Some(SyntaxTokenKind::HttpMethod)
        }
        (SyntaxLanguage::Http, "string.special.url" | "uri") => Some(SyntaxTokenKind::HttpUrl),
        (SyntaxLanguage::Http, "property.http.header" | "constant.http.header") => {
            Some(SyntaxTokenKind::HttpHeader)
        }
        (SyntaxLanguage::Http, "number.http.status" | "constant.http.version") => {
            Some(SyntaxTokenKind::HttpStatus)
        }
        (SyntaxLanguage::Toml | SyntaxLanguage::Json | SyntaxLanguage::Yaml, "label")
        | (SyntaxLanguage::Xml, "tag.attribute") => Some(SyntaxTokenKind::Property),
        (SyntaxLanguage::Yaml, "string.special") => Some(SyntaxTokenKind::Constant),
        (SyntaxLanguage::Xml, "tag") => Some(SyntaxTokenKind::Keyword),
        (SyntaxLanguage::Xml, "tag.delimiter") => Some(SyntaxTokenKind::Punctuation),
        (SyntaxLanguage::Xml, "constant.character" | "string.special.symbol") => {
            Some(SyntaxTokenKind::StringEscape)
        }
        _ => None,
    }
}

fn resolve_shared_capture(capture: &str) -> Option<SyntaxTokenKind> {
    match capture {
        "keyword" | "keyword.modifier" | "keyword.import" | "keyword.type" => {
            Some(SyntaxTokenKind::Keyword)
        }
        "keyword.control"
        | "keyword.conditional"
        | "keyword.repeat"
        | "keyword.return"
        | "keyword.exception" => Some(SyntaxTokenKind::KeywordControl),
        "type" | "type.definition" | "constructor" => Some(SyntaxTokenKind::Type),
        "type.builtin" => Some(SyntaxTokenKind::TypeBuiltin),
        "attribute" | "annotation" => Some(SyntaxTokenKind::Annotation),
        "function" | "function.method" | "function.call" | "method" | "method.call" => {
            Some(SyntaxTokenKind::Function)
        }
        "variable" | "variable.builtin" => Some(SyntaxTokenKind::Variable),
        "variable.parameter" | "parameter" => Some(SyntaxTokenKind::Parameter),
        "property" | "variable.member" | "field" => Some(SyntaxTokenKind::Property),
        "constant" | "constant.builtin" => Some(SyntaxTokenKind::Constant),
        "string" | "string.special" => Some(SyntaxTokenKind::String),
        "string.escape" | "punctuation.special" => Some(SyntaxTokenKind::StringEscape),
        "number" | "number.float" => Some(SyntaxTokenKind::Number),
        "boolean" | "constant.builtin.boolean" | "null" => Some(SyntaxTokenKind::Boolean),
        "operator" => Some(SyntaxTokenKind::Operator),
        "punctuation" | "punctuation.bracket" | "punctuation.delimiter" => {
            Some(SyntaxTokenKind::Punctuation)
        }
        "comment" => Some(SyntaxTokenKind::Comment),
        "comment.documentation" => Some(SyntaxTokenKind::CommentDoc),
        _ => None,
    }
}
