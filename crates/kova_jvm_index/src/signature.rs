//! JVMS §4.3 descriptor and JVMS §4.7.9 generic signature decoder.
//!
//! Converts raw bytecode descriptors (such as `(Ljava/lang/String;I)Ljava/util/Optional;`)
//! and generic `Signature` attributes (such as `<T:Ljava/lang/Comparable<TT;>;>(Ljava/util/List<TT;>;)TT;`)
//! into human-readable Java-style signatures with strict recursion-depth protection.

use crate::error::JvmIndexError;

/// Maximum permitted nesting depth while decoding recursive JVM descriptors or generic signatures.
pub const MAX_SIGNATURE_DEPTH: usize = 64;

/// Controls whether object type names are rendered in short or package-qualified form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypeDisplayMode {
    /// Strip package prefix, keeping nested class qualifiers (`java/util/Map$Entry` -> `Map.Entry`).
    #[default]
    Short,
    /// Preserve full package qualification (`java/util/Map$Entry` -> `java.util.Map.Entry`).
    Qualified,
}

/// Structured representation of a decoded JVMS §4.7.9 `ClassSignature`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedClassSignature {
    /// Generic type parameters declared on the class (e.g. `Some("<T extends Number>")`), if any.
    pub type_parameters: Option<String>,
    /// Decoded superclass type (e.g. `"Object"` or `"AbstractList<T>"`).
    pub superclass: String,
    /// Decoded superinterface types.
    pub interfaces: Vec<String>,
}

impl DecodedClassSignature {
    /// Formats the class signature into a concise human-readable declaration suffix.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::new();
        if let Some(params) = &self.type_parameters {
            out.push_str(params);
        }
        if self.superclass != "Object" && self.superclass != "java.lang.Object" {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str("extends ");
            out.push_str(&self.superclass);
        }
        if !self.interfaces.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str("implements ");
            out.push_str(&self.interfaces.join(", "));
        }
        out
    }
}

/// Decodes a JVMS §4.3 field descriptor (e.g. `"[Ljava/lang/String;"` -> `"String[]"`).
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is empty, malformed, contains trailing
/// characters, or exceeds [`MAX_SIGNATURE_DEPTH`] array nesting.
pub fn decode_field_descriptor(raw: &str) -> Result<String, JvmIndexError> {
    decode_field_descriptor_with_mode(raw, TypeDisplayMode::Short)
}

/// Decodes a JVMS §4.3 field descriptor using the specified [`TypeDisplayMode`].
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is empty, malformed, contains trailing
/// characters, or exceeds [`MAX_SIGNATURE_DEPTH`] array nesting.
pub fn decode_field_descriptor_with_mode(
    raw: &str,
    mode: TypeDisplayMode,
) -> Result<String, JvmIndexError> {
    let mut cursor = Cursor::new(raw, mode);
    let ty = cursor.parse_field_type(0)?;
    cursor.expect_eof()?;
    Ok(ty)
}

/// Decodes a JVMS §4.3 method descriptor (e.g. `"(Ljava/lang/String;I)Ljava/util/Optional;"`
/// -> `"(String, int): Optional"`).
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_method_descriptor(raw: &str) -> Result<String, JvmIndexError> {
    decode_method_descriptor_with_mode(raw, TypeDisplayMode::Short)
}

/// Decodes a JVMS §4.3 method descriptor using the specified [`TypeDisplayMode`].
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_method_descriptor_with_mode(
    raw: &str,
    mode: TypeDisplayMode,
) -> Result<String, JvmIndexError> {
    let mut cursor = Cursor::new(raw, mode);
    cursor.expect_byte(b'(', "method descriptor must start with '('")?;
    let mut params = Vec::new();
    while !cursor.eat_byte(b')') {
        params.push(cursor.parse_field_type(0)?);
    }
    let ret = cursor.parse_return_descriptor(0)?;
    cursor.expect_eof()?;
    Ok(format!("({}): {ret}", params.join(", ")))
}

/// Decodes a JVMS §4.3 constructor descriptor into its parameter list only
/// (e.g. `"(J)V"` -> `"(long)"`).
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_constructor_descriptor(raw: &str) -> Result<String, JvmIndexError> {
    let mut cursor = Cursor::new(raw, TypeDisplayMode::Short);
    cursor.expect_byte(b'(', "constructor descriptor must start with '('")?;
    let mut params = Vec::new();
    while !cursor.eat_byte(b')') {
        params.push(cursor.parse_field_type(0)?);
    }
    let _ret = cursor.parse_return_descriptor(0)?;
    cursor.expect_eof()?;
    Ok(format!("({})", params.join(", ")))
}

