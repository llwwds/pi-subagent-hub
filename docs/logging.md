# 日志与事件持久化

本文定义 `pi-subagent-hub` 2.1 的诊断日志与语义事件边界。目标是：语义结果可靠、可回读；诊断日志体量有上限；任何 Agent 都不能凭日志拖垮 daemon。

## 1. 分层：SQLite 存语义，文件存诊断

| 层 | 内容 | 位置 |
| --- | --- | --- |
| 语义事件 | `agent_created`、`agent_ready`、`agent_settled`、`message_end`、终态、启动/协议/进程错误等 | `$PIHUB_HOME/state/hub.sqlite` 的 `events` 表 |
| 诊断日志 | 协议安全摘要、stderr、Agent 生命周期备注、背压丢弃汇总 | `$PIHUB_HOME/state/logs/hub.log` 及旋转档 |

流式增量不是历史：它们不写 SQLite、不消耗 event cursor，也不复制进诊断日志。被过滤的增量从未插入 `events`，所以 `cursor` 天然连续，不伪造 floor、不删除历史行。

### 流式增量的判定范围（2.1）

| 类别 | 事件 | 是否持久 |
| --- | --- | --- |
| 增量，过滤 | `message_update`、`message_delta`、`text_delta`、`thinking_delta`、`content_block_delta`、`stream_event` | 否 |
| 增量，过滤 | `tool_execution_update`（Pi 反复发送累计 `partialResult`） | 否 |
| 增量，过滤 | `item.updated`（Codex 每块流式输出一次） | 否 |
| 边界与结果，持久 | `tool_execution_start`、`tool_execution_end`、`item.completed`、`message_end` | 是 |
| 错误与终态，持久 | `error`、`protocol_error`、`process_error`、`process_exit`、`agent_settled`、`agent_start_failed` | 是 |
| 未知语义 | 其余未知 `type`（如 `turn.started`、`thread.started`） | 是（失败关闭，不猜测为增量） |

判断统一使用 `src/logging.js` 的 `isEphemeralStreamEvent(value)`，避免 manager、rpc-process 和各 carrier 各写一套。

`carrier_event` 会沿 `value` 逐层解包：真实嵌套（≥3 层）同样识别；解包有 `MAX_CARRIER_UNWRAP_DEPTH = 32` 的硬上限，并用 visited 集合防止循环引用。超过上限或出现环时返回 `false`（即按语义事件持久），只做有界防护，不引入新框架。

## 2. Hub 级单一日志文件族

```text
$PIHUB_HOME/state/logs/
  hub.log                 # 当前段
  hub.1.log, hub.2.log …  # 旋转档，最大 5 个
  .hub.log.history        # rotating-file-stream 的档名清单
```

| 限制 | 默认值 | 说明 |
| --- | --- | --- |
| 当前段 | 10 MiB | 单次 write 不会被拆到两个文件 |
| 旋转档总额 | 50 MiB | 按修改时间保留最近的档 |
| 旋转档数量 | 5 | 与总额共同约束 |
| 单条日志 | 16 KiB | 超限写 `truncated` + `preview` |
| 单字符串 | 2048 字符 | 递归截断嵌套字段 |
| 单行 stderr | 8 KiB | 超长部分截断并标记 |
| 压缩 | 关闭 | 不生成 `.gz`，便于 tail |
| 权限 | 文件 `0600`，目录 `0700` | 每次 open/rotate 后再次 chmod |

每个 Hub home 只允许一个 `HubLogWriter`（一个 daemon 进程一个）。`rotating-file-stream` 不是多进程协调器；不要让两个进程写同一目录。

## 3. 背压与丢弃

日志写入直接进入旋转流，不在内存里另建队列：

- 流缓冲达到 `softLimitBytes`（默认 512 KiB）时，丢弃可丢诊断（协议摘要、stderr、普通 note）。
- 达到 `hardLimitBytes`（默认 4 MiB）时，连 error note 也会丢弃；缓冲因此有上界。
- 只累计有界计数器：总数、按 `kind`、按原因（`soft_limit`、`hard_limit`），最多跟踪 16 个 key。
- 流恢复或关闭时写一条 `kind: "log_dropped"` 汇总，包含本次丢弃数与累计数。
- `stats()` 随时返回 `written`、`writtenBytes`、`bufferedBytes`、`dropped`、`error`，供 daemon/诊断接口只读查询。

丢弃只影响诊断，不影响 SQLite 语义事件和最终结果。慢盘、旋转卡顿或 stderr 洪峰都不会让 daemon 内存无界增长。

## 4. 统一接口

```js
import {
  createHubLogWriter,
  createAgentLogWriter,
  resolveHubLogDir,
  isEphemeralStreamEvent,
} from "./logging.js";

const logger = createHubLogWriter({
  dir: resolveHubLogDir(paths), // $PIHUB_HOME/state/logs
  // debug: true,               // 仅显式调试：协议原文，默认关闭
});

const adapter = logger.createAgentLogWriter({
  agentId,
  carrier,          // "pi" | "codex" | "claude"
  source,           // "pi-rpc" | "cli-runtime" | "carrier" 等
  // debug: false,  // 覆盖 Hub 级 debug
});

adapter.protocol("in", command);      // 安全摘要
adapter.protocol("out", value);       // 安全摘要
adapter.protocol("error", { ... });   // 解析错误等
adapter.note("agent_started", { pid });         // level=info，可丢
adapter.note("stdout parse failed", data, "error"); // error 优先保留

child.stderr.pipe(adapter.stderr);    // Writable，缺行缓冲有上限

await adapter.end();                  // 刷新 stderr 残行，不关闭 Hub 流
await logger.close();                 // daemon 退出时调用一次
```

