//! Structural DocxXML → Markdown. Inspired by lark-cli's IM Markdown (MIT),
//! but uses XML order, prefers existing Mermaid and never silently drops blocks.
use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use roxmltree::{Document, Node};
use serde_json::Value;

use crate::remote::{ContentAsset, ContentBundle, ContentRequest};

const MAX_DEPTH: usize = 128;
pub(crate) const ASSET_SCHEME: &str = "feishufs-asset:";

pub(crate) fn render_document(
    xml: &str,
    references: &Value,
    base_url: &str,
    limit: usize,
    mut download_board: impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<ContentBundle> {
    // DocxXML is a fragment, not necessarily a single XML element.
    let wrapped = format!("<feishufs-document>{xml}</feishufs-document>");
    let document =
        Document::parse(&wrapped).context("Failed to parse DocxXML returned by Docs AI")?;
    let mut renderer = Renderer {
        xml: &wrapped,
        references,
        base_url,
        limit,
        assets: Vec::new(),
        board_names: HashMap::new(),
        download_board: &mut download_board,
        preserved_reference: false,
    };
    let mut markdown = renderer.children(document.root_element(), 0)?;
    if renderer.preserved_reference && !references.is_null() {
        markdown.push_str("\n\n> FeishuFS: reference data for unconverted resources.\n\n");
        markdown.push_str(&fenced(&serde_json::to_string_pretty(references)?, "json"));
    }
    let bytes = format!("{}\n", compact_spacing(&markdown).trim()).into_bytes();
    if bytes.len() > limit {
        bail!("Document content exceeds the {limit} byte per-file limit");
    }
    Ok(ContentBundle {
        bytes,
        assets: renderer.assets,
    })
}

struct Renderer<'a, F> {
    xml: &'a str,
    references: &'a Value,
    base_url: &'a str,
    limit: usize,
    assets: Vec<ContentAsset>,
    board_names: HashMap<String, String>,
    download_board: &'a mut F,
    preserved_reference: bool,
}

