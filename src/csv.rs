use std::{borrow::Cow, collections::HashMap};

use crate::markdown::{DocumentTarget, local_document_destination};

/// Rewrite whole-cell document URLs without reserializing the exported CSV.
/// Unsupported encodings or malformed quoting retain all original bytes.
pub(crate) fn rewrite_document_links(
    source: &[u8],
    current_file: &str,
    documents: &HashMap<String, DocumentTarget>,
) -> Vec<u8> {
    let Ok(csv) = std::str::from_utf8(source) else {
        return source.to_vec();
    };
    rewrite_csv(csv, current_file, documents)
        .map(String::into_bytes)
        .unwrap_or_else(|| source.to_vec())
}

fn rewrite_csv(
    csv: &str,
    current_file: &str,
    documents: &HashMap<String, DocumentTarget>,
) -> Option<String> {
    let mut index = if csv.starts_with('\u{feff}') { 3 } else { 0 };
    let mut output = String::with_capacity(csv.len());
    output.push_str(&csv[..index]);
    while index < csv.len() {
        let (end, quoted) = field_end(csv.as_bytes(), index)?;
        let raw = &csv[index..end];
        let local = rewrite_field(raw, quoted, current_file, documents);
        output.push_str(local.as_deref().unwrap_or(raw));
        index = end;
        if index < csv.len() {
            // Copy commas and line endings literally, including mixed CRLF/LF.
            output.push(char::from(csv.as_bytes()[index]));
            index += 1;
        }
    }
    Some(output)
}

fn field_end(bytes: &[u8], start: usize) -> Option<(usize, bool)> {
    let quoted = bytes[start] == b'"';
    let mut index = start + usize::from(quoted);
    while let Some(&byte) = bytes.get(index) {
        if quoted && byte == b'"' {
            if bytes.get(index + 1) == Some(&b'"') {
                index += 2;
                continue;
            }
            index += 1;
            return (index == bytes.len() || is_separator(bytes[index])).then_some((index, true));
        }
        if !quoted {
            if is_separator(byte) {
                return Some((index, false));
            }
            if byte == b'"' {
                return None;
            }
        }
        index += 1;
    }
    (!quoted).then_some((index, false))
}

fn is_separator(byte: u8) -> bool {
    matches!(byte, b',' | b'\r' | b'\n')
}

