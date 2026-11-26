//! Deterministic, linear-time Markdown block and inline parser (`KOVA-057`).

use super::{
    InlineSpan, ListItem, ListMarkerKind, MarkdownBlock, MarkdownDocument, TableAlignment,
    TableBlock,
};

const MAX_QUOTE_DEPTH: usize = 8;
const MAX_INLINE_DEPTH: usize = 8;

/// Parses a UTF-8 Markdown document into a [`MarkdownDocument`] AST.
///
/// The parser is total and panic-free for arbitrary input (including unclosed
/// fences, malformed tables, and unmatched inline markers).
#[must_use]
pub fn parse_markdown(input: &str) -> MarkdownDocument {
    let lines: Vec<&str> = input.lines().collect();
    MarkdownDocument {
        blocks: parse_blocks(&lines, 0),
    }
}

/// Parses inline Markdown spans (`**bold**`, `*italic*`, `~~strike~~`, `` `code` ``, `[label](url)`).
#[must_use]
pub fn parse_inline(input: &str) -> Vec<InlineSpan> {
    let mut ctx = InlineParseCtx::default();
    parse_inline_depth(input, 0, &mut ctx)
}

fn parse_blocks(lines: &[&str], quote_depth: usize) -> Vec<MarkdownBlock> {
    let mut blocks = Vec::new();
    let mut idx = 0usize;

    while idx < lines.len() {
        let line = lines[idx];
        if line.trim().is_empty() {
            idx += 1;
            continue;
        }

        if let Some((block, next_idx)) = consume_code_block(lines, idx) {
            blocks.push(block);
            idx = next_idx;
            continue;
        }

        if let Some((level, heading_text)) = parse_heading(line) {
            blocks.push(MarkdownBlock::Heading {
                level,
                spans: parse_inline(heading_text),
            });
            idx += 1;
            continue;
        }

        if let Some((block, next_idx)) = consume_blockquote(lines, idx, quote_depth) {
            blocks.push(block);
            idx = next_idx;
            continue;
        }

        if let Some((block, next_idx)) = consume_table(lines, idx) {
            blocks.push(block);
            idx = next_idx;
            continue;
        }

        if is_thematic_break(line) {
            blocks.push(MarkdownBlock::ThematicBreak);
            idx += 1;
            continue;
        }

        if let Some((block, next_idx)) = consume_list(lines, idx) {
            blocks.push(block);
            idx = next_idx;
            continue;
        }

        let (block, next_idx) = consume_paragraph(lines, idx);
        blocks.push(block);
        idx = next_idx;
    }

    blocks
}

fn consume_code_block(lines: &[&str], start: usize) -> Option<(MarkdownBlock, usize)> {
    let (fence_char, fence_len, language) = parse_fence_open(lines[start])?;
    let mut idx = start + 1;
    let mut code_lines = Vec::new();
    while idx < lines.len() && !is_fence_close(lines[idx], fence_char, fence_len) {
        code_lines.push(lines[idx]);
        idx += 1;
    }
    if idx < lines.len() {
        idx += 1;
    }
    Some((
        MarkdownBlock::CodeBlock {
            language,
            code: code_lines.join("\n"),
        },
        idx,
    ))
}

fn consume_blockquote(
    lines: &[&str],
    start: usize,
    quote_depth: usize,
) -> Option<(MarkdownBlock, usize)> {
    if !is_quote_line(lines[start]) {
        return None;
    }
    let mut idx = start;
    let mut quoted_lines = Vec::new();
    while idx < lines.len() && is_quote_line(lines[idx]) {
        quoted_lines.push(strip_quote_prefix(lines[idx]));
        idx += 1;
    }
    let inner = if quote_depth < MAX_QUOTE_DEPTH {
        parse_blocks(&quoted_lines, quote_depth + 1)
    } else {
        quoted_lines
            .into_iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| MarkdownBlock::Paragraph(parse_inline(l.trim())))
            .collect()
    };
    Some((MarkdownBlock::BlockQuote(inner), idx))
}

fn consume_table(lines: &[&str], start: usize) -> Option<(MarkdownBlock, usize)> {
    if start + 1 >= lines.len() || !lines[start].contains('|') {
        return None;
    }
    let alignments = parse_table_separator(lines[start + 1])?;
    let header_cells = parse_table_header_cells(lines[start], alignments.len())?;
    let col_count = alignments.len();
    let headers = header_cells.into_iter().map(|c| parse_inline(&c)).collect();
    let mut idx = start + 2;

    let mut rows = Vec::new();
    while idx < lines.len() {
        let row_line = lines[idx];
        if row_line.trim().is_empty()
            || !row_line.contains('|')
            || parse_fence_open(row_line).is_some()
            || parse_heading(row_line).is_some()
            || is_quote_line(row_line)
            || is_thematic_break(row_line)
            || parse_list_item_header(row_line).is_some()
        {
            break;
        }
        let mut cells = split_table_row(row_line);
        if cells.len() < col_count {
            cells.resize_with(col_count, String::new);
        } else if cells.len() > col_count {
            cells.truncate(col_count);
        }
        rows.push(cells.into_iter().map(|c| parse_inline(&c)).collect());
        idx += 1;
    }

    Some((
        MarkdownBlock::Table(TableBlock {
            alignments,
            headers,
            rows,
        }),
        idx,
    ))
}