impl<F: FnMut(&str) -> Result<Vec<u8>>> Renderer<'_, F> {
    fn children(&mut self, node: Node<'_, '_>, depth: usize) -> Result<String> {
        let mut text = String::new();
        for child in node.children() {
            text.push_str(&self.render(child, depth + 1)?);
        }
        Ok(text)
    }

    fn attr(&self, node: Node<'_, '_>, key: &str) -> Option<String> {
        node.attribute(key).map(str::to_owned).or_else(|| {
            let reference = node.attribute("data-ref")?;
            self.references
                .get(node.tag_name().name())?
                .get(reference)?
                .get(key)?
                .as_str()
                .map(str::to_owned)
        })
    }

    fn render(&mut self, node: Node<'_, '_>, depth: usize) -> Result<String> {
        if depth > MAX_DEPTH {
            bail!(
                "DocxXML nesting exceeds {MAX_DEPTH} levels; refusing to publish truncated content"
            );
        }
        if node.is_text() {
            return Ok(escape_text(&normalize_text(
                node.text().unwrap_or_default(),
            )));
        }
        if !node.is_element() {
            return Ok(String::new());
        }
        let tag = node.tag_name().name();
        if matches!(tag, "code" | "pre") {
            let text = raw_text(node);
            let language = node
                .attribute("lang")
                .or(node.attribute("language"))
                .or_else(|| {
                    node.children()
                        .find(|n| n.has_tag_name("code"))
                        .and_then(|n| n.attribute("lang").or(n.attribute("language")))
                })
                .unwrap_or_default();
            return Ok(
                if tag == "pre" || !language.is_empty() || text.contains('\n') {
                    fenced(&text, language)
                } else {
                    inline_code(&text)
                },
            );
        }
        if tag == "whiteboard" {
            if self
                .attr(node, "type")
                .is_some_and(|t| t.eq_ignore_ascii_case("mermaid"))
            {
                let source = raw_text(node);
                if !source.trim().is_empty() {
                    return Ok(fenced(&source, "mermaid"));
                }
            }
            let token = self
                .attr(node, "token")
                .context("Whiteboard has neither a token nor Mermaid source")?;
            let name = if let Some(name) = self.board_names.get(&token) {
                name.clone()
            } else {
                let bytes = (self.download_board)(&token)
                    .with_context(|| format!("Failed to fetch an image for whiteboard {token}"))?;
                let extension = image_extension(&bytes)?;
                if bytes.len() > self.limit {
                    bail!(
                        "Whiteboard image exceeds the {} byte per-file limit",
                        self.limit
                    );
                }
                let name = format!("{}.{}", asset_token_name(&token)?, extension);
                self.assets.push(ContentAsset {
                    name: name.clone(),
                    request: ContentRequest::Whiteboard {
                        token: token.clone(),
                    },
                    bytes,
                });
                self.board_names.insert(token, name.clone());
                name
            };
            return Ok(format!("\n\n![Whiteboard]({ASSET_SCHEME}{name})\n\n"));
        }
        if tag == "cite" {
            if self.attr(node, "type").as_deref() == Some("doc") {
                let token = self
                    .attr(node, "doc-id")
                    .or_else(|| self.attr(node, "token"));
                let kind = self
                    .attr(node, "file-type")
                    .unwrap_or_else(|| "docx".into());
                let title = self
                    .attr(node, "title")
                    .or_else(|| self.attr(node, "name"))
                    .or_else(|| token.clone())
                    .unwrap_or_else(|| "Document".into());
                if let Some(href) = self.attr(node, "href").or_else(|| self.attr(node, "url")) {
                    return Ok(markdown_link(&escape_text(&title), &href));
                }
                if let Some(token) = token
                    && valid_token(&token)
                    && matches!(
                        kind.as_str(),
                        "wiki"
                            | "docx"
                            | "doc"
                            | "sheets"
                            | "sheet"
                            | "bitable"
                            | "base"
                            | "file"
                            | "slides"
                    )
                {
                    return Ok(markdown_link(
                        &escape_text(&title),
                        &format!("{}/{kind}/{token}", self.base_url.trim_end_matches('/')),
                    ));
                }
            }
            return Ok(self.unsupported(node));
        }
        if tag == "img" || tag == "source" {
            if let Some(url) = self.attr(node, "url").or_else(|| self.attr(node, "src"))
                && url.starts_with("https://")
            {
                let title = self
                    .attr(node, "alt")
                    .or_else(|| self.attr(node, "name"))
                    .unwrap_or_default();
                return Ok(format!(
                    "{}{}",
                    if tag == "img" { "!" } else { "" },
                    markdown_link(&escape_text(&title), &url)
                ));
            }
            return Ok(self.unsupported(node));
        }
        if tag == "table" {
            return self.table(node, depth);
        }
        if tag == "ul" || tag == "ol" {
            let start = node
                .attribute("start")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1);
            let mut output = String::from("\n\n");
            for (index, item) in node.children().filter(|n| n.has_tag_name("li")).enumerate() {
                let content = self.children(item, depth)?.trim().to_owned();
                let marker = if tag == "ul" {
                    "- ".into()
                } else {
                    format!("{}. ", start.saturating_add(index as u64))
                };
                output.push_str(&marker);
                output.push_str(&content.replace('\n', &format!("\n{}", " ".repeat(marker.len()))));
                output.push('\n');
            }
            output.push('\n');
            return Ok(output);
        }
        if !matches!(
            tag,
            "title"
                | "p"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "h7"
                | "h8"
                | "h9"
                | "b"
                | "strong"
                | "em"
                | "i"
                | "del"
                | "s"
                | "u"
                | "span"
                | "a"
                | "blockquote"
                | "callout"
                | "grid"
                | "column"
                | "fragment"
                | "br"
                | "hr"
                | "latex"
        ) {
            return Ok(self.unsupported(node));
        }
        let inner = self.children(node, depth)?;
        Ok(match tag {
            "title" => format!("# {}\n\n", inner.trim()),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "h7" | "h8" | "h9" => {
                let level = tag.as_bytes()[1].saturating_sub(b'0').min(6) as usize;
                format!("\n\n{} {}\n\n", "#".repeat(level), inner.trim())
            }
            "p" => format!("{}\n\n", inner.trim()),
            "b" | "strong" => format!("**{inner}**"),
            "em" | "i" => format!("*{inner}*"),
            "del" | "s" => format!("~~{inner}~~"),
            "a" => node
                .attribute("href")
                .map_or(inner.clone(), |href| markdown_link(&inner, href)),
            "blockquote" | "callout" => format!(
                "\n\n{}\n\n",
                inner
                    .trim()
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            "br" => "  \n".into(),
            "hr" => "\n\n---\n\n".into(),
            "latex" => format!("${}$", raw_text(node)),
            _ => inner,
        })
    }

    fn unsupported(&mut self, node: Node<'_, '_>) -> String {
        self.preserved_reference |= node.attribute("data-ref").is_some();
        format!(
            "\n\n> FeishuFS: unconverted `{}` block; original structure preserved.\n\n{}",
            node.tag_name().name(),
            fenced(&self.xml[node.range()], "xml")
        )
    }

    fn table(&mut self, node: Node<'_, '_>, depth: usize) -> Result<String> {
        if node.descendants().any(|n| {
            n.is_element()
                && (n.attribute("rowspan").is_some_and(|v| v != "1")
                    || n.attribute("colspan").is_some_and(|v| v != "1")
                    || (n != node && n.has_tag_name("table")))
        }) {
            return Ok(self.unsupported(node));
        }
        let mut rows = Vec::new();
        for row in node.descendants().filter(|n| n.has_tag_name("tr")) {
            let mut cells = Vec::new();
            for cell in row
                .children()
                .filter(|n| n.has_tag_name("td") || n.has_tag_name("th"))
            {
                let text = self.children(cell, depth + 1)?;
                cells.push(text.trim().replace('\n', "<br>"));
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        if width == 0 {
            return Ok(self.unsupported(node));
        }
        let mut output = String::from("\n\n");
        for (index, mut row) in rows.into_iter().enumerate() {
            row.resize(width, String::new());
            output.push_str(&format!("| {} |\n", row.join(" | ")));
            if index == 0 {
                output.push_str(&format!("| {} |\n", vec!["---"; width].join(" | ")));
            }
        }
        output.push('\n');
        Ok(output)
    }
}

fn raw_text(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect()
}

fn normalize_text(text: &str) -> String {
    // Remove outer Markdown escapes only inside an entity. Never decode twice.
    let mut output = String::new();
    let mut offset = 0;
    while offset < text.len() {
        let rest = &text[offset..];
        if (rest.starts_with('&') || rest.starts_with("\\&"))
            && let Some(end) = rest.find(';').filter(|end| *end <= 40)
        {
            let candidate = &rest[..=end];
            let plain = candidate.replace("\\&", "&").replace("\\#", "#");
            let decoded = html_escape::decode_html_entities(&plain);
            if decoded != plain {
                output.push_str(&decoded);
                offset += candidate.len();
                continue;
            }
        }
        let character = rest.chars().next().expect("nonempty text");
        output.push(character);
        offset += character.len_utf8();
    }
    output
}

fn escape_text(text: &str) -> String {
    let mut output = String::new();
    let mut line_start = true;
    for character in text.chars() {
        if line_start && matches!(character, '-' | '+') {
            output.push('\\');
        }
        if character == '\n' {
            line_start = true;
        } else if !character.is_whitespace() {
            line_start = false;
        }
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '~' | '!'
        ) {
            output.push('\\');
        }
        output.push(character);
    }
    output
}

