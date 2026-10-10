use std::{sync::Arc, thread, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use openlark_auth::AuthTokenProvider;
use openlark_core::{
    api::{ApiRequest, ApiResponseTrait, ResponseFormat},
    config::Config,
    http::Transport,
    req_option::RequestOption,
};
use openlark_docs::{
    ccm::{
        drive::v1::{
            CreateExportTaskRequest, DownloadExportRequest, GetExportTaskRequest,
            file::DownloadFileRequest,
        },
        wiki::v2::space::{
            get::GetWikiSpaceRequest,
            get_node::{GetWikiSpaceNodeParams, GetWikiSpaceNodeRequest},
        },
    },
    common::api_endpoints::WikiApiV2,
};
use serde::{Deserialize, Serialize};
use tokio::runtime::{Builder, Runtime};

use crate::{
    docx::{image_extension, render_document, valid_token},
    link::KnowledgeLink,
    remote::{
        ContentBundle, ContentRequest, RemoteNode, RemoteSource, RemoteSpace, SPACE_OBJECT_TYPE,
    },
    sdk_error::SdkError,
};

const EXPORT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(45);
const PAGE_SIZE: i32 = 50;

#[derive(Clone, Debug)]
pub struct OpenLarkOptions {
    pub app_id: String,
    pub app_secret: String,
    pub user_access_token: Option<String>,
    pub api_base_url: String,
    pub allow_custom_api_base_url: bool,
    pub max_download_bytes: usize,
    pub request_timeout: Duration,
}

pub struct OpenLarkSource {
    config: Config,
    runtime: Arc<Runtime>,
    user_access_token: Option<String>,
    max_download_bytes: usize,
}

impl OpenLarkSource {
    pub fn new(options: OpenLarkOptions) -> Result<Self> {
        if options.max_download_bytes == 0 {
            bail!("The per-file download limit must be greater than zero");
        }
        let max_response_size = u64::try_from(options.max_download_bytes)
            .context("The per-file download limit exceeds the u64 range")?;
        let base_config = Config::builder()
            .app_id(options.app_id)
            .app_secret(options.app_secret)
            .base_url(options.api_base_url)
            .allow_custom_base_url(options.allow_custom_api_base_url)
            .req_timeout(options.request_timeout)
            .max_response_size(max_response_size)
            .retry_count(3)
            .enable_token_cache(true)
            .build();
        base_config
            .validate()
            .map_err(SdkError)
            .context("Invalid OpenLark configuration")?;
        let provider = AuthTokenProvider::new(base_config.clone());
        let config = base_config.with_token_provider(provider);
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .thread_name("feishufs-api")
            .build()
            .context("Failed to create the OpenLark Tokio runtime")?;

        Ok(Self {
            config,
            runtime: Arc::new(runtime),
            user_access_token: options.user_access_token,
            max_download_bytes: options.max_download_bytes,
        })
    }

    fn request_option(&self) -> RequestOption {
        match &self.user_access_token {
            Some(token) => RequestOption::builder().user_access_token(token).build(),
            None => RequestOption::default(),
        }
    }

    /// List one page of children.
    ///
    /// This bypasses `ListWikiSpaceNodesRequest` only to read `has_child`, which
    /// that model drops even though the API returns it. Knowing it lets the tree
    /// walk skip one request per childless document, which matters when a whole
    /// space (100 requests/minute) has to be re-read every minute.
    fn list_children_page(
        &self,
        space_id: &str,
        parent_node_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<WikiNodePage> {
        let mut request: ApiRequest<WikiNodePage> =
            WikiApiV2::SpaceNodeList(space_id.to_owned()).to_request();
        if let Some(parent_node_token) = parent_node_token {
            request = request.query("parent_node_token", parent_node_token);
        }
        request = request.query("page_size", PAGE_SIZE.to_string());
        if let Some(page_token) = page_token {
            request = request.query("page_token", page_token);
        }
        self.runtime
            .block_on(Transport::request_typed(
                request,
                &self.config,
                Some(self.request_option()),
                "List space child nodes",
            ))
            .map_err(SdkError)
            .with_context(|| {
                format!(
                    "Wiki nodes request failed for space {space_id}, parent {}",
                    parent_node_token.unwrap_or("(space root)")
                )
            })
    }

    fn fetch_document(&self, token: &str, url: Option<&str>) -> Result<ContentBundle> {
        if !valid_token(token) {
            bail!("Invalid document token");
        }
        let request = ApiRequest::<DocsAiResponse>::post(format!(
            "/open-apis/docs_ai/v1/documents/{token}/fetch"
        ))
        .json_body(&DocsAiRequest {
            format: "xml",
            extra_param: r#"{"enable_user_cite_reference_map":true,"return_html5_block_data":true}"#,
            export_option: DocsAiExportOptions {
                export_block_id: false,
                export_style_attrs: false,
                export_cite_extra_data: false,
            },
        });
        let response: DocsAiResponse = self
            .runtime
            .block_on(Transport::request_typed(
                request,
                &self.config,
                Some(self.request_option()),
                "Fetch the Docs AI document structure",
            ))
            .map_err(SdkError)
            .with_context(|| {
                format!("Failed to fetch XML for document {token}; check document read permissions")
            })?;
        let origin = url
            .and_then(|url| url.parse::<KnowledgeLink>().ok())
            .map(|link| link.url.origin().ascii_serialization())
            .unwrap_or_else(|| {
                if self.config.base_url().contains("larksuite") {
                    "https://larkoffice.com".into()
                } else {
                    "https://feishu.cn".into()
                }
            });
        render_document(
            &response.document.content,
            &response.document.reference_map,
            &origin,
            self.max_download_bytes,
            |token| self.download_whiteboard(token),
        )
    }

    fn download_whiteboard(&self, token: &str) -> Result<Vec<u8>> {
        if !valid_token(token) {
            bail!("Invalid whiteboard token");
        }
        let request = ApiRequest::<Vec<u8>>::get(format!(
            "/open-apis/board/v1/whiteboards/{token}/download_as_image"
        ));
        let response = self
            .runtime
            .block_on(Transport::request(
                request,
                &self.config,
                Some(self.request_option()),
            ))
            .map_err(SdkError)
            .with_context(|| {
                format!("Failed to download whiteboard {token}; check board:whiteboard:node:read permissions")
            })?;
        // Raw downloads may carry an API error instead of bytes. Do not hide it
        // behind `data: None`, nor publish its JSON as a picture.
        if response.raw_response.code != 0 {
            bail!(
                "Whiteboard download API error {}: {}",
                response.raw_response.code,
                response.raw_response.msg
            );
        }
        let bytes: Vec<u8> = response
            .data
            .context("Whiteboard download succeeded but returned no data")?;
        if bytes.len() > self.max_download_bytes {
            bail!(
                "Whiteboard image exceeds the {} byte limit",
                self.max_download_bytes
            );
        }
        image_extension(&bytes)?;
        Ok(bytes)
    }

    fn export(&self, token: &str, document_type: &str, extension: &str) -> Result<Vec<u8>> {
        let created = self
            .runtime
            .block_on(
                CreateExportTaskRequest::new(self.config.clone(), extension, token, document_type)
                    .execute_with_options(self.request_option()),
            )
            .map_err(SdkError)
            .with_context(|| {
                format!("Failed to create a {extension} export task for {document_type} document {token}")
            })?;

        let deadline = std::time::Instant::now() + EXPORT_TIMEOUT;
        let mut last_status = None;
        let mut last_message = None;
        loop {
            let response = self
                .runtime
                .block_on(
                    GetExportTaskRequest::new(self.config.clone(), &created.ticket, token)
                        .execute_with_options(self.request_option()),
                )
                .map_err(SdkError)
                .with_context(|| format!("Failed to query export task {}", created.ticket))?;

            if let Some(result) = response.result {
                last_status = result.job_status;
                last_message = result.job_error_msg.clone();
                if result.job_status == Some(0) {
                    let file_token = result.file_token.with_context(|| {
                        format!(
                            "Export task {} completed without a file_token",
                            created.ticket
                        )
                    })?;
                    let response = self
                        .runtime
                        .block_on(
                            DownloadExportRequest::new(self.config.clone(), file_token)
                                .max_size(self.max_download_bytes)
                                .execute_with_options(self.request_option()),
                        )
                        .map_err(SdkError)
                        .with_context(|| {
                            format!(
                                "Failed to download the output of export task {}",
                                created.ticket
                            )
                        })?;
                    return response.data.with_context(|| {
                        format!("Export task {} returned no data", created.ticket)
                    });
                }
            }

            if std::time::Instant::now() >= deadline {
                bail!(
                    "Export task {} did not complete within {} (status {:?}, message {:?})",
                    created.ticket,
                    humantime::format_duration(EXPORT_TIMEOUT),
                    last_status,
                    last_message
                );
            }
            thread::sleep(EXPORT_POLL_INTERVAL);
        }
    }
}

impl RemoteSource for OpenLarkSource {
    fn resolve_space(&self, link: &KnowledgeLink) -> Result<RemoteSpace> {
        let response = self
            .runtime
            .block_on(
                GetWikiSpaceNodeRequest::new(self.config.clone()).execute_with_options(
                    GetWikiSpaceNodeParams {
                        token: link.token.clone(),
                        obj_type: link.object_type.clone(),
                    },
                    self.request_option(),
                ),
            )
            .map_err(SdkError)
            .context(
                "Wiki get_node request failed; verify the document belongs to a wiki and the application can read it",
            )?;
        let node = response
            .node
            .context("Feishu returned success without data.node")?;
        if node.space_id.trim().is_empty() {
            bail!("The node response is missing space_id; the space cannot be selected");
        }

        Ok(RemoteSpace {
            name: format!("Space {}", node.space_id),
            space_id: node.space_id,
        })
    }

    fn refresh_space(&self, space: &RemoteSpace) -> Result<RemoteSpace> {
        let response = self.runtime.block_on(
            GetWikiSpaceRequest::new(self.config.clone())
                .space_id(space.space_id.clone())
                .execute_with_options(self.request_option()),
        );
        match response {
            Ok(response) => Ok(RemoteSpace {
                space_id: space.space_id.clone(),
                name: response
                    .space
                    .map(|space| space.name)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| space.name.clone()),
            }),
            Err(error) => {
                crate::logging::failure(
                    "Failed to refresh space metadata; retaining the previous title",
                    &anyhow!(SdkError(error)),
                );
                Ok(space.clone())
            }
        }
    }

    fn list_children(&self, space_id: &str, parent: &RemoteNode) -> Result<Vec<RemoteNode>> {
        if !parent.has_child {
            return Ok(Vec::new());
        }
        let parent_node_token = (parent.object_type.as_deref() != Some(SPACE_OBJECT_TYPE))
            .then_some(parent.node_token.as_str());
        let mut page_token = None;
        let mut children = Vec::new();
        loop {
            let page =
                self.list_children_page(space_id, parent_node_token, page_token.as_deref())?;
            children.extend(page.items.into_iter().map(remote_node));

            if page.has_more != Some(true) {
                break;
            }
            let next = page.page_token.context(
                "Wiki nodes response has_more=true but has no page_token; pagination stopped",
            )?;
            if page_token.as_deref() == Some(next.as_str()) {
                bail!("Wiki nodes pagination token did not advance; pagination stopped");
            }
            page_token = Some(next);
        }
        Ok(children)
    }

    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
        match request {
            ContentRequest::Markdown { .. } => {
                self.fetch_bundle(request).map(|bundle| bundle.bytes)
            }
            ContentRequest::Whiteboard { token } => self.download_whiteboard(token),
            ContentRequest::Download { token } => {
                let response = self
                    .runtime
                    .block_on(
                        DownloadFileRequest::new(self.config.clone(), token)
                            .max_size(self.max_download_bytes)
                            .execute_with_options(self.request_option()),
                    )
                    .map_err(SdkError)
                    .with_context(|| format!("Failed to download attachment {token}"))?;
                response
                    .data
                    .context("Attachment download returned no data")
            }
            ContentRequest::Export {
                token,
                document_type,
                extension,
            } => self.export(token, document_type, extension),
            ContentRequest::Inline { bytes } => Ok(bytes.clone()),
        }
    }

    fn fetch_bundle(&self, request: &ContentRequest) -> Result<ContentBundle> {
        match request {
            ContentRequest::Markdown {
                token,
                document_type,
                url,
            } => {
                if document_type != "docx" {
                    bail!("Docs AI content fetching currently supports only docx");
                }
                self.fetch_document(token, url.as_deref())
            }
            _ => self.fetch_content(request).map(ContentBundle::from),
        }
    }
}