fn consume_list(lines: &[&str], start: usize) -> Option<(MarkdownBlock, usize)> {
    parse_list_item_header(lines[start])?;
    let mut idx = start;
    let mut items = Vec::new();
    while idx < lines.len() {
        let Some((indent, kind, first_text)) = parse_list_item_header(lines[idx]) else {
            break;
        };
        idx += 1;
        let mut parts = vec![first_text.trim_end()];
        while idx < lines.len() {
            let next = lines[idx];
            if next.trim().is_empty()
                || starts_non_paragraph_block(lines, idx)
                || parse_list_item_header(next).is_some()
            {
                break;
            }
            parts.push(next.trim());
            idx += 1;
        }
        let joined = parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        items.push(ListItem {
            kind,
            indent,
            spans: parse_inline(&joined),
        });
    }
    Some((MarkdownBlock::List(items), idx))
}

fn consume_paragraph(lines: &[&str], start: usize) -> (MarkdownBlock, usize) {
    let mut idx = start;
    let mut para = String::new();
    while idx < lines.len() {
        let next = lines[idx];
        if next.trim().is_empty() || (!para.is_empty() && starts_non_paragraph_block(lines, idx)) {
            break;
        }
        if !para.is_empty() {
            if para.ends_with("  ") || para.ends_with('\\') {
                if para.ends_with('\\') {
                    para.pop();
                } else {
                    while para.ends_with(' ') {
                        para.pop();
                    }
                }
                para.push('\n');
            } else {
                para.push(' ');
            }
        }
        para.push_str(next.trim_start());
        idx += 1;
        if idx < lines.len()
            && (lines[idx].trim().is_empty() || starts_non_paragraph_block(lines, idx))
        {
            break;
        }
    }
    let trimmed_para = para.trim_end_matches(' ');
    (MarkdownBlock::Paragraph(parse_inline(trimmed_para)), idx)
}

fn starts_non_paragraph_block(lines: &[&str], idx: usize) -> bool {
    let line = lines[idx];
    parse_fence_open(line).is_some()
        || parse_heading(line).is_some()
        || is_quote_line(line)
        || is_thematic_break(line)
        || parse_list_item_header(line).is_some()
        || (idx + 1 < lines.len()
            && line.contains('|')
            && parse_table_separator(lines[idx + 1])
                .is_some_and(|aligns| parse_table_header_cells(line, aligns.len()).is_some()))
}

fn strip_up_to_3_spaces(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut spaces = 0usize;
    while spaces < bytes.len() && spaces < 3 && bytes[spaces] == b' ' {
        spaces += 1;
    }
    if spaces < bytes.len() && (bytes[spaces] == b' ' || bytes[spaces] == b'\t') {
        return None;
    }
    Some(&line[spaces..])
}

fn parse_fence_open(line: &str) -> Option<(char, usize, Option<String>)> {
    let rest = strip_up_to_3_spaces(line)?;
    let first = rest.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let fence_len = rest.chars().take_while(|&c| c == first).count();
    if fence_len < 3 {
        return None;
    }
    let info = rest[fence_len..].trim();
    if first == '`' && info.contains('`') {
        return None;
    }
    let language = info
        .split_whitespace()
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some((first, fence_len, language))
}

fn is_fence_close(line: &str, fence_char: char, min_len: usize) -> bool {
    let Some(rest) = strip_up_to_3_spaces(line) else {
        return false;
    };
    let trimmed = rest.trim_end_matches([' ', '\t']);
    let count = trimmed.chars().take_while(|&c| c == fence_char).count();
    count >= min_len && count == trimmed.len()
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let rest = strip_up_to_3_spaces(line)?;
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after_hashes = &rest[hashes..];
    let content = if after_hashes.is_empty() {
        ""
    } else {
        after_hashes
            .strip_prefix(' ')
            .or_else(|| after_hashes.strip_prefix('\t'))?
    };
    let level = u8::try_from(hashes).ok()?;
    Some((level, strip_closing_hashes(content.trim())))
}

