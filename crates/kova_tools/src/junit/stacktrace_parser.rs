//! JVM stack trace parser turning `at com.acme.Foo.bar(Foo.java:42)` frames into clickable
//! source links (`KOVA-040`).

use std::path::{Path, PathBuf};

/// One parsed `at …` frame of a JVM stack trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// Fully qualified declaring class, e.g. `com.acme.Foo$Inner` (module / class-loader prefixes
    /// such as `java.base/` or `app//` are stripped).
    pub class_name: String,
    /// Method name (`<init>`, `lambda$run$0`, …).
    pub method: String,
    /// Source file name reported by the JVM (`Foo.java`, `Foo.kt`), when known.
    pub file: Option<String>,
    /// 1-based line number, when known.
    pub line: Option<u32>,
    /// The original line, trimmed.
    pub raw: String,
}

impl StackFrame {
    /// Package of [`Self::class_name`] (`None` for the default package).
    #[must_use]
    pub fn package(&self) -> Option<&str> {
        self.class_name.rsplit_once('.').map(|(pkg, _)| pkg)
    }

    /// Simple name of the top-level class (`Foo` for `com.acme.Foo$Inner`).
    #[must_use]
    pub fn outer_simple_name(&self) -> &str {
        let simple = self
            .class_name
            .rsplit_once('.')
            .map_or(self.class_name.as_str(), |(_, s)| s);
        simple.split('$').next().unwrap_or(simple)
    }

    /// Whether the frame points at a `.java` / `.kt` / `.groovy` / `.scala` source with a line.
    #[must_use]
    pub fn is_navigable(&self) -> bool {
        self.line.is_some()
            && self.file.as_deref().is_some_and(|file| {
                [".java", ".kt", ".kts", ".groovy", ".scala"]
                    .iter()
                    .any(|ext| file.ends_with(ext))
            })
    }

    /// Expected source path relative to a source root, e.g. `com/acme/Foo.java`.
    #[must_use]
    pub fn source_path_suffix(&self) -> Option<PathBuf> {
        let file = self.file.as_deref()?;
        let mut path = PathBuf::new();
        if let Some(pkg) = self.package() {
            for segment in pkg.split('.') {
                path.push(segment);
            }
        }
        path.push(file);
        Some(path)
    }

    /// Picks the project file this frame points to among `candidates` (project-relative or
    /// absolute paths).
    ///
    /// A path ending with the package-qualified suffix (`com/acme/Foo.java`) wins; otherwise a
    /// unique file with the same name is accepted (Kotlin files do not have to follow the
    /// package layout). Returns `None` for JDK / library frames.
    pub fn resolve_in<'a, I>(&self, candidates: I) -> Option<&'a Path>
    where
        I: IntoIterator<Item = &'a Path>,
    {
        if !self.is_navigable() {
            return None;
        }
        let suffix = self.source_path_suffix()?;
        let file = self.file.as_deref()?;
        let mut by_name = None;
        let mut by_name_count = 0_usize;
        for candidate in candidates {
            if candidate.ends_with(&suffix) {
                return Some(candidate);
            }
            if candidate.file_name().and_then(|n| n.to_str()) == Some(file) {
                by_name = Some(candidate);
                by_name_count += 1;
            }
        }
        (by_name_count == 1).then_some(by_name).flatten()
    }
}

/// Classified line of a stack trace or test output block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceLine {
    /// `at …` frame.
    Frame(StackFrame),
    /// `Caused by: …` header.
    CausedBy(String),
    /// `... 12 more` elision.
    Elided(u32),
    /// Exception header or any other text.
    Text(String),
}

/// Parses one stack frame line such as `\tat com.acme.Foo.bar(Foo.java:42)`.
///
/// Accepts module prefixes (`java.base/java.lang.Thread.run(Thread.java:1583)`), class-loader
/// prefixes (`app//com.acme.Foo.bar(Foo.java:3)`), `Native Method` and `Unknown Source`.
#[must_use]
pub fn parse_stack_frame(line: &str) -> Option<StackFrame> {
    let raw = line.trim();
    let body = raw.strip_prefix("at ")?.trim_start();
    let open = body.find('(')?;
    let close = body.rfind(')')?;
    if close < open {
        return None;
    }
    let qualified = &body[..open];
    let location = &body[open + 1..close];
    // `app//com.acme.Foo.bar` or `java.base@21/java.lang.Thread.run`.
    let qualified = qualified.rsplit('/').next().unwrap_or(qualified);
    let (class_name, method) = qualified.rsplit_once('.')?;
    if class_name.is_empty() || method.is_empty() || class_name.contains(char::is_whitespace) {
        return None;
    }
    let (file, line_no) = match location.rsplit_once(':') {
        Some((file, number)) => match number.trim().parse::<u32>() {
            Ok(number) => (Some(file.trim()), Some(number)),
            Err(_) => (Some(location.trim()), None),
        },
        None => (Some(location.trim()), None),
    };
    let file = file
        .filter(|f| !f.is_empty() && *f != "Native Method" && *f != "Unknown Source")
        .map(str::to_owned);
    Some(StackFrame {
        class_name: class_name.to_owned(),
        method: method.to_owned(),
        file,
        line: line_no.filter(|n| *n > 0),
        raw: raw.to_owned(),
    })
}