/// Decodes a JVMS §4.7.9 generic field signature (e.g. `"Ljava/util/List<Ljava/lang/String;>;"`
/// -> `"List<String>"`).
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_field_signature(raw: &str) -> Result<String, JvmIndexError> {
    decode_field_signature_with_mode(raw, TypeDisplayMode::Short)
}

/// Decodes a JVMS §4.7.9 generic field signature using the specified [`TypeDisplayMode`].
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_field_signature_with_mode(
    raw: &str,
    mode: TypeDisplayMode,
) -> Result<String, JvmIndexError> {
    let mut cursor = Cursor::new(raw, mode);
    let ty = cursor.parse_reference_type_signature(0)?;
    cursor.expect_eof()?;
    Ok(ty)
}

/// Decodes a JVMS §4.7.9 generic method signature into a display string
/// (e.g. `"<T:Ljava/lang/Comparable<TT;>;>(Ljava/util/List<TT;>;)TT;"`
/// -> `"<T extends Comparable<T>>(List<T>): T"`).
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_method_signature(raw: &str) -> Result<String, JvmIndexError> {
    decode_method_signature_with_mode(raw, TypeDisplayMode::Short)
}

/// Decodes a JVMS §4.7.9 generic method signature using the specified [`TypeDisplayMode`].
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_method_signature_with_mode(
    raw: &str,
    mode: TypeDisplayMode,
) -> Result<String, JvmIndexError> {
    let mut cursor = Cursor::new(raw, mode);
    let type_params = if cursor.peek_byte() == Some(b'<') {
        Some(cursor.parse_type_parameters(0)?)
    } else {
        None
    };

    cursor.expect_byte(b'(', "method signature must contain '('")?;
    let mut params = Vec::new();
    while !cursor.eat_byte(b')') {
        params.push(cursor.parse_java_type_signature(0)?);
    }

    let ret = cursor.parse_result_signature(0)?;
    let mut throws = Vec::new();
    while cursor.eat_byte(b'^') {
        let exception_type = match cursor.peek_byte() {
            Some(b'L') => cursor.parse_class_type_signature(0)?,
            Some(b'T') => cursor.parse_type_variable_signature()?,
            _ => {
                return Err(
                    cursor.err("expected class or type variable after '^' in throws clause")
                );
            }
        };
        throws.push(exception_type);
    }
    cursor.expect_eof()?;

    let mut out = String::new();
    if let Some(tp) = type_params {
        out.push_str(&tp);
    }
    out.push('(');
    out.push_str(&params.join(", "));
    out.push_str("): ");
    out.push_str(&ret);
    if !throws.is_empty() {
        out.push_str(" throws ");
        out.push_str(&throws.join(", "));
    }
    Ok(out)
}

/// Decodes a JVMS §4.7.9 generic constructor signature into its type parameters (if any) and
/// parameter list (e.g. `"(Ljava/util/List<Ljava/lang/String;>;)V"` -> `"(List<String>)"`).
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_constructor_signature(raw: &str) -> Result<String, JvmIndexError> {
    let mut cursor = Cursor::new(raw, TypeDisplayMode::Short);
    let type_params = if cursor.peek_byte() == Some(b'<') {
        Some(cursor.parse_type_parameters(0)?)
    } else {
        None
    };
    cursor.expect_byte(b'(', "constructor signature must contain '('")?;
    let mut params = Vec::new();
    while !cursor.eat_byte(b')') {
        params.push(cursor.parse_java_type_signature(0)?);
    }
    let _ret = cursor.parse_result_signature(0)?;
    while cursor.eat_byte(b'^') {
        match cursor.peek_byte() {
            Some(b'L') => {
                drop(cursor.parse_class_type_signature(0)?);
            }
            Some(b'T') => {
                drop(cursor.parse_type_variable_signature()?);
            }
            _ => {
                return Err(
                    cursor.err("expected class or type variable after '^' in throws clause")
                );
            }
        }
    }
    cursor.expect_eof()?;

    let mut out = String::new();
    if let Some(tp) = type_params {
        out.push_str(&tp);
    }
    out.push('(');
    out.push_str(&params.join(", "));
    out.push(')');
    Ok(out)
}