fn strip_closing_hashes(text: &str) -> &str {
    let trimmed = text.trim_end_matches([' ', '\t']);
    if trimmed.is_empty() {
        return "";
    }
    let without_hashes = trimmed.trim_end_matches('#');
    if without_hashes.len() == trimmed.len() {
        return trimmed;
    }
    if without_hashes.is_empty() {
        return "";
    }
    if without_hashes.ends_with([' ', '\t']) {
        without_hashes.trim_end_matches([' ', '\t'])
    } else {
        trimmed
    }
}

fn is_thematic_break(line: &str) -> bool {
    let Some(rest) = strip_up_to_3_spaces(line) else {
        return false;
    };
    let mut marker: Option<char> = None;
    let mut count = 0usize;
    for ch in rest.chars() {
        if ch == ' ' || ch == '\t' {
            continue;
        }
        if ch != '-' && ch != '*' && ch != '_' {
            return false;
        }
        if let Some(prev) = marker {
            if prev != ch {
                return false;
            }
        } else {
            marker = Some(ch);
        }
        count += 1;
    }
    count >= 3
}

fn is_quote_line(line: &str) -> bool {
    strip_up_to_3_spaces(line).is_some_and(|rest| rest.starts_with('>'))
}

fn strip_quote_prefix(line: &str) -> &str {
    let rest = strip_up_to_3_spaces(line).unwrap_or(line);
    let after_gt = rest.strip_prefix('>').unwrap_or(rest);
    after_gt.strip_prefix(' ').unwrap_or(after_gt)
}

fn parse_table_separator(line: &str) -> Option<Vec<TableAlignment>> {
    if !line.contains('|') {
        return None;
    }
    let cells = split_table_row(line);
    if cells.is_empty() {
        return None;
    }
    let mut alignments = Vec::with_capacity(cells.len());
    for cell in cells {
        let trimmed = cell.trim();
        if trimmed.is_empty() {
            return None;
        }
        let left_colon = trimmed.starts_with(':');
        let right_colon = trimmed.ends_with(':');
        let core_start = usize::from(left_colon);
        let core_end = trimmed.len().saturating_sub(usize::from(right_colon));
        if core_start >= core_end {
            return None;
        }
        let core = &trimmed[core_start..core_end];
        if !core.bytes().all(|b| b == b'-') {
            return None;
        }
        let align = match (left_colon, right_colon) {
            (true, true) => TableAlignment::Center,
            (false, true) => TableAlignment::Right,
            _ => TableAlignment::Left,
        };
        alignments.push(align);
    }
    Some(alignments)
}

fn parse_table_header_cells(line: &str, expected_cols: usize) -> Option<Vec<String>> {
    let cells = split_table_row(line);
    (cells.len() == expected_cols).then_some(cells)
}

fn split_table_row(line: &str) -> Vec<String> {
    let mut trimmed = line.trim();
    if let Some(after_pipe) = trimmed.strip_prefix('|') {
        trimmed = after_pipe;
    }
    if trimmed.ends_with('|') && !ends_with_escaped_pipe(trimmed) {
        trimmed = &trimmed[..trimmed.len() - 1];
    }

    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = trimmed.chars().peekable();
    let mut backtick_run = 0usize;

    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(&next) = chars.peek()
        {
            if next == '|' {
                current.push('\\');
                current.push(chars.next().unwrap_or('|'));
                continue;
            }
            if next == '\\' {
                current.push('\\');
                current.push(chars.next().unwrap_or('\\'));
                continue;
            }
        }
        if ch == '`' {
            let mut run = 1usize;
            while chars.peek() == Some(&'`') {
                chars.next();
                run += 1;
            }
            for _ in 0..run {
                current.push('`');
            }
            if backtick_run == 0 {
                backtick_run = run;
            } else if backtick_run == run {
                backtick_run = 0;
            }
            continue;
        }
        if ch == '|' && backtick_run == 0 {
            cells.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    cells.push(current.trim().to_owned());
    cells
}

fn ends_with_escaped_pipe(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.last() != Some(&b'|') {
        return false;
    }
    let mut backslashes = 0usize;
    let mut idx = bytes.len() - 1;
    while idx > 0 {
        idx -= 1;
        if bytes[idx] == b'\\' {
            backslashes += 1;
        } else {
            break;
        }
    }
    backslashes % 2 == 1
}

fn parse_list_item_header(line: &str) -> Option<(u8, ListMarkerKind, &str)> {
    let mut spaces = 0usize;
    let mut byte_idx = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => {
                spaces += 1;
                byte_idx += 1;
            }
            '\t' => {
                spaces += 4;
                byte_idx += 1;
            }
            _ => break,
        }
    }
    let rest = &line[byte_idx..];
    let indent = u8::try_from((spaces / 2).min(16)).unwrap_or(16);

    if let Some(after_bullet) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("-\t"))
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("*\t"))
        .or_else(|| rest.strip_prefix("+ "))
        .or_else(|| rest.strip_prefix("+\t"))
    {
        if let Some((checked, task_text)) = parse_task_prefix(after_bullet) {
            return Some((indent, ListMarkerKind::Task { checked }, task_text));
        }
        return Some((indent, ListMarkerKind::Bullet, after_bullet.trim_start()));
    }

    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let after_digits = &rest[digits..];
        let after_delim = after_digits
            .strip_prefix(". ")
            .or_else(|| after_digits.strip_prefix(".\t"))
            .or_else(|| after_digits.strip_prefix(") "))
            .or_else(|| after_digits.strip_prefix(")\t"))?;
        let num = rest[..digits].parse::<usize>().ok()?;
        return Some((
            indent,
            ListMarkerKind::Ordered(num),
            after_delim.trim_start(),
        ));
    }

    None
}

