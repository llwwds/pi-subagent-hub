# 平台接口与开发状态（2.0.0.0 源码）

本页描述源码仓库中正在开发的接口，不代表已发布的 `1.0.0.0` 部署。现有 `/v1` HTTP 路由、CLI 命令和 manifest v1 保留原有形状；新版功能增加 `/v2` 路由与 `pihub carriers|skills|agents` 命令。

## 统一载体

首批载体 ID 为 `pi`、`codex`、`claude`。平台只向载体传入 prompt、指定 Skill 副本并归一化输出事件，不修改载体内部。每个 subagent 有独立 ID、状态、会话与日志。Pi 使用长驻 RPC 进程；Codex CLI 和 Claude Code CLI 每轮启动一个命令行进程，并在后续轮次续接私有会话。三者共用创建、`prompt`、事件查询和停止入口；实时 `steer` / `follow-up` 取决于载体能力，CLI 轮次执行期间不能保证支持。

`GET /v2/carriers` 返回每个载体的 `id`、`displayName`、`available`、`reason` 和 `capabilities`。`available` 只表示启动条件的静态诊断，不等于真实模型调用通过。Codex/Claude 固定从当前 `PIHUB_HOME/toolchains/` 下的独立副本启动；两者已在应用私有开发目录安装，真实模型调用尚未验收。

Claude Code 使用 `--bare` 加载平台指定目录中的 Skill，并以无交互 `dontAsk` 模式运行。平台通过 `--allowedTools` 明确批准常规读写、命令、笔记本和 Web 工具，否则该模式会拒绝需要审批的开发操作。这个设置不提供文件系统或网络隔离；任务执行者必须按工作目录和用户指令控制实际操作范围。

## CC Switch 供应商配置

