use std::{
    ops::Range,
    time::{Duration, Instant},
};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;
use similar::{Algorithm, DiffTag, TextDiff, capture_diff_slices_deadline};

#[derive(Debug, Serialize)]
pub struct Comparison {
    pub rows: Vec<DiffRow>,
    pub added: usize,
    pub removed: usize,
    pub source_equal: bool,
}

#[derive(Debug, Serialize)]
pub struct DiffRow {
    pub kind: String,
    pub left: Option<Block>,
    pub right: Option<Block>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Block {
    pub source: String,
    pub html: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlighted_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlighted_source: Option<Vec<InlinePart>>,
    #[serde(skip)]
    events: Vec<Event<'static>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InlinePart {
    pub text: String,
    pub changed: bool,
}

pub fn compare(old: &str, new: &str) -> Comparison {
    let source_equal = old == new;
    let old_blocks = blocks(old);
    let new_blocks = if source_equal {
        old_blocks.clone()
    } else {
        blocks(new)
    };
    // Source catches formatting changes; HTML catches changes to shared link definitions.
    let old_keys: Vec<_> = old_blocks
        .iter()
        .map(|block| (block.source.as_str(), block.html.as_str()))
        .collect();
    let new_keys: Vec<_> = new_blocks
        .iter()
        .map(|block| (block.source.as_str(), block.html.as_str()))
        .collect();
    let ops = capture_diff_slices_deadline(
        Algorithm::Myers,
        &old_keys,
        &new_keys,
        Some(Instant::now() + Duration::from_millis(100)),
    );
    let mut old_blocks = old_blocks.into_iter();
    let mut new_blocks = new_blocks.into_iter();
    let mut comparison = Comparison {
        rows: Vec::new(),
        added: 0,
        removed: 0,
        source_equal,
    };
    let inline_deadline = Instant::now() + Duration::from_millis(100);
    let sanitizer = sanitizer();
    for op in ops {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        let old_len = old_range.len();
        let new_len = new_range.len();
        let alignment = if tag == DiffTag::Equal {
            positional_alignment(old_len, new_len)
        } else {
            align_changed_blocks(
                &old_blocks.as_slice()[..old_len],
                &new_blocks.as_slice()[..new_len],
                inline_deadline,
            )
        };
        for (has_left, has_right) in alignment {
            let mut left = has_left.then(|| old_blocks.next().unwrap());
            let mut right = has_right.then(|| new_blocks.next().unwrap());
            let kind = match (&left, &right) {
                (Some(_), Some(_)) if tag == DiffTag::Equal => "equal",
                (Some(_), Some(_)) => "replace",
                (Some(_), None) => "delete",
                (None, Some(_)) => "insert",
                (None, None) => unreachable!(),
            };
            if tag != DiffTag::Equal {
                comparison.added += usize::from(right.is_some());
                comparison.removed += usize::from(left.is_some());
                highlight_pair(left.as_mut(), right.as_mut(), inline_deadline, &sanitizer);
            }
            for block in left.iter_mut().chain(right.iter_mut()) {
                block.events = Vec::new();
            }
            comparison.rows.push(DiffRow {
                kind: kind.to_owned(),
                left,
                right,
            });
        }
    }
    comparison
}

fn positional_alignment(old_len: usize, new_len: usize) -> Vec<(bool, bool)> {
    (0..old_len.max(new_len))
        .map(|i| (i < old_len, i < new_len))
        .collect()
}

fn align_changed_blocks(old: &[Block], new: &[Block], deadline: Instant) -> Vec<(bool, bool)> {
    // shortcut: groups above 16,384 candidate pairs use positional pairing; use windowed matching if these need refinement.
    if old.is_empty()
        || new.is_empty()
        || (old.len() == 1 && new.len() == 1)
        || old.len().saturating_mul(new.len()) > 16_384
        || Instant::now() >= deadline
    {
        return positional_alignment(old.len(), new.len());
    }
    let [old_text, new_text] = [old, new].map(|blocks| {
        blocks
            .iter()
            .map(|block| {
                let text = visible_text(&block.events).0;
                if text.trim().is_empty() {
                    block.source.clone()
                } else {
                    text
                }
            })
            .collect::<Vec<_>>()
    });
    let [old_words, new_words] = [&old_text, &new_text].map(|texts| {
        texts
            .iter()
            .map(|text| text.split_whitespace().collect::<Vec<_>>())
            .collect::<Vec<_>>()
    });
    let width = new.len() + 1;
    let mut scores = vec![0usize; (old.len() + 1) * width];
    let mut steps = vec![0u8; scores.len()];
    for i in 1..=old.len() {
        for j in 1..=new.len() {
            if Instant::now() >= deadline {
                return positional_alignment(old.len(), new.len());
            }
            let index = i * width + j;
            let above = scores[index - width];
            let before = scores[index - 1];
            scores[index] = above.max(before);
            steps[index] = u8::from(above >= before);
            let a = &old_words[i - 1];
            let b = &new_words[j - 1];
            let common: usize =
                capture_diff_slices_deadline(Algorithm::Myers, a, b, Some(deadline))
                    .into_iter()
                    .filter_map(|op| {
                        let (tag, old_range, _) = op.as_tag_tuple();
                        (tag == DiffTag::Equal).then_some(old_range.len())
                    })
                    .sum();
            // More than half the words must match before a pair can anchor the alignment.
            let weight = if a.len() + b.len() == 0 {
                0
            } else {
                (2_000 * common / (a.len() + b.len())).saturating_sub(500)
            };
            let paired = scores[index - width - 1] + weight;
            if weight > 0 && paired >= scores[index] {
                scores[index] = paired;
                steps[index] = 2;
            }
        }
    }
    let (mut i, mut j) = (old.len(), new.len());
    let mut anchors = Vec::new();
    while i > 0 && j > 0 {
        match steps[i * width + j] {
            2 => {
                anchors.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            }
            1 => i -= 1,
            _ => j -= 1,
        }
    }
    anchors.reverse();
    let mut alignment = Vec::new();
    let (mut old_start, mut new_start) = (0, 0);
    for (old_end, new_end) in anchors
        .into_iter()
        .chain(std::iter::once((old.len(), new.len())))
    {
        alignment.extend(positional_alignment(
            old_end - old_start,
            new_end - new_start,
        ));
        if old_end < old.len() && new_end < new.len() {
            alignment.push((true, true));
        }
        old_start = old_end + 1;
        new_start = new_end + 1;
    }
    alignment
}

fn sanitizer() -> ammonia::Builder<'static> {
    let mut sanitizer = ammonia::Builder::default();
    sanitizer
        .add_allowed_classes("mark", &["diff-change"])
        .add_tags(&["input"])
        .add_tag_attributes("input", &["checked"])
        .set_tag_attribute_value("input", "type", "checkbox")
        .set_tag_attribute_value("input", "disabled", "");
    sanitizer
}

