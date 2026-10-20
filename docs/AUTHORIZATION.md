# 应用授权

**简体中文** | [English](AUTHORIZATION.en.md) · [返回 README](../README.zh-CN.md)

## 创建自建应用

1. 登录[飞书开发者后台](https://open.feishu.cn/app)，创建企业自建应用；Lark 用户在对应的 Lark 开发者后台创建。流程见官方[企业自建应用开发流程](https://open.feishu.cn/document/home/introduction-to-custom-app-development/self-built-application-development-process)。
2. 在应用的 **凭证与基础信息** 页面取得 App ID、App Secret，供下方环境变量使用；不要公开或提交这些凭证。
3. 若采用下方的群授权方式，在 **添加应用能力** 中添加 **机器人**，使自建应用可作为应用机器人加入群聊。机器人不是所有授权方式的必需条件。

这里使用的是有 App ID / App Secret 的自建应用。群聊中通过 Webhook 推送消息的“自定义机器人”不提供知识库数据访问能力，不能代替此应用；两者区别见官方[机器人概述](https://open.feishu.cn/document/client-docs/bot-v3/bot-overview)。FeishuFS 无需配置消息接收或机器人事件订阅。

## 配置接口权限

在 **开发配置 > 权限管理** 中按实际使用的功能申请权限。[权限导入示例](../misc/feishu-scopes.json)只包含读取、下载和导出权限，`tenant` / `user` 分别供应用身份 / 用户身份使用；仅使用应用身份时可删去 `user` 列表。

| 功能 | 示例权限 | 对应接口 |
| --- | --- | --- |
| 定位知识库、读取空间和节点目录 | `wiki:wiki:readonly` | [获取节点](https://open.feishu.cn/document/server-docs/docs/wiki-v2/space/get_node)、[空间信息](https://open.feishu.cn/document/server-docs/docs/wiki-v2/space/get)、[子节点列表](https://open.feishu.cn/document/server-docs/docs/wiki-v2/space-node/list) |
| Docx 正文 | `docx:document:readonly` | Docs AI Fetch；权限参考官方 CLI 的 [fetch 声明](https://github.com/larksuite/cli/blob/017b19a793ad71794a02887ede04757f3d82e10d/shortcuts/doc/docs_fetch.go) |
| 电子表格工作表清单 | `sheets:spreadsheet:read` | [查询工作表](https://open.feishu.cn/document/server-docs/docs/sheets-v3/spreadsheet-sheet/query) |
| 多维表格数据表清单 | `base:table:read` | [列出数据表](https://open.feishu.cn/document/server-docs/docs/bitable-v1/app-table/list) |
| 旧版文档 / 表格 CSV | `docs:document:export` | [导出任务](https://open.feishu.cn/document/server-docs/docs/drive-v1/export_task/create)及结果下载 |
| 文件节点下载 | `drive:file:download` | [下载文件](https://open.feishu.cn/document/server-docs/docs/drive-v1/file/download) |
| 无 Mermaid 源码的画板 | `board:whiteboard:node:read` | [画板图片下载](https://open.feishu.cn/document/server-docs/board-v1/whiteboard/download_as_image) |

这是一份按当前调用接口整理的权限起点，**尚未使用全新应用逐项验证为最低权限集合**。不使用的内容类型可移除对应权限；但目录遍历遇到表格时仍需读取子表清单，`inspect` 也会用到这部分权限。电子表格使用当前的 `sheets:spreadsheet:read` 名称，旧的 `sheets:spreadsheet:readonly` 说明见[官方 CLI 维护者确认](https://github.com/larksuite/cli/issues/838)。Docs AI Fetch 的权限参考官方 CLI，不能把其他正文接口的权限要求直接视为本接口的完整保证。

导入时，在 **开发配置 > 权限管理 > 批量导入/导出权限 > 导入** 中粘贴 JSON，确认后申请开通并完成发布 / 审批。**导入不会移除已有权限**；已有应用如需收紧权限，应另行检查并撤销不再需要的写入、删除和权限管理授权。官方操作流程见[申请 API 权限](https://open.feishu.cn/document/ukTMukTMukTM/uQjN3QjL0YzN04CN2cDN)。使用用户访问凭证时，还需用户以相应 scopes 授权；应用勾选了 `user` 权限不代表现有 token 已包含它们。

## 发布与知识库访问授权

在 **应用发布 > 版本管理与发布** 创建并申请发布应用版本，确认能力、权限及可用范围；需审核的权限等管理员批准后生效。群授权时，可用范围应包含知识库资源所有者及添加机器人的人员。免审和测试规则见官方[发布与审核自建应用](https://open.feishu.cn/document/home/intro-to-custom-app-review)。

**接口权限不等于具体知识库 / 文档的访问权**。默认以应用身份访问，还需知识库管理员或文档管理者授权。官方[知识库常见问题](https://open.feishu.cn/document/server-docs/docs/wiki-v2/wiki-qa)列出了授权方式：

- **整个空间的群授权**：将已发布的应用作为应用机器人加入群聊；知识库管理员在 **知识库设置 > 成员设置** 添加该群为可阅读成员。不要在此使用 Webhook 自定义机器人，也无需为只读使用授予管理员或可编辑角色。
- **节点文档授权**：文档所有者、拥有管理权限的协作者或知识库管理员，可在文档 **… > …更多 > 添加文档应用** 中添加应用。仅节点授权不代表能访问整个知识空间。已有应用成员授权、文档应用授权或使用已获授权的用户身份时，按对应身份检查权限，不必一律开启机器人。

## 配置凭证

推荐使用环境变量，不要把密钥写入命令行或提交到 Git。

| 环境变量 | 对应参数 | 用途 |
| --- | --- | --- |
| `FEISHU_APP_ID` | `--app-id` | 必填，应用 ID |
| `FEISHU_APP_SECRET` | `--app-secret` | 必填，应用密钥 |
| `FEISHU_USER_ACCESS_TOKEN` | `--user-access-token` | 可选，以用户身份访问 |
| `FEISHU_API_BASE_URL` | `--api-base-url` | 可选，覆盖默认 API 地址 |

[`.env.example`](../.env.example)仅作模板。Binary（包括 `nix run`）不自动加载 `.env`，可直接使用环境变量，无需安装 SecretSpec；开发时用 SecretSpec dotenv 仅向应用子进程注入凭证，见[贡献指南](../CONTRIBUTING.md#开发环境)。

Lark 链接自动使用 `https://open.larksuite.com`，飞书链接使用 `https://open.feishu.cn`。非官方 API 地址还需显式传入 `--allow-custom-api-base-url`；仅对可信服务启用。

支持 `wiki`、`docx`、`doc/docs`、`sheet/sheets`、`base/bitable`、`file`、`mindnote/mindnotes`、`slides` 路径。可解析链接不代表能转换其内容，见[文件映射](PRODUCT.md#文件映射)。

