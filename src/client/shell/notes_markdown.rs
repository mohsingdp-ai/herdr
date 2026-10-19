//! The markdown the Notes tab understands: headings, lists, tasks, quotes,
//! fenced code, rules, `code`, **bold**, [[links]], #tags, @paths and web
//! links. Parsing is per line and keeps byte ranges into the source, so read
//! mode can hide the syntax and still map a click back to where it came from,
//! and edit mode can tint the raw text with the same rules.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// What a whole line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Block {
    Paragraph,
    Heading(u8),
    Bullet,
    Numbered,
    Task {
        checked: bool,
    },
    Quote,
    /// A ``` line.
    Fence,
    /// A line inside a fence.
    Code,
    /// `---`, `***` or `___`.
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Inline {
    Plain,
    /// Syntax: hidden in read mode, dimmed in edit mode.
    Mark,
    Bold,
    Code,
    /// A `[[link]]`'s shown text, with the note it points at.
    Link(String),
    Tag(String),
    FileRef,
    Url(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Span {
    pub(super) range: Range<usize>,
    pub(super) kind: Inline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedLine {
    pub(super) block: Block,
    /// Leading spaces before a list item or quote.
    pub(super) indent: usize,
    /// The block's own syntax (`## `, `- [ ] `, `> `, `1. `), after the indent.
    pub(super) marker: Range<usize>,
    /// The rest of the line, in order and without gaps.
    pub(super) spans: Vec<Span>,
}

/// Parses every line of `text`; fences carry over from line to line.
pub(super) fn parse(text: &str) -> Vec<ParsedLine> {
    let mut in_fence = false;
    text.split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                return whole(line, Block::Fence, Inline::Mark);
            }
            if in_fence {
                return whole(line, Block::Code, Inline::Code);
            }
            parse_line(line)
        })
        .collect()
}

fn whole(line: &str, block: Block, kind: Inline) -> ParsedLine {
    ParsedLine {
        block,
        indent: 0,
        marker: 0..0,
        spans: (!line.is_empty())
            .then_some(Span {
                range: 0..line.len(),
                kind,
            })
            .into_iter()
            .collect(),
    }
}

fn parse_line(line: &str) -> ParsedLine {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let trimmed = rest.trim_end();
    if indent < 4
        && trimmed.len() >= 3
        && ["-", "*", "_"].iter().any(|mark| {
            trimmed
                .chars()
                .all(|ch| ch.to_string() == *mark || ch == ' ')
                && trimmed.matches(mark).count() >= 3
        })
    {
        return ParsedLine {
            block: Block::Rule,
            indent,
            marker: indent..line.len(),
            spans: Vec::new(),
        };
    }
    let (block, marker_len) = block_marker(rest, indent);
    let marker = indent..indent + marker_len;
    let content_start = marker.end;
    ParsedLine {
        block,
        indent: if matches!(block, Block::Heading(_)) {
            0
        } else {
            indent
        },
        marker,
        spans: inline_spans(&line[content_start..], content_start),
    }
}

/// The block a line starts with and how many bytes its marker takes.
fn block_marker(rest: &str, indent: usize) -> (Block, usize) {
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    if indent == 0 && (1..=6).contains(&hashes) && rest[hashes..].starts_with(' ') {
        // Levels past three look like three.
        return (Block::Heading(hashes.min(3) as u8), hashes + 1);
    }
    for (prefix, checked) in [
        ("- [ ] ", false),
        ("- [x] ", true),
        ("- [X] ", true),
        ("* [ ] ", false),
        ("* [x] ", true),
    ] {
        if rest.starts_with(prefix) {
            return (Block::Task { checked }, prefix.len());
        }
    }
    if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        return (Block::Bullet, 2);
    }
    if rest.starts_with("> ") {
        return (Block::Quote, 2);
    }
    if rest == ">" {
        return (Block::Quote, 1);
    }
    let digits = rest.len()
        - rest
            .trim_start_matches(|ch: char| ch.is_ascii_digit())
            .len();
    if (1..=9).contains(&digits) && rest[digits..].starts_with(". ") {
        return (Block::Numbered, digits + 2);
    }
    (Block::Paragraph, 0)
}