const MARKDOWN_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS);

pub(crate) fn within_hosted_complexity(markdown: &str) -> bool {
    let bytes = markdown.as_bytes();
    let breaks = bytes
        .iter()
        .enumerate()
        .filter(|(i, b)| **b == b'\n' || (**b == b'\r' && bytes.get(i + 1) != Some(&b'\n')))
        .take(10_001)
        .count();
    let final_line = usize::from(bytes.last().is_some_and(|b| !matches!(*b, b'\r' | b'\n')));
    if breaks + final_line > 10_000 {
        return false;
    }
    let (mut depth, mut html_tags) = (0usize, 0usize);
    for (index, event) in Parser::new_ext(markdown, MARKDOWN_OPTIONS).enumerate() {
        if index >= 32_768 {
            return false;
        }
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Html(value) | Event::InlineHtml(value) => {
                html_tags += value.bytes().filter(|b| *b == b'<').take(257).count();
            }
            _ => {}
        }
        if depth > 128 || html_tags > 256 {
            return false;
        }
    }
    true
}

fn blocks(markdown: &str) -> Vec<Block> {
    // shortcut: align complete top-level blocks; add nested alignment if structural edits need finer pairing.
    let sanitizer = sanitizer();
    let mut rendered = Vec::new();
    let mut events = Vec::new();
    let mut depth = 0;
    let mut block_start = 0;
    // Parse the whole document so references resolve before events are split into blocks.
    for (event, range) in Parser::new_ext(markdown, MARKDOWN_OPTIONS).into_offset_iter() {
        if depth == 0 {
            block_start = range.start;
        }
        match &event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
        events.push(event.into_static());
        if depth == 0 {
            let mut output = String::new();
            html::push_html(&mut output, events.iter().cloned());
            rendered.push((
                block_start,
                sanitizer.clean(&output).to_string(),
                std::mem::take(&mut events),
            ));
        }
    }
    if rendered.is_empty() && !markdown.is_empty() {
        rendered.push((0, String::new(), Vec::new()));
    }
    let mut rendered = rendered.into_iter().peekable();
    let mut output = Vec::new();
    let mut source_start = 0;
    let mut start_line = 1;
    while let Some((_, html, events)) = rendered.next() {
        // Keep gaps and reference definitions, which do not produce parser events.
        let source_end = rendered
            .peek()
            .map_or(markdown.len(), |(start, _, _)| *start);
        let source = &markdown[source_start..source_end];
        let newlines = source
            .bytes()
            .enumerate()
            .filter(|(index, byte)| {
                *byte == b'\n'
                    || (*byte == b'\r' && source.as_bytes().get(index + 1) != Some(&b'\n'))
            })
            .count();
        output.push(Block {
            source: source.to_owned(),
            html,
            start_line,
            end_line: start_line + newlines
                - usize::from(source.ends_with('\n') || source.ends_with('\r')),
            highlighted_html: None,
            highlighted_source: None,
            events,
        });
        source_start = source_end;
        start_line += newlines;
    }
    output
}

