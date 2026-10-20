# System Prompt 注入（2.1.0.0 源码）

本页说明 pi-subagent-hub 的 system prompt 注入：入口、载体参数、冻结语义与已知边界。它只描述源码开发分支的行为。

## 原则

1. **只追加，不替换。** 平台从不使用载体的替换型参数（例如 Pi 的 `--system-prompt`、Codex 的 `model_instructions_file`）。载体自带的系统约束、上下文文件与 Skill 说明保持有效，平台内容追加在其后；两者冲突时以载体原有约束为准。
2. **先确认上游真实参数。** 每个载体只有在其 CLI 存在追加语义通道时才允许注入，否则明确拒绝。注入通道本身也记入冻结记录。
3. **冻结后不复算。** 创建时解析一次并记录，重启、重新连接或全局配置变化都不再影响该 Agent。

## 上游参数核验

| 载体 | 通道 | 参数 | 语义依据 |
| --- | --- | --- | --- |
| `pi` | `append`（system prompt 末尾） | `--append-system-prompt <text>`，可重复 | 锁定上游 Pi `v0.85.1`（commit `d981de1`）：`docs/usage.md` 参数表与 `dist/cli/args.js` 解析；值进入 `resourceLoaderOptions.appendSystemPrompt`，由 resource loader 追加，不作为 user message 发送 |
| `claude` | `append`（system prompt 末尾） | `--append-system-prompt <text>` | 应用私有 Claude Code CLI `2.1.280` 参数表（另有 `--append-system-prompt-file`） |
| `codex` | `developer`（developer 指令层） | `-c developer_instructions="<text>"` | 见下方定点核验 |

### Codex `developer_instructions` 定点核验

用应用私有 Codex CLI 的 `debug prompt-input`（渲染模型可见输入，只读、不调用模型）对比：

```bash
# 基线
codex debug prompt-input "hello"
# 注入
codex -c 'developer_instructions="HUB-MARK"' debug prompt-input "hello"
```

结果（同一 workspace、同一 `CODEX_HOME`）：

- 基线 5 条输入：3 个 developer 片段（`<skills_instructions>`、`<permissions instructions>`、`<collaboration_mode>`）、1 个 `<multi_agent_role>` developer、1 个 `<environment_context>` user。
- 注入后仍是 5 条输入，但 developer 消息新增一个独立 `input_text` 片段承载注入文本；原有三个片段内容**逐字节不变**，只是位置后移。
- 其余 4 条输入（多 Agent 角色、协作模式、环境上下文、用户消息）文本完全一致。

结论：`developer_instructions` 是**额外的高优先级 developer 指令层**，不覆盖 base 指令、Skills 说明、权限说明或协作模式，也不是普通 user prompt。因此 Codex 使用该通道接入，`capabilities.systemPrompt` 为 `"developer"`，冻结记录 `channel` 为 `developer`。

保留边界：`codex exec` 的 `-c` 与 `debug prompt-input` 的 `-c` 由同一配置层解析，但上述证据是 prompt 组合结果，而非真实模型调用验收；`-c` 值按 TOML 解析，因此平台用 JSON 字符串（合法的 TOML basic string）编码多行正文，保证引号、反斜杠与换行原样往返。

参数值始终作为**一个 argv 元素**传递，或从文件/stdin 读入，不经过 shell。因此多行文本、`$()`、反引号和引号都保持字面值，不会被执行，也不会进入命令历史。

## 预置原则

四项编码原则默认全部勾选，可全部取消、任意组合，或完全自定义正文：

| ID | 标题 |
| --- | --- |
| `first-principles` | 第一性原理 |
| `high-cohesion-low-coupling` | 高内聚低耦合 |
| `behavior-preserving-refactor` | 保证功能完全不变前提下的局部重构 |
| `no-unnecessary-entities` | 如无必要勿增实体 |

解析后的正文由固定引导句 + `### 工作原则` 列表 + 可选的 `### 用户追加要求` 组成。四段文本是平台常量，不读取磁盘上的可编辑文件，避免运行期被静默改写。

## 入口

统一负载形状（HTTP 与 manifest 相同）：

```json
{
  "promptConfig": {
    "principleIds": ["first-principles", "high-cohesion-low-coupling"],
    "customText": "先给结论，再给证据。"
  }
}
```