fn parse_task_prefix(text: &str) -> Option<(bool, &str)> {
    let bytes = text.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'[' || bytes[2] != b']' {
        return None;
    }
    let checked = match bytes[1] {
        b' ' => false,
        b'x' | b'X' => true,
        _ => return None,
    };
    let rest = &text[3..];
    if rest.is_empty() {
        Some((checked, ""))
    } else if let Some(after_ws) = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t')) {
        Some((checked, after_ws.trim_start()))
    } else {
        None
    }
}

#[derive(Clone, Copy)]
enum InlineMarkerBit {
    TripleStar = 0,
    DoubleStar = 1,
    DoubleUnderscore = 2,
    Strikethrough = 3,
    SingleStar = 4,
    SingleUnderscore = 5,
}

#[derive(Default)]
struct InlineParseCtx {
    failed_markers: u8,
    max_failed_backtick_len: usize,
}

impl InlineParseCtx {
    const fn is_failed(&self, marker: InlineMarkerBit) -> bool {
        (self.failed_markers & (1 << (marker as u8))) != 0
    }

    fn mark_failed(&mut self, marker: InlineMarkerBit) {
        self.failed_markers |= 1 << (marker as u8);
    }
}

fn parse_inline_depth(input: &str, depth: usize, ctx: &mut InlineParseCtx) -> Vec<InlineSpan> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut pos = 0usize;

    while pos < input.len() {
        let rest = &input[pos..];

        if let Some(after_slash) = rest.strip_prefix('\\') {
            if let Some(next_ch) = after_slash.chars().next()
                && is_escapable_ascii_punct(next_ch)
            {
                plain.push(next_ch);
                pos += 1 + next_ch.len_utf8();
                continue;
            }
            plain.push('\\');
            pos += 1;
            continue;
        }

        if rest.starts_with('`') {
            let tick_len = rest.bytes().take_while(|&b| b == b'`').count();
            if (ctx.max_failed_backtick_len == 0 || tick_len < ctx.max_failed_backtick_len)
                && let Some((code_text, total_consumed)) = find_closing_backticks(rest, tick_len)
            {
                flush_text(&mut plain, &mut spans);
                spans.push(InlineSpan::Code(code_text));
                pos += total_consumed;
                continue;
            }
            if ctx.max_failed_backtick_len == 0 || tick_len < ctx.max_failed_backtick_len {
                ctx.max_failed_backtick_len = tick_len;
            }
            for _ in 0..tick_len {
                plain.push('`');
            }
            pos += tick_len;
            continue;
        }

        if depth < MAX_INLINE_DEPTH
            && let Some((span, next_pos)) = try_parse_link_or_delimited(input, pos, depth, ctx)
        {
            flush_text(&mut plain, &mut spans);
            spans.push(span);
            pos = next_pos;
            continue;
        }

        if let Some(ch) = rest.chars().next() {
            plain.push(ch);
            pos += ch.len_utf8();
        }
    }

    flush_text(&mut plain, &mut spans);
    spans
}