fn changed_ranges(
    old: &str,
    new: &str,
    deadline: Instant,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let diff = TextDiff::configure()
        .deadline(deadline)
        .diff_words(old, new);
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let (mut old_offset, mut new_offset) = (0, 0);
    for op in diff.ops() {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        let old_len: usize = old_range.map(|i| diff.old_lookup()[i].len()).sum();
        let new_len: usize = new_range.map(|i| diff.new_lookup()[i].len()).sum();
        if tag != DiffTag::Equal {
            if old_len > 0 {
                left.push(old_offset..old_offset + old_len);
            }
            if new_len > 0 {
                right.push(new_offset..new_offset + new_len);
            }
        }
        old_offset += old_len;
        new_offset += new_len;
    }
    (left, right)
}

fn marked_text(text: &str, ranges: &[Range<usize>]) -> String {
    let mut output = String::new();
    let mut offset = 0;
    for range in ranges {
        output.push_str(&ammonia::clean_text(&text[offset..range.start]));
        output.push_str("<mark class=\"diff-change\">");
        output.push_str(&ammonia::clean_text(&text[range.clone()]));
        output.push_str("</mark>");
        offset = range.end;
    }
    output.push_str(&ammonia::clean_text(&text[offset..]));
    output
}

fn source_parts(text: &str, ranges: &[Range<usize>]) -> Vec<InlinePart> {
    let mut output = Vec::new();
    let mut offset = 0;
    for range in ranges {
        output.push(InlinePart {
            text: text[offset..range.start].to_owned(),
            changed: false,
        });
        output.push(InlinePart {
            text: text[range.clone()].to_owned(),
            changed: true,
        });
        offset = range.end;
    }
    output.push(InlinePart {
        text: text[offset..].to_owned(),
        changed: false,
    });
    output
}