fn markdown_link(title: &str, url: &str) -> String {
    // Targets are not passed through the text normalizer. Reject active schemes.
    if !(url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("mailto:")
        || matches!(
            url::Url::parse(url),
            Err(url::ParseError::RelativeUrlWithoutBase)
        ))
    {
        return title.to_owned();
    }
    let mut escaped = String::new();
    for character in url.chars() {
        if character.is_ascii_whitespace() || matches!(character, '<' | '>' | '\\') {
            escaped.push_str(&format!("%{:02X}", u32::from(character)));
        } else {
            escaped.push(character);
        }
    }
    format!("[{title}](<{escaped}>)")
}

fn fenced(text: &str, language: &str) -> String {
    let ticks = "`".repeat(longest_ticks(text).saturating_add(1).max(3));
    let language: String = language
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
        .collect();
    format!(
        "\n\n{ticks}{language}\n{}{ending}{ticks}\n\n",
        text,
        ending = if text.ends_with('\n') { "" } else { "\n" }
    )
}

fn inline_code(text: &str) -> String {
    let ticks = "`".repeat(longest_ticks(text).saturating_add(1).max(1));
    format!("{ticks} {text} {ticks}")
}

/// Collapse renderer separators, not source code. Generated fences always use
/// more backticks than any run in their contents, including nested containers.
fn compact_spacing(markdown: &str) -> String {
    let mut output = String::new();
    let mut fence = None;
    let mut blank = false;
    for line in markdown.split_inclusive('\n') {
        let mut candidate = line.trim_start();
        while let Some(rest) = candidate.strip_prefix('>') {
            candidate = rest.trim_start();
        }
        if let Some(rest) = candidate
            .strip_prefix("- ")
            .or_else(|| candidate.strip_prefix("+ "))
        {
            candidate = rest;
        } else if let Some((number, rest)) = candidate.split_once(". ")
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
        {
            candidate = rest;
        }
        let ticks = candidate.bytes().take_while(|b| *b == b'`').count();
        if let Some(length) = fence {
            output.push_str(line);
            if ticks >= length && candidate[ticks..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if line.trim().is_empty() {
            if !blank {
                output.push_str(line);
            }
            blank = true;
        } else {
            output.push_str(line);
            blank = false;
            if ticks >= 3 {
                fence = Some(ticks);
            }
        }
    }
    output
}

fn longest_ticks(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

pub(crate) fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 80
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn asset_token_name(token: &str) -> Result<String> {
    if !valid_token(token) {
        bail!("Invalid whiteboard token");
    }
    Ok(token.bytes().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn image_extension(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"\xff\xd8\xff") {
        Ok("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("png")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Ok("webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Ok("gif")
    } else {
        bail!("Whiteboard response is not a supported image; refusing to publish it as an image")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(xml: &str) -> ContentBundle {
        render_document(xml, &Value::Null, "https://example.feishu.cn", 4096, |_| {
            bail!("unexpected download")
        })
        .unwrap()
    }

    #[test]
    fn converts_unicode_structure_and_document_citations_without_dropping_quotes() {
        let bundle = render(
            r#"<title>首页</title><blockquote><p>说明</p></blockquote><h2><cite type="doc" file-type="wiki" doc-id="other" title="文档"/></h2><p>正文<b>粗体</b></p><ul><li>一</li><li>二</li></ul>"#,
        );
        let text = String::from_utf8(bundle.bytes).unwrap();
        assert!(text.starts_with("# 首页"));
        assert!(text.contains("> 说明"));
        assert!(text.contains("## [文档](<https://example.feishu.cn/wiki/other>)"));
        assert!(text.contains("正文**粗体**"));
        assert!(text.contains("- 一\n- 二"));
    }

    #[test]
    fn mermaid_source_has_priority_over_images_and_keeps_its_position() {
        let text = String::from_utf8(
            render(
                r#"<p>Before</p><whiteboard type="mermaid" token="board">flowchart LR
A["&lt;IP&gt;"] --&gt; B</whiteboard><p>After</p>"#,
            )
            .bytes,
        )
        .unwrap();
        assert!(text.contains("```mermaid\nflowchart LR\nA[\"<IP>\"] --> B\n```"));
        assert!(text.find("Before") < text.find("```mermaid"));
        assert!(text.find("```mermaid") < text.find("After"));
    }

    #[test]
    fn normalizes_entities_in_unicode_text_not_code_or_link_targets() {
        let bundle = render(
            r#"<p>\&amp;\#34; http://\&amp;lt;服务器IP\&amp;gt;:8020 &amp;#x22;</p><pre lang="text">\&amp;\#34; &amp;lt;IP&amp;gt;</pre><a href="https://example.com/?q=&amp;lt;">链接</a>"#,
        );
        let text = String::from_utf8(bundle.bytes).unwrap();
        assert!(text.contains("\" http://\\<服务器IP\\>:8020 \""));
        assert!(text.contains("```text\n\\&\\#34; &lt;IP&gt;\n```"));
        assert!(text.contains("https://example.com/?q=&lt;"));
    }

    #[test]
    fn downloads_blank_boards_once_and_detects_actual_extension() {
        let mut fetches = 0;
        let bundle = render_document(
            "<whiteboard token=\"b\"/><whiteboard token=\"b\"/>",
            &Value::Null,
            "https://example.feishu.cn",
            4096,
            |_| {
                fetches += 1;
                Ok(b"\xff\xd8\xffimage".to_vec())
            },
        )
        .unwrap();
        assert_eq!(fetches, 1);
        assert_eq!(bundle.assets[0].name, "62.jpg");
        assert_eq!(
            String::from_utf8(bundle.bytes)
                .unwrap()
                .matches("![Whiteboard](feishufs-asset:62.jpg)")
                .count(),
            2
        );
    }

    #[test]
    fn input_errors_are_propagated_without_a_recovery_policy() {
        assert!(
            render_document(
                "<p>before</p><whiteboard token=\"b\"/>",
                &Value::Null,
                "https://example.feishu.cn",
                4096,
                |_| bail!("denied")
            )
            .is_err()
        );
        assert!(
            render_document(
                "<p>broken",
                &Value::Null,
                "https://example.feishu.cn",
                4096,
                |_| Ok(vec![])
            )
            .is_err()
        );
        assert!(
            render_document(
                "<!DOCTYPE foo [<!ENTITY x 'danger'>]><p>&x;</p>",
                &Value::Null,
                "https://example.feishu.cn",
                4096,
                |_| Ok(vec![])
            )
            .is_err()
        );
        assert!(
            render_document(
                "<whiteboard token=\"b\"/>",
                &Value::Null,
                "https://example.feishu.cn",
                4,
                |_| Ok(b"\xff\xd8\xffimage".to_vec())
            )
            .is_err()
        );
    }

    #[test]
    fn preserves_unsupported_blocks_and_reference_data() {
        let bundle = render_document(
            "<synced_reference src-token=\"other\"/><html5-block data-ref=\"r\"/>",
            &serde_json::json!({"html5-block": {"r": {"data": "hello"}}}),
            "https://example.feishu.cn",
            4096,
            |_| bail!("unexpected"),
        )
        .unwrap();
        let text = String::from_utf8(bundle.bytes).unwrap();
        assert!(text.contains("```xml"));
        assert!(text.contains("src-token=\"other\""));
        assert!(text.contains("```json"));
        assert!(text.contains("hello"));
    }

    #[test]
    fn spacing_cleanup_keeps_code_blank_lines_and_adaptive_delimiters() {
        let text = String::from_utf8(
            render("<p>before</p><pre lang=\"text\">one\n\n\n```\n\n\ntwo</pre><p>after</p>").bytes,
        )
        .unwrap();
        assert!(text.contains("````text\none\n\n\n```\n\n\ntwo\n````"));
        assert!(!text.starts_with("\n"));
        assert!(text.ends_with("\n"));
        let text = String::from_utf8(render("<p>one</p><h2>two</h2><p>three</p>").bytes).unwrap();
        assert!(!text.contains("\n\n\n"));
        let text =
            String::from_utf8(render("<ul><li><pre>one\n\n\ntwo</pre></li></ul>").bytes).unwrap();
        assert!(text.contains("one\n  \n  \n  two"));
        assert_eq!(markdown_link("section", "#anchor"), "[section](<#anchor>)");
        assert_eq!(markdown_link("unsafe", "javascript:alert(1)"), "unsafe");
        assert_eq!(
            escape_text("- literal\n+ literal"),
            "\\- literal\n\\+ literal"
        );
    }

    #[test]
    fn renders_simple_tables_but_preserves_merged_cells() {
        let text = String::from_utf8(
            render("<table><tr><th>Title</th></tr><tr><td>Value | Data</td></tr></table>").bytes,
        )
        .unwrap();
        assert!(text.contains("| Title |\n| --- |\n| Value \\| Data |"));
        let text = String::from_utf8(
            render("<table><tr><td colspan=\"2\">Merged</td></tr></table>").bytes,
        )
        .unwrap();
        assert!(text.contains("colspan=\"2\""));
        assert!(text.contains("```xml"));
    }
}