fn try_parse_link_or_delimited(
    input: &str,
    pos: usize,
    depth: usize,
    ctx: &mut InlineParseCtx,
) -> Option<(InlineSpan, usize)> {
    if let Some(link) = try_parse_link_span(input, pos, depth, ctx) {
        return Some(link);
    }
    let rest = &input[pos..];
    try_delimited(
        input,
        pos,
        "***",
        false,
        InlineMarkerBit::TripleStar,
        depth,
        ctx,
        |inner| InlineSpan::Bold(vec![InlineSpan::Italic(inner)]),
    )
    .or_else(|| {
        try_delimited(
            input,
            pos,
            "**",
            false,
            InlineMarkerBit::DoubleStar,
            depth,
            ctx,
            InlineSpan::Bold,
        )
    })
    .or_else(|| {
        try_delimited(
            input,
            pos,
            "__",
            true,
            InlineMarkerBit::DoubleUnderscore,
            depth,
            ctx,
            InlineSpan::Bold,
        )
    })
    .or_else(|| {
        try_delimited(
            input,
            pos,
            "~~",
            false,
            InlineMarkerBit::Strikethrough,
            depth,
            ctx,
            InlineSpan::Strikethrough,
        )
    })
    .or_else(|| {
        (!rest.starts_with("**")).then_some(()).and_then(|()| {
            try_delimited(
                input,
                pos,
                "*",
                false,
                InlineMarkerBit::SingleStar,
                depth,
                ctx,
                InlineSpan::Italic,
            )
        })
    })
    .or_else(|| {
        (!rest.starts_with("__")).then_some(()).and_then(|()| {
            try_delimited(
                input,
                pos,
                "_",
                true,
                InlineMarkerBit::SingleUnderscore,
                depth,
                ctx,
                InlineSpan::Italic,
            )
        })
    })
}

fn try_parse_link_span(
    input: &str,
    pos: usize,
    depth: usize,
    ctx: &mut InlineParseCtx,
) -> Option<(InlineSpan, usize)> {
    let rest = &input[pos..];
    if rest.starts_with("![")
        && let Some((label_raw, url, consumed)) = try_parse_link(&rest[1..])
    {
        let label = if label_raw.trim().is_empty() {
            vec![InlineSpan::Text(url.to_owned())]
        } else {
            parse_inline_depth(label_raw, depth + 1, ctx)
        };
        return Some((
            InlineSpan::Link {
                label,
                url: unescape_inline_text(url),
            },
            pos + 1 + consumed,
        ));
    }

    if rest.starts_with('[')
        && let Some((label_raw, url, consumed)) = try_parse_link(rest)
    {
        let label = parse_inline_depth(label_raw, depth + 1, ctx);
        return Some((
            InlineSpan::Link {
                label,
                url: unescape_inline_text(url),
            },
            pos + consumed,
        ));
    }

    None
}

#[allow(
    clippy::too_many_arguments,
    reason = "shared delimiter parser avoids duplicating 6 emphasis branches"
)]
fn try_delimited(
    input: &str,
    pos: usize,
    marker: &str,
    require_word_boundary: bool,
    bit: InlineMarkerBit,
    depth: usize,
    ctx: &mut InlineParseCtx,
    wrap: impl FnOnce(Vec<InlineSpan>) -> InlineSpan,
) -> Option<(InlineSpan, usize)> {
    let rest = &input[pos..];
    if !rest.starts_with(marker) || ctx.is_failed(bit) {
        return None;
    }
    if let Some(end) = try_open_and_close(input, pos, marker, require_word_boundary) {
        let inner = parse_inline_depth(&input[pos + marker.len()..end], depth + 1, ctx);
        return Some((wrap(inner), end + marker.len()));
    }
    let boundary_ok = !require_word_boundary || !has_preceding_alnum(input, pos);
    if boundary_ok && starts_non_whitespace(&rest[marker.len()..]) {
        ctx.mark_failed(bit);
    }
    None
}

fn flush_text(plain: &mut String, spans: &mut Vec<InlineSpan>) {
    if plain.is_empty() {
        return;
    }
    let taken = std::mem::take(plain);
    if let Some(InlineSpan::Text(prev)) = spans.last_mut() {
        prev.push_str(&taken);
    } else {
        spans.push(InlineSpan::Text(taken));
    }
}

const fn is_escapable_ascii_punct(ch: char) -> bool {
    matches!(
        ch,
        '\\' | '`'
            | '*'
            | '_'
            | '{'
            | '}'
            | '['
            | ']'
            | '('
            | ')'
            | '#'
            | '+'
            | '-'
            | '.'
            | '!'
            | '|'
            | '~'
            | '>'
    )
}

fn unescape_inline_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(&next) = chars.peek()
            && is_escapable_ascii_punct(next)
        {
            out.push(chars.next().unwrap_or(next));
            continue;
        }
        out.push(ch);
    }
    out
}

fn find_closing_backticks(rest: &str, tick_len: usize) -> Option<(String, usize)> {
    let mut pos = tick_len;
    while pos < rest.len() {
        let slice = &rest[pos..];
        if slice.starts_with('`') {
            let run = slice.bytes().take_while(|&b| b == b'`').count();
            if run == tick_len {
                let raw = &rest[tick_len..pos];
                let normalized = raw.replace('\n', " ");
                let trimmed = if normalized.len() >= 2
                    && normalized.starts_with(' ')
                    && normalized.ends_with(' ')
                    && normalized.bytes().any(|b| b != b' ')
                {
                    normalized[1..normalized.len() - 1].to_owned()
                } else {
                    normalized
                };
                return Some((trimmed, pos + tick_len));
            }
            pos += run;
            continue;
        }
        if let Some(ch) = slice.chars().next() {
            pos += ch.len_utf8();
        }
    }
    None
}