fn rewrite_field(
    raw: &str,
    quoted: bool,
    current_file: &str,
    documents: &HashMap<String, DocumentTarget>,
) -> Option<String> {
    if !raw.contains("://") {
        return None;
    }
    let value = if quoted {
        Cow::Owned(raw[1..raw.len() - 1].replace("\"\"", "\""))
    } else {
        Cow::Borrowed(raw)
    };
    let destination = value.trim();
    if destination.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let mut local = local_document_destination(destination, current_file, documents)?;
    if local
        .bytes()
        .next()
        .is_some_and(|byte| matches!(byte, b'=' | b'+' | b'-' | b'@'))
    {
        // A local filename must not turn a URL cell into a spreadsheet formula.
        local.insert_str(0, "./");
    }
    let start = value.len() - value.trim_start().len();
    let end = value.trim_end().len();
    let replacement = format!("{}{local}{}", &value[..start], &value[end..]);
    if quoted
        || replacement
            .bytes()
            .any(|byte| is_separator(byte) || byte == b'"')
    {
        Some(format!("\"{}\"", replacement.replace('"', "\"\"")))
    } else {
        Some(replacement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> HashMap<String, DocumentTarget> {
        let attachment = DocumentTarget {
            path: ".feishufs-assets/12/论文,报告 100% #1.pdf".into(),
            subtables: HashMap::new(),
        };
        HashMap::from([
            ("file".into(), attachment.clone()),
            ("obj-file".into(), attachment),
            (
                "notes".into(),
                DocumentTarget {
                    path: "Notes/index.md".into(),
                    subtables: HashMap::new(),
                },
            ),
            (
                "table".into(),
                DocumentTarget {
                    path: "Report.First.csv".into(),
                    subtables: HashMap::from([
                        ("a".into(), "Report.First.csv".into()),
                        ("b".into(), "Report.Second.csv".into()),
                    ]),
                },
            ),
        ])
    }

    fn rewrite(csv: &str) -> String {
        String::from_utf8(rewrite_document_links(
            csv.as_bytes(),
            "index.csv",
            &targets(),
        ))
        .unwrap()
    }

    #[test]
    fn rewrites_pdf_and_notes_cells_preserving_the_rest_of_the_export() {
        let csv = concat!(
            "\u{feff}name,PDF,notes,summary\r\n",
            "\"论文,一\",https://acme.feishu.cn/file/obj-file?from=copy#page=2,",
            "\"https://acme.feishu.cn/wiki/notes\",\"逗号,引号\"\"与\n换行\"\r\n",
        );
        assert_eq!(
            rewrite(csv),
            concat!(
                "\u{feff}name,PDF,notes,summary\r\n",
                "\"论文,一\",\".feishufs-assets/12/论文,报告 100%25 %231.pdf#page=2\",",
                "\"Notes/index.md\",\"逗号,引号\"\"与\n换行\"\r\n",
            )
        );
    }

    #[test]
    fn quoted_urls_and_surrounding_whitespace_stay_quoted_and_spaced() {
        assert_eq!(
            rewrite("\"  https://acme.larksuite.com/wiki/notes?from=\"\"copy\"\"  \"\r\n"),
            "\"  Notes/index.md  \"\r\n",
        );
        assert_eq!(
            rewrite(" https://acme.feishu.cn/wiki/notes \n"),
            " Notes/index.md \n"
        );
    }

    #[test]
    fn only_whole_cell_known_official_urls_are_rewritten() {
        let csv = concat!(
            "https://arxiv.org/abs/2604.15395,https://acme.feishu.cn/file/missing\n",
            "https://feishu.cn.example.com/wiki/notes,\"see https://acme.feishu.cn/wiki/notes\"\n",
            "\"[Notes](https://acme.feishu.cn/wiki/notes)\",feishufs-asset:board.png\n",
            "\"https://acme.feishu.cn/wi\nki/notes\",https://acme.feishu.cn/base/table?table=unknown",
        );
        assert_eq!(rewrite(csv), csv);
    }

    #[test]
    fn subtable_selectors_and_relative_directories_match_markdown_links() {
        let csv = concat!(
            "https://acme.feishu.cn/wiki/file,",
            "https://acme.feishu.cn/base/table?table=b&view=v#cell,",
            "https://acme.feishu.cn/wiki/table?sheet=a,",
            "https://acme.feishu.cn/wiki/table",
        );
        let rendered = rewrite_document_links(csv.as_bytes(), "Folder/index.csv", &targets());
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            concat!(
                "\"../.feishufs-assets/12/论文,报告 100%25 %231.pdf\",",
                "../Report.Second.csv#cell,../Report.First.csv,../Report.First.csv",
            )
        );
    }

    #[test]
    fn line_endings_empty_cells_and_trailing_delimiters_are_not_normalized() {
        let csv = concat!(
            "\u{feff}\"PDF\",,\r\n",
            "https://acme.feishu.cn/wiki/notes,,\n",
            ",\"\",\rhttps://acme.feishu.cn/wiki/notes,",
        );
        assert_eq!(
            rewrite(csv),
            concat!(
                "\u{feff}\"PDF\",,\r\n",
                "Notes/index.md,,\n",
                ",\"\",\rNotes/index.md,",
            )
        );
        assert_eq!(rewrite(""), "");
        assert_eq!(rewrite("\u{feff}"), "\u{feff}");
    }

    #[test]
    fn malformed_csv_retains_the_complete_source_not_a_partial_rewrite() {
        for suffix in ["\"unterminated", "\"closed\"junk", "unquoted\"quote"] {
            let csv = format!("https://acme.feishu.cn/wiki/notes,{suffix}");
            assert_eq!(rewrite(&csv), csv);
        }
    }

    #[test]
    fn formula_like_local_names_remain_relative_paths_not_spreadsheet_formulas() {
        for name in ["=notes.md", "+notes.md", "-notes.md", "@notes.md"] {
            let documents = HashMap::from([(
                "notes".into(),
                DocumentTarget {
                    path: name.into(),
                    subtables: HashMap::new(),
                },
            )]);
            for csv in [
                "https://acme.feishu.cn/wiki/notes",
                "\"https://acme.feishu.cn/wiki/notes\"",
            ] {
                let expected = if csv.starts_with('"') {
                    format!("\"./{name}\"")
                } else {
                    format!("./{name}")
                };
                assert_eq!(
                    rewrite_document_links(csv.as_bytes(), "index.csv", &documents),
                    expected.as_bytes()
                );
            }
        }
    }

    #[test]
    fn non_utf8_exports_remain_byte_identical() {
        let csv = b"PDF,\xff\nhttps://acme.feishu.cn/wiki/notes\n";
        assert_eq!(rewrite_document_links(csv, "index.csv", &targets()), csv);
        let utf16 = b"\xff\xfeP\0D\0F\0";
        assert_eq!(
            rewrite_document_links(utf16, "index.csv", &targets()),
            utf16
        );
    }
}