等价入口：`new HubLogWriter(options)`、`createAgentLogWriter(logger, options)`。`dir` 与注入 `stream` 二选一；测试可注入内存流，生产只传 `dir`。

每条日志都是单行 JSON，公共字段：

```json
{"ts":"…","level":"debug","kind":"protocol","agentId":"…","carrier":"pi","source":"pi-rpc","direction":"in","summary":{"type":"prompt","keys":["id","type","message"],"message":{"valueType":"string"}}}
```

`kind` 取值：`protocol`、`protocol_raw`（仅 debug）、`stderr`、`agent_note`、`log_dropped`。Agent 适配器不拥有文件、不拥有队列，只带 `agentId/carrier/source` 引用同一个 Hub writer。

## 5. 安全摘要与 debug

默认 `protocol()` 只记录：方向、类型、命令、id、成功标志、顶层 key、消息角色/块类型/字符数/工具调用数、错误文本（截断）、字节数。**不写消息正文、prompt 正文、工具参数或凭据。**

`debug: true` 时改记 `protocol_raw`，包含协议 JSON 的截断预览。这是显式调试开关，可能包含用户文本；生产默认必须关闭，且日志目录保持私有。

## 6. SQLite 的 `prompt_config_json`

`agents` 表新增：

```sql
prompt_config_json TEXT NOT NULL DEFAULT '{}'
```

- 新建数据库直接建列；既有数据库只执行 `ALTER TABLE … ADD COLUMN`，不 drop、不重写、不搬运行。
- `HubStore.createAgent()` 接受 `prompt_config_json` 字符串并原样保存；`getAgent()`/`listAgents()` 返回原字符串，供 manager 冻结启动 prompt。
- 创建后不可通过 `updateAgent()` 修改，符合“启动即冻结”。
- 该列可能包含完整 prompt 文本。对外 API 输出必须由 manager 的 `promptSummary()` 脱敏；`publicAgent()` 需要排除 `prompt_config_json`（集成方负责，勿直接透传 store 行）。

旧库迁移由临时 home 的 fixture 测试覆盖：`test/logging.test.js` 先建旧 schema 与旧行/事件，再用 `HubStore` 打开，断言列默认值、行数、事件 cursor 和再次打开后的持久性。

## 7. 明确不做

- 不自动做 SQLite retention 删除，不移动 cursor floor；历史语义事件由运维显式处理。
- 不读取、不修改、不清理旧生产部署的数据库、会话或日志。
- 不 gzip 旋转档，不为每个 Agent 建独立日志文件。
- 不把流式 delta（含工具部分输出与 Codex `item.updated`）写入 SQLite 或诊断日志。
- 不在日志里写真实凭据；`debug` 也不做凭据脱敏之外的内容审计。

## 8. 集成要点（A 负责）

1. daemon 启动、`ensureHubLayout()` 后创建唯一 `HubLogWriter`，daemon 退出时 `await logger.close()`。
2. `manager` 为每个 Agent 创建一个轻适配器，随 runtime spec 传给 `rpc-process`/carrier，不新建文件。
3. `rpc-process` 用 `adapter.protocol("in"/"out"/"error")` 替换 `appendFileSync(protocol.jsonl)`，用 `child.stderr.pipe(adapter.stderr)` 替换 `stderr.log`。
4. carrier 的 `carrier_event`/CLI 输出使用同一适配器，`source` 标明来源。
5. runtime 事件写入 SQLite 前用 `isEphemeralStreamEvent()` 过滤；确保 `message_end` 与终态完整落库。
6. 维护 `stats()` 暴露（如需）时只读，不在请求路径同步写大对象。

## 9. 验证

```bash
node --test test/logging.test.js test/store.test.js
```

覆盖：单文件族与 0600/0700、Agent 作用域、安全摘要不含正文、debug 原文上限、旋转档数/总额/无 gzip、背压丢弃与汇总、stderr 跨块多字节与超长行、流式 delta 过滤（含 `tool_execution_update`、`item.updated`、≥3 层 `carrier_event` 与循环引用）、**30 次累计大 payload 更新不新增 event cursor 且 `tool_execution_end`/`message_end` 仍可查**、旧库增量迁移与持久化。测试只使用 `mkdtemp` 临时 home 和注入流，不调用真实模型、不接触生产数据。

## 10. 风险与已知限制

- 单写者假设：多进程共写同一目录会违反旋转协调。
- 慢盘下会主动丢弃诊断；需要完整结果时以 SQLite 为准。
- `debug` 可能记录用户文本，只应短时启用。
- `rotating-file-stream` 在旋转进行中短暂超出总额，旋转结束后收敛到上限。
- 依赖版本锁定为 `3.2.10`，通过 `package-lock.json` 固定。
