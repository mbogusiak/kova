; Kova IDE Tree-sitter entry-point queries for Java (.java)
; Matches package declarations and concrete methods (with a `block` body) for JVM `main` and `@Test*` detection.

(package_declaration
  [
    (identifier)
    (scoped_identifier)
  ] @package.name) @package.decl

(method_declaration
  name: (identifier) @method.name
  parameters: (formal_parameters) @method.params
  body: (block) @method.body) @method.decl