#[derive(Serialize)]
struct DocsAiRequest<'a> {
    format: &'a str,
    extra_param: &'a str,
    export_option: DocsAiExportOptions,
}

#[derive(Serialize)]
struct DocsAiExportOptions {
    export_block_id: bool,
    export_style_attrs: bool,
    export_cite_extra_data: bool,
}

#[derive(Debug, Deserialize)]
struct DocsAiResponse {
    document: DocsAiDocument,
}

#[derive(Debug, Deserialize)]
struct DocsAiDocument {
    content: String,
    #[serde(default)]
    reference_map: serde_json::Value,
}

impl ApiResponseTrait for DocsAiResponse {
    fn data_format() -> ResponseFormat {
        ResponseFormat::Data
    }
}

/// One page of the space child-node list.
#[derive(Debug, Deserialize)]
struct WikiNodePage {
    #[serde(default)]
    items: Vec<WikiNode>,
    page_token: Option<String>,
    has_more: Option<bool>,
}

/// The fields of a wiki node that FeishuFS consumes.
#[derive(Debug, Deserialize)]
struct WikiNode {
    space_id: String,
    node_token: String,
    obj_token: Option<String>,
    obj_type: Option<String>,
    title: Option<String>,
    url: Option<String>,
    /// Missing in older responses: assume children may exist and ask for them.
    #[serde(default = "assume_children")]
    has_child: bool,
}

impl ApiResponseTrait for WikiNodePage {
    fn data_format() -> ResponseFormat {
        ResponseFormat::Data
    }
}

fn assume_children() -> bool {
    true
}

fn remote_node(node: WikiNode) -> RemoteNode {
    let fallback: String = node.node_token.chars().take(8).collect();
    RemoteNode {
        space_id: node.space_id,
        node_token: node.node_token,
        object_token: node.obj_token,
        object_type: node.obj_type,
        title: node
            .title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| format!("Untitled-{fallback}")),
        url: node.url,
        has_child: node.has_child,
    }
}
