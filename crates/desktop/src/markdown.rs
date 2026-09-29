//! Markdown for model replies: CommonMark block structure (headings,
//! paragraphs, fenced code, block quotes, nested lists, rules) plus GFM
//! tables, task lists and strike-through.
//!
//! It is built for streaming: anything unterminated (an open code fence, a
//! half-received table) renders as it would once complete, and unmatched
//! emphasis markers stay literal instead of swallowing the rest.
//!
//! Deliberate differences from CommonMark: single newlines inside a paragraph
//! are kept as line breaks (models use them for poems and addresses), and raw
//! HTML, setext headings and indented code blocks are treated as text.

use std::ops::Range;

/// Inline styling of a text span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mark {
    /// `**bold**`.
    pub strong: bool,
    /// `*italic*`.
    pub emphasis: bool,
    /// `` `code` ``.
    pub code: bool,
    /// `~~struck~~`.
    pub strike: bool,
    /// Index into [`Inline::links`] plus one; zero when not a link.
    pub link: u16,
}

/// Paragraph-level text with its styled spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    /// The visible text, markers removed.
    pub text: String,
    /// Sorted, non-overlapping styled ranges of `text`.
    pub spans: Vec<(Range<usize>, Mark)>,
    /// Link targets referenced by [`Mark::link`].
    pub links: Vec<String>,
}

/// Column alignment of a table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// Default / left.
    #[default]
    Left,
    /// `:-:`.
    Center,
    /// `-:`.
    Right,
}

/// One list item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// `[ ]` / `[x]` task state, if a task item.
    pub task: Option<bool>,
    /// Item contents.
    pub blocks: Vec<Block>,
}

/// A block-level element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// Running text.
    Paragraph(Inline),
    /// `#` to `######`.
    Heading {
        /// 1 to 6.
        level: u8,
        /// Heading text.
        text: Inline,
    },
    /// A fenced code block.
    Code {
        /// First word of the info string, lower-cased.
        lang: String,
        /// The code, without the fences.
        code: String,
    },
    /// `>` quotation.
    Quote(Vec<Block>),
    /// Bulleted (`start` is `None`) or numbered list.
    List {
        /// Number of the first item for ordered lists.
        start: Option<u64>,
        /// The items.
        items: Vec<Item>,
    },
    /// `---`.
    Rule,
    /// GFM table.
    Table {
        /// Per-column alignment.
        align: Vec<Align>,
        /// Header cells.
        header: Vec<Inline>,
        /// Body rows, padded to the header's width.
        rows: Vec<Vec<Inline>>,
    },
}

/// Parses a document.
#[must_use]
pub fn parse(src: &str) -> Vec<Block> {
    let lines: Vec<&str> = src.lines().collect();
    parse_lines(&lines, 0)
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// Removes up to `n` columns of leading indentation.
fn dedent(line: &str, n: usize) -> &str {
    let mut cols = 0;
    for (i, c) in line.char_indices() {
        if cols >= n || (c != ' ' && c != '\t') {
            return &line[i..];
        }
        cols += if c == '\t' { 4 } else { 1 };
    }
    ""
}

/// An opening code fence: its character, length and info string.
fn fence(trimmed: &str) -> Option<(char, usize, &str)> {
    let c = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = trimmed.chars().take_while(|x| *x == c).count();
    let info = trimmed[len..].trim();
    (len >= 3 && !(c == '`' && info.contains('`'))).then_some((c, len, info))
}

fn heading(trimmed: &str) -> Option<(u8, &str)> {
    let level = trimmed.bytes().take_while(|b| *b == b'#').count();
    let rest = &trimmed[level..];
    if !(1..=6).contains(&level) || !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    // A closing run of #s is decoration.
    let text = rest.trim();
    let text = text.trim_end_matches('#');
    let text = if text.is_empty() || text.ends_with(' ') { text.trim_end() } else { rest.trim() };
    Some((level as u8, text))
}

fn is_rule(trimmed: &str) -> bool {
    let mut chars = trimmed.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next().filter(|c| matches!(c, '-' | '*' | '_')) else { return false };
    let rest: Vec<char> = chars.collect();
    rest.len() >= 2 && rest.iter().all(|c| *c == first)
}

/// The item text after a list marker.
fn after_marker(line: &str) -> &str {
    let trimmed = line.trim_start();
    let marker = trimmed.find([' ', '\t']).unwrap_or(trimmed.len());
    trimmed[marker..].strip_prefix([' ', '\t']).unwrap_or(&trimmed[marker..])
}

/// A list marker: `(content column, ordered start)`.
fn list_marker(line: &str) -> Option<(usize, Option<u64>)> {
    let indent = indent_of(line);
    let trimmed = line.trim_start();
    let (width, start) = if let Some(rest) = trimmed.strip_prefix(['-', '*', '+']) {
        if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
            return None;
        }
        (1, None)
    } else {
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        let rest = trimmed.get(digits..)?;
        if !(1..=9).contains(&digits) || !rest.starts_with(['.', ')']) {
            return None;
        }
        let after = &rest[1..];
        if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
            return None;
        }
        (digits + 1, trimmed[..digits].parse().ok())
    };
    let spaces = trimmed[width..].chars().take_while(|c| *c == ' ').count().clamp(1, 4);
    Some((indent + width + spaces, start))
}