/// A character a `#tag` may contain.
fn tag_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '/')
}

/// Splits `content` (which starts at byte `base` of its line) into spans.
fn inline_spans(content: &str, base: usize) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain_start = 0;
    let mut i = 0;
    let push = |spans: &mut Vec<Span>, range: Range<usize>, kind: Inline| {
        if !range.is_empty() {
            spans.push(Span {
                range: base + range.start..base + range.end,
                kind,
            });
        }
    };
    while i < content.len() {
        let rest = &content[i..];
        let word_start = content[..i]
            .chars()
            .next_back()
            .is_none_or(|prev| prev.is_whitespace() || prev == '(');
        let token: Option<Vec<(Range<usize>, Inline)>> =
            if let Some(after) = rest.strip_prefix("[[") {
                after.find("]]").filter(|&end| end > 0).map(|end| {
                    let inner = &after[..end];
                    let target = inner
                        .split(['|', '#'])
                        .next()
                        .unwrap_or(inner)
                        .trim()
                        .to_owned();
                    let open = i..i + 2;
                    let inner_start = i + 2;
                    let close = inner_start + end..inner_start + end + 2;
                    match inner.find('|') {
                        Some(bar) => vec![
                            (open.start..inner_start + bar + 1, Inline::Mark),
                            (inner_start + bar + 1..close.start, Inline::Link(target)),
                            (close, Inline::Mark),
                        ],
                        None => vec![
                            (open, Inline::Mark),
                            (inner_start..close.start, Inline::Link(target)),
                            (close, Inline::Mark),
                        ],
                    }
                })
            } else if let Some(after) = rest.strip_prefix("**") {
                after.find("**").filter(|&end| end > 0).map(|end| {
                    let inner = i + 2..i + 2 + end;
                    vec![
                        (i..i + 2, Inline::Mark),
                        (inner.clone(), Inline::Bold),
                        (inner.end..inner.end + 2, Inline::Mark),
                    ]
                })
            } else if let Some(after) = rest.strip_prefix('`') {
                after.find('`').filter(|&end| end > 0).map(|end| {
                    let inner = i + 1..i + 1 + end;
                    vec![
                        (i..i + 1, Inline::Mark),
                        (inner.clone(), Inline::Code),
                        (inner.end..inner.end + 1, Inline::Mark),
                    ]
                })
            } else if word_start && rest.starts_with('#') {
                let name: String = rest[1..].chars().take_while(|&ch| tag_char(ch)).collect();
                // `#1` is a number, not a tag.
                (!name.is_empty() && !name.chars().all(|ch| ch.is_ascii_digit()))
                    .then(|| vec![(i..i + 1 + name.len(), Inline::Tag(name))])
            } else if word_start && (rest.starts_with("https://") || rest.starts_with("http://")) {
                let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let url = rest[..len].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
                Some(vec![(i..i + url.len(), Inline::Url(url.to_owned()))])
            } else if word_start && rest.starts_with('@') {
                let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let path = rest[..len].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
                (path.len() > 1).then(|| vec![(i..i + path.len(), Inline::FileRef)])
            } else {
                None
            };
        match token {
            Some(parts) => {
                push(&mut spans, plain_start..i, Inline::Plain);
                let end = parts.last().map_or(i, |(range, _)| range.end);
                for (range, kind) in parts {
                    push(&mut spans, range, kind);
                }
                i = end;
                plain_start = i;
            }
            None => i += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    push(&mut spans, plain_start..content.len(), Inline::Plain);
    spans
}

/// The `[[links]]` in a note, by target name, skipping code.
pub(super) fn links(text: &str) -> Vec<String> {
    parse(text)
        .into_iter()
        .flat_map(|line| line.spans)
        .filter_map(|span| match span.kind {
            Inline::Link(target) => Some(target),
            _ => None,
        })
        .collect()
}

/// The `#tags` in a note, skipping code.
pub(super) fn tags(text: &str) -> Vec<String> {
    parse(text)
        .into_iter()
        .flat_map(|line| line.spans)
        .filter_map(|span| match span.kind {
            Inline::Tag(name) => Some(name),
            _ => None,
        })
        .collect()
}

/// Headings as (level, title, line index).
pub(super) fn headings(text: &str) -> Vec<(u8, String, usize)> {
    let lines: Vec<&str> = text.split('\n').collect();
    parse(text)
        .into_iter()
        .enumerate()
        .filter_map(|(index, parsed)| match parsed.block {
            Block::Heading(level) => {
                let line = lines.get(index)?;
                let title = line.get(parsed.marker.end..)?.trim().to_owned();
                Some((level, title, index))
            }
            _ => None,
        })
        .collect()
}

/// How a piece of a row looks; the renderer turns it into a style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Look {
    Plain,
    Heading(u8),
    Bold,
    Code,
    Link {
        exists: bool,
    },
    Tag,
    FileRef,
    Url,
    /// A list bullet or number.
    Bullet,
    Checkbox {
        checked: bool,
    },
    /// A done task's text.
    Done,
    Quote,
    QuoteBar,
    CodeBlock,
    Rule,
    /// Syntax shown in edit mode.
    Mark,
}

