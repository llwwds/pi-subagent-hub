# pi-subagent-hub

`pi-subagent-hub` 是一个面向人类与任意父 Agent 的本地控制面。它在独立边界内运行官方 [Pi Agent](https://github.com/earendil-works/pi)，可按需拉起多个拥有独立 context 的 Pi subagent，并通过同一套 CLI 和网页面板统一查看、调度与停止它们。

当前源码包含两条线：

- `3.0.0.0` Rust 自有运行层重写位于 [`rust/`](rust/README.md)，目前仍在开发中。
- `2.1.0.0` Node 实现是已验收的行为参考（tag `v2.1.0.0`），用于对照，不再是 3.0 的默认执行路径。

3.0 的构建、运行和隔离测试入口见 [`rust/README.md`](rust/README.md) 与 [`docs/rust-contracts.md`](docs/rust-contracts.md)。普通 CLI 只通过 loopback HTTP 访问 daemon；只有隐藏的 daemon 入口构造 Store/AgentManager/RuntimeFactory。源码仓库与运行实例相互独立：修改源码后，需要按实例的安装方式自行构建和更新。

`2.1.0.0` 平台功能包括 Pi、Codex CLI、Claude Code CLI 载体接口、Skill 快照目录、CC Switch 私有供应商配置、Skills Manager 手动连接、System Prompt 注入和 Hub 级诊断日志。新接口和现阶段可用范围见 [`docs/platform-api.md`](docs/platform-api.md)，面板使用说明见 [`docs/user-guide.md`](docs/user-guide.md)，System Prompt 见 [`docs/system-prompts.md`](docs/system-prompts.md)，日志见 [`docs/logging.md`](docs/logging.md)，Skills Manager 见 [`docs/skills-manager.md`](docs/skills-manager.md)。

## 它解决什么问题

- 任意数量：一个 batch manifest 可以一次创建多个 subagent。
- 任意模型：每个 subagent 可分别指定 provider、model 和 thinking level。
- 独立上下文：每个 subagent 都有自己的进程、Pi 目录、session、context、日志和生命周期。
- 一致能力集：同一 Hub profile 下的 subagent 共享相同的 tool/skill/extension 定义，但不共享进程内状态。
- 统一管理：CLI 提供稳定 JSON 接口，本地面板可查看全部或单个 subagent。
- 配置与代码解耦：更换 AI API 只改一个 `models.json`；不需要改代码、重新构建或重新部署。

> 进程与 context 隔离不是操作系统沙箱。如果多个 subagent 指向同一个工作目录，它们仍可能修改同一批文件。

## 环境要求

- Rust/Cargo `>=1.99`：构建和测试 3.0 原生 Hub。已验证 macOS aarch64、Rust 1.99.1；Linux/Windows 的 3.0 原生路径尚未验收。
- Node.js `>=22.19.0`：Pi 载体的外部 bundle 仍由 Node 执行；2.1 legacy 入口和测试也需要 Node。
- Git；npm 仅用于 legacy、bootstrap 和 `npm link`。
- 项目不自动安装工具链，也不修改全局 PATH。默认构建目录为 `${TMPDIR:-/tmp}/pihub-rust-target`，可用 `CARGO_TARGET_DIR` 显式指定。

## 快速上手

### 交给你的 Agent 安装与配置

把下面的提示词发给你正在使用的 Agent，即可让它检查环境、按仓库文档执行安装和验证，并引导你配置模型 API：

```text
请帮我安装并配置 pi-subagent-hub，项目地址：
https://github.com/llwwds/pi-subagent-hub

请先读取仓库 README 和项目说明，检查我的系统、依赖及现有安装，按当前仓库支持的步骤安装、验证并启动；不要覆盖已有实例或配置。请交互引导我配置模型 API，使用安全的凭据存储，不在对话、日志或代码中暴露密钥。最后告诉我启动方式、验证结果和未完成项。
```

给执行安装的 Agent：

1. 先核对当前仓库版本与平台支持。`main` 的 3.0 仍在开发中；本仓库没有提供可直接运行的远程一键安装脚本。
2. 检查 Git、Node.js `>=22.19.0`、npm 和 Rust/Cargo `>=1.99`；缺失时向用户说明。检查是否已有 Pi、PiHub、同名源码目录或正在运行的实例，不覆盖或修改既有安装和数据。
3. 在独立目录获取源码，按下方流程准备锁定版本的 Pi 运行时并构建 Hub；明确指定独立的源码、构建和状态路径，不让源码试用实例复用现有 `PIHUB_HOME` 或继承指向其他实例的 `PIHUB_APP_ROOT`、`PIHUB_RUST_BIN`。下方临时目录适合验证，若要持续使用，需要选用持久目录并记录启动所需路径。
4. 执行 `config init`、`config validate` 和 `doctor`，检查返回内容；首次检查时 daemon 尚未启动是正常情况。启动后确认 daemon 和面板可用。配置校验通过不等于模型请求已经成功。
5. 向用户询问供应商、模型和 API 配置方式，引导使用密钥管理器或受限配置文件。不要在聊天、日志、仓库或命令行参数中暴露密钥，也不要未经授权发送可能产生费用的模型请求。报告版本、源码和状态目录、启动方式、已验证与未验证项。

### 从源码试用 3.0

开发或试用时请 **显式指定独立的 `PIHUB_HOME`**。CLI 使用该变量选择实例；
如果当前 shell 沿用了已有实例的值，`daemon start` / `daemon stop` 会操作该实例。
以下示例使用临时目录隔离，长期使用请改用持久目录：

```bash
git clone https://github.com/llwwds/pi-subagent-hub.git
cd pi-subagent-hub

# 显式隔离构建产物、状态和入口，避免继承其他实例的路径
export CARGO_TARGET_DIR="$(mktemp -d "${TMPDIR:-/tmp}/pihub-build.XXXXXX")"
export PIHUB_HOME="$(mktemp -d "${TMPDIR:-/tmp}/pihub-dev-home.XXXXXX")"
export PIHUB_APP_ROOT="$PWD"
export PIHUB_RUST_BIN="$CARGO_TARGET_DIR/release/pihub"

# 首次获取锁定版本的 Pi 源码与外置运行时
npm run bootstrap:pi
# 使用现有 Cargo；不会自动安装或改 PATH
npm run build
# 等价：bash rust/scripts/build-release.sh

./bin/pihub config init --json
./bin/pihub config validate --json
./bin/pihub doctor --json
./bin/pihub daemon start --json
./bin/pihub panel
```

如需运行带 daemon 和 panel 的隔离开发实例，可使用 QA helper。该脚本要求显式指定
独立的绝对路径作为 home，并核验 receipt、daemon.json、健康状态及进程身份；
start/status/stop 不使用环境中已有的 `PIHUB_HOME`：

```bash
scripts/qa-instance.sh start  --home /tmp/pihub-rust-qa-<name>/hub \
    --binary "${CARGO_TARGET_DIR}/release/pihub" --receipt /tmp/pihub-dev-qa.json
scripts/qa-instance.sh status --home /tmp/pihub-rust-qa-<name>/hub \
    --receipt /tmp/pihub-dev-qa.json
scripts/qa-instance.sh stop   --home /tmp/pihub-rust-qa-<name>/hub \
    --receipt /tmp/pihub-dev-qa.json --cleanup
```

`./bin/pihub` / `pihub` 是 3.0 Rust launcher。它只查找已构建的原生二进制；找不到时打印构建指令并失败，**不会**回退到旧 Node Hub。需要把命令链接到 PATH 时可执行 `npm link`，它会同时提供 `pihub`（3.0）和 `pihub-legacy`（2.1）。

## 2.1 legacy 参考入口

2.1 Node 实现保留为已验收的行为参考和对照测试，不是默认执行路径：

```bash
npm ci
npm run bootstrap:pi       # 准备锁定版本的外置 Pi 运行时
npm run check:legacy       # 2.1 Node 源码检查与测试
node bin/pihub.js --help   # 或：pihub-legacy --help / npm run legacy
```

两者不会静默互相调用。3.0 的构建、运行和隔离测试入口见 [`rust/README.md`](rust/README.md)；阶段记录见 [`docs/releases/v3.0.0.0.md`](docs/releases/v3.0.0.0.md)。

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
npm run build         # 构建 3.0 release 二进制（现有 Cargo）
npm run check         # 3.0 Rust workspace 测试 + launcher/无 fallback 检查
npm run check:legacy  # 2.1 Node 源码检查与测试（显式 legacy）
npm run smoke:pi      # legacy/external Pi RPC 冒烟，不发起付费模型请求
```

主要目录：

- `rust/crates/contracts`：3.0 共享契约；`core`/`runtime`/`app` 实现；`bin` 装配。
- `bin/pihub`：3.0 原生 launcher；`bin/pihub.js`：显式 2.1 legacy 入口。
- `src/`、`test/`、`fixtures/`：2.1 Node 参考实现与对照测试。
- `vendor/*.lock.json`、`vendor/pi-runtime.package-lock.json`：Pi 源码和运行时的可复现锁。
- `docs/releases/v3.0.0.0.md`：3.0 版本和构建的阶段记录。

源码仓库的变更不会自动更新正在运行的实例；更新实例前请确认目标路径、版本和备份。

## 当前限制

- 3.0 仍在开发中；独立行为一致性与 SSR/DOM 验收范围见版本阶段记录。
- 3.0 只实现 `shared` workspace mode；不实现进程池或 worktree。需要文件写入隔离时，请由调用方先准备不同工作目录。
- 2.1 Node 代码保留为 legacy 参考；默认 `pihub` 不会再调用它。
- skill/tool profile 在 agent 启动时冻结，运行中不热切换。
- 不自动安装 skill、不自动批准 extension UI 请求。
- “任意模型”指 Pi 内置、`models.json` 可配置或 extension 可注册的 provider/model；私有协议仍需对应适配器。

## License

本项目使用 [MIT License](LICENSE)。Pi Agent 的归属和许可证信息见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。v1.0 只发布 GitHub 源码包，不发布包含 `node_modules` 的二进制或部署包。
