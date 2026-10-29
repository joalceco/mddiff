//! Bounded, deterministic Jira ADF v1 import. Unsupported content stays visible as JSON.

use std::{borrow::Cow, collections::BTreeSet};

use axum::http::StatusCode;
use serde_json::Value;

use crate::{AppError, MAX_DOCUMENT_BYTES};

pub const VERSION: &str = "adf-md-v1";
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 32_768;

#[derive(Debug)]
pub struct Converted {
    pub markdown: String,
    pub warnings: Vec<String>,
}

fn malformed() -> AppError {
    AppError::bad_request("La descripción de Jira no contiene un documento ADF v1 válido.")
}
fn excessive() -> AppError {
    AppError(
        StatusCode::PAYLOAD_TOO_LARGE,
        "La descripción de Jira supera los límites de conversión o de 2 MiB de Markdown.".into(),
    )
}
fn bounded(text: String) -> Result<String, AppError> {
    if text.len() as u64 > MAX_DOCUMENT_BYTES {
        Err(excessive())
    } else {
        Ok(text)
    }
}
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if "\\`*_{}[]()<>#+-.!|~".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}
fn longest_backticks(text: &str) -> usize {
    text.split(|ch| ch != '`').map(str::len).max().unwrap_or(0)
}
fn inline_code(text: &str) -> String {
    let fence = "`".repeat(longest_backticks(text) + 1);
    if text.chars().all(|ch| ch == ' ') {
        format!("{fence}{text}{fence}")
    } else {
        format!("{fence} {text} {fence}")
    }
}
fn code_block(text: &str, language: &str) -> String {
    let fence = "`".repeat((longest_backticks(text) + 1).max(3));
    format!(
        "{fence}{language}\n{text}{}{fence}",
        if text.ends_with('\n') { "" } else { "\n" }
    )
}

fn table_cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut slashes = 0;
    for ch in text.replace("  \n", "<br>").replace('\n', " ").chars() {
        if ch == '|' && slashes % 2 == 0 {
            out.push('\\');
        }
        out.push(ch);
        slashes = if ch == '\\' { slashes + 1 } else { 0 };
    }
    out
}

fn same_text_run(left: &Value, right: &Value) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    left.get("type").is_some_and(|v| v == "text")
        && right.get("text").is_some_and(Value::is_string)
        && left.len() == right.len()
        && left
            .iter()
            .filter(|(key, _)| key.as_str() != "text")
            .all(|(key, value)| right.get(key) == Some(value))
}

