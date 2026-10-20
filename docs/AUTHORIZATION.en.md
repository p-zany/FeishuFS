# Authorization

[简体中文](AUTHORIZATION.md) | **English** · [Back to README](../README.md)

## Create a custom app

1. Open the [Feishu developer console](https://open.feishu.cn/app) and create an enterprise custom app. Lark users should use the corresponding Lark developer console. See the official [custom app development process](https://open.feishu.cn/document/home/introduction-to-custom-app-development/self-built-application-development-process).
2. Obtain the App ID and App Secret from the app's credentials page. Use them in the environment variables below; do not publish or commit these credentials.
3. If using group-based authorization below, add the **Bot** capability so the app can join a group as an app bot. A bot is not required for every authorization method.

FeishuFS uses a custom app with an App ID and App Secret. A webhook-based custom bot cannot provide knowledge base access or replace this app. See the official [bot overview](https://open.feishu.cn/document/client-docs/bot-v3/bot-overview). FeishuFS does not require message handling or bot event subscriptions.

## Configure API permissions

In the app's permission management page, request the permissions needed for the content you use. The [permission import example](../misc/feishu-scopes.json) contains only read, download, and export permissions. Its `tenant` and `user` lists apply to app and user identities respectively; remove the `user` list if you only use the app identity.

| Function | Example permission | API reference |
| --- | --- | --- |
| Resolve knowledge bases and read spaces and nodes | `wiki:wiki:readonly` | [Get node](https://open.feishu.cn/document/server-docs/docs/wiki-v2/space/get_node), [get space](https://open.feishu.cn/document/server-docs/docs/wiki-v2/space/get), [list child nodes](https://open.feishu.cn/document/server-docs/docs/wiki-v2/space-node/list) |
| Docx document content | `docx:document:readonly` | Docs AI Fetch; see the official CLI's [fetch declaration](https://github.com/larksuite/cli/blob/017b19a793ad71794a02887ede04757f3d82e10d/shortcuts/doc/docs_fetch.go) |
| List spreadsheet worksheets | `sheets:spreadsheet:read` | [Query worksheets](https://open.feishu.cn/document/server-docs/docs/sheets-v3/spreadsheet-sheet/query) |
| List Base tables | `base:table:read` | [List tables](https://open.feishu.cn/document/server-docs/docs/bitable-v1/app-table/list) |
| Legacy documents / table CSV exports | `docs:document:export` | [Export tasks](https://open.feishu.cn/document/server-docs/docs/drive-v1/export_task/create) and result downloads |
| Download file nodes | `drive:file:download` | [Download file](https://open.feishu.cn/document/server-docs/docs/drive-v1/file/download) |
| Whiteboards without Mermaid source | `board:whiteboard:node:read` | [Download whiteboard image](https://open.feishu.cn/document/server-docs/board-v1/whiteboard/download_as_image) |

This is a starting point based on the APIs currently called by FeishuFS. **It has not been verified permission by permission with a fresh app as a minimal permission set.** You can omit permissions for unused content types, but directory traversal still needs subtable metadata when it encounters tables; `inspect` also requires those permissions. The spreadsheet permission uses the current `sheets:spreadsheet:read` name; see the [official CLI maintainer's clarification](https://github.com/larksuite/cli/issues/838) about the older `sheets:spreadsheet:readonly` name. The Docs AI Fetch permission is based on the official CLI; permissions for other document APIs do not establish the full requirements of this endpoint.

Use the permission management page's batch import/export dialog to import the JSON, then request the permissions and complete publication or approval as required. **Importing does not remove existing permissions.** To reduce an existing app's access, separately review and revoke unused write, delete, and permission-management grants. See [requesting API permissions](https://open.feishu.cn/document/ukTMukTMukTM/uQjN3QjL0YzN04CN2cDN). A user access token also needs consent for the relevant scopes; enabling `user` permissions in the app does not add scopes to an existing token.

## Publish the app and grant knowledge base access

Create and submit an app version in the app's version management and release page. Check its capabilities, permissions, and availability range, and wait for administrator approval where required. For group-based authorization, the availability range should include the resource owner and the people adding the bot. See the official [custom app publication and review guide](https://open.feishu.cn/document/home/intro-to-custom-app-review) for review and testing rules.

**API permissions do not grant access to individual knowledge bases or documents.** FeishuFS uses the app identity by default, so a knowledge base administrator or document manager must also grant resource access. The official [knowledge base FAQ](https://open.feishu.cn/document/server-docs/docs/wiki-v2/wiki-qa) describes these methods:

- **Group access to an entire space**: Add the published app to a group as an app bot. A knowledge base administrator then adds that group as a member with read access in the knowledge base's member settings. Use an app bot, not a webhook-based custom bot; read-only use does not require administrator or editor access.
- **Access to a document node**: The document owner, a collaborator with management permissions, or a knowledge base administrator can add the app through the document's **… > More > Add document app** menu. Access to one node does not grant access to the entire space. If you already have app membership, document app access, or an authorized user identity, check that identity's access; enabling a bot is not always necessary.

## Configure credentials

Prefer environment variables. Do not put secrets in command-line arguments or commit them to Git.

| Environment variable | CLI option | Purpose |
| --- | --- | --- |
| `FEISHU_APP_ID` | `--app-id` | Required app ID |
| `FEISHU_APP_SECRET` | `--app-secret` | Required app secret |
| `FEISHU_USER_ACCESS_TOKEN` | `--user-access-token` | Optional token to use a user identity |
| `FEISHU_API_BASE_URL` | `--api-base-url` | Optional API base URL override |

[`.env.example`](../.env.example) is only a template. The binary, including when invoked through `nix run`, does not load `.env` automatically. You can use environment variables directly without installing SecretSpec. For development, SecretSpec's dotenv provider injects credentials only into the app subprocess; see [Contributing](../CONTRIBUTING.md#开发环境) (Chinese).

Lark URLs automatically use `https://open.larksuite.com`; Feishu URLs use `https://open.feishu.cn`. An unofficial API URL also requires `--allow-custom-api-base-url`; enable this only for a trusted service.

Supported URL paths include `wiki`, `docx`, `doc/docs`, `sheet/sheets`, `base/bitable`, `file`, `mindnote/mindnotes`, and `slides`. Recognizing a URL does not mean its content can be converted; see [file mapping](PRODUCT.md#文件映射) (Chinese).