/// What clicking a piece in read mode does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DocTarget {
    Link(String),
    Tag(String),
    Url(String),
    /// A task's box, by line index.
    Checkbox(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Piece {
    pub(super) text: String,
    pub(super) look: Look,
    /// Byte offset in the source line of the piece's first character, when
    /// the piece shows source text as is.
    pub(super) src: Option<usize>,
    pub(super) target: Option<DocTarget>,
}

/// One screen row of a note in read mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DocRow {
    /// The source line it shows part of.
    pub(super) line: usize,
    pub(super) pieces: Vec<Piece>,
}

impl DocRow {
    /// The source byte offset under column `x`, for putting the cursor where
    /// a click landed. Past the text, the end of what the row shows.
    pub(super) fn source_offset(&self, x: u16) -> Option<usize> {
        let mut used = 0u16;
        let mut last = None;
        for piece in &self.pieces {
            let Some(src) = piece.src else {
                used = used.saturating_add(piece.text.width() as u16);
                continue;
            };
            let mut offset = src;
            for grapheme in piece.text.graphemes(true) {
                let width = grapheme.width() as u16;
                if x < used.saturating_add(width.max(1)) {
                    return Some(offset);
                }
                used = used.saturating_add(width);
                offset += grapheme.len();
            }
            last = Some(offset);
        }
        last
    }

    /// The click target under column `x`.
    pub(super) fn target_at(&self, x: u16) -> Option<&DocTarget> {
        let mut used = 0u16;
        for piece in &self.pieces {
            let width = piece.text.width() as u16;
            if x >= used && x < used.saturating_add(width) {
                return piece.target.as_ref();
            }
            used = used.saturating_add(width);
        }
        None
    }
}

/// One character of a line laid out for read mode.
struct Atom<'a> {
    text: &'a str,
    look: Look,
    src: Option<usize>,
    target: Option<DocTarget>,
}

