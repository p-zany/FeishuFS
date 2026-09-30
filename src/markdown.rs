use std::collections::HashMap;

use crate::link::KnowledgeLink;

/// Rewrite Feishu/Lark links that point into the mounted subtree to local,
/// relative filesystem paths. Formatting outside link destinations is retained.
pub(crate) fn rewrite_document_links(
    markdown: &str,
    current_file: &str,
    content_path_by_token: &HashMap<String, String>,
) -> String {
    let mut output = String::with_capacity(markdown.len());
    let mut fence = None;
    let mut code_ticks = None;

    for line in markdown.split_inclusive('\n') {
        if let Some((marker, length)) = fence {
            output.push_str(line);
            if is_closing_fence(line, marker, length) {
                fence = None;
            }
            continue;
        }

        if code_ticks.is_none()
            && let Some(opening) = opening_fence(line)
        {
            output.push_str(line);
            fence = Some(opening);
            continue;
        }
        if code_ticks.is_none() && is_indented_code(line) {
            output.push_str(line);
            continue;
        }

        output.push_str(&rewrite_line(
            line,
            current_file,
            content_path_by_token,
            &mut code_ticks,
        ));
    }

    output
}

fn rewrite_line(
    line: &str,
    current_file: &str,
    content_path_by_token: &HashMap<String, String>,
    code_ticks: &mut Option<usize>,
) -> String {
    let bytes = line.as_bytes();
    let reference_start = reference_destination_start(line);
    let mut output = String::with_capacity(line.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'`' && !is_escaped(bytes, index) {
            let length = marker_run(bytes, index, b'`');
            output.push_str(&line[index..index + length]);
            match *code_ticks {
                Some(opening) if opening == length => *code_ticks = None,
                None => *code_ticks = Some(length),
                _ => {}
            }
            index += length;
            continue;
        }

        if code_ticks.is_some() {
            let length = next_char_len(line, index);
            output.push_str(&line[index..index + length]);
            index += length;
            continue;
        }

        if bytes[index] == b']' && bytes.get(index + 1) == Some(&b'(') && !is_escaped(bytes, index)
        {
            output.push_str("](");
            index += 2;
            while bytes
                .get(index)
                .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            {
                output.push(char::from(bytes[index]));
                index += 1;
            }
            index = rewrite_destination(
                line,
                index,
                DestinationKind::Inline,
                current_file,
                content_path_by_token,
                &mut output,
            );
            continue;
        }

        if reference_start == Some(index) {
            index = rewrite_destination(
                line,
                index,
                DestinationKind::Reference,
                current_file,
                content_path_by_token,
                &mut output,
            );
            continue;
        }

        if bytes[index] == b'<'
            && !is_escaped(bytes, index)
            && let Some(end) = line[index + 1..].find('>')
        {
            let end = index + 1 + end;
            let destination = &line[index + 1..end];
            if !destination.bytes().any(|byte| byte.is_ascii_whitespace())
                && let Some(local) =
                    local_destination(destination, current_file, content_path_by_token)
            {
                output.push('[');
                output.push_str(&destination.replace(']', "\\]"));
                output.push_str("](<");
                output.push_str(&local);
                output.push_str(">)");
                index = end + 1;
                continue;
            }
        }

        let length = next_char_len(line, index);
        output.push_str(&line[index..index + length]);
        index += length;
    }

    output
}

#[derive(Clone, Copy)]
enum DestinationKind {
    Inline,
    Reference,
}

fn rewrite_destination(
    line: &str,
    start: usize,
    kind: DestinationKind,
    current_file: &str,
    content_path_by_token: &HashMap<String, String>,
    output: &mut String,
) -> usize {
    let Some((end, destination)) = destination_at(line, start, kind) else {
        if start >= line.len() {
            return start;
        }
        let length = next_char_len(line, start);
        output.push_str(&line[start..start + length]);
        return start + length;
    };
    if let Some(local) = local_destination(destination, current_file, content_path_by_token) {
        output.push('<');
        output.push_str(&local);
        output.push('>');
    } else {
        output.push_str(&line[start..end]);
    }
    end
}

fn destination_at(line: &str, start: usize, kind: DestinationKind) -> Option<(usize, &str)> {
    let bytes = line.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    if bytes[start] == b'<' {
        let relative_end = line[start + 1..].find('>')?;
        let end = start + 1 + relative_end;
        return Some((end + 1, &line[start + 1..end]));
    }

    let mut index = start;
    let mut nested_parentheses = 0_u32;
    while let Some(&byte) = bytes.get(index) {
        if byte.is_ascii_whitespace() {
            break;
        }
        if matches!(kind, DestinationKind::Inline) {
            match byte {
                b'(' if !is_escaped(bytes, index) => {
                    nested_parentheses = nested_parentheses.saturating_add(1);
                }
                b')' if !is_escaped(bytes, index) => {
                    if nested_parentheses == 0 {
                        break;
                    }
                    nested_parentheses -= 1;
                }
                _ => {}
            }
        }
        index += next_char_len(line, index);
    }
    (index > start).then_some((index, &line[start..index]))
}