fn split_row(line: &str) -> Vec<&str> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    let mut cells = Vec::new();
    let (mut start, mut escaped, mut in_code) = (0, false, false);
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '`' => in_code = !in_code,
            '|' if !in_code => {
                cells.push(line[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    cells.push(line[start..].trim());
    cells
}

fn delimiter_row(line: &str) -> Option<Vec<Align>> {
    if !line.contains('-') {
        return None;
    }
    split_row(line)
        .into_iter()
        .map(|cell| {
            let (left, right) = (cell.starts_with(':'), cell.ends_with(':'));
            let dashes = cell.trim_matches(':');
            (!dashes.is_empty() && dashes.chars().all(|c| c == '-')).then_some(match (left, right) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// Whether `line` would start a block other than a paragraph.
fn starts_block(line: &str) -> bool {
    let trimmed = line.trim_start();
    fence(trimmed).is_some() || heading(trimmed).is_some() || trimmed.starts_with('>') || is_rule(trimmed) || list_marker(line).is_some()
}

/// Deepest quote/list nesting parsed as structure; deeper input stays text so
/// hostile input cannot exhaust the stack.
const MAX_DEPTH: usize = 16;

fn parse_lines(lines: &[&str], depth: usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let flush = |paragraph: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(parse_inline(&paragraph.join("\n"))));
            paragraph.clear();
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut blocks);
            i += 1;
            continue;
        }
        if let Some((c, len, info)) = fence(trimmed) {
            flush(&mut paragraph, &mut blocks);
            let indent = indent_of(line);
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim();
                if t.len() >= len && t.chars().all(|x| x == c) {
                    i += 1;
                    break;
                }
                code.push(dedent(lines[i], indent));
                i += 1;
            }
            let lang = info.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
            blocks.push(Block::Code { lang, code: code.join("\n") });
            continue;
        }
        if let Some((level, text)) = heading(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Heading { level, text: parse_inline(text) });
            i += 1;
            continue;
        }
        if is_rule(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }
        if trimmed.starts_with('>') && depth < MAX_DEPTH {
            flush(&mut paragraph, &mut blocks);
            let mut inner = Vec::new();
            while let Some(rest) = lines.get(i).map(|l| l.trim_start()).and_then(|l| l.strip_prefix('>')) {
                inner.push(rest.strip_prefix(' ').unwrap_or(rest));
                i += 1;
            }
            blocks.push(Block::Quote(parse_lines(&inner, depth + 1)));
            continue;
        }
        if let Some((_, start)) = list_marker(line).filter(|_| depth < MAX_DEPTH) {
            flush(&mut paragraph, &mut blocks);
            let (list, next) = parse_list(lines, i, start.is_some(), depth);
            blocks.push(Block::List { start, items: list });
            i = next;
            continue;
        }
        if paragraph.is_empty()
            && line.contains('|')
            && let Some(align) = lines.get(i + 1).and_then(|l| delimiter_row(l))
            && split_row(line).len() == align.len()
        {
            let header: Vec<Inline> = split_row(line).into_iter().map(parse_inline).collect();
            let mut rows = Vec::new();
            i += 2;
            while let Some(row) = lines.get(i).filter(|l| l.contains('|') && !l.trim().is_empty()) {
                let mut cells: Vec<Inline> = split_row(row).into_iter().map(parse_inline).collect();
                cells.resize(header.len(), Inline::default());
                rows.push(cells);
                i += 1;
            }
            blocks.push(Block::Table { align, header, rows });
            continue;
        }
        paragraph.push(trimmed);
        i += 1;
    }
    flush(&mut paragraph, &mut blocks);
    blocks
}