/// Lays `text` out for read mode at `width` columns: syntax hidden, long
/// lines wrapped at spaces, list items hanging under their bullet.
/// `exists` says whether a `[[link]]` target is a note.
pub(super) fn layout(text: &str, width: u16, exists: &dyn Fn(&str) -> bool) -> Vec<DocRow> {
    let width = width.max(4);
    let mut rows = Vec::new();
    for (index, (line, parsed)) in text.split('\n').zip(parse(text)).enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if parsed.block == Block::Rule {
            rows.push(DocRow {
                line: index,
                pieces: vec![Piece {
                    text: "─".repeat(usize::from(width)),
                    look: Look::Rule,
                    src: None,
                    target: None,
                }],
            });
            continue;
        }
        if parsed.block == Block::Fence {
            // The fence line itself shows as a thin rule over the code.
            rows.push(DocRow {
                line: index,
                pieces: Vec::new(),
            });
            continue;
        }
        let indent = " ".repeat(parsed.indent);
        let lead: Vec<Piece> = match parsed.block {
            Block::Bullet => vec![lead_piece(format!("{indent}• "), Look::Bullet, None)],
            Block::Numbered => {
                let marker = line.get(parsed.marker.clone()).unwrap_or("").trim_end();
                vec![lead_piece(format!("{indent}{marker} "), Look::Bullet, None)]
            }
            Block::Task { checked } => vec![lead_piece(
                format!("{indent}{} ", if checked { "☑" } else { "☐" }),
                Look::Checkbox { checked },
                Some(DocTarget::Checkbox(index)),
            )],
            Block::Quote => vec![lead_piece(format!("{indent}▎ "), Look::QuoteBar, None)],
            Block::Code => vec![lead_piece("  ".to_owned(), Look::CodeBlock, None)],
            _ => Vec::new(),
        };
        let lead_width: u16 = lead.iter().map(|piece| piece.text.width() as u16).sum();
        let base = match parsed.block {
            Block::Heading(level) => Look::Heading(level),
            Block::Task { checked: true } => Look::Done,
            Block::Quote => Look::Quote,
            Block::Code => Look::CodeBlock,
            _ => Look::Plain,
        };
        let mut atoms = Vec::new();
        for span in &parsed.spans {
            let (look, target) = match &span.kind {
                Inline::Mark => continue,
                Inline::Plain => (base, None),
                Inline::Bold => (
                    if base == Look::Plain {
                        Look::Bold
                    } else {
                        base
                    },
                    None,
                ),
                Inline::Code if parsed.block == Block::Code => (Look::CodeBlock, None),
                Inline::Code => (Look::Code, None),
                Inline::Link(name) => (
                    Look::Link {
                        exists: exists(name),
                    },
                    Some(DocTarget::Link(name.clone())),
                ),
                Inline::Tag(name) => (Look::Tag, Some(DocTarget::Tag(name.clone()))),
                Inline::FileRef => (Look::FileRef, None),
                Inline::Url(url) => (Look::Url, Some(DocTarget::Url(url.clone()))),
            };
            let Some(source) = line.get(span.range.clone()) else {
                continue;
            };
            for (offset, grapheme) in source.grapheme_indices(true) {
                atoms.push(Atom {
                    text: grapheme,
                    look,
                    src: Some(span.range.start + offset),
                    target: target.clone(),
                });
            }
        }
        let avail = width.saturating_sub(lead_width).max(1);
        let mut first = true;
        for chunk in wrap_atoms(&atoms, avail) {
            let mut pieces = if first {
                lead.clone()
            } else if lead_width > 0 {
                vec![lead_piece(" ".repeat(usize::from(lead_width)), base, None)]
            } else {
                Vec::new()
            };
            first = false;
            for atom in chunk {
                match pieces.last_mut() {
                    Some(last)
                        if last.look == atom.look
                            && last.target == atom.target
                            && last.src.is_some()
                            && last.src.map(|src| src + last.text.len()) == atom.src =>
                    {
                        last.text.push_str(atom.text);
                    }
                    _ => pieces.push(Piece {
                        text: atom.text.to_owned(),
                        look: atom.look,
                        src: atom.src,
                        target: atom.target.clone(),
                    }),
                }
            }
            rows.push(DocRow {
                line: index,
                pieces,
            });
        }
    }
    rows
}

fn lead_piece(text: String, look: Look, target: Option<DocTarget>) -> Piece {
    Piece {
        text,
        look,
        src: None,
        target,
    }
}