开发环境单独暂存了官方 CC Switch `3.20.4` 桌面本体，并以 [CC-Switch CLI fork `5.10.5`](https://github.com/SaladDay/cc-switch-cli) 实现平台的非交互配置接口。桌面本体尚未启动：正式版的单实例身份可能唤醒本机现有 CC Switch。CLI fork 使用当前 `PIHUB_HOME/cc-switch/` 下的私有数据库、配置与 Pi 模型目录；不会复用本机已有 CC Switch 或三个 Agent CLI 的配置。该 fork 是 CC Switch 同源代码的命令行分支，不是平台仿写的切换器。

`GET /v2/providers` 返回三种载体各自的供应商、API URL、协议、模型列表、是否已配置和当前选择，不返回密钥。`POST /v2/providers` 接受 `{ "app": "codex|claude|pi", "id": "...", "name": "...", "baseUrl": "https://...", "apiFormat": "...", "apiKey": "...", "modelList": ["..."], "defaultModel": "..." }`；密钥只从请求进入应用私有的 CC Switch 数据库，不能放在命令参数中。Codex 当前接 OpenAI Responses，Claude 当前接 Anthropic Messages，Pi 接其原生 `models.json` 支持的协议。Claude 可另传 `authMode: "api-key" | "bearer"`，默认 `api-key` 使用 `X-Api-Key`（Anthropic 官方），选 `bearer` 则使用 `Authorization`（兼容网关）；查询只返回认证方式，不回显密钥。`POST /v2/providers/:app/:id/switch` 设置 Codex/Claude 的平台默认供应商；Pi 的默认供应商和模型由创建 Agent 时的 Pi 参数选择，因此该切换路由会明确拒绝 `pi`。

新建 Agent 时可指定 `provider`（该载体在 CC Switch 中的供应商 ID）和 `model`。未指定供应商时采用该载体的当前选择；只有一个已配置供应商时采用它。Agent 创建后保存该供应商 ID，切换平台默认供应商不会悄悄改动现有 Agent 的选择。Codex/Claude 每轮由 CC Switch 按 Agent 的供应商启动独立 CLI 会话；Pi 通过私有模型配置及 `--provider`、`--model` 选择。Codex 使用 CC Switch 持久会话模式时，推理强度由供应商配置决定，暂不支持逐 Agent 的 `thinking` 覆盖。

## Skill 广场与授权

`POST /v2/skills/import` 接受 `{ "id": "skill-id", "sourcePath": "/absolute/skill/directory" }`，把单个含根层 `SKILL.md` 的目录复制到平台私有存储；不会修改源目录。也可使用 `pihub skills import --id skill-id --source /absolute/skill/directory --json`。导入会拒绝符号链接、运行时目录及疑似凭据文件；每份副本按内容 SHA-256 固定，`GET /v2/skills` 可查看目录与版本。用户指定三个来源目录中的 49 份 Skill 已导入应用私有开发目录。

创建时 `skillPolicy` 支持 `{ "mode": "all" }` 或 `{ "mode": "only", "ids": ["skill-id"] }`。省略时默认为 `all`，含义是**冻结创建当时广场里的全部 Skill 快照**。之后导入的新快照不会改变现有 subagent 的授权。`only` 只限制平台配置的 Skill 自动发现和加载，不是操作系统级安全隔离；载体仍可能自行读取宿主文件、凭据或访问网络。

首批快照包含 `pi-subagent-hub` Skill。平台给每个 v2 Agent 设置当前实例的 `PIHUB_HOME`，并在其私有 PATH 首位挂载指向当前应用 CLI 的 `pihub` 命令，因此该 Skill 的普通命令解析会进入当前平台实例；它创建的后续 Agent 也会进入本平台看板。Skill 本身仍是可读文本，Agent 若自行使用绝对路径访问其他安装，不受此机制阻止。

## 新增 HTTP 与 CLI

所有 `/v2` 路由沿用本地 daemon 的 Bearer control token，响应沿用 `{ "ok", "data", "error" }` envelope。

| HTTP | 用途 | CLI |
| --- | --- | --- |
| `GET /v2/snapshot` | 全部 subagent、进程运行数、受管理会话数与事件游标 | `pihub agents list --json` |
| `GET /v2/events?after=CURSOR&limit=N` | 按游标读取全部载体及新旧 Agent 的事件，与 `/v2/snapshot.eventCursor` 对应 | — |
| `GET /v2/carriers` | 载体可用性及能力 | `pihub carriers list --json` |
| `GET /v2/providers` | CC Switch 供应商和模型目录 | `pihub providers list --json` |
| `POST /v2/providers` | 增加供应商 | `pihub providers add --stdin --json` |
| `POST /v2/providers/:app/:id/switch` | 切换新建 Agent 的默认供应商 | `pihub providers switch --app APP --id ID --json` |
| `GET /v2/skills` | Skill 广场目录 | `pihub skills list --json` |
| `POST /v2/skills/import` | 复制一份 Skill | `pihub skills import ... --json` |
| `POST /v2/agents` | 创建 subagent | `pihub agents create ... --json` |
| `GET /v2/agents/:id` | subagent 详情 | `pihub agents inspect ID --json` |
| `GET /v2/agents/:id/events` | 事件与输出 | `pihub agents logs ID --json` |
| `POST /v2/agents/:id/prompt` | 发送 prompt | `pihub agents prompt ID --stdin --json` |
| `POST /v2/agents/:id/stop` | 停止 subagent | `pihub agents stop ID --json` |

创建示例：

```json
{
  "carrier": "codex",
  "name": "reviewer",
  "cwd": "/absolute/path/to/project",
  "skillPolicy": { "mode": "all" }
}
```

CLI 等价入口为 `pihub agents create --carrier codex --name reviewer --cwd /absolute/path/to/project --skills all --json`。批量创建使用 `schemaVersion: 2` 的 manifest；现有 `schemaVersion: 1` manifest 继续通过 `pihub spawn --manifest` 运行 Pi。

## 验证边界

自动化测试使用临时状态目录和模拟载体验证接口、事件及 v1 回归。另用三个应用私有真实 CLI、CC Switch 私有配置、假密钥与 `127.0.0.1` 模拟服务完成 Pi Chat Completions、Codex Responses、Claude Messages 的成功回合；三者都产生助手 `message_end` 与 `agent_settled`，没有发送真实模型请求。真实上游与真实凭据尚需在平台中配置可用 API 后验收。

广场首批 Skill 是内容快照，不自动安装 Skill 所需的外部命令或认证。例如 Lark 系列 Skill 依赖 `lark-cli`，当前平台私有载体 PATH 中没有该命令；授权和加载成功不代表该 Skill 的外部操作已可用。
