use std::str::FromStr;

use thiserror::Error;
use url::Url;

/// A Feishu/Lark document URL resolved into the token expected by Wiki v2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnowledgeLink {
    pub url: Url,
    pub token: String,
    pub object_type: Option<String>,
}

impl KnowledgeLink {
    #[must_use]
    pub fn inferred_api_base_url(&self) -> &'static str {
        match self.url.host_str() {
            Some(host)
                if host == "larksuite.com"
                    || host.ends_with(".larksuite.com")
                    || host == "larkoffice.com"
                    || host.ends_with(".larkoffice.com") =>
            {
                "https://open.larksuite.com"
            }
            _ => "https://open.feishu.cn",
        }
    }
}

impl FromStr for KnowledgeLink {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(value).map_err(LinkError::InvalidUrl)?;
        if url.scheme() != "https" && url.scheme() != "http" {
            return Err(LinkError::UnsupportedScheme(url.scheme().to_owned()));
        }
        let host = url.host_str().ok_or(LinkError::MissingHost)?;
        if !is_supported_host(host) {
            return Err(LinkError::UnsupportedHost(host.to_owned()));
        }

        let segments: Vec<_> = url
            .path_segments()
            .ok_or(LinkError::MissingToken)?
            .filter(|segment| !segment.is_empty())
            .collect();
        let (kind, token) = segments
            .windows(2)
            .find_map(|pair| object_type_for_path(pair[0]).map(|kind| (kind, pair[1])))
            .ok_or_else(|| LinkError::UnsupportedPath(url.path().to_owned()))?;

        if token.is_empty()
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(LinkError::InvalidToken(token.to_owned()));
        }

        let token = token.to_owned();
        let object_type = kind.map(str::to_owned);
        Ok(Self {
            url,
            token,
            object_type,
        })
    }
}

fn is_supported_host(host: &str) -> bool {
    ["feishu.cn", "larksuite.com", "larkoffice.com"]
        .iter()
        .any(|domain| {
            host == *domain
                || host
                    .strip_suffix(domain)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}

fn object_type_for_path(segment: &str) -> Option<Option<&'static str>> {
    match segment.to_ascii_lowercase().as_str() {
        "wiki" => Some(None),
        "docx" => Some(Some("docx")),
        "docs" | "doc" => Some(Some("doc")),
        "sheets" | "sheet" => Some(Some("sheet")),
        "base" | "bitable" => Some(Some("bitable")),
        "file" => Some(Some("file")),
        "mindnotes" | "mindnote" => Some(Some("mindnote")),
        "slides" => Some(Some("slides")),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum LinkError {
    #[error("链接不是合法 URL: {0}")]
    InvalidUrl(#[source] url::ParseError),
    #[error("仅支持 http/https 链接，收到协议 {0}")]
    UnsupportedScheme(String),
    #[error("链接缺少域名")]
    MissingHost,
    #[error("不支持的域名 {0}；仅支持 feishu.cn、larksuite.com 和 larkoffice.com")]
    UnsupportedHost(String),
    #[error("链接缺少文档 token")]
    MissingToken,
    #[error(
        "不支持的飞书链接路径 {0}；支持 /wiki、/docx、/docs、/sheets、/base、/file、/mindnotes、/slides"
    )]
    UnsupportedPath(String),
    #[error("文档 token 含有不支持的字符: {0}")]
    InvalidToken(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wiki_link_and_ignores_query() {
        let link: KnowledgeLink = "https://acme.feishu.cn/wiki/wikcnAbc_123?from=space"
            .parse()
            .unwrap();
        assert_eq!(link.token, "wikcnAbc_123");
        assert_eq!(link.object_type, None);
        assert_eq!(link.inferred_api_base_url(), "https://open.feishu.cn");
    }

    #[test]
    fn parses_lark_doc_link() {
        let link: KnowledgeLink = "https://acme.larksuite.com/docx/doxcn-123".parse().unwrap();
        assert_eq!(link.token, "doxcn-123");
        assert_eq!(link.object_type.as_deref(), Some("docx"));
        assert_eq!(link.inferred_api_base_url(), "https://open.larksuite.com");
    }

    #[test]
    fn rejects_impostor_hosts() {
        for url in [
            "https://example.com/wiki/token",
            "https://feishu.cn.example.com/wiki/token",
        ] {
            let error = url.parse::<KnowledgeLink>().unwrap_err();
            assert!(matches!(error, LinkError::UnsupportedHost(_)));
        }
    }

    #[test]
    fn rejects_unrecognised_paths() {
        let error = "https://acme.feishu.cn/calendar/123"
            .parse::<KnowledgeLink>()
            .unwrap_err();
        assert!(matches!(error, LinkError::UnsupportedPath(_)));
    }

    #[test]
    fn rejects_opaque_token_characters() {
        let error = "https://acme.feishu.cn/wiki/abc%2Fdef"
            .parse::<KnowledgeLink>()
            .unwrap_err();
        assert!(matches!(error, LinkError::InvalidToken(_)));
    }
}
