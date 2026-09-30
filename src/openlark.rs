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
        docs::v1::{GetDocsContentRequest, content::get::get_docs_content},
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
use serde::Deserialize;
use tokio::runtime::{Builder, Runtime};

use crate::{
    link::KnowledgeLink,
    remote::{ContentRequest, RemoteNode, RemoteSource, RemoteSpace, SPACE_OBJECT_TYPE},
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
            bail!("单文件下载上限必须大于 0");
        }
        let max_response_size =
            u64::try_from(options.max_download_bytes).context("单文件下载上限超出 u64 范围")?;
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
            .map_err(|error| anyhow!(error))
            .context("OpenLark 配置无效")?;
        let provider = AuthTokenProvider::new(base_config.clone());
        let config = base_config.with_token_provider(provider);
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .thread_name("feishufs-api")
            .build()
            .context("无法创建 OpenLark Tokio runtime")?;

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
    /// knowledge space (100 requests/minute) has to be re-read every minute.
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
                "获取知识空间子节点列表",
            ))
            .map_err(|error| anyhow!(error))
            .with_context(|| {
                format!(
                    "Wiki nodes 请求失败，space {space_id}，父节点 {}",
                    parent_node_token.unwrap_or("（空间根）")
                )
            })
    }

    fn export(&self, token: &str, document_type: &str, extension: &str) -> Result<Vec<u8>> {
        let created = self
            .runtime
            .block_on(
                CreateExportTaskRequest::new(self.config.clone(), extension, token, document_type)
                    .execute_with_options(self.request_option()),
            )
            .map_err(|error| anyhow!(error))
            .with_context(|| {
                format!("无法创建 {document_type} 文档 {token} 的 {extension} 导出任务")
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
                .map_err(|error| anyhow!(error))
                .with_context(|| format!("无法查询导出任务 {}", created.ticket))?;

            if let Some(result) = response.result {
                last_status = result.job_status;
                last_message = result.job_error_msg.clone();
                if result.job_status == Some(0) {
                    let file_token = result.file_token.with_context(|| {
                        format!("导出任务 {} 已完成但未返回 file_token", created.ticket)
                    })?;
                    let response = self
                        .runtime
                        .block_on(
                            DownloadExportRequest::new(self.config.clone(), file_token)
                                .max_size(self.max_download_bytes)
                                .execute_with_options(self.request_option()),
                        )
                        .map_err(|error| anyhow!(error))
                        .with_context(|| format!("无法下载导出任务 {} 的产物", created.ticket))?;
                    return response
                        .data
                        .with_context(|| format!("导出任务 {} 返回空文件", created.ticket));
                }
            }

            if std::time::Instant::now() >= deadline {
                bail!(
                    "导出任务 {} 在 {} 内未完成（状态 {:?}，消息 {:?}）",
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
            .map_err(|error| anyhow!(error))
            .context(
                "Wiki get_node 请求失败；请确认该链接指向知识库内的文档，且应用有知识库读取权限",
            )?;
        let node = response
            .node
            .context("飞书返回成功，但节点 data.node 为空")?;
        if node.space_id.trim().is_empty() {
            bail!("飞书返回的节点缺少 space_id，无法确定要挂载的知识库");
        }

        let name = self
            .runtime
            .block_on(
                GetWikiSpaceRequest::new(self.config.clone())
                    .space_id(node.space_id.clone())
                    .execute_with_options(self.request_option()),
            )
            .map_err(|error| anyhow!(error))
            .with_context(|| format!("无法读取知识库 {} 的空间信息", node.space_id))
            .ok()
            .and_then(|response| response.space)
            .map(|space| space.name)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| format!("知识库 {}", node.space_id));

        Ok(RemoteSpace {
            space_id: node.space_id,
            name,
        })
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
                "Wiki nodes 响应 has_more=true，但未返回 page_token；为避免死循环已停止",
            )?;
            if page_token.as_deref() == Some(next.as_str()) {
                bail!("Wiki nodes 分页 token 未前进；为避免死循环已停止");
            }
            page_token = Some(next);
        }
        Ok(children)
    }

    fn fetch_content(&self, request: &ContentRequest) -> Result<Vec<u8>> {
        match request {
            ContentRequest::Markdown {
                token,
                document_type,
            } => {
                let response = self
                    .runtime
                    .block_on(get_docs_content(
                        GetDocsContentRequest::new(token, document_type, "markdown"),
                        &self.config,
                        Some(self.request_option()),
                    ))
                    .map_err(|error| anyhow!(error))
                    .with_context(|| format!("无法读取文档 {token} 的 Markdown 内容"))?;
                let mut bytes = response.content.into_bytes();
                if !bytes.ends_with(b"\n") {
                    bytes.push(b'\n');
                }
                Ok(bytes)
            }
            ContentRequest::Download { token } => {
                let response = self
                    .runtime
                    .block_on(
                        DownloadFileRequest::new(self.config.clone(), token)
                            .max_size(self.max_download_bytes)
                            .execute_with_options(self.request_option()),
                    )
                    .map_err(|error| anyhow!(error))
                    .with_context(|| format!("无法下载附件 {token}"))?;
                response.data.context("附件下载响应为空")
            }
            ContentRequest::Export {
                token,
                document_type,
                extension,
            } => self.export(token, document_type, extension),
            ContentRequest::Inline { bytes } => Ok(bytes.clone()),
        }
    }
}

/// One page of the knowledge-space child-node list.
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
            .unwrap_or_else(|| format!("未命名-{fallback}")),
        url: node.url,
        has_child: node.has_child,
    }
}