/// Parses list items starting at `lines[i]`; returns them and the index after the list.
fn parse_list(lines: &[&str], mut i: usize, ordered: bool, depth: usize) -> (Vec<Item>, usize) {
    let marker_indent = indent_of(lines[i]);
    let mut items = Vec::new();
    while let Some((content, start)) = lines.get(i).and_then(|l| list_marker(l)) {
        if start.is_some() != ordered || indent_of(lines[i]) != marker_indent {
            break;
        }
        let mut body = vec![after_marker(lines[i])];
        i += 1;
        while let Some(&line) = lines.get(i) {
            let blank = line.trim().is_empty();
            if blank {
                // A blank line continues the item only if indented content follows.
                let next = lines[i + 1..].iter().find(|l| !l.trim().is_empty());
                if next.is_some_and(|l| indent_of(l) >= content) {
                    body.push("");
                    i += 1;
                    continue;
                }
                break;
            }
            if indent_of(line) >= content {
                body.push(dedent(line, content));
            } else if !starts_block(line) && body.last().is_some_and(|l| !l.trim().is_empty()) && !line.contains('|') {
                // Lazy continuation of the item's paragraph.
                body.push(line.trim_start());
            } else {
                break;
            }
            i += 1;
        }
        let mut task = None;
        if let Some(first) = body.first_mut() {
            for (prefix, done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
                if let Some(rest) = first.strip_prefix(prefix) {
                    task = Some(done);
                    *first = rest;
                }
            }
        }
        items.push(Item { task, blocks: parse_lines(&body, depth + 1) });
    }
    (items, i)
}

/// Parses inline markup.
#[must_use]
pub fn parse_inline(src: &str) -> Inline {
    let mut out = Inline::default();
    inline_into(src, Mark::default(), &mut out, 0);
    out
}

fn push_text(out: &mut Inline, text: &str, mark: Mark) {
    if text.is_empty() {
        return;
    }
    let start = out.text.len();
    out.text.push_str(text);
    let end = out.text.len();
    if mark == Mark::default() {
        return;
    }
    match out.spans.last_mut() {
        Some((range, last)) if *last == mark && range.end == start => range.end = end,
        _ => out.spans.push((start..end, mark)),
    }
}

/// Finds the closing run of exactly `n` `c`s after `from`, not preceded by
/// whitespace and not inside a code span.
fn closing_run(src: &str, from: usize, c: u8, n: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'`' => {
                let run = bytes[i..].iter().take_while(|b| **b == b'`').count();
                i += run;
                // Skip to the matching backtick run, if any.
                if let Some(end) = find_backticks(src, i, run) {
                    i = end + run;
                }
            }
            b if b == c => {
                let run = bytes[i..].iter().take_while(|b| **b == c).count();
                let after_space = i > 0 && bytes[i - 1].is_ascii_whitespace();
                let intraword = c == b'_' && bytes.get(i + run).is_some_and(u8::is_ascii_alphanumeric);
                // A `***` run closes an outer marker and an inner one at once
                // (`**bold *inner***`); match the outer from its end.
                if (run == n || run == 3) && !after_space && i > from && !intraword {
                    return Some(i + run - n);
                }
                i += run;
            }
            _ => i += 1,
        }
    }
    None
}