fn try_parse_link(input: &str) -> Option<(&str, &str, usize)> {
    if !input.starts_with('[') {
        return None;
    }
    let mut pos = 1usize;
    let mut bracket_depth = 1usize;
    let mut close_bracket = None;

    while pos < input.len() {
        let slice = &input[pos..];
        if let Some(after_slash) = slice.strip_prefix('\\') {
            if let Some(next_ch) = after_slash.chars().next() {
                pos += 1 + next_ch.len_utf8();
                continue;
            }
            pos += 1;
            continue;
        }
        if slice.starts_with('`') {
            let tick_len = slice.bytes().take_while(|&b| b == b'`').count();
            if let Some((_, consumed)) = find_closing_backticks(slice, tick_len) {
                pos += consumed;
                continue;
            }
            pos += tick_len;
            continue;
        }
        if slice.starts_with('[') {
            bracket_depth += 1;
            pos += 1;
            continue;
        }
        if slice.starts_with(']') {
            bracket_depth -= 1;
            if bracket_depth == 0 {
                close_bracket = Some(pos);
                break;
            }
            pos += 1;
            continue;
        }
        if let Some(ch) = slice.chars().next() {
            pos += ch.len_utf8();
        }
    }

    let close_bracket = close_bracket?;
    let after_label = &input[close_bracket + 1..];
    let after_paren = after_label.strip_prefix('(')?;

    let mut url_pos = 0usize;
    let mut paren_depth = 1usize;
    let mut close_paren = None;
    while url_pos < after_paren.len() {
        let slice = &after_paren[url_pos..];
        if let Some(after_slash) = slice.strip_prefix('\\') {
            if let Some(next_ch) = after_slash.chars().next() {
                url_pos += 1 + next_ch.len_utf8();
                continue;
            }
            url_pos += 1;
            continue;
        }
        if slice.starts_with('(') {
            paren_depth += 1;
            url_pos += 1;
            continue;
        }
        if slice.starts_with(')') {
            paren_depth -= 1;
            if paren_depth == 0 {
                close_paren = Some(url_pos);
                break;
            }
            url_pos += 1;
            continue;
        }
        if let Some(ch) = slice.chars().next() {
            url_pos += ch.len_utf8();
        }
    }

    let close_paren = close_paren?;
    let label = &input[1..close_bracket];
    let raw_dest = after_paren[..close_paren].trim();
    let url = extract_link_destination(raw_dest);
    let total = close_bracket + 1 + 1 + close_paren + 1;
    Some((label, url, total))
}

fn extract_link_destination(raw: &str) -> &str {
    if let Some(after_lt) = raw.strip_prefix('<')
        && let Some(end_gt) = after_lt.find('>')
    {
        return after_lt[..end_gt].trim();
    }
    raw.split_whitespace().next().unwrap_or("")
}

fn has_preceding_alnum(input: &str, pos: usize) -> bool {
    input[..pos]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric)
}

fn starts_non_whitespace(s: &str) -> bool {
    s.chars().next().is_some_and(|c| !c.is_whitespace())
}

fn ends_non_whitespace(s: &str) -> bool {
    s.chars().next_back().is_some_and(|c| !c.is_whitespace())
}