- `principleIds` 省略 = 四项全选；`[]` = 不注入任何原则。
- `customText` 省略或为空 = 不追加自定义正文；上限 8000 字符，解析后正文上限 20000 字符。
- 两者都为空时 `mode` 为 `none`，载体启动参数中不会出现追加参数。
- 载体是否默认可注入：`pi`、`claude`、`codex` 均支持，默认注入四项原则；无注入通道的载体默认 `principleIds: []`（`mode: none`），只有显式注入请求才被拒绝。
- 便捷字段：`systemPrompt`（字符串，等价于 `customText`）与 `principles`（数组），用于旧脚本或简短调用；`promptConfig` 优先。

入口清单：

| 入口 | 用法 |
| --- | --- |
| HTTP | `POST /v2/agents` 的 `promptConfig`；`GET /v2/agents/:id` 返回冻结结果；`GET /v2/prompts` 返回原则目录与默认勾选项 |
| CLI | `pihub agents create ... --principles all\|none\|ID,ID`、`--system-prompt TEXT`、`--system-prompt-file FILE`、`--system-prompt-stdin`；`pihub prompts list` |
| CLI（旧入口） | `pihub spawn` 与 manifest `schemaVersion: 1` 支持透传 `promptConfig` |
| Manfiest v2 | 顶层 `promptConfig` 作为默认值，单个 Agent 的 `promptConfig` 覆盖同名键；也接受 `systemPrompt` / `principles` |
| 面板 | 「创建 Agent」中的「System Prompt 追加」区：四项勾选 + 自定义正文；详情页显示追加摘要、原则标签、字符数和正文 |

`--system-prompt`、`--system-prompt-file`、`--system-prompt-stdin` 三者互斥；长文本或多行文本应使用文件或 stdin。

示例（正文从文件读取，避免 shell 转义与历史泄露；示例内容为占位文本，不含任何凭据）：

```bash
printf '%s\n' '先给结论。' '每个结论附证据。' > /tmp/team-prompt.txt
pihub agents create --carrier pi --name reviewer --cwd /absolute/path/to/project \
  --principles first-principles,no-unnecessary-entities \
  --system-prompt-file /tmp/team-prompt.txt --json
```

## 冻结记录

创建时解析一次，结果写入两处：

- Agent 私有状态目录 `<hub home>/agents/<id>/state/prompt-config.json`（0600），作为运行期唯一来源；
- 存储列 `agents.prompt_config_json`（0600 数据库），作为可查询投影。该列由日志/存储维护方提供；列尚不存在时平台只使用私有文件，不影响注入行为。

记录字段：`schemaVersion`、`mode`（`append` / `none`）、`carrier`、`channel`（`append` / `developer`）、`support`、`principleIds`、`customText`、`prompt`（最终正文）、`digest`（`sha256:` + 规范化 JSON 哈希）、`resolvedAt`。`inspect` 返回的 `promptConfig` 含 `channel`、`principleIds`、`customTextChars`、`promptChars`、`digest`，详情视图另含完整正文。

- `restartAgent` 读取该冻结记录，**不**重新解析默认值，因此后续修改全局默认不会改变已有 Agent。
- 事件 `agent_created` 的 `payload.prompt` 只含摘要（模式、原则 ID、字节数、digest），不重复写入正文。
- 详情接口返回摘要和正文；快照列表只返回摘要，避免每次轮询放大响应体积。

## 面板与接口约定

- `GET /v2/prompts` 返回 `{ principles: [{ id, title, text }], defaultPrincipleIds: [...] }`，面板据此渲染勾选项，不在前端硬编码原则文本。
- 载体能力字段 `capabilities.systemPrompt`：`"append"`、`"developer"` 或 `null`。面板按通道显示生效方式；对 `null` 载体禁用控件并说明平台会拒绝，而不是把系统提示降级为普通用户消息。
- 原始列不会直接暴露：`prompt_config_json` 在接口响应中被剥离，统一以 `promptConfig` 视图返回。

## 边界与未完成项

- **未用真实模型验收。** 当前只验证到「参数被正确构造并传给载体进程」。真实上游接受该参数并影响模型行为，需要在后续真实凭据环境中验证。
- **Codex 走 developer 层，不是 system 层。** 冻结记录明确标注 `channel: developer`；它比 user 消息优先，但不改写 base 指令。`model_instructions_file` 属替换语义，平台不使用。
- **不是安全边界。** 追加的系统提示不能限制 Agent 读取文件、凭据或访问网络；它只是行为约束文本。
- **Claude 仅在 `--bare` 模式下按现有平台参数启动。** 该组合未做真实模型调用验收；若上游行为变化，`capabilities.systemPrompt` 应改为 `null` 并明确拒绝。
- 面板不提供「编辑已有 Agent 的系统提示」：冻结后不可原地修改，需要新建 Agent。
