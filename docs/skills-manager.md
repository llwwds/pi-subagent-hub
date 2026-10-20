# Skills Manager 手动连接

本页描述 `pi-subagent-hub` 如何**手动连接**本机已安装的 `skills-manager` 桌面应用，并让该应用继续作为 Skill 的唯一托管方。实现位于 `src/skills/manager-connection.js`，不属于 PiHub 自建的第二个 Skill Manager。

## 1. 真实管理器事实

| 项 | 实测值（macOS，2026-10） |
| --- | --- |
| 应用本体 | `/Applications/skills-manager.app`（`1.41.0`） |
| CLI | `/Users/llwwds/.skills-manager/bin/skills-manager-cli`（`1.41.0`） |
| 应用基目录 | `~/.skills-manager`（可用 `SKILLS_MANAGER_HOME` 覆盖） |
| 数据库 | `~/.skills-manager/skills-manager.db`（WAL，GUI 与本 CLI 共用） |
| 中央技能库 | `~/.skills-manager/skills`（实测 54 份） |

管理器**没有** HTTP/socket 服务。它与外部集成的唯一真实协议就是共享核 CLI：

```
skills-manager-cli [--json] <repo|agents|skills|presets|git> ...
```

PiHub 只使用其中五个只读查询（`READ_ONLY_COMMANDS`）：

- `repo status` → `base_dir`、`skills_dir`、`skill_count`、`active_preset_id`
- `presets list` → `[{id,name,skill_count,active}]`
- `skills list` → `[{id,name,description,path,enabled,tags,source_type,source_ref,preset_ids,presets,deployed_to}]`
- `skills show <id>` → `{name,path,skill_file,files,markdown}`，仅用于解析 `path` 目录下没有根 `SKILL.md` 的条目
- `agents list` → `[{key,display_name,installed,skills_dir,is_custom,has_path_override,...}]`

PiHub 从不调用 `skills deploy|undeploy|export|sync`、`presets apply|deploy`、`agents add-custom|enable|disable`、`repo set-path`、`git *` 等会改变管理器状态或全局分发目标的命令。代码在 `assertReadOnly()` 里以白名单硬性拦截。

> 说明：管理器 CLI 自身在查询时会写 `~/.skills-manager/.skills-manager.lock`（`operation=reindex sync metadata`）并刷新自身索引。这是管理器内部簿记，不是 PiHub 的写操作，PiHub 也不读取该文件。
>
> `--skills-root` 会在 `~/.skills-manager/external/<name>-<hash>/` 下新建数据库。PiHub **不使用**该参数，避免向管理器基目录写入。

## 2. 连接模型

```text
skills-manager（技能内容 + 元数据 + 分发，唯一托管方）
        │  只读 CLI 查询（repo/presets/skills/agents）
        ▼
$PIHUB_HOME/skills/manager-connection.json   连接配置（0600）
$PIHUB_HOME/skills/manager-catalog.json      catalog 快照（0600）
        │  sync
        ▼
$PIHUB_HOME/skills/objects/<sha256>/         创建 agent 时冻结的内容副本（0500，只读）
```

- **手动**：`connect` 必须由用户显式调用，不存在自动注册、自动部署或后台轮询。
- **管理器源只读**：所有查询走白名单；PiHub 不写管理器任何文件。
- **PiHub 只写自己的 home**：两个 JSON 文件 + 冻结对象目录，写入前拒绝符号链接与其他非普通文件。
- **断开不破坏已有 agent**：`disconnect()` 只删除连接配置；快照与冻结对象默认保留。

### 连接配置

```json
{
  "schemaVersion": 1,
  "connectedAt": "2026-10-20T14:00:00.000Z",
  "lastSyncedAt": "2026-10-20T14:00:00.000Z",
  "cliPath": "/Users/llwwds/.skills-manager/bin/skills-manager-cli",
  "managerBaseDir": "/Users/llwwds/.skills-manager",
  "managerSkillsDir": "/Users/llwwds/.skills-manager/skills",
  "managerVersion": "1.41.0",
  "agentKey": null,
  "selection": { "presetIds": [], "tags": [], "ids": [] },
  "source": "manual"
}
```

`selection` 让连接只纳入管理器某个预设/标签/技能集合（例如 `{"presetIds":["<preset-uuid>"]}`），从而由管理器的预设继续决定 PiHub 的可用范围。`agentKey` 仅记录用户**手动**注册给 PiHub 的管理器 agent key，PiHub 自己不做注册。

### Catalog 快照