/// Splits a line's atoms into rows of at most `width` columns, breaking after
/// a space when a word would not fit. An empty line is one empty row.
fn wrap_atoms<'a, 'b>(atoms: &'b [Atom<'a>], width: u16) -> Vec<&'b [Atom<'a>]> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut start = 0;
    while start < atoms.len() {
        let mut used = 0usize;
        let mut end = start;
        let mut last_break = None;
        while end < atoms.len() {
            let cols = atoms[end].text.width();
            if used + cols > width && end > start {
                break;
            }
            used += cols;
            if atoms[end].text.chars().all(char::is_whitespace) {
                last_break = Some(end + 1);
            }
            end += 1;
        }
        let cut = if end < atoms.len() {
            last_break.filter(|&at| at > start).unwrap_or(end)
        } else {
            end
        };
        rows.push(&atoms[start..cut]);
        start = cut;
    }
    if rows.is_empty() {
        rows.push(&atoms[0..0]);
    }
    rows
}

/// The look of each byte range of a raw line, for tinting it in edit mode.
pub(super) fn tint(line: &str, parsed: &ParsedLine) -> Vec<(Range<usize>, Look)> {
    let base = match parsed.block {
        Block::Heading(level) => Look::Heading(level),
        Block::Task { checked: true } => Look::Done,
        Block::Quote => Look::Quote,
        Block::Code | Block::Fence => Look::CodeBlock,
        Block::Rule => Look::Mark,
        _ => Look::Plain,
    };
    let mut out = Vec::new();
    if !parsed.marker.is_empty() {
        let look = match parsed.block {
            Block::Task { checked } => Look::Checkbox { checked },
            Block::Bullet | Block::Numbered => Look::Bullet,
            Block::Quote => Look::QuoteBar,
            _ => Look::Mark,
        };
        out.push((parsed.marker.clone(), look));
    }
    for span in &parsed.spans {
        let look = match &span.kind {
            Inline::Plain => base,
            Inline::Mark => Look::Mark,
            Inline::Bold if base == Look::Plain => Look::Bold,
            Inline::Bold => base,
            Inline::Code if matches!(parsed.block, Block::Code | Block::Fence) => Look::CodeBlock,
            Inline::Code => Look::Code,
            Inline::Link(_) => Look::Link { exists: true },
            Inline::Tag(_) => Look::Tag,
            Inline::FileRef => Look::FileRef,
            Inline::Url(_) => Look::Url,
        };
        out.push((span.range.clone(), look));
    }
    out.retain(|(range, _)| range.end <= line.len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<(&str, Inline)> {
        let parsed = parse_line(line);
        parsed
            .spans
            .into_iter()
            .map(|span| (&line[span.range], span.kind))
            .collect()
    }

    fn shown(rows: &[DocRow]) -> Vec<String> {
        rows.iter()
            .map(|row| row.pieces.iter().map(|piece| piece.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn blocks_are_recognized_with_their_markers() {
        let cases = [
            ("# Title", Block::Heading(1), "# "),
            ("### Deep", Block::Heading(3), "### "),
            ("##### Deeper", Block::Heading(3), "##### "),
            ("#tag only", Block::Paragraph, ""),
            ("- item", Block::Bullet, "- "),
            ("- [ ] todo", Block::Task { checked: false }, "- [ ] "),
            ("- [x] done", Block::Task { checked: true }, "- [x] "),
            ("> quoted", Block::Quote, "> "),
            ("12. twelfth", Block::Numbered, "12. "),
            ("---", Block::Rule, "---"),
            ("plain", Block::Paragraph, ""),
        ];
        for (line, block, marker) in cases {
            let parsed = parse_line(line);
            assert_eq!(parsed.block, block, "{line}");
            assert_eq!(&line[parsed.marker.clone()], marker, "{line}");
        }
        let nested = parse_line("    - nested");
        assert_eq!((nested.block, nested.indent), (Block::Bullet, 4));
    }

    #[test]
    fn inline_tokens_keep_their_source_ranges() {
        assert_eq!(
            kinds("see [[File tree]] and **bold** `code` #ui/tab"),
            vec![
                ("see ", Inline::Plain),
                ("[[", Inline::Mark),
                ("File tree", Inline::Link("File tree".into())),
                ("]]", Inline::Mark),
                (" and ", Inline::Plain),
                ("**", Inline::Mark),
                ("bold", Inline::Bold),
                ("**", Inline::Mark),
                (" ", Inline::Plain),
                ("`", Inline::Mark),
                ("code", Inline::Code),
                ("`", Inline::Mark),
                (" ", Inline::Plain),
                ("#ui/tab", Inline::Tag("ui/tab".into())),
            ]
        );
        assert_eq!(
            kinds("[[Decisions#Board|why]]"),
            vec![
                ("[[Decisions#Board|", Inline::Mark),
                ("why", Inline::Link("Decisions".into())),
                ("]]", Inline::Mark),
            ]
        );
        assert_eq!(
            kinds("issue#4 is #1 not a tag, (https://x.dev/a). @src/a.rs."),
            vec![
                ("issue#4 is #1 not a tag, (", Inline::Plain),
                ("https://x.dev/a", Inline::Url("https://x.dev/a".into())),
                ("). ", Inline::Plain),
                ("@src/a.rs", Inline::FileRef),
                (".", Inline::Plain),
            ]
        );
        // Unclosed syntax stays text.
        assert_eq!(
            kinds("a [[b and **c"),
            vec![("a [[b and **c", Inline::Plain)]
        );
    }

    #[test]
    fn fenced_code_is_not_parsed_for_links_or_tags() {
        let text = "[[a]]\n```\n[[b]] #c\n```\n#d";
        assert_eq!(links(text), vec!["a".to_owned()]);
        assert_eq!(tags(text), vec!["d".to_owned()]);
        assert_eq!(
            headings("# One\ntext\n## Two"),
            vec![(1, "One".into(), 0), (2, "Two".into(), 2)]
        );
    }

    #[test]
    fn read_layout_hides_syntax_and_hangs_list_items() {
        let rows = layout(
            "## Plan\n- [ ] ship [[Notes]] now please\n> quoted",
            14,
            &|name| name == "Notes",
        );
        assert_eq!(
            shown(&rows),
            vec!["Plan", "☐ ship Notes ", "  now please", "▎ quoted"]
        );
        assert_eq!(rows[1].pieces[0].target, Some(DocTarget::Checkbox(1)));
        let link = rows[1]
            .pieces
            .iter()
            .find(|piece| piece.text == "Notes")
            .expect("link piece");
        assert_eq!(link.look, Look::Link { exists: true });
        assert_eq!(rows[1].target_at(8), Some(&DocTarget::Link("Notes".into())));
    }

    #[test]
    fn a_click_maps_back_to_the_source_byte() {
        let text = "- see **bold** end";
        let rows = layout(text, 40, &|_| true);
        // "• see bold end": column 6 is the "b" of bold, byte 8 in the source.
        assert_eq!(rows[0].source_offset(6), Some(8));
        assert_eq!(&text[8..9], "b");
        // Past the end: the end of the line.
        assert_eq!(rows[0].source_offset(39), Some(text.len()));
    }

    #[test]
    fn long_words_break_and_wide_characters_fit() {
        let rows = layout("abcdefghij", 4, &|_| true);
        assert_eq!(shown(&rows), vec!["abcd", "efgh", "ij"]);
        let rows = layout("日本語です", 4, &|_| true);
        assert!(rows.iter().all(|row| row
            .pieces
            .iter()
            .map(|piece| piece.text.width())
            .sum::<usize>()
            <= 4));
        assert_eq!(shown(&layout("", 10, &|_| true)), vec![""]);
    }

    #[test]
    fn edit_tint_covers_markers_and_spans() {
        let line = "- [x] **done** [[x]]";
        let tint = tint(line, &parse_line(line));
        assert_eq!(tint[0], (0..6, Look::Checkbox { checked: true }));
        assert!(tint.contains(&(8..12, Look::Done)));
        assert!(tint.contains(&(17..18, Look::Link { exists: true })));
    }
}