struct Converter {
    nodes: usize,
    warnings: BTreeSet<String>,
}
impl Converter {
    fn children<'a>(&self, node: &'a Value) -> Result<&'a [Value], AppError> {
        match node.get("content") {
            None => Ok(&[]),
            Some(Value::Array(children)) => Ok(children),
            _ => Err(malformed()),
        }
    }
    fn warning(&mut self, text: String) -> Result<(), AppError> {
        self.warnings.insert(text);
        if self.warnings.len() > 64 {
            return Err(excessive());
        }
        Ok(())
    }
    fn unsupported(&mut self, node: &Value, reason: &str) -> Result<String, AppError> {
        self.warning(reason.to_owned())?;
        let json = serde_json::to_string(node).map_err(|_| malformed())?;
        if json.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(excessive());
        }
        bounded(format!(
            "[ADF no representable: {reason}]\n\n{}",
            code_block(&json, "json")
        ))
    }
    fn join(
        &mut self,
        children: &[Value],
        depth: usize,
        separator: &str,
    ) -> Result<String, AppError> {
        let mut out = String::new();
        let mut remaining = children.iter().peekable();
        let mut index = 0;
        while let Some(child) = remaining.next() {
            let mut node = Cow::Borrowed(child);
            if separator.is_empty() && child["type"] == "text" {
                let mut joined: Option<String> = None;
                while let Some(next) = remaining.peek().filter(|next| same_text_run(child, next)) {
                    self.nodes += 1;
                    if self.nodes > MAX_NODES {
                        return Err(excessive());
                    }
                    let previous = child["text"].as_str().ok_or_else(malformed)?;
                    let text = next["text"].as_str().ok_or_else(malformed)?;
                    let joined = joined.get_or_insert_with(|| previous.to_owned());
                    if joined.len().saturating_add(text.len()) as u64 > MAX_DOCUMENT_BYTES {
                        return Err(excessive());
                    }
                    joined.push_str(text);
                    remaining.next();
                }
                if let Some(joined) = joined {
                    node.to_mut()["text"] = joined.into();
                }
            }
            let next = self.node(&node, depth)?;
            if index > 0 {
                out.push_str(separator);
            }
            // Separate changing inline delimiters without adding visible text or empty links.
            if separator.is_empty()
                && out
                    .chars()
                    .next_back()
                    .is_some_and(|last| "*~`".contains(last) && next.starts_with(last))
            {
                out.push_str("<!---->");
            }
            if out.len().saturating_add(next.len()) as u64 > MAX_DOCUMENT_BYTES {
                return Err(excessive());
            }
            out.push_str(&next);
            index += 1;
        }
        bounded(out)
    }

    fn text(&mut self, node: &Value) -> Result<String, AppError> {
        let raw = node["text"].as_str().ok_or_else(malformed)?;
        let mut value = escape(raw);
        let marks = match node.get("marks") {
            None => &[][..],
            Some(Value::Array(marks)) => marks,
            _ => return Err(malformed()),
        };
        if marks.len() > 32 {
            return Err(excessive());
        }
        // Core marks use a fixed order; raw unsupported marks retain their input order.
        let mut kinds = BTreeSet::new();
        for mark in marks {
            let kind = mark["type"].as_str().ok_or_else(malformed)?;
            if !kinds.insert(kind) {
                return Err(malformed());
            }
        }
        if kinds.contains("code") {
            value = inline_code(raw);
        }
        for (kind, delimiter) in [("strong", "**"), ("em", "*"), ("strike", "~~")] {
            if kinds.contains(kind) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    let start = value.len() - value.trim_start().len();
                    let end = value.trim_end().len();
                    value = format!(
                        "{}{delimiter}{}{delimiter}{}",
                        &value[..start],
                        &value[start..end],
                        &value[end..]
                    );
                }
            }
        }
        for mark in marks {
            match mark["type"].as_str().ok_or_else(malformed)? {
                "code" | "strong" | "em" | "strike" => {
                    if let Some(attrs) = self.attributes(mark, &[])? {
                        value.push_str(&self.attribute_note(&attrs, true)?);
                    }
                }
                "link" => {
                    let href = mark["attrs"]["href"].as_str().ok_or_else(malformed)?;
                    let url = reqwest::Url::parse(href).ok().filter(|u| {
                        matches!(u.scheme(), "http" | "https" | "mailto")
                            && u.username().is_empty()
                            && u.password().is_none()
                            && !href.chars().any(char::is_control)
                            && !href.contains(['<', '>'])
                    });
                    if let Some(url) = url {
                        value = format!("[{value}](<{}>)", url.as_str());
                        if mark["attrs"].get("title").is_some() {
                            self.warning(
                                "El título auxiliar de un enlace ADF no se representa en Markdown."
                                    .into(),
                            )?;
                            value.push_str(&format!(
                                " [ADF título: {}]",
                                inline_code(
                                    &serde_json::to_string(&mark["attrs"]["title"])
                                        .map_err(|_| malformed())?
                                )
                            ));
                        }
                    } else {
                        self.warning("Un enlace ADF no seguro o relativo se conserva como datos sin enlace activo.".into())?;
                        value.push_str(&format!(
                            " [ADF enlace: {}]",
                            inline_code(&serde_json::to_string(mark).map_err(|_| malformed())?)
                        ));
                    }
                    if let Some(attrs) = self.attributes(mark, &["href", "title"])? {
                        value.push_str(&self.attribute_note(&attrs, true)?);
                    }
                }
                _ => {
                    self.warning("Hay marcas ADF no representables; sus atributos se conservan junto al texto.".into())?;
                    value.push_str(&format!(
                        " [ADF marca: {}]",
                        inline_code(&serde_json::to_string(mark).map_err(|_| malformed())?)
                    ));
                }
            }
        }
        bounded(value)
    }
    fn list(&mut self, node: &Value, depth: usize, ordered: bool) -> Result<String, AppError> {
        let mut out = String::new();
        let start = if ordered {
            node["attrs"]
                .get("order")
                .map_or(Some(1), Value::as_u64)
                .filter(|v| *v > 0 && *v <= 999_999_999)
                .ok_or_else(malformed)?
        } else {
            1
        };
        for (index, child) in self.children(node)?.iter().enumerate() {
            if child["type"] != "listItem" {
                return Err(malformed());
            }
            let content = self.node(child, depth)?;
            let marker = if ordered {
                format!(
                    "{}. ",
                    start
                        .checked_add(index as u64)
                        .filter(|v| *v <= 999_999_999)
                        .ok_or_else(malformed)?
                )
            } else {
                "- ".into()
            };
            let indent = " ".repeat(marker.len());
            if index > 0 {
                out.push('\n');
            }
            for (line_index, line) in content.lines().enumerate() {
                if line_index > 0 {
                    out.push('\n');
                }
                out.push_str(if line_index == 0 { &marker } else { &indent });
                out.push_str(line);
            }
            if content.is_empty() {
                out.push_str(&marker);
            }
            if out.len() as u64 > MAX_DOCUMENT_BYTES {
                return Err(excessive());
            }
        }
        bounded(out)
    }
    fn attributes(&self, node: &Value, represented: &[&str]) -> Result<Option<Value>, AppError> {
        let Some(attrs) = node.get("attrs") else {
            return Ok(None);
        };
        let mut extra = attrs.as_object().ok_or_else(malformed)?.clone();
        // Editor-local IDs do not affect document content or presentation.
        extra.remove("localId");
        for key in represented {
            extra.remove(*key);
        }
        if extra.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Value::Object(extra)))
        }
    }
    fn attribute_note(&mut self, attrs: &Value, inline: bool) -> Result<String, AppError> {
        self.warning("Hay atributos ADF que Markdown no representa; se conservan como JSON junto al contenido.".into())?;
        let json = serde_json::to_string(attrs).map_err(|_| malformed())?;
        if json.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(excessive());
        }
        bounded(if inline {
            format!(" [ADF atributos: {}]", inline_code(&json))
        } else {
            format!(
                "\n\n[ADF atributos no representados]\n\n{}",
                code_block(&json, "json")
            )
        })
    }

    fn table(&mut self, node: &Value, depth: usize) -> Result<String, AppError> {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut total = 0usize;
        let mut cell_attributes = Vec::new();
        let mut header = false;
        for (ri, row) in self.children(node)?.iter().enumerate() {
            self.nodes += 1;
            if self.nodes > MAX_NODES {
                return Err(excessive());
            }
            if row["type"] != "tableRow" {
                return Err(malformed());
            }
            if let Some(attrs) = self.attributes(row, &[])? {
                cell_attributes.push(serde_json::json!({"row":ri,"attrs":attrs}));
            }
            let cells = self.children(row)?;
            if cells.is_empty() {
                return Err(malformed());
            }
            if ri == 0
                && cells.iter().any(|c| c["type"] == "tableHeader")
                && cells.iter().any(|c| c["type"] != "tableHeader")
            {
                return self.unsupported(
                    node,
                    "Tabla ADF con encabezados parciales; se conserva su JSON completo.",
                );
            }
            let mut rendered = Vec::new();
            for (ci, cell) in cells.iter().enumerate() {
                self.nodes += 1;
                if self.nodes > MAX_NODES {
                    return Err(excessive());
                }
                let kind = cell["type"].as_str().ok_or_else(malformed)?;
                if !matches!(kind, "tableCell" | "tableHeader") {
                    return Err(malformed());
                }
                if (kind == "tableHeader" && ri > 0)
                    || ["colspan", "rowspan"]
                        .iter()
                        .any(|key| cell["attrs"].get(key).is_some_and(|v| v != 1))
                    || self
                        .children(cell)?
                        .iter()
                        .any(|n| n["type"] != "paragraph")
                {
                    return self.unsupported(node, "Tabla ADF con celdas combinadas, encabezados intermedios o contenido complejo; se conserva su JSON completo.");
                }
                if ri == 0 && kind == "tableHeader" {
                    header = true;
                }
                if let Some(attrs) = self.attributes(cell, &["colspan", "rowspan"])? {
                    cell_attributes.push(serde_json::json!({"row":ri,"column":ci,"attrs":attrs}));
                }
                let content = self.join(self.children(cell)?, depth + 2, "<br><br>")?;
                let content = table_cell(&content);
                total = total.saturating_add(content.len() + 6);
                if total as u64 > MAX_DOCUMENT_BYTES {
                    return Err(excessive());
                }
                rendered.push(content);
            }
            if !rows.is_empty() && rendered.len() != rows[0].len() {
                return self
                    .unsupported(node, "Tabla ADF irregular; se conserva su JSON completo.");
            }
            rows.push(rendered);
        }
        if rows.is_empty() {
            return Err(malformed());
        }
        let width = rows[0].len();
        let mut out = String::new();
        if !header {
            rows.insert(0, vec![String::new(); width]);
        }
        for (i, row) in rows.iter().enumerate() {
            out.push_str("| ");
            out.push_str(&row.join(" | "));
            out.push_str(" |\n");
            if i == 0 {
                out.push_str("| ");
                out.push_str(&vec!["---"; width].join(" | "));
                out.push_str(" |\n");
            }
        }
        let mut out = out.trim_end_matches('\n').to_owned();
        if !cell_attributes.is_empty() {
            out.push_str(&self.attribute_note(&Value::Array(cell_attributes), false)?);
        }
        bounded(out)
    }
    fn node(&mut self, node: &Value, depth: usize) -> Result<String, AppError> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err(excessive());
        }
        let kind = node["type"].as_str().ok_or_else(malformed)?;
        if kind.len() > 128 {
            return Err(malformed());
        }
        let children = self.children(node)?;
        let result = match kind {
            "doc" => self.join(children, depth + 1, "\n\n")?,
            "paragraph" => self.join(children, depth + 1, "")?,
            "text" => {
                if !children.is_empty() {
                    return Err(malformed());
                }
                self.text(node)?
            }
            "hardBreak" => {
                if !children.is_empty() {
                    return Err(malformed());
                }
                "  \n".into()
            }
            "heading" => {
                let level = node["attrs"]["level"]
                    .as_u64()
                    .filter(|v| (1..=6).contains(v))
                    .ok_or_else(malformed)?;
                format!(
                    "{} {}",
                    "#".repeat(level as usize),
                    self.join(children, depth + 1, "")?
                )
            }
            "blockquote" => self
                .join(children, depth + 1, "\n\n")?
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            "bulletList" => self.list(node, depth + 1, false)?,
            "orderedList" => self.list(node, depth + 1, true)?,
            "listItem" => self.join(children, depth + 1, "\n\n")?,
            "rule" => {
                if !children.is_empty() {
                    return Err(malformed());
                }
                "---".into()
            }
            "codeBlock" => {
                let language = match node["attrs"].get("language") {
                    None => "",
                    Some(Value::String(language)) => language,
                    _ => return Err(malformed()),
                };
                if language.len() > 64
                    || !language
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_+.-".contains(&b))
                {
                    return self.unsupported(node, "Lenguaje ADF no representable como info de un bloque de código; se conserva su JSON completo.");
                }
                let mut raw = String::new();
                for child in children {
                    self.nodes += 1;
                    if self.nodes > MAX_NODES
                        || child["type"] != "text"
                        || child.get("marks").is_some()
                    {
                        return Err(malformed());
                    }
                    if self.attributes(child, &[])?.is_some() {
                        return self.unsupported(node, "Código ADF con atributos de texto no representables; se conserva su JSON completo.");
                    }
                    raw.push_str(child["text"].as_str().ok_or_else(malformed)?);
                    if raw.len() as u64 > MAX_DOCUMENT_BYTES {
                        return Err(excessive());
                    }
                }
                code_block(&raw, language)
            }
            "table" => self.table(node, depth + 1)?,
            _ => {
                return self.unsupported(
                    node,
                    "Nodo ADF no soportado; se conserva su tipo, atributos y contenido como JSON.",
                );
            }
        };
        let represented = match kind {
            "heading" => &["level"][..],
            "orderedList" => &["order"][..],
            "codeBlock" => &["language"][..],
            _ => &[][..],
        };
        let mut result = result;
        if kind != "text"
            && let Some(marks) = node.get("marks")
        {
            if !marks.is_array() {
                return Err(malformed());
            }
            if !marks.as_array().ok_or_else(malformed)?.is_empty() {
                result.push_str(&self.attribute_note(&serde_json::json!({"marks":marks}), false)?);
            }
        }
        if let Some(attrs) = self.attributes(node, represented)? {
            result.push_str(&self.attribute_note(&attrs, kind == "text")?);
        }
        bounded(result)
    }
}

