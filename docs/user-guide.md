# pi-subagent-hub 2.0 用户手册

本文对应源码中的 2.0 Web 看板和 CLI。Mac 上已安装的稳定版仍为 1.0.0.0；2.0 当前作为源码预览运行，尚未发布部署。预览使用独立状态目录，不会显示 1.0 里的 Agent、供应商或 Skill 配置。

## 1. 打开面板

在同一台 Mac 的浏览器打开本轮提供的 `http://127.0.0.1:<端口>/` 地址。面板只绑定本机回环地址，其他设备无法通过这个地址访问。daemon 停止或重启后端口可能变化；此时从启动面板的 `pihub panel` 命令输出中取最新地址。

页面顶部显示 Hub 连接状态、刷新按钮和“创建 Agent”。主导航分为三页：

- **Agents**：查看和管理 Agent 实例。
- **Skill 广场**：查看已导入的 Skill 快照。
- **供应商 / CC Switch**：管理各载体的 API 供应商与模型。

当前 2.0 预览使用空白状态目录，所以初次打开时没有 Agent、供应商和 Skill 是正常现象。这个预览状态里尚未安装应用私有的 CC Switch CLI，供应商页会报告其不可用；Codex、Claude 载体也会显示未安装。Pi 当前只通过环境检查，不代表真实模型请求已经通过。请勿把生产 API Key 输入这个临时预览实例。

## 2. 查看 Agent

Agents 页面顶部汇总运行中的进程、Agent 实例、可用载体和 Skill 快照数量。左侧列表支持按名称或 ID 搜索，并按载体、状态筛选；选择一项后，右侧显示详情、状态时间线和最近事件。

常见状态：

| 状态 | 含义 |
| --- | --- |
| `provisioning` / `starting` | 正在准备或启动 |
| `running` | 正在执行当前回合 |
| `idle` | 当前没有运行中的回合 |
| `waiting_input` | Agent 等待用户或调用方处理输入 |
| `stopping` / `stopped` | 正在停止 / 已停止 |
| `failed` / `crashed` | 启动或运行失败 / 进程异常退出 |

详情中的“展开原始数据”可查看较完整的事件字段。完成回合时，最终回复以助手的 `message_end` 事件为准；单看 Agent 已空闲不能证明某一回合已经成功。

## 3. 创建 Agent

点击右上角“创建 Agent”，填写：

1. **名称**：便于在列表里识别的名字。
2. **载体**：Pi Agent CLI、Codex CLI 或 Claude Code CLI。可用性提示是静态环境检查，不代表模型调用已成功。
3. **工作目录**：载体执行任务时使用的绝对路径。
4. **供应商与模型**：使用该载体已配置的默认项，或选择一个供应商和模型；Pi 需要明确的模型配置。
5. **思考级别**：载体支持时选择 `off`、`minimal`、`low`、`medium`、`high`、`xhigh` 或 `max`。实际支持以载体提示为准。
6. **Skill 自动加载范围**：默认授权创建时广场中的全部快照，也可改为只选指定快照。允许不加载 Skill。
7. **初始任务**：可选。填写后，创建成功会立即发送。

确认后点击“创建 Agent ↗”。列表和详情会显示新实例的准备、运行和结束状态。

**工作目录边界：** Agent 各自拥有进程、会话、上下文和日志；如果多个 Agent 使用同一目录，它们仍会读写同一批文件。平台没有操作系统级文件或网络沙箱，创建前请确认工作目录和任务范围。

## 4. 与 Agent 交互

在 Agent 详情底部输入消息，并选择动作：

- **消息**：发送新的 prompt。
- **调整当前任务**：尝试改变正在运行的回合；是否即时生效取决于载体能力。
- **后续任务**：当前回合后再排入一项任务。

顶部操作按钮：

- **中止当前回合**：取消正在执行的回合。
- **停止**：结束该 Agent 进程。
- **重启**：重新启动该实例。

发送任务后观察详情事件和状态变化。等待用户输入的实例需要人工处理；不要把创建成功当作模型调用成功。

## 5. Skill 广场

Skill 广场展示平台私有存储中的 Skill 内容快照；当前 Web 页面是只读目录，导入需要使用 CLI：

```bash
pihub skills import --id skill-id --source /absolute/path/to/skill-directory --json
```

源文件夹需包含根目录 `SKILL.md`。平台复制内容，不会修改源文件。每份快照带有内容摘要，便于区分版本。疑似凭据文件、符号链接和运行时目录会被拒绝。

Agent 创建时的 Skill 授权列表会固定在该实例上。后续新导入的 Skill 不会自动加入已存在的 Agent。

Skill 选择只是控制平台自动发现和加载哪些说明文件；它不阻止 Agent 直接读取工作目录、其他文件或凭据，也不隔离网络访问。

## 6. 供应商与模型

“供应商 / CC Switch”页面按 Pi、Codex、Claude 三种载体展示供应商、API 地址、协议、模型目录和当前选择。页面不显示已保存的 API Key。

新增供应商时填写载体、唯一 ID、显示名称、HTTP(S) API URL、该载体使用的 API 协议、模型列表和默认模型。API Key 输入框仅接收这次提交的凭据；提交后凭据保存在应用私有配置中，页面不会回读或显示旧值。不要把密钥放在名称、模型 ID、URL 查询参数或任务 prompt 里。

供应商切换只影响之后创建的 Agent；已有 Agent 保留创建时选择的供应商。Pi 的 provider/model 由创建 Agent 时的选项确定。

## 7. CLI 快速参考

以下命令适用于 2.0.0.0 CLI，并且必须指向对应的 `PIHUB_HOME`。Mac 上的已安装稳定版仍是 1.0.0.0；它没有 2.0 的 `agents` 子命令，也不能管理本轮启动的隔离预览状态。不要让源码版 CLI 与 1.0 daemon 共用状态目录。

```bash
pihub panel
pihub doctor --json
pihub carriers list --json
pihub providers list --json
pihub skills list --json
pihub agents list --json
pihub agents inspect AGENT_ID --json
pihub agents prompt AGENT_ID --stdin --json
pihub agents logs AGENT_ID --json
pihub agents stop AGENT_ID --json
```

长 prompt 可通过标准输入传递，避免 shell 引号问题和进入命令历史。配置或状态目录可用 `pihub config paths --json` 查看；初始化后先用 `pihub config validate --json` 校验。`pihub panel --no-open --json` 返回本机面板 URL，但该命令会启动相应状态目录下的 daemon。

## 8. 安全与可用性边界

- daemon 和 Web 面板仅监听本机回环地址。不要将该服务映射到公网或其他设备可访问的网卡。
- 2.0 源码预览与已安装的 1.0 部署状态分离；预览没有生产供应商、API Key、Agent 或 Skill 数据。
- 载体或模型出现在可选列表中，只能证明环境能识别配置；真实模型调用仍需配置有效供应商并单独验证。
- Agent 的工具权限、Skill 授权和独立会话不等同于文件系统或网络沙箱。
- Codex、Claude 等命令行载体在单轮执行中不一定支持实时调整或中止；按界面显示和事件状态判断结果。

更多接口和当前验证范围见 [`platform-api.md`](platform-api.md)；总体隔离设计见 [`architecture.md`](architecture.md)。