```json
{
  "schemaVersion": 1,
  "syncedAt": "2026-10-20T14:00:00.000Z",
  "manager": { "cliPath": "...", "version": "1.41.0", "baseDir": "...", "skillsDir": "...", "mode": "cli", "activePresetId": "...", "presets": [] },
  "digest": "sha256:<64 hex>",
  "skills": [{ "id": "ast-grep", "name": "ast-grep", "path": "/Users/llwwds/.skills-manager/skills/ast-grep", "entryPath": "/Users/llwwds/.skills-manager/skills/ast-grep", "tags": [], "presets": ["llwwds常用 skills"], "presetIds": ["..."], "deployedTo": ["codex"], "sourceType": "local" }],
  "invalid": [{ "id": "llwwds-standards", "code": "invalid_manager_skill", "reason": "manager cannot resolve a SKILL.md entry for llwwds-standards: ..." }],
  "warnings": []
}
```

`digest` 对 `id/name/path/enabled/tags/presets/presetIds/deployedTo/sourceType` 做规范化 SHA-256，用于漂移判断。

- `entryPath`：真正含 `SKILL.md` 的目录。普通条目等于 `path`；管理器通过 `skills show` 明确报告的嵌套条目则为其 `skill_file` 的父目录。
- `invalid`：**在本次连接选择范围内**却无法导入的条目。
- `warnings`：**不在选择范围内**且无法导入的条目，只作为 catalog 警告，不影响连接新鲜度。

## 3. 生命周期

| 能力 | 行为 | 失败语义 |
| --- | --- | --- |
| `discoverManager()` | 仅探测 CLI 存在与 `--version`，不读数据库 | 返回 `available:false` + `reason` |
| `connect()` | 校验 CLI/版本、`repo status`、`presets list`、`skills list`、可选 `agents list`，写快照再写配置 | 任何一步失败都抛 `ManagerConnectionError`，不留下半连接状态 |
| `status({live})` | 读配置/快照，返回 `freshness`/`availability`/`source`；`live` 时再查一次管理器并给出 `drift` | 配置损坏时返回 `error` 字段而不抛异常 |
| `sync()` | 重新查询并原子替换快照 | 失败返回 `{synced:false, error, keptSnapshot}`，**磁盘上的上次完整快照保持不变** |
| `disconnect()` | 删除连接配置，保留快照与冻结对象 | 幂等；`purgeSnapshot:true` 才删快照 |
| `listSkills(filters)` | 从快照读，支持 `query/preset/tag/deployedTo` | 快照缺失时返回空数组 |
| `resolveSources(policy)` | 解析 `{mode:'all'}` / `{mode:'only',ids}` 为真实 `entryPath` 目录 | 目录消失报 `manager_source_missing`，越界/符号链接报 `unsafe_manager_skill_path` |
| `resolveAuthorization(policy, {registry})` | 逐份 `registry.importSkill()` 冻结内容，再返回精确范围授权 | 与 `SkillRegistry` 的冻结/校验路径一致 |

### 新鲜度与漂移

`status()` 把三个维度分开返回，避免“一个坏条目毁掉整个连接”的误判：

| 字段 | 含义 |
| --- | --- |
| `snapshot.freshness = {ageMs, stale}` | 仅快照时效（默认 24h） |
| `source = {skillsDirOk, missing, checkedAt}` | 管理器目录与已选 `entryPath` 是否还在 |
| `snapshot.availability = {selectedInvalid, warnings, invalidCount, warningCount}` | 条目可用性，按选择范围拆分 |
| `snapshot.selectedIds` | 当前快照授权的技能 id 列表 |
| `snapshot.stale` / `staleReason` | 仅当 `freshness.stale`、源目录消失、或**选中范围内**有条目不可导入时为 `true`；范围外坏条目不会置 `stale` |

`status({live:true})` 额外返回：

```json
{ "drift": { "added": [], "removed": [], "changed": false, "digest": "sha256:…", "catalogUnavailable": ["llwwds-standards"] } }
```

- `added`/`removed` 只统计**全新或真正消失**的条目；
- `catalogUnavailable` 列出仍存在于目录、但已知不可导入的条目（不会被误报为新增）；
- `changed` 为「有 added/removed」或「已知条目元数据变化」。

不可导入条目**不会**被静默当作新鲜内容：

- 在选择范围外 → 进入 `snapshot.warnings`，`status` 可见，不影响 `stale`；
- 在选择范围内（含 preset 范围）→ 进入 `snapshot.invalid`，`stale=true` 且 `staleReason` 明确；
- 显式 `{mode:'only',ids:[...]}` 请求该 id → 直接失败并给出管理器自身错误（`invalid_manager_skill`）。

### 嵌套入口解析

管理器目录中可能出现 `path` 下没有根 `SKILL.md` 的条目。PiHub 不猜测目录结构，而是读取管理器自身的权威答案：

1. `path/SKILL.md` 存在 → 直接使用该目录；
2. 否则调用 `skills show <id> --json`，取 `skill_file`；
3. `skill_file` 必须名为 `SKILL.md`、是普通文件、父目录存在于 `managerSkillsDir` 内；
4. 条目目录必须落在库内，且**库内部不得经过任何符号链接**（用 `realpath` 与「字面根 → 真实根」映射逐段比较），越界/链接一律 `unsafe_manager_skill_path`。