/// Classifies every line of `text` (a stack trace, test failure output or console log).
#[must_use]
pub fn parse_stacktrace(text: &str) -> Vec<TraceLine> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let trimmed = line.trim();
            if let Some(frame) = parse_stack_frame(trimmed) {
                TraceLine::Frame(frame)
            } else if let Some(rest) = trimmed.strip_prefix("Caused by:") {
                TraceLine::CausedBy(rest.trim().to_owned())
            } else if let Some(count) = trimmed
                .strip_prefix("...")
                .and_then(|rest| rest.trim().strip_suffix("more"))
                .and_then(|n| n.trim().parse().ok())
            {
                TraceLine::Elided(count)
            } else {
                TraceLine::Text(line.trim_end().to_owned())
            }
        })
        .collect()
}

/// Returns only the frames of `text`.
#[must_use]
pub fn stack_frames(text: &str) -> Vec<StackFrame> {
    parse_stacktrace(text)
        .into_iter()
        .filter_map(|line| match line {
            TraceLine::Frame(frame) => Some(frame),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = "java.lang.AssertionError: Expected IllegalArgumentException
  but no exception was thrown for amount = -15.00
\tat com.kova.shop.OrderService.validate(OrderService.java:84)
\tat com.kova.shop.OrderService.createOrder(OrderService.java:41)
\tat java.base/jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:103)
\tat app//com.kova.shop.OrderServiceTest$Nested.shouldReject(OrderServiceTest.kt:52)
\tat java.base/java.lang.Thread.run(Native Method)
Caused by: java.lang.IllegalStateException: boom
\tat com.kova.shop.Repo.save(Unknown Source)
\t... 12 more
";

    #[test]
    fn parses_project_frames_with_file_and_line() {
        let frame =
            parse_stack_frame("\tat com.kova.shop.OrderService.validate(OrderService.java:84)")
                .unwrap();
        assert_eq!(frame.class_name, "com.kova.shop.OrderService");
        assert_eq!(frame.method, "validate");
        assert_eq!(frame.file.as_deref(), Some("OrderService.java"));
        assert_eq!(frame.line, Some(84));
        assert!(frame.is_navigable());
        assert_eq!(
            frame.source_path_suffix().unwrap(),
            PathBuf::from("com/kova/shop/OrderService.java")
        );
    }

    #[test]
    fn strips_module_and_classloader_prefixes() {
        let frame = parse_stack_frame(
            "at app//com.kova.shop.OrderServiceTest$Nested.shouldReject(OrderServiceTest.kt:52)",
        )
        .unwrap();
        assert_eq!(frame.class_name, "com.kova.shop.OrderServiceTest$Nested");
        assert_eq!(frame.outer_simple_name(), "OrderServiceTest");
        let jdk = parse_stack_frame("at java.base/java.lang.Thread.run(Native Method)").unwrap();
        assert_eq!(jdk.class_name, "java.lang.Thread");
        assert_eq!(jdk.file, None);
        assert!(!jdk.is_navigable());
    }

    #[test]
    fn classifies_trace_lines() {
        let lines = parse_stacktrace(TRACE);
        assert!(
            matches!(&lines[0], TraceLine::Text(t) if t.starts_with("java.lang.AssertionError"))
        );
        assert!(matches!(&lines[2], TraceLine::Frame(f) if f.line == Some(84)));
        assert!(matches!(&lines[7], TraceLine::CausedBy(t) if t.contains("IllegalStateException")));
        assert_eq!(lines.last(), Some(&TraceLine::Elided(12)));
        assert_eq!(stack_frames(TRACE).len(), 6);
    }

    #[test]
    fn rejects_non_frames() {
        assert!(parse_stack_frame("at the end of the day (really)").is_none());
        assert!(parse_stack_frame("expected: <1> but was: <2>").is_none());
        assert!(parse_stack_frame("at Foo(Foo.java:1)").is_none());
    }

    #[test]
    fn resolves_against_project_files() {
        let files = [
            PathBuf::from("src/main/java/com/kova/shop/OrderService.java"),
            PathBuf::from("src/test/kotlin/OrderServiceTest.kt"),
            PathBuf::from("legacy/OrderService.java"),
        ];
        let paths: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
        let frame =
            parse_stack_frame("at com.kova.shop.OrderService.validate(OrderService.java:84)")
                .unwrap();
        assert_eq!(frame.resolve_in(paths.iter().copied()), Some(paths[0]));

        let kt = parse_stack_frame("at com.kova.shop.OrderServiceTest.ok(OrderServiceTest.kt:9)")
            .unwrap();
        assert_eq!(kt.resolve_in(paths.iter().copied()), Some(paths[1]));

        let jdk = parse_stack_frame("at java.lang.Thread.run(Thread.java:1583)").unwrap();
        assert_eq!(jdk.resolve_in(paths.iter().copied()), None);
    }

    #[test]
    fn ambiguous_file_name_without_package_match_is_not_resolved() {
        let files = [PathBuf::from("a/Util.java"), PathBuf::from("b/Util.java")];
        let frame = parse_stack_frame("at x.y.Util.run(Util.java:3)").unwrap();
        assert_eq!(frame.resolve_in(files.iter().map(PathBuf::as_path)), None);
    }
}
