# pi-subagent-hub

`pi-subagent-hub` 是一个面向人类与任意父 Agent 的本地控制面。它在独立边界内运行官方 [Pi Agent](https://github.com/earendil-works/pi)，可按需拉起多个拥有独立 context 的 Pi subagent，并通过同一套 CLI 和网页面板统一查看、调度与停止它们。

当前稳定版：`1.0.0.0`（npm 元数据版本 `1.0.0`）。

## 它解决什么问题

- 任意数量：一个 batch manifest 可以一次创建多个 subagent。
- 任意模型：每个 subagent 可分别指定 provider、model 和 thinking level。
- 独立上下文：每个 subagent 都有自己的进程、Pi 目录、session、context、日志和生命周期。
- 一致能力集：同一 Hub profile 下的 subagent 共享相同的 tool/skill/extension 定义，但不共享进程内状态。
- 统一管理：CLI 提供稳定 JSON 接口，本地面板可查看全部或单个 subagent。
- 配置与代码解耦：更换 AI API 只改一个 `models.json`；不需要改代码、重新构建或重新部署。

> 进程与 context 隔离不是操作系统沙箱。如果多个 subagent 指向同一个工作目录，它们仍可能修改同一批文件。

## 环境要求

- Node.js `>=22.19.0`
- npm、Git
- 当前在 macOS 上完成真实 Pi RPC 验证，并由 CI 在 Linux 上验证；Windows 尚未验证。

首次 bootstrap 会从 GitHub 下载一份锁定版本的 Pi 源码，并从 npm Registry 安装同版本运行包。项目不会复用或修改你已有的 Pi 安装。

## 快速开始

```bash
git clone https://github.com/llwwds/pi-subagent-hub.git
cd pi-subagent-hub
git checkout v1.0.0.0

npm run bootstrap:pi
npm run check
npm run smoke:pi
npm link
```

初始化本地状态和私有配置：

```bash
pihub config init
pihub config validate
pihub doctor
```

启动 daemon 和面板：

```bash
pihub daemon start --json
pihub panel
```

`npm link` 只创建本机命令链接，不会发布 npm 包。若不想链接，也可以把下文的 `pihub` 替换为 `node bin/pihub.js`。

## AI API：只改一个配置文件

AI API 的 endpoint、协议类型、凭据引用和自定义模型列表都由这一份文件控制：

```text
$PIHUB_HOME/config/pi-shared/models.json
```

未设置 `PIHUB_HOME` 时，默认位置为：

- macOS：`~/Library/Application Support/pi-subagent-hub`
- Linux：`$XDG_DATA_HOME/pi-subagent-hub`，未设置时为 `~/.local/share/pi-subagent-hub`
- Windows：`%LOCALAPPDATA%\pi-subagent-hub`（尚未验证）

示例：

```json
{
  "providers": {
    "my-gateway": {
      "baseUrl": "https://api.example.com/v1",
      "api": "openai-completions",
      "apiKey": "!security find-generic-password -ws 'pihub-my-gateway'",
      "models": [
        {
          "id": "my-model",
          "name": "My Model",
          "reasoning": false,
          "contextWindow": 128000,
          "maxTokens": 16384
        }
      ]
    }
  }
}
```

仓库内还提供了 [`config/models.example.json`](config/models.example.json)。修改后执行：

```bash
pihub config validate
pihub restart AGENT_ID
```

这就是完整的 AI API 切换流程：编辑配置、校验、重启受影响的 agent。无需改源码、重新 bootstrap 或重新部署 Hub。新建 agent 会直接读取新配置。

`config validate` 只输出 provider/model 标识和布尔状态，不输出 endpoint、API key 或 header 值。配置文件会被强制设为 `0600`。推荐让 `apiKey` 引用 macOS Keychain、1Password CLI 或其他凭据命令；不要把真实密钥写进仓库。Pi 的 OAuth/login 状态单独保存在同目录的 `auth.json`，它不是 AI endpoint/model 配置的一部分。

## 创建和管理 subagent

创建单个 subagent：

```bash
pihub spawn \
  --name reviewer-a \
  --cwd /absolute/path/to/project \
  --model my-gateway/my-model \
  --thinking high \
  --json
```

批量创建：

```json
{
  "schemaVersion": 1,
  "groupName": "review",
  "agents": [
    {
      "name": "reviewer-a",
      "cwd": "/absolute/path/a",
      "model": { "provider": "my-gateway", "id": "model-a", "thinking": "high" }
    },
    {
      "name": "reviewer-b",
      "cwd": "/absolute/path/b",
      "model": { "provider": "another-provider", "id": "model-b", "thinking": "medium" }
    }
  ]
}
```

```bash
pihub spawn --manifest manifest.json --json
```

常用控制命令：

```bash
pihub list --json
pihub inspect AGENT_ID --json
printf '%s' '审查当前项目，只返回关键结论' | pihub prompt AGENT_ID --stdin --json
pihub steer AGENT_ID '优先检查并发安全' --json
pihub wait AGENT_ID --timeout 120 --json
pihub logs AGENT_ID --after 0 --json
pihub stop AGENT_ID --json
```

长 prompt 推荐使用 `--stdin`，避免 shell 转义问题以及内容进入命令历史。

## 架构与隔离

```text
父 Agent / 人类
    -> pihub CLI / 本地 Web 面板
    -> Hub daemon（唯一状态写入者）
    -> 每个 subagent 一个独立 Pi --mode rpc 进程
    -> 私有 session / context / logs
    -> SQLite 事件流与统一控制面
```

Hub 不调用 `PATH` 中已有的 `pi`。它固定使用本项目 bootstrap 的运行时：

- Pi release：`v0.85.1`
- Pi commit：`d981de1229ef899957bbe968bc8dcda02a21f477`
- 完整上游源码：本地 `vendor/pi/`，只读且不提交
- npm 运行时：本地 `vendor/pi-runtime/`，由受控 lockfile 和 `npm ci` 重建

详细设计、RPC 语义、状态机和安全边界见 [`docs/architecture.md`](docs/architecture.md)。

## 安全边界

- daemon 只监听 `127.0.0.1` 的随机端口。
- control token、配置、数据库、session 和日志都留在仓库外的 `PIHUB_HOME`。
- daemon 会拒绝将 `PIHUB_HOME` 设为源码目录或其子目录，防止凭据和会话被误提交。
- 面板不把 token 写入 URL 或浏览器 localStorage。
- `--json` 错误也保持单一、机器可解析的 envelope。
- 本项目不提供容器/虚拟机级沙箱，也不替多个 agent 协调同一工作目录中的并发写入。

## 开发与验证

```bash
npm run bootstrap:pi  # 校验上游源码锁，并以 npm ci 重建锁定运行时
npm run check         # 语法检查与单元/集成测试
npm run smoke:pi      # 真实 Pi RPC 冒烟，不发起付费模型请求
```

主要目录：

- `src/`：CLI、daemon、RPC 监管、本地 API 和面板。
- `test/`、`fixtures/`：测试和 fake Pi RPC。
- `config/`：不含密钥的配置模板。
- `docs/`：架构与版本发布说明。
- `vendor/*.lock.json`、`vendor/pi-runtime.package-lock.json`：Pi 源码和运行时的可复现锁。

源码仓库与正式部署副本不会自动同步；发布或部署必须显式执行，避免开发中的修改直接影响正在运行的应用。

## 当前限制

- v1.0 只实现 `shared` workspace mode；需要文件写入隔离时，请由调用方先准备不同工作目录或 Git worktree。
- skill/tool profile 在 agent 启动时冻结，运行中不热切换。
- 不自动安装 skill、不自动批准 extension UI 请求。
- “任意模型”指 Pi 内置、`models.json` 可配置或 extension 可注册的 provider/model；私有协议仍需对应适配器。

## License

本项目使用 [MIT License](LICENSE)。Pi Agent 的归属和许可证信息见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。v1.0 只发布 GitHub 源码包，不发布包含 `node_modules` 的二进制或部署包。