实测：本机 54 份条目中只有 `llwwds-standards` 无根 `SKILL.md`，而管理器自己的 `skills show` 明确返回 `no SKILL.md found`，因此它被如实登记为不可导入条目的**逐项错误**，而不是被猜测成嵌套技能。

### 冻结与“新增/移除只影响新 agent”

`resolveAuthorization()` 通过 `SkillRegistry.importSkill()` 把管理器目录内容复制到 `$PIHUB_HOME/skills/objects/<sha256>`（0500 只读），并返回 `{mode:'only', ids:[...]}` 的精确授权：

- `sync` 后新增的 skill 立即出现在新 agent 的可选范围；
- `sync` 后被移除的 skill 不再进入新 agent 的授权；
- 已创建 agent 持有的是冻结 grant 与内容副本，管理器内容变化或 `disconnect()` 都不影响其校验。

## 4. 手动注册为管理器分发目标（可选，由用户执行）

如果希望**管理器界面**直接向 PiHub 分发 skill，可把 PiHub 自己的目录注册为管理器自定义 agent。PiHub 只提供命令文本，不执行：

```js
connection.manualRegistrationCommand({ key: "pihub", displayName: "PiHub" });
// { argv: [cli, "agents", "add-custom", "pihub", "--path", "<PIHUB_HOME>/skills/manager-target", "--name", "PiHub", "--json"], executed: false }
```

- 目标目录固定为 PiHub home 内的 `skills/manager-target`（可通过 `path` 显式覆盖为其他 PiHub 自有目录）。
- 注册动作会写管理器数据库，属于**用户**决定的对外部应用写操作，PiHub 不会自动执行。
- 注册后由管理器负责把技能部署到该目录；PiHub 仍通过 `connect/sync` 读取 catalog，二者不冲突。

## 5. 集成入口（供 CLI/HTTP 层接入）

```js
import { SkillManagerConnection, discoverManager } from "../skills/manager-connection.js";

const connection = new SkillManagerConnection({ home: paths.home });

await connection.status({ live: true });          // GET  /v2/skills/manager
await connection.connect({ cliPath, selection });  // POST /v2/skills/manager/connect（显式手动）
await connection.sync();                           // POST /v2/skills/manager/sync
connection.disconnect();                           // POST /v2/skills/manager/disconnect
```

创建 agent 时把 `skillPolicy` 交给 `resolveAuthorization(policy, { registry })`，即可在保持 `SkillRegistry` 冻结语义的前提下由管理器提供技能集合。

### 错误码

`manager_unavailable`、`manager_not_connected`、`manager_query_failed`、`manager_catalog_empty`、`manager_source_missing`、`manager_snapshot_missing`、`manager_write_refused`、`unsafe_manager_path`、`unsafe_manager_skill_path`、`invalid_manager_config`、`invalid_manager_snapshot`、`invalid_manager_selection`、`invalid_manager_agent`、`invalid_manager_skill`、`invalid_skill_policy`、`skill_not_found`。

## 6. 安全边界

- 只允许四个只读管理器命令；其余一律 `manager_write_refused`。
- 不读取、不落盘管理器数据库、密钥、`.secret.key` 或任何凭据；配置与快照 ~0600，且不含凭据。
- skill 源路径必须是管理器技能库内的真实目录（`realpath` 包含校验），拒绝符号链接、`..` 路径与库外目录，避免路径遍历与覆盖。
- 写入 PiHub home 前拒绝符号链接与非普通文件，写入按原子替换，目录 0700。
- 不自动注册全局、不改其他应用的 skill 目录；`agents add-custom` 只作为显式命令文本给出。

## 7. 验证

```bash
node --test test/skills-manager-connection.test.js
```

- 19 个测试全部使用 fixture CLI + 临时独立 home，不访问真实管理器数据库、不调用模型。
- 覆盖：手动连接、只读命令白名单、快照/配置权限、preset/tag/query 过滤、显式 selection、**管理器报告嵌套 `skill_file` 的解析与冻结**、**范围外坏条目仅作 warning 不置 stale**、**显式 id 遇坏条目快速失败**、**`skill_file` 越界/跨符号链接拒绝**、路径越界与符号链接防护、缺失源标记、同步失败保留上次快照、live 漂移不误报不可用条目、断开后冻结授权仍有效、手动注册命令不执行。
- 有界真实烟测（`skills-manager-cli 1.41.0` + `/tmp` 独立 home，未注册 agent、未部署、未改管理器内容）：
  - 只选 `ast-grep`：`status.stale=false`，`warnings=[llwwds-standards]`，冻结 grant `a4c7647b…46eca6` 校验通过；
  - `llwwds常用 skills` preset：19 条中 18 条可用，`llwwds-standards` 进入 `invalidSources` 并给出管理器自身错误，`drift.added=[]`、`catalogUnavailable=[llwwds-standards]`，`resolveSources` 返回 18 条；
  - `disconnect` 后快照保留、冻结授权仍有效。