fn try_open_and_close(
    input: &str,
    open_pos: usize,
    marker: &str,
    require_word_boundary: bool,
) -> Option<usize> {
    let mlen = marker.len();
    if require_word_boundary && has_preceding_alnum(input, open_pos) {
        return None;
    }
    let after_open = &input[open_pos + mlen..];
    if !starts_non_whitespace(after_open) {
        return None;
    }

    let marker_byte = marker.as_bytes().first().copied()?;
    let mut pos = open_pos + mlen;
    while pos < input.len() {
        let slice = &input[pos..];
        if let Some(after_slash) = slice.strip_prefix('\\') {
            if let Some(next_ch) = after_slash.chars().next() {
                pos += 1 + next_ch.len_utf8();
                continue;
            }
            pos += 1;
            continue;
        }
        if slice.starts_with('`') {
            let tick_len = slice.bytes().take_while(|&b| b == b'`').count();
            if let Some((_, consumed)) = find_closing_backticks(slice, tick_len) {
                pos += consumed;
                continue;
            }
            pos += tick_len;
            continue;
        }
        if slice.as_bytes().first().copied() == Some(marker_byte) {
            let run_len = slice.bytes().take_while(|&b| b == marker_byte).count();
            if run_len >= mlen && pos > open_pos + mlen {
                let before = &input[open_pos + mlen..pos];
                let after_run = &input[pos + run_len..];
                let boundary_ok = !require_word_boundary
                    || !after_run.chars().next().is_some_and(char::is_alphanumeric);
                if ends_non_whitespace(before) && boundary_ok {
                    return Some(pos + run_len - mlen);
                }
            }
            pos += run_len;
            continue;
        }
        if let Some(ch) = slice.chars().next() {
            pos += ch.len_utf8();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> InlineSpan {
        InlineSpan::Text(s.to_owned())
    }

    #[test]
    fn parses_atx_headings_h1_to_h6_and_rejects_missing_space() {
        let doc = parse_markdown(
            "# Heading 1\n## Heading 2 ##\n### Heading 3\t###\n#### H4\n##### H5\n###### H6\n####### NotH7\n#NoSpace",
        );
        assert_eq!(
            doc.blocks,
            vec![
                MarkdownBlock::Heading {
                    level: 1,
                    spans: vec![text("Heading 1")],
                },
                MarkdownBlock::Heading {
                    level: 2,
                    spans: vec![text("Heading 2")],
                },
                MarkdownBlock::Heading {
                    level: 3,
                    spans: vec![text("Heading 3")],
                },
                MarkdownBlock::Heading {
                    level: 4,
                    spans: vec![text("H4")],
                },
                MarkdownBlock::Heading {
                    level: 5,
                    spans: vec![text("H5")],
                },
                MarkdownBlock::Heading {
                    level: 6,
                    spans: vec![text("H6")],
                },
                MarkdownBlock::Paragraph(vec![text("####### NotH7 #NoSpace")]),
            ]
        );
    }

    #[test]
    fn parses_inline_styles_escapes_code_links_and_snake_case() {
        let spans = parse_inline(
            "Use **bold**, __also bold__, *italic*, _also italic_, ***both***, ~~strike~~, `` code `inner` ``, [Kova](https://kova.dev/docs_(v1) \"Docs\"), ![Logo](logo.png), \\*escaped\\*, and snake_case_identifier.",
        );
        assert_eq!(
            spans,
            vec![
                text("Use "),
                InlineSpan::Bold(vec![text("bold")]),
                text(", "),
                InlineSpan::Bold(vec![text("also bold")]),
                text(", "),
                InlineSpan::Italic(vec![text("italic")]),
                text(", "),
                InlineSpan::Italic(vec![text("also italic")]),
                text(", "),
                InlineSpan::Bold(vec![InlineSpan::Italic(vec![text("both")])]),
                text(", "),
                InlineSpan::Strikethrough(vec![text("strike")]),
                text(", "),
                InlineSpan::Code("code `inner`".to_owned()),
                text(", "),
                InlineSpan::Link {
                    label: vec![text("Kova")],
                    url: "https://kova.dev/docs_(v1)".to_owned(),
                },
                text(", "),
                InlineSpan::Link {
                    label: vec![text("Logo")],
                    url: "logo.png".to_owned(),
                },
                text(", *escaped*, and snake_case_identifier."),
            ]
        );
    }

    #[test]
    fn ignores_formatting_markers_inside_inline_code_and_handles_unclosed_linearly() {
        let spans = parse_inline("`**not bold**` and *unclosed **also unclosed ~~nope `raw");
        assert_eq!(
            spans,
            vec![
                InlineSpan::Code("**not bold**".to_owned()),
                text(" and *unclosed **also unclosed ~~nope `raw"),
            ]
        );

        let nested = parse_inline("**bold *italic*** and *italic **bold***");
        assert_eq!(
            nested,
            vec![
                InlineSpan::Bold(vec![
                    text("bold "),
                    InlineSpan::Italic(vec![text("italic")]),
                ]),
                text(" and "),
                InlineSpan::Italic(vec![text("italic "), InlineSpan::Bold(vec![text("bold")]),]),
            ]
        );

        let pathological = "**a ".repeat(4000);
        let parsed = parse_inline(&pathological);
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn parses_fenced_code_blocks_including_nested_and_unclosed_eof() {
        let doc = parse_markdown(
            "```rust\nfn main() {}\n```\n\n````markdown\n```json\n{\"ok\": true}\n```\n````\n\n~~~java\nclass Unclosed {",
        );
        assert_eq!(
            doc.blocks,
            vec![
                MarkdownBlock::CodeBlock {
                    language: Some("rust".to_owned()),
                    code: "fn main() {}".to_owned(),
                },
                MarkdownBlock::CodeBlock {
                    language: Some("markdown".to_owned()),
                    code: "```json\n{\"ok\": true}\n```".to_owned(),
                },
                MarkdownBlock::CodeBlock {
                    language: Some("java".to_owned()),
                    code: "class Unclosed {".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn parses_multiline_and_nested_blockquotes() {
        let doc = parse_markdown("> First line\n> second line\n>\n> > Nested **quote**");
        assert_eq!(
            doc.blocks,
            vec![MarkdownBlock::BlockQuote(vec![
                MarkdownBlock::Paragraph(vec![text("First line second line")]),
                MarkdownBlock::BlockQuote(vec![MarkdownBlock::Paragraph(vec![
                    text("Nested "),
                    InlineSpan::Bold(vec![text("quote")]),
                ])]),
            ])]
        );
    }

    #[test]
    fn parses_bullet_ordered_nested_and_task_lists() {
        let doc = parse_markdown(
            "- First item\n  * Nested bullet\n+ Plus item\n- [ ] Unchecked task\n- [x] Checked task\n* [X] Upper checked\n1. Ordered one\n2) Ordered two",
        );
        assert_eq!(
            doc.blocks,
            vec![MarkdownBlock::List(vec![
                ListItem {
                    kind: ListMarkerKind::Bullet,
                    indent: 0,
                    spans: vec![text("First item")],
                },
                ListItem {
                    kind: ListMarkerKind::Bullet,
                    indent: 1,
                    spans: vec![text("Nested bullet")],
                },
                ListItem {
                    kind: ListMarkerKind::Bullet,
                    indent: 0,
                    spans: vec![text("Plus item")],
                },
                ListItem {
                    kind: ListMarkerKind::Task { checked: false },
                    indent: 0,
                    spans: vec![text("Unchecked task")],
                },
                ListItem {
                    kind: ListMarkerKind::Task { checked: true },
                    indent: 0,
                    spans: vec![text("Checked task")],
                },
                ListItem {
                    kind: ListMarkerKind::Task { checked: true },
                    indent: 0,
                    spans: vec![text("Upper checked")],
                },
                ListItem {
                    kind: ListMarkerKind::Ordered(1),
                    indent: 0,
                    spans: vec![text("Ordered one")],
                },
                ListItem {
                    kind: ListMarkerKind::Ordered(2),
                    indent: 0,
                    spans: vec![text("Ordered two")],
                },
            ])]
        );
    }

    #[test]
    fn parses_gfm_tables_with_alignments_escaped_pipes_and_uneven_rows() {
        let doc = parse_markdown(
            "| Feature | Status | Score |\n| :--- | :---: | ---: |\n| **Parser** | `ok` | 100 |\n| Escaped \\| Pipe | ~~old~~\n| Extra | Cell | 1 | Dropped |",
        );
        assert_eq!(
            doc.blocks,
            vec![MarkdownBlock::Table(TableBlock {
                alignments: vec![
                    TableAlignment::Left,
                    TableAlignment::Center,
                    TableAlignment::Right,
                ],
                headers: vec![
                    vec![text("Feature")],
                    vec![text("Status")],
                    vec![text("Score")],
                ],
                rows: vec![
                    vec![
                        vec![InlineSpan::Bold(vec![text("Parser")])],
                        vec![InlineSpan::Code("ok".to_owned())],
                        vec![text("100")],
                    ],
                    vec![
                        vec![text("Escaped | Pipe")],
                        vec![InlineSpan::Strikethrough(vec![text("old")])],
                        vec![],
                    ],
                    vec![vec![text("Extra")], vec![text("Cell")], vec![text("1")],],
                ],
            })]
        );
    }

    #[test]
    fn distinguishes_thematic_breaks_from_tables_and_code_blocks() {
        let doc = parse_markdown("---\n\n* * *\n\n___\n\n| A |\n| --- |\n| 1 |");
        assert_eq!(doc.blocks.len(), 4);
        assert_eq!(doc.blocks[0], MarkdownBlock::ThematicBreak);
        assert_eq!(doc.blocks[1], MarkdownBlock::ThematicBreak);
        assert_eq!(doc.blocks[2], MarkdownBlock::ThematicBreak);
        assert!(matches!(doc.blocks[3], MarkdownBlock::Table(_)));
    }

    #[test]
    fn handles_unicode_and_nested_links_without_panicking() {
        let doc = parse_markdown(
            "# Zażółć gęślą jaźń\n\nZobacz [**dokumentację** *Kova*](docs/README.md) i `ąęć`.",
        );
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(
            doc.blocks[1],
            MarkdownBlock::Paragraph(vec![
                text("Zobacz "),
                InlineSpan::Link {
                    label: vec![
                        InlineSpan::Bold(vec![text("dokumentację")]),
                        text(" "),
                        InlineSpan::Italic(vec![text("Kova")]),
                    ],
                    url: "docs/README.md".to_owned(),
                },
                text(" i "),
                InlineSpan::Code("ąęć".to_owned()),
                text("."),
            ])
        );
    }
}