fn local_destination(
    destination: &str,
    current_file: &str,
    content_path_by_token: &HashMap<String, String>,
) -> Option<String> {
    let link = destination.parse::<KnowledgeLink>().ok()?;
    let target = content_path_by_token.get(&link.token)?;
    let mut relative = relative_file_path(current_file, target);
    relative = relative.replace('%', "%25").replace('#', "%23");
    if let Some(fragment) = link.url.fragment().filter(|fragment| !fragment.is_empty()) {
        relative.push('#');
        relative.push_str(fragment);
    }
    Some(relative)
}

fn relative_file_path(current_file: &str, target_file: &str) -> String {
    let mut current: Vec<_> = current_file.split('/').collect();
    current.pop();
    let target: Vec<_> = target_file.split('/').collect();
    let common = current
        .iter()
        .zip(&target)
        .take_while(|(left, right)| left == right)
        .count();

    let mut parts = vec![".."; current.len().saturating_sub(common)];
    parts.extend_from_slice(&target[common..]);
    parts.join("/")
}

fn reference_destination_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let leading = bytes.iter().take_while(|byte| **byte == b' ').count();
    if leading > 3 || bytes.get(leading) != Some(&b'[') {
        return None;
    }

    let mut index = leading + 1;
    while index + 1 < bytes.len() {
        if bytes[index] == b']' && bytes[index + 1] == b':' && !is_escaped(bytes, index) {
            index += 2;
            while bytes
                .get(index)
                .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
            {
                index += 1;
            }
            return Some(index);
        }
        index += next_char_len(line, index);
    }
    None
}

fn opening_fence(line: &str) -> Option<(u8, usize)> {
    let bytes = line.as_bytes();
    let leading = bytes.iter().take_while(|byte| **byte == b' ').count();
    if leading > 3 {
        return None;
    }
    let marker = *bytes.get(leading)?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = marker_run(bytes, leading, marker);
    (length >= 3).then_some((marker, length))
}

fn is_closing_fence(line: &str, marker: u8, opening_length: usize) -> bool {
    let bytes = line.as_bytes();
    let leading = bytes.iter().take_while(|byte| **byte == b' ').count();
    if leading > 3 || bytes.get(leading) != Some(&marker) {
        return false;
    }
    let length = marker_run(bytes, leading, marker);
    length >= opening_length
        && bytes[leading + length..]
            .iter()
            .all(|byte| byte.is_ascii_whitespace())
}

fn is_indented_code(line: &str) -> bool {
    line.starts_with("    ") || line.starts_with('\t')
}

fn marker_run(bytes: &[u8], start: usize, marker: u8) -> usize {
    bytes[start..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count()
}

fn is_escaped(bytes: &[u8], index: usize) -> bool {
    let preceding_backslashes = bytes[..index]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count();
    preceding_backslashes % 2 == 1
}

fn next_char_len(value: &str, index: usize) -> usize {
    value[index..].chars().next().map_or(1, char::len_utf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> HashMap<String, String> {
        HashMap::from([
            ("root".into(), "index.md".into()),
            ("guide".into(), "指南 A/index.md".into()),
            ("api".into(), "指南 A/API #1/index.md".into()),
            ("file".into(), "附件/index.pdf".into()),
        ])
    }

    #[test]
    fn rewrites_inline_links_relative_to_the_current_document() {
        let markdown = concat!(
            "[root](https://acme.feishu.cn/wiki/root)\n",
            "[guide](https://acme.feishu.cn/docx/guide?from=copy#heading)\n",
            "![file](https://acme.feishu.cn/file/file \"download\")\n",
        );
        assert_eq!(
            rewrite_document_links(markdown, "指南 A/API #1/index.md", &targets()),
            concat!(
                "[root](<../../index.md>)\n",
                "[guide](<../index.md#heading>)\n",
                "![file](<../../附件/index.pdf> \"download\")\n",
            )
        );
    }

    #[test]
    fn escapes_hashes_in_local_names() {
        let markdown = "[api](https://acme.feishu.cn/wiki/api)\n";
        assert_eq!(
            rewrite_document_links(markdown, "index.md", &targets()),
            "[api](<指南 A/API %231/index.md>)\n"
        );
    }

    #[test]
    fn rewrites_reference_definitions_and_autolinks() {
        let markdown = concat!(
            "Read [the guide][guide].\n",
            "[guide]: https://acme.larksuite.com/wiki/guide \"Guide\"\n",
            "<https://acme.larksuite.com/wiki/root>\n",
        );
        assert_eq!(
            rewrite_document_links(markdown, "index.md", &targets()),
            concat!(
                "Read [the guide][guide].\n",
                "[guide]: <指南 A/index.md> \"Guide\"\n",
                "[https://acme.larksuite.com/wiki/root](<index.md>)\n",
            )
        );
    }

    #[test]
    fn leaves_external_missing_and_code_links_unchanged() {
        let markdown = concat!(
            "[external](https://example.com/wiki/root)\n",
            "[missing](https://acme.feishu.cn/wiki/missing)\n",
            "`[inline](https://acme.feishu.cn/wiki/root)`\n",
            "```markdown\n",
            "[fenced](https://acme.feishu.cn/wiki/root)\n",
            "```\n",
            "    [indented](https://acme.feishu.cn/wiki/root)\n",
        );
        assert_eq!(
            rewrite_document_links(markdown, "index.md", &targets()),
            markdown
        );
    }
}