pub fn convert(document: &Value) -> Result<Converted, AppError> {
    if document["type"] != "doc" || document["version"] != 1 || !document["content"].is_array() {
        return Err(malformed());
    }
    let mut converter = Converter {
        nodes: 0,
        warnings: BTreeSet::new(),
    };
    let mut markdown = converter.node(document, 0)?;
    if !markdown.is_empty() && !markdown.ends_with('\n') {
        markdown.push('\n');
    }
    Ok(Converted {
        markdown: bounded(markdown)?,
        warnings: converter.warnings.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn supported_adf_has_deterministic_markdown() {
        let doc = json!({"type":"doc","version":1,"content":[
            {"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"Review"}]},
            {"type":"paragraph","content":[{"type":"text","text":"Strong","marks":[{"type":"strong"}]},{"type":"text","text":" and "},{"type":"text","text":"link","marks":[{"type":"link","attrs":{"href":"https://example.com/a?q=1"}},{"type":"em"}]}]},
            {"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"One"}]},{"type":"orderedList","attrs":{"order":3},"content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"Nested"}]}]}]}]}]},
            {"type":"codeBlock","attrs":{"language":"rust"},"content":[{"type":"text","text":"let x = `x`;\n```"}]},
            {"type":"table","content":[{"type":"tableRow","content":[{"type":"tableHeader","content":[{"type":"paragraph","content":[{"type":"text","text":"Name"}]}]}]},{"type":"tableRow","content":[{"type":"tableCell","content":[{"type":"paragraph","content":[{"type":"text","text":"A|B"}]}]}]}]}
        ]});
        let out = convert(&doc).unwrap();
        assert_eq!(
            out.markdown,
            "## Review\n\n**Strong** and [*link*](<https://example.com/a?q=1>)\n\n- One\n  \n  3. Nested\n\n````rust\nlet x = `x`;\n```\n````\n\n| Name |\n| --- |\n| A\\|B |\n"
        );
        assert!(out.warnings.is_empty());
        assert_eq!(convert(&doc).unwrap().markdown, out.markdown);
    }
    #[test]
    fn adjacent_inline_runs_keep_text_and_styles_in_the_actual_viewer() {
        let text = |value: &str, marks: &[&str]| json!({"type":"text","text":value,"marks":marks.iter().map(|kind| json!({"type":kind})).collect::<Vec<_>>()});
        for (nodes, expected) in [
            (
                vec![text("a", &["strong"]), text("b", &["strong"])],
                "<p><strong>ab</strong></p>\n",
            ),
            (
                vec![text("a", &["em"]), text("b", &["em"])],
                "<p><em>ab</em></p>\n",
            ),
            (
                vec![text("a", &["code"]), text("b", &["code"])],
                "<p><code>ab</code></p>\n",
            ),
            (
                vec![text("a", &["strike"]), text("b", &["strike"])],
                "<p><del>ab</del></p>\n",
            ),
            (
                vec![text("a", &["strong"]), text("b", &["em"])],
                "<p><strong>a</strong><em>b</em></p>\n",
            ),
            (
                vec![text("a", &["strong"]), text("b", &["strong", "em"])],
                "<p><strong>a</strong><em><strong>b</strong></em></p>\n",
            ),
            (
                vec![text("a", &["code"]), text("b", &["code", "strong"])],
                "<p><code>a</code><strong><code>b</code></strong></p>\n",
            ),
            (
                vec![text("a", &[]), text("b", &["strong"]), text("c", &[])],
                "<p>a<strong>b</strong>c</p>\n",
            ),
        ] {
            let converted = convert(
                &json!({"type":"doc","version":1,"content":[{"type":"paragraph","content":nodes}]}),
            )
            .unwrap();
            let comparison = crate::diff::compare(&converted.markdown, &converted.markdown);
            let rendered = comparison
                .rows
                .into_iter()
                .filter_map(|row| row.left)
                .map(|block| block.html)
                .collect::<String>();
            assert_eq!(rendered, expected, "{}", converted.markdown);
            assert!(!rendered.contains("<!--"));
            assert!(converted.warnings.is_empty());
        }
    }

    #[test]
    fn whitespace_marks_code_pipes_and_literal_markdown_keep_their_rendered_meaning() {
        let header = json!({"type":"tableHeader","content":[{"type":"paragraph","content":[{"type":"text","text":"Code"}]}]});
        let cell = json!({"type":"tableCell","content":[{"type":"paragraph","content":[{"type":"text","text":"a|b","marks":[{"type":"code"}]}]}]});
        let table = json!({"type":"table","content":[{"type":"tableRow","content":[header]},{"type":"tableRow","content":[cell]}]});
        let doc = json!({"type":"doc","version":1,"content":[
            {"type":"paragraph","content":[{"type":"text","text":" bold ","marks":[{"type":"strong"}]},{"type":"text","text":"*literal* # heading"}]}, table]});
        let out = convert(&doc).unwrap();
        let mut html = String::new();
        pulldown_cmark::html::push_html(
            &mut html,
            pulldown_cmark::Parser::new_ext(&out.markdown, pulldown_cmark::Options::ENABLE_TABLES),
        );
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("*literal* # heading"));
        assert!(html.contains("<td><code>a|b</code></td>"));
        assert_eq!(html.matches("<td>").count(), 1);
    }

    #[test]
    fn unsupported_nodes_marks_and_unsafe_links_remain_identifiable() {
        let doc = json!({"type":"doc","version":1,"content":[{"type":"paragraph","content":[{"type":"text","text":"unsafe <script>","marks":[{"type":"underline"},{"type":"link","attrs":{"href":"javascript:alert(1)"}}]}]},{"type":"mediaSingle","content":[{"type":"media","attrs":{"id":"private-attachment","type":"file"}}]}]});
        let out = convert(&doc).unwrap();
        assert!(out.markdown.contains("\\<script\\>"));
        assert!(out.markdown.contains("javascript:alert(1)"));
        assert!(!out.markdown.contains("](<javascript:"));
        assert!(out.markdown.contains("private-attachment"));
        assert_eq!(out.warnings.len(), 3);
    }
    #[test]
    fn unrepresented_attributes_are_visible_without_losing_readable_content() {
        let cell = serde_json::json!({"type":"tableCell","attrs":{"background":"#ffee00","colwidth":[180]},"content":[{"type":"paragraph","content":[{"type":"text","text":"Cell"}]}]});
        let doc = serde_json::json!({"type":"doc","version":1,"content":[
            {"type":"heading","marks":[{"type":"alignment","attrs":{"align":"center"}}],"attrs":{"level":2,"textAlign":"center","localId":"editor-metadata"},"content":[{"type":"text","text":"Readable"}]},
            {"type":"table","attrs":{"layout":"wide"},"content":[{"type":"tableRow","content":[cell]}]}]});
        let out = convert(&doc).unwrap();
        assert!(out.markdown.contains("## Readable"));
        assert!(out.markdown.contains("| Cell |"));
        for attribute in [
            "textAlign",
            "center",
            "background",
            "#ffee00",
            "colwidth",
            "180",
            "layout",
            "wide",
        ] {
            assert!(out.markdown.contains(attribute));
        }
        assert!(!out.markdown.contains("editor-metadata"));
        assert_eq!(out.warnings.len(), 1);
    }

    #[test]
    fn malformed_and_excessive_adf_is_rejected_without_truncation() {
        assert!(convert(&json!({"type":"doc","version":2,"content":[]})).is_err());
        assert_eq!(convert(&json!({"type":"doc","version":1,"content":[{"type":"paragraph","content":[{"type":"text","text":"x".repeat(MAX_DOCUMENT_BYTES as usize+1)}]}]})).unwrap_err().0,StatusCode::PAYLOAD_TOO_LARGE);
        let adjacent =
            vec![json!({"type":"text","text":"","marks":[{"type":"strong"}]}); MAX_NODES];
        assert_eq!(convert(&json!({"type":"doc","version":1,"content":[{"type":"paragraph","content":adjacent}]})).unwrap_err().0,StatusCode::PAYLOAD_TOO_LARGE);
        let mut nested = json!({"type":"paragraph","content":[]});
        for _ in 0..70 {
            nested = json!({"type":"blockquote","content":[nested]});
        }
        assert_eq!(
            convert(&json!({"type":"doc","version":1,"content":[nested]}))
                .unwrap_err()
                .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