fn visible_text(events: &[Event<'_>]) -> (String, Vec<Option<Range<usize>>>) {
    // shortcut: raw HTML detail stays in Source; add DOM mapping when rendered HTML needs word highlights.
    let mut text = String::new();
    let mut spans = Vec::with_capacity(events.len());
    let mut image = false;
    for event in events {
        let mut span = None;
        match event {
            Event::Start(Tag::Image { .. }) => image = true,
            Event::End(TagEnd::Image) => image = false,
            Event::Text(value) | Event::Code(value) if !image => {
                let start = text.len();
                text.push_str(value);
                span = Some(start..text.len());
            }
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::TableCell
                | TagEnd::CodeBlock,
            ) if !image => text.push('\n'),
            _ => {}
        }
        spans.push(span);
    }
    (text, spans)
}

fn highlight_pair(
    left: Option<&mut Block>,
    right: Option<&mut Block>,
    deadline: Instant,
    sanitizer: &ammonia::Builder<'_>,
) {
    let (old_text, old_spans) = visible_text(left.as_ref().map_or(&[], |b| &b.events));
    let (new_text, new_spans) = visible_text(right.as_ref().map_or(&[], |b| &b.events));
    let (old_changes, new_changes) = changed_ranges(&old_text, &new_text, deadline);
    let (old_source, new_source) = changed_ranges(
        left.as_ref().map_or("", |b| b.source.as_str()),
        right.as_ref().map_or("", |b| b.source.as_str()),
        deadline,
    );
    for (block, spans, changes, source_changes) in [
        (left, old_spans, old_changes, old_source),
        (right, new_spans, new_changes, new_source),
    ] {
        let Some(block) = block else { continue };
        if !source_changes.is_empty() {
            block.highlighted_source = Some(source_parts(&block.source, &source_changes));
        }
        if changes.is_empty() {
            continue;
        }
        let mut events = Vec::with_capacity(block.events.len());
        for (event, span) in block.events.iter().zip(spans) {
            let local: Vec<_> = span
                .iter()
                .flat_map(|span| {
                    let first = changes.partition_point(|change| change.end <= span.start);
                    changes[first..]
                        .iter()
                        .take_while(|change| change.start < span.end)
                        .map(|change| {
                            span.start.max(change.start) - span.start
                                ..span.end.min(change.end) - span.start
                        })
                })
                .collect();
            match event {
                Event::Text(value) | Event::Code(value) if !local.is_empty() => {
                    let mut marked = marked_text(value, &local);
                    if matches!(event, Event::Code(_)) {
                        marked = format!("<code>{marked}</code>");
                    }
                    events.push(Event::InlineHtml(marked.into()));
                }
                _ => events.push(event.clone()),
            }
        }
        let mut html = String::new();
        html::push_html(&mut html, events.into_iter());
        block.highlighted_html = Some(sanitizer.clean(&html).to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_sources(comparison: &Comparison, old: &str, new: &str) {
        let left: String = comparison
            .rows
            .iter()
            .filter_map(|row| row.left.as_ref())
            .map(|block| block.source.as_str())
            .collect();
        let right: String = comparison
            .rows
            .iter()
            .filter_map(|row| row.right.as_ref())
            .map(|block| block.source.as_str())
            .collect();
        assert_eq!(left, old);
        assert_eq!(right, new);
    }

    #[test]
    fn identical_documents_have_no_changes() {
        let source = "# Título\n\nTexto con **énfasis**.\n";
        let result = compare(source, source);
        assert!(result.source_equal);
        assert_eq!((result.added, result.removed), (0, 0));
        assert!(result.rows.iter().all(|row| row.kind == "equal"));
        assert!(result.rows.iter().filter_map(|row| row.left.as_ref()).all(|block| block.highlighted_html.is_none() && block.highlighted_source.is_none()));
        assert_sources(&result, source, source);
    }

    #[test]
    fn insert_delete_and_unbalanced_replacement_preserve_source() {
        for (old, new) in [
            ("", "# Nuevo\n\nTexto\n"),
            ("# Antes\n\nTexto\n", ""),
            ("Viejo\n", "# Nuevo\n\nOtro\n"),
            ("A\n\nB\n\nC\n", "A\n\nNuevo\n\nC\n"),
        ] {
            let result = compare(old, new);
            assert!(!result.source_equal);
            assert!(result.added + result.removed > 0);
            assert_sources(&result, old, new);
        }
        let result = compare("Viejo\n", "# Nuevo\n\nOtro\n");
        assert_eq!((result.added, result.removed), (2, 1));
        assert_eq!(result.rows[0].kind, "replace");
        assert_eq!(result.rows[1].kind, "insert");
    }

    #[test]
    fn lists_tables_and_fenced_code_remain_complete_blocks() {
        let source = "- uno\n  - dos\n- tres\n\n| A | B |\n| - | - |\n| 1 | 2 |\n\n```rust\nlet x = 1;\n\nlet y = 2;\n```\n";
        let parsed = blocks(source);
        assert_eq!(parsed.len(), 3);
        assert!(parsed[0].html.contains("<ul>"));
        assert_eq!(parsed[0].html.matches("<li>").count(), 3);
        assert!(parsed[1].html.contains("<table>"));
        assert!(parsed[1].html.contains("<td>2</td>"));
        assert!(parsed[2].html.contains("<pre><code"));
        assert!(parsed[2].html.contains("let y = 2;"));
        assert_sources(&compare(source, source), source, source);
    }

    #[test]
    fn reference_changes_mark_the_block_with_the_visible_link() {
        let old = "[Visitar][ref]\n\nOtro párrafo.\n\n[ref]: https://old.example/\n";
        let new = "[Visitar][ref]\n\nOtro párrafo.\n\n[ref]: https://new.example/\n";
        let result = compare(old, new);
        let link = &result.rows[0];
        assert_eq!(link.kind, "replace");
        assert_eq!(
            link.left.as_ref().unwrap().source,
            link.right.as_ref().unwrap().source
        );
        assert!(
            link.left
                .as_ref()
                .unwrap()
                .html
                .contains("https://old.example/")
        );
        assert!(
            link.right
                .as_ref()
                .unwrap()
                .html
                .contains("https://new.example/")
        );
        assert_sources(&result, old, new);
    }

    #[test]
    fn source_only_changes_are_not_hidden_by_identical_rendering() {
        let result = compare("**hola**\n", "__hola__\n");
        assert_eq!(result.rows[0].kind, "replace");
        assert_eq!(
            result.rows[0].left.as_ref().unwrap().html,
            result.rows[0].right.as_ref().unwrap().html
        );
        assert!(!result.source_equal);
        assert!(
            result.rows[0]
                .left
                .as_ref()
                .unwrap()
                .highlighted_html
                .is_none()
        );
        assert!(
            result.rows[0]
                .left
                .as_ref()
                .unwrap()
                .highlighted_source
                .is_some()
        );
        assert_sources(&result, "**hola**\n", "__hola__\n");
    }

    #[test]
    fn whitespace_definitions_crlf_and_missing_final_newline_are_preserved() {
        for source in [
            "",
            "\n \t\n",
            "[ref]: https://example.org\n",
            "\n# Árbol\r\n\r\nTexto sin salto final",
            "---\n\n# fin\n\n",
            "<!-- comentario -->\n\nTexto\n",
        ] {
            assert_sources(&compare(source, ""), source, "");
            assert_sources(&compare("", source), "", source);
        }
        let result = compare("Texto\n", "Texto\n\n");
        assert_eq!(result.rows[0].kind, "replace");
    }

    #[test]
    fn line_ranges_refer_to_the_exact_source_segments() {
        let parsed = blocks("# Título\n\nPárrafo\n\nFin");
        assert_eq!((parsed[0].start_line, parsed[0].end_line), (1, 2));
        assert_eq!((parsed[1].start_line, parsed[1].end_line), (3, 4));
        assert_eq!((parsed[2].start_line, parsed[2].end_line), (5, 5));
        let parsed = blocks("# Título\r\rPárrafo\r\rFin");
        assert_eq!((parsed[0].start_line, parsed[0].end_line), (1, 2));
        assert_eq!((parsed[1].start_line, parsed[1].end_line), (3, 4));
        assert_eq!((parsed[2].start_line, parsed[2].end_line), (5, 5));
    }

    #[test]
    fn unsafe_html_and_urls_are_removed_but_source_is_retained() {
        let source = "<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\n[click](javascript:alert%281%29)\n";
        let parsed = blocks(source);
        let rendered: String = parsed.iter().map(|block| block.html.as_str()).collect();
        assert!(!rendered.contains("<script"));
        assert!(!rendered.contains("onerror"));
        assert!(!rendered.contains("javascript:"));
        assert_sources(&compare(source, source), source, source);
    }

    #[test]
    fn task_checkboxes_are_disabled_and_keep_their_checked_state() {
        let parsed = blocks("- [x] hecho\n- [ ] pendiente\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].html.matches("type=\"checkbox\"").count(), 2);
        assert_eq!(parsed[0].html.matches("disabled").count(), 2);
        assert_eq!(parsed[0].html.matches("checked").count(), 1);
    }

    #[test]
    fn highlights_only_changed_words_and_preserves_plain_html_and_source() {
        let old = "El árbol verde sigue aquí.\n";
        let new = "El árbol azul sigue aquí.\n";
        let result = compare(old, new);
        let row = &result.rows[0];
        let left = row.left.as_ref().unwrap();
        let right = row.right.as_ref().unwrap();
        assert_eq!(
            left.highlighted_html.as_deref(),
            Some("<p>El árbol <mark class=\"diff-change\">verde</mark> sigue aquí.</p>\n")
        );
        assert_eq!(
            right.highlighted_html.as_deref(),
            Some("<p>El árbol <mark class=\"diff-change\">azul</mark> sigue aquí.</p>\n")
        );
        assert!(!left.html.contains("<mark"));
        assert!(!right.html.contains("<mark"));
        assert_sources(&result, old, new);
    }

    #[test]
    fn highlights_preserve_emphasis_inline_code_lists_tables_and_fences() {
        for (old, new, expected) in [
            (
                "Usa **verde** y `uno` al salir.\n",
                "Usa **azul** y `dos` al salir.\n",
                "<strong><mark class=\"diff-change\">azul</mark></strong>",
            ),
            (
                "Usa `uno` al salir.\n",
                "Usa `dos` al salir.\n",
                "<code><mark class=\"diff-change\">dos</mark></code>",
            ),
            (
                "- uno\n- igual\n",
                "- dos\n- igual\n",
                "<li><mark class=\"diff-change\">dos</mark></li>",
            ),
            (
                "| A | B |\n| - | - |\n| uno | igual |\n",
                "| A | B |\n| - | - |\n| dos | igual |\n",
                "<td><mark class=\"diff-change\">dos</mark></td>",
            ),
            (
                "```rust\nlet x = 1;\n```\n",
                "```rust\nlet x = 2;\n```\n",
                "let x = <mark class=\"diff-change\">2;</mark>",
            ),
        ] {
            let result = compare(old, new);
            let html = result.rows[0]
                .right
                .as_ref()
                .unwrap()
                .highlighted_html
                .as_ref()
                .unwrap();
            assert!(html.contains(expected), "{html}");
            assert!(
                !html.contains("<mark class=\"diff-change\">igual"),
                "{html}"
            );
            assert_sources(&result, old, new);
        }
    }

    #[test]
    fn highlights_escape_source_and_never_turn_text_into_active_html() {
        let old = "Texto `seguro` sigue.\n";
        let new = "Texto `<script>alert(1)</script>` sigue.\n\n<script>alert(2)</script>\n";
        let result = compare(old, new);
        for block in result
            .rows
            .iter()
            .flat_map(|row| row.left.iter().chain(row.right.iter()))
        {
            if let Some(output) = &block.highlighted_html {
                assert!(!output.contains("<script"), "{output}");
                assert!(!output.contains("onclick="), "{output}");
            }
        }
        assert_sources(&result, old, new);
    }

    #[test]
    fn source_highlights_preserve_crlf_null_and_markup_as_plain_text() {
        let old = "Hola árbol\r\n\0<script>viejo</script>\r\n";
        let new = "Hola bosque\r\n\0<script>nuevo</script>\r\n";
        let result = compare(old, new);
        for block in result
            .rows
            .iter()
            .flat_map(|row| row.left.iter().chain(row.right.iter()))
        {
            if let Some(parts) = &block.highlighted_source {
                assert_eq!(
                    parts
                        .iter()
                        .map(|part| part.text.as_str())
                        .collect::<String>(),
                    block.source
                );
            }
        }
        assert_sources(&result, old, new);
    }

    #[test]
    fn highlights_handle_emoji_and_keep_image_alt_text_intact() {
        let old = "Un árbol 🌳 junto a ![árbol](foto.png) sigue.\n";
        let new = "Un árbol 🌲 junto a ![bosque](foto.png) sigue.\n";
        let result = compare(old, new);
        let html = result.rows[0]
            .right
            .as_ref()
            .unwrap()
            .highlighted_html
            .as_ref()
            .unwrap();
        assert!(
            html.contains("<mark class=\"diff-change\">🌲</mark>"),
            "{html}"
        );
        assert!(html.contains("alt=\"bosque\""), "{html}");
        assert_sources(&result, old, new);
    }

    #[test]
    fn inserted_paragraph_does_not_shift_similar_changed_paragraphs() {
        let old = "## Contexto\n\nEl flujo genera un informe con las respuestas del formulario.\n\nLas firmas se agregan al final y el proceso se completa al recibir validación.\n\nEl caso debe admitir un documento editable con respuestas y firmas en sus posiciones originales.\n\n## Cierre\n";
        let new = "## Contexto\n\nEl flujo genera un informe PDF con las respuestas del formulario.\n\nSe conserva el flujo con un solo destinatario y se omiten participantes adicionales.\n\nLas firmas predeterminadas se agregan al final y el proceso se completa cuando llega la validación.\n\nEl caso debe admitir un documento PDF con respuestas y firmas en sus posiciones originales. Ahora incluye una aclaración.\n\n## Cierre\n";
        for (before, after) in [(old, new), (new, old)] {
            let result = compare(before, after);
            let row = result
                .rows
                .iter()
                .find(|row| {
                    row.left
                        .as_ref()
                        .is_some_and(|b| b.source.starts_with("El caso debe"))
                })
                .unwrap();
            assert!(
                row.right
                    .as_ref()
                    .is_some_and(|b| b.source.starts_with("El caso debe")),
                "{row:?}"
            );
            for block in row.left.iter().chain(row.right.iter()) {
                assert!(
                    !block
                        .highlighted_html
                        .as_ref()
                        .unwrap()
                        .contains("<mark class=\"diff-change\">El caso debe")
                );
            }
            let extra = result
                .rows
                .iter()
                .find(|row| {
                    row.left
                        .iter()
                        .chain(row.right.iter())
                        .any(|b| b.source.starts_with("Se conserva el flujo"))
                })
                .unwrap();
            assert_ne!(extra.left.is_some(), extra.right.is_some());
            assert_sources(&result, before, after);
        }
    }
}