/// Parses a JVMS §4.7.9 generic class signature into structured components.
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn parse_class_signature(
    raw: &str,
    mode: TypeDisplayMode,
) -> Result<DecodedClassSignature, JvmIndexError> {
    let mut cursor = Cursor::new(raw, mode);
    let type_parameters = if cursor.peek_byte() == Some(b'<') {
        Some(cursor.parse_type_parameters(0)?)
    } else {
        None
    };
    let superclass = cursor.parse_class_type_signature(0)?;
    let mut interfaces = Vec::new();
    while !cursor.is_eof() {
        interfaces.push(cursor.parse_class_type_signature(0)?);
    }
    Ok(DecodedClassSignature {
        type_parameters,
        superclass,
        interfaces,
    })
}

/// Decodes a JVMS §4.7.9 generic class signature into a concise Java-style string.
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidDescriptor`] if `raw` is malformed or exceeds
/// [`MAX_SIGNATURE_DEPTH`].
pub fn decode_class_signature(raw: &str) -> Result<String, JvmIndexError> {
    parse_class_signature(raw, TypeDisplayMode::Short).map(|decoded| decoded.display())
}

struct Cursor<'a> {
    raw: &'a str,
    pos: usize,
    mode: TypeDisplayMode,
}

impl<'a> Cursor<'a> {
    const fn new(raw: &'a str, mode: TypeDisplayMode) -> Self {
        Self { raw, pos: 0, mode }
    }

    fn rest(&self) -> &'a str {
        &self.raw[self.pos..]
    }

    const fn is_eof(&self) -> bool {
        self.pos >= self.raw.len()
    }

    fn peek_byte(&self) -> Option<u8> {
        self.raw.as_bytes().get(self.pos).copied()
    }

    fn next_byte(&mut self) -> Result<u8, JvmIndexError> {
        let Some(&byte) = self.raw.as_bytes().get(self.pos) else {
            return Err(self.err("unexpected end of descriptor"));
        };
        self.pos += 1;
        Ok(byte)
    }

    fn eat_byte(&mut self, expected: u8) -> bool {
        if self.peek_byte() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_byte(&mut self, expected: u8, context: &str) -> Result<(), JvmIndexError> {
        if self.eat_byte(expected) {
            Ok(())
        } else {
            Err(self.err(context))
        }
    }

    fn expect_eof(&self) -> Result<(), JvmIndexError> {
        if self.is_eof() {
            Ok(())
        } else {
            Err(self.err("unexpected trailing characters"))
        }
    }

    fn err(&self, reason: impl Into<String>) -> JvmIndexError {
        JvmIndexError::InvalidDescriptor {
            raw: self.raw.to_owned(),
            reason: format!("{} at byte offset {}", reason.into(), self.pos),
        }
    }

    fn check_depth(&self, depth: usize) -> Result<(), JvmIndexError> {
        if depth > MAX_SIGNATURE_DEPTH {
            return Err(JvmIndexError::InvalidDescriptor {
                raw: self.raw.to_owned(),
                reason: format!("signature nesting depth exceeded limit ({MAX_SIGNATURE_DEPTH})"),
            });
        }
        Ok(())
    }

    fn parse_base_type(byte: u8) -> Option<&'static str> {
        match byte {
            b'B' => Some("byte"),
            b'C' => Some("char"),
            b'D' => Some("double"),
            b'F' => Some("float"),
            b'I' => Some("int"),
            b'J' => Some("long"),
            b'S' => Some("short"),
            b'Z' => Some("boolean"),
            _ => None,
        }
    }

    fn parse_field_type(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        self.check_depth(depth)?;
        let byte = self.next_byte()?;
        if let Some(prim) = Self::parse_base_type(byte) {
            return Ok(prim.to_owned());
        }
        match byte {
            b'[' => {
                let inner = self.parse_field_type(depth + 1)?;
                Ok(format!("{inner}[]"))
            }
            b'L' => self.parse_descriptor_object_name(),
            other => Err(self.err(format!(
                "unexpected field descriptor token '{}'",
                char::from(other)
            ))),
        }
    }

    fn parse_return_descriptor(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        if self.eat_byte(b'V') {
            Ok("void".to_owned())
        } else {
            self.parse_field_type(depth)
        }
    }

    fn parse_descriptor_object_name(&mut self) -> Result<String, JvmIndexError> {
        let start = self.pos;
        let rest = self.rest();
        let Some(semi_rel) = rest.find(';') else {
            return Err(self.err("unterminated object descriptor (missing ';')"));
        };
        let internal = &rest[..semi_rel];
        if internal.is_empty()
            || internal.starts_with('/')
            || internal.ends_with('/')
            || internal.contains("//")
            || internal
                .bytes()
                .any(|b| matches!(b, b'.' | b'[' | b'<' | b'>'))
        {
            return Err(self.err("invalid internal class name in object descriptor"));
        }
        self.pos = start + semi_rel + 1;
        Ok(self.format_internal_class_name(internal))
    }

    fn format_internal_class_name(&self, internal: &str) -> String {
        match self.mode {
            TypeDisplayMode::Short => {
                let last = internal.rsplit('/').next().unwrap_or(internal);
                last.replace('$', ".")
            }
            TypeDisplayMode::Qualified => internal.replace(['/', '$'], "."),
        }
    }

    fn parse_java_type_signature(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        self.check_depth(depth)?;
        match self.peek_byte() {
            Some(byte) if Self::parse_base_type(byte).is_some() => {
                self.pos += 1;
                Ok(Self::parse_base_type(byte).unwrap_or_default().to_owned())
            }
            Some(_) => self.parse_reference_type_signature(depth),
            None => Err(self.err("unexpected end of signature")),
        }
    }

    fn parse_result_signature(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        if self.eat_byte(b'V') {
            Ok("void".to_owned())
        } else {
            self.parse_java_type_signature(depth)
        }
    }

    fn parse_reference_type_signature(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        self.check_depth(depth)?;
        match self.peek_byte() {
            Some(b'L') => self.parse_class_type_signature(depth),
            Some(b'T') => self.parse_type_variable_signature(),
            Some(b'[') => {
                self.pos += 1;
                let inner = self.parse_java_type_signature(depth + 1)?;
                Ok(format!("{inner}[]"))
            }
            Some(other) => Err(self.err(format!(
                "unexpected reference type token '{}'",
                char::from(other)
            ))),
            None => Err(self.err("unexpected end of reference type signature")),
        }
    }

    fn parse_type_variable_signature(&mut self) -> Result<String, JvmIndexError> {
        self.expect_byte(b'T', "expected 'T' for type variable")?;
        let ident = self.parse_identifier()?;
        self.expect_byte(b';', "expected ';' after type variable identifier")?;
        Ok(ident.to_owned())
    }

    fn parse_class_type_signature(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        self.check_depth(depth)?;
        self.expect_byte(b'L', "expected 'L' for class type signature")?;

        let mut segments = Vec::new();
        loop {
            let ident = self.parse_identifier()?;
            if self.eat_byte(b'/') {
                segments.push(ident);
            } else {
                segments.push(ident);
                break;
            }
        }

        let mut out = match self.mode {
            TypeDisplayMode::Short => segments
                .last()
                .copied()
                .unwrap_or_default()
                .replace('$', "."),
            TypeDisplayMode::Qualified => segments.join(".").replace('$', "."),
        };

        if self.peek_byte() == Some(b'<') {
            let args = self.parse_type_arguments(depth + 1)?;
            out.push_str(&args);
        }

        while self.eat_byte(b'.') {
            let suffix_ident = self.parse_identifier()?;
            out.push('.');
            out.push_str(&suffix_ident.replace('$', "."));
            if self.peek_byte() == Some(b'<') {
                let args = self.parse_type_arguments(depth + 1)?;
                out.push_str(&args);
            }
        }

        self.expect_byte(b';', "expected ';' at end of class type signature")?;
        Ok(out)
    }

    fn parse_type_arguments(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        self.check_depth(depth)?;
        self.expect_byte(b'<', "expected '<' for type arguments")?;
        let mut args = Vec::new();
        while !self.eat_byte(b'>') {
            if self.is_eof() {
                return Err(self.err("unterminated type arguments (missing '>')"));
            }
            args.push(self.parse_type_argument(depth)?);
        }
        if args.is_empty() {
            return Err(self.err("empty type argument list '<>' is invalid in JVMS"));
        }
        Ok(format!("<{}>", args.join(", ")))
    }

    fn parse_type_argument(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        self.check_depth(depth)?;
        match self.peek_byte() {
            Some(b'*') => {
                self.pos += 1;
                Ok("?".to_owned())
            }
            Some(b'+') => {
                self.pos += 1;
                let bound = self.parse_reference_type_signature(depth)?;
                Ok(format!("? extends {bound}"))
            }
            Some(b'-') => {
                self.pos += 1;
                let bound = self.parse_reference_type_signature(depth)?;
                Ok(format!("? super {bound}"))
            }
            Some(_) => self.parse_reference_type_signature(depth),
            None => Err(self.err("unexpected end of type argument")),
        }
    }

    fn parse_type_parameters(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        self.check_depth(depth)?;
        self.expect_byte(b'<', "expected '<' for type parameters")?;
        let mut params = Vec::new();
        while !self.eat_byte(b'>') {
            if self.is_eof() {
                return Err(self.err("unterminated type parameters (missing '>')"));
            }
            params.push(self.parse_type_parameter(depth + 1)?);
        }
        if params.is_empty() {
            return Err(self.err("empty type parameter list '<>' is invalid in JVMS"));
        }
        Ok(format!("<{}>", params.join(", ")))
    }

    fn parse_type_parameter(&mut self, depth: usize) -> Result<String, JvmIndexError> {
        self.check_depth(depth)?;
        let name = self.parse_identifier()?;
        self.expect_byte(b':', "expected ':' after type parameter name")?;

        let mut bounds = Vec::new();
        // ClassBound may be omitted when an InterfaceBound follows immediately (e.g. `T::Ljava/io/Serializable;`).
        if self.peek_byte() != Some(b':') {
            let class_bound = self.parse_reference_type_signature(depth)?;
            if class_bound != "Object" && class_bound != "java.lang.Object" {
                bounds.push(class_bound);
            }
        }
        while self.eat_byte(b':') {
            let iface_bound = self.parse_reference_type_signature(depth)?;
            bounds.push(iface_bound);
        }

        if bounds.is_empty() {
            Ok(name.to_owned())
        } else {
            Ok(format!("{name} extends {}", bounds.join(" & ")))
        }
    }

    fn parse_identifier(&mut self) -> Result<&'a str, JvmIndexError> {
        let start = self.pos;
        let rest = self.rest();
        let mut len = 0;
        for (idx, ch) in rest.char_indices() {
            if matches!(ch, '.' | ';' | '[' | '/' | '<' | '>' | ':') {
                break;
            }
            len = idx + ch.len_utf8();
        }
        if len == 0 {
            return Err(self.err("expected JVM identifier"));
        }
        self.pos = start + len;
        Ok(&self.raw[start..self.pos])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_primitive_array_and_object_field_descriptors() {
        assert_eq!(decode_field_descriptor("B").unwrap(), "byte");
        assert_eq!(decode_field_descriptor("C").unwrap(), "char");
        assert_eq!(decode_field_descriptor("D").unwrap(), "double");
        assert_eq!(decode_field_descriptor("F").unwrap(), "float");
        assert_eq!(decode_field_descriptor("I").unwrap(), "int");
        assert_eq!(decode_field_descriptor("J").unwrap(), "long");
        assert_eq!(decode_field_descriptor("S").unwrap(), "short");
        assert_eq!(decode_field_descriptor("Z").unwrap(), "boolean");
        assert_eq!(decode_field_descriptor("[[I").unwrap(), "int[][]");
        assert_eq!(
            decode_field_descriptor("[Ljava/lang/String;").unwrap(),
            "String[]"
        );
        assert_eq!(
            decode_field_descriptor("Ljava/util/Map$Entry;").unwrap(),
            "Map.Entry"
        );
        assert_eq!(
            decode_field_descriptor_with_mode("Ljava/util/Map$Entry;", TypeDisplayMode::Qualified)
                .unwrap(),
            "java.util.Map.Entry"
        );
    }

    #[test]
    fn decodes_method_and_constructor_descriptors() {
        assert_eq!(
            decode_method_descriptor("(Ljava/lang/String;I)Ljava/util/Optional;").unwrap(),
            "(String, int): Optional"
        );
        assert_eq!(decode_method_descriptor("()V").unwrap(), "(): void");
        assert_eq!(
            decode_method_descriptor_with_mode(
                "(Ljava/lang/String;I)Ljava/util/Optional;",
                TypeDisplayMode::Qualified
            )
            .unwrap(),
            "(java.lang.String, int): java.util.Optional"
        );
        assert_eq!(
            decode_constructor_descriptor("(JLjava/lang/String;)V").unwrap(),
            "(long, String)"
        );
    }

    #[test]
    fn decodes_generic_signatures_wildcards_nested_and_type_params() {
        assert_eq!(
            decode_field_signature(
                "Ljava/util/Map<Ljava/lang/String;Ljava/util/List<+Lcom/example/Order;>;>;"
            )
            .unwrap(),
            "Map<String, List<? extends Order>>"
        );
        assert_eq!(
            decode_field_signature("Ljava/util/Comparator<-Lcom/example/Item;>;").unwrap(),
            "Comparator<? super Item>"
        );
        assert_eq!(
            decode_field_signature("Ljava/lang/Class<*>;").unwrap(),
            "Class<?>"
        );
        assert_eq!(
            decode_field_signature("Lcom/example/Outer<TA;>.Inner<TB;>;").unwrap(),
            "Outer<A>.Inner<B>"
        );
        assert_eq!(
            decode_method_signature("<T:Ljava/lang/Comparable<TT;>;>(Ljava/util/List<TT;>;)TT;")
                .unwrap(),
            "<T extends Comparable<T>>(List<T>): T"
        );
        assert_eq!(
            decode_method_signature("<T:Ljava/lang/Number;:Ljava/io/Serializable;>(TT;)V").unwrap(),
            "<T extends Number & Serializable>(T): void"
        );
        assert_eq!(
            decode_method_signature("<K:Ljava/lang/Object;V::Ljava/lang/Runnable;>(TK;TV;)V")
                .unwrap(),
            "<K, V extends Runnable>(K, V): void"
        );
        assert_eq!(
            decode_class_signature(
                "<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;Ljava/io/Serializable;"
            )
            .unwrap(),
            "<E> extends AbstractList<E> implements Serializable"
        );
        assert_eq!(
            decode_class_signature("<T:Ljava/lang/Comparable<TT;>;>Ljava/lang/Object;").unwrap(),
            "<T extends Comparable<T>>"
        );
    }

    #[test]
    fn rejects_malformed_and_excessively_deep_signatures_without_panic() {
        assert!(matches!(
            decode_field_descriptor(""),
            Err(JvmIndexError::InvalidDescriptor { .. })
        ));
        assert!(matches!(
            decode_field_descriptor("V"),
            Err(JvmIndexError::InvalidDescriptor { .. })
        ));
        assert!(matches!(
            decode_field_descriptor("[V"),
            Err(JvmIndexError::InvalidDescriptor { .. })
        ));
        assert!(matches!(
            decode_field_descriptor("Ljava/lang/String"),
            Err(JvmIndexError::InvalidDescriptor { .. })
        ));
        assert!(matches!(
            decode_method_descriptor("(I)Vextra"),
            Err(JvmIndexError::InvalidDescriptor { .. })
        ));
        assert!(matches!(
            decode_field_signature("Ljava/util/List<>;"),
            Err(JvmIndexError::InvalidDescriptor { .. })
        ));
        assert!(matches!(
            decode_method_signature("<>(I)V"),
            Err(JvmIndexError::InvalidDescriptor { .. })
        ));

        // 70 levels of nested generics (> MAX_SIGNATURE_DEPTH = 64)
        let mut deep = String::new();
        for _ in 0..70 {
            deep.push_str("Ljava/util/List<");
        }
        deep.push_str("Ljava/lang/String;");
        for _ in 0..70 {
            deep.push_str(">;");
        }
        let err = decode_field_signature(&deep).expect_err("deep signature must fail cleanly");
        assert!(matches!(err, JvmIndexError::InvalidDescriptor { .. }));

        // 70 array dimensions (> MAX_SIGNATURE_DEPTH = 64)
        let deep_array = format!("{}I", "[".repeat(70));
        assert!(matches!(
            decode_field_descriptor(&deep_array),
            Err(JvmIndexError::InvalidDescriptor { .. })
        ));
    }
}