fn find_backticks(src: &str, from: usize, n: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = bytes[i..].iter().take_while(|b| **b == b'`').count();
            if run == n {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// Length of a bare URL starting at `i`, trailing punctuation excluded.
fn bare_url(src: &str, i: usize) -> Option<usize> {
    let rest = &src[i..];
    if !(rest.starts_with("https://") || rest.starts_with("http://")) {
        return None;
    }
    if i > 0 && src.as_bytes()[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    let mut len = rest.find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"').unwrap_or(rest.len());
    while len > 0 && rest[..len].ends_with(['.', ',', ':', ';', '!', '?', '\'']) {
        len -= 1;
    }
    // Keep a closing parenthesis only when the URL opened one.
    if rest[..len].ends_with(')') && rest[..len].matches('(').count() < rest[..len].matches(')').count() {
        len -= 1;
    }
    (len > "https://".len()).then_some(len)
}

fn with_link(out: &mut Inline, mark: Mark, url: &str) -> Mark {
    out.links.push(url.to_owned());
    Mark { link: u16::try_from(out.links.len()).unwrap_or(u16::MAX), ..mark }
}

fn inline_into(src: &str, mark: Mark, out: &mut Inline, depth: usize) {
    if depth >= MAX_DEPTH {
        push_text(out, src, mark);
        return;
    }
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut plain = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let consumed = match b {
            b'\\' if bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) => {
                push_text(out, &src[plain..i], mark);
                push_text(out, &src[i + 1..i + 2], mark);
                Some(i + 2)
            }
            b'`' => {
                let run = bytes[i..].iter().take_while(|x| **x == b'`').count();
                find_backticks(src, i + run, run).map(|end| {
                    push_text(out, &src[plain..i], mark);
                    let code = &src[i + run..end];
                    let code = if code.len() > 2 && code.starts_with(' ') && code.ends_with(' ') { &code[1..code.len() - 1] } else { code };
                    push_text(out, code, Mark { code: true, ..mark });
                    end + run
                })
            }
            b'*' | b'_' => {
                let run = bytes[i..].iter().take_while(|x| **x == b).count().min(3);
                let opens = bytes.get(i + run).is_some_and(|n| !n.is_ascii_whitespace());
                let intraword = b == b'_' && i > 0 && bytes[i - 1].is_ascii_alphanumeric();
                (opens && !intraword).then(|| closing_run(src, i + run, b, run)).flatten().map(|end| {
                    push_text(out, &src[plain..i], mark);
                    let inner = Mark { strong: mark.strong || run >= 2, emphasis: mark.emphasis || run != 2, ..mark };
                    inline_into(&src[i + run..end], inner, out, depth + 1);
                    end + run
                })
            }
            b'~' if bytes.get(i + 1) == Some(&b'~') => closing_run(src, i + 2, b'~', 2).map(|end| {
                push_text(out, &src[plain..i], mark);
                inline_into(&src[i + 2..end], Mark { strike: true, ..mark }, out, depth + 1);
                end + 2
            }),
            b'[' => link_at(src, i).map(|(text, url, end)| {
                push_text(out, &src[plain..i], mark);
                let linked = with_link(out, mark, url);
                inline_into(text, linked, out, depth + 1);
                end
            }),
            b'<' => src[i + 1..].find('>').map(|n| &src[i + 1..i + 1 + n]).filter(|u| bare_url(u, 0) == Some(u.len())).map(|url| {
                push_text(out, &src[plain..i], mark);
                let linked = with_link(out, mark, url);
                push_text(out, url, linked);
                i + url.len() + 2
            }),
            b'h' => bare_url(src, i).map(|len| {
                push_text(out, &src[plain..i], mark);
                let url = &src[i..i + len];
                let linked = with_link(out, mark, url);
                push_text(out, url, linked);
                i + len
            }),
            _ => None,
        };
        match consumed {
            Some(next) => {
                i = next;
                plain = next;
            }
            None => i += src[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    push_text(out, &src[plain..], mark);
}

/// `[text](url "title")` at `i`: the text, the URL and the index after it.
fn link_at(src: &str, i: usize) -> Option<(&str, &str, usize)> {
    let bytes = src.as_bytes();
    let mut depth = 0;
    let mut close = None;
    let mut j = i;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    let close = close?;
    let rest = src.get(close + 1..)?.strip_prefix('(')?;
    let end = rest.find(')')?;
    let target = rest[..end].trim();
    let url = target.split_once([' ', '\t']).map_or(target, |(u, _)| u);
    let url = url.trim_start_matches('<').trim_end_matches('>');
    (!url.is_empty()).then_some((&src[i + 1..close], url, close + 2 + end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(inline: &Inline) -> Vec<(&str, Mark)> {
        inline.spans.iter().map(|(r, m)| (&inline.text[r.clone()], *m)).collect()
    }

    #[test]
    fn inline_markup() {
        let i = parse_inline(r"**bold** *it* `co*de` ~~no~~ snake_case_name \*lit\* 2*3*4");
        // `*` may emphasise inside a word, as in CommonMark.
        assert_eq!(i.text, "bold it co*de no snake_case_name *lit* 234");
        let bold = Mark { strong: true, ..Mark::default() };
        let it = Mark { emphasis: true, ..Mark::default() };
        let code = Mark { code: true, ..Mark::default() };
        let strike = Mark { strike: true, ..Mark::default() };
        assert_eq!(marks(&i), [("bold", bold), ("it", it), ("co*de", code), ("no", strike), ("3", it)]);

        let nested = parse_inline("***both*** and **bold *inner***");
        assert_eq!(nested.text, "both and bold inner");
        assert!(nested.spans.iter().any(|(_, m)| m.strong && m.emphasis));
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        for text in ["**streaming", "a * b", "`open", "~~x", "[half](", "_x"] {
            assert_eq!(parse_inline(text).text, text);
        }
    }

    #[test]
    fn links() {
        let i = parse_inline("see [the **docs**](https://a.io/x \"t\") or https://b.io/y). <https://c.io>");
        assert_eq!(i.text, "see the docs or https://b.io/y). https://c.io");
        assert_eq!(i.links, ["https://a.io/x", "https://b.io/y", "https://c.io"]);
        let docs = i.spans.iter().find(|(r, _)| &i.text[r.clone()] == "docs").unwrap().1;
        assert!(docs.strong && docs.link == 1);
    }

    #[test]
    fn blocks() {
        let doc = parse("# Title #\n\nPara one\nline two\n\n---\n> quoted\n> more\n\n```Rust extra\nfn main() {}\n\n```\n");
        assert_eq!(doc.len(), 5);
        assert!(matches!(&doc[0], Block::Heading { level: 1, text } if text.text == "Title"));
        assert!(matches!(&doc[1], Block::Paragraph(p) if p.text == "Para one\nline two"));
        assert_eq!(doc[2], Block::Rule);
        assert!(matches!(&doc[3], Block::Quote(inner) if inner.len() == 1));
        assert_eq!(doc[4], Block::Code { lang: "rust".into(), code: "fn main() {}\n".into() });
    }

    #[test]
    fn unterminated_fence_runs_to_the_end() {
        assert_eq!(parse("```py\nprint(1)"), [Block::Code { lang: "py".into(), code: "print(1)".into() }]);
    }

    #[test]
    fn nested_lists_and_tasks() {
        let doc = parse("1. one\n2. two\n   - inner a\n   - [x] inner b\n     continued\n3. three\n\nafter");
        let [Block::List { start: Some(1), items }, Block::Paragraph(after)] = doc.as_slice() else { panic!("{doc:?}") };
        assert_eq!(after.text, "after");
        assert_eq!(items.len(), 3);
        let [Block::Paragraph(two), Block::List { start: None, items: inner }] = items[1].blocks.as_slice() else { panic!() };
        assert_eq!(two.text, "two");
        assert_eq!(inner[1].task, Some(true));
        assert!(matches!(&inner[1].blocks[0], Block::Paragraph(p) if p.text == "inner b\ncontinued"));
    }

    #[test]
    fn tables() {
        let doc = parse("| a | b |\n|:--|--:|\n| 1 | `x|y` |\n| 2 |\n\ntext");
        let Block::Table { align, header, rows } = &doc[0] else { panic!("{doc:?}") };
        assert_eq!(align, &[Align::Left, Align::Right]);
        assert_eq!(header[1].text, "b");
        assert_eq!(rows[0][1].text, "x|y");
        assert_eq!(rows[1].len(), 2, "short rows are padded");
        assert!(matches!(&doc[1], Block::Paragraph(p) if p.text == "text"));
    }

    #[test]
    fn rules_are_not_list_items() {
        assert_eq!(parse("* * *"), [Block::Rule]);
        assert!(matches!(&parse("**bold** start")[0], Block::Paragraph(_)));
    }

    #[test]
    fn hostile_nesting_is_bounded() {
        let quotes = ">".repeat(100_000) + " x";
        assert!(!parse(&quotes).is_empty());
        let lists = "- ".repeat(50_000) + "x";
        assert!(!parse(&lists).is_empty());
        let links = "[".repeat(20_000) + &"x](u)".repeat(20_000);
        assert!(!parse_inline(&links).text.is_empty());
        let stars = "*".repeat(3) + &"_a ".repeat(10_000);
        assert!(!parse_inline(&stars).text.is_empty());
    }
}
