# Rust 3.0 本地运行手册

状态：**3.0.0.0 local development**。本目录是应用自有运行层的 Rust 重写；
Node 2.1 实现只作为已验收的行为参考，不是 3.0 的默认执行路径。尚未发布、
未部署、未切换生产，也没有 Git 提交/tag/push。

## 包与依赖方向

```text
pihub-contracts  <-  pihub-core
                 <-  pihub-runtime
                 <-  pihub-app
core/runtime/app <-  pihub-bin (仅装配)
```

| crate | 包名 | 职责 |
| --- | --- | --- |
| `crates/contracts` | `pihub-contracts` | 共享类型/窄接口 |
| `crates/core` | `pihub-core` | Store、生命周期、prompts、skills、manager connection、日志 |
| `crates/runtime` | `pihub-runtime` | Pi/Codex/Claude 载体、CC Switch |
| `crates/app` | `pihub-app` | CLI、HTTP、Rust SSR、LocalOps |
| `crates/bin` | `pihub-bin` | `pihub` 二进制装配 |

唯一依赖锁是 `rust/Cargo.lock`；共享版本在 `rust/Cargo.toml` 的
`[workspace.dependencies]`。不要在 crate 内另加版本或解锁依赖。

## 构建与隔离测试

```bash
rust/scripts/verify.sh
# 或默认 npm 验证入口
npm run check
```

脚本使用现有 Cargo，不硬编码本机工具链；可用 `PIHUB_RUST_TOOLCHAIN`
显式指向已有 toolchain 的 bin 目录（仅本次进程生效）。默认
`CARGO_HOME=${TMPDIR:-/tmp}/pihub-cargo-home`、
`CARGO_TARGET_DIR=${TMPDIR:-/tmp}/pihub-rust-target`、`CARGO_BUILD_JOBS=2`；
均可显式覆盖。脚本运行 workspace 测试、构建 debug/release 二进制，并检查
native launcher 与“无 binary 时不回退 Node”的错误路径。集成测试使用私有
`/tmp` home、随机 loopback 端口和有界超时；不读取生产 DB、不触碰生产 daemon。

构建入口：

```bash
npm run build                 # = bash rust/scripts/build-release.sh
```

产物（默认 target，可用 `CARGO_TARGET_DIR` 覆盖）：

```text
${TMPDIR:-/tmp}/pihub-rust-target/debug/pihub
${TMPDIR:-/tmp}/pihub-rust-target/release/pihub
```

根目录 `./bin/pihub` 是原生 launcher；它按 `PIHUB_RUST_BIN`、
`CARGO_TARGET_DIR`、默认 target 的顺序查找二进制，找不到时只打印构建指令。
旧 Node 2.1 使用独立入口 `./bin/pihub.js` 或 `pihub-legacy`，不会被静默调用。

## 运行

```bash
# 固定 app root（部署/源码树之外的 home）
export PIHUB_HOME=/tmp/pihub-3.0-home
export PIHUB_APP_ROOT="$PWD"   # 含 vendor/ 的仓库或安装根

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/pihub-rust-target}"
BIN="$CARGO_TARGET_DIR/release/pihub"
$BIN --version                 # 3.0.0.0
# 或：./bin/pihub --version
$BIN config init --json
$BIN config validate --json
$BIN daemon start --json       # 隐藏 __daemon 分支才构造 Store/Manager/RuntimeFactory
$BIN daemon status --json
$BIN panel --no-open           # 需要已启动 daemon
$BIN daemon stop --json
```

普通 CLI 命令（含 `config`/`doctor`/daemon 启停）只使用 `LocalOps` 和
loopback HTTP client，不会打开 daemon 的 SQLite、不会执行
`markInterrupted`、也不会创建载体状态。daemon 进程是唯一状态写入者。

`PIHUB_HOME` 必须位于源码/app root 之外。通过 `./bin/pihub` 启动时，launcher
在 `PIHUB_APP_ROOT` 未设置的情况下会无条件传入仓库根（显式值优先）；直接执行
`$BIN` 时，二进制会向上查找 `vendor/pi.lock.json`，再回退到 `<exe>/../..`
（部署布局 `<app_root>/bin/pihub`）。`PIHUB_PI_CLI` 可显式覆盖 Pi bundle 路径。

开发/试用时不要依赖继承的 `PIHUB_HOME`：如果当前 shell 已指向在用实例，照抄
上面的 `daemon start`/`daemon stop` 会操作那个实例。要跑带 daemon + panel 的
完整开发环境，用 `scripts/qa-instance.sh start|status|stop`（强制显式 canonical
私有 home，并做 receipt + daemon.json + health + 真实进程身份核验）。

## 关键约束

- 运行时事件经有界 channel 交给 core；runtime 不写 SQLite。
- Hub 日志是单个 inline、同步、按容量/档数轮转的 file-rotate family，无额外
  队列或伪 pressure-drop 策略。
- 8 个 streaming delta 类型不落库、不消耗 cursor；`carrier_event` 最多解包
  32 层。
- prompt digest 与 skill authorization digest 算法不同；customText/prompt
  限制按 UTF-16 code units。
- Skills Manager 的 registry / manager 两条 policy 路径按 2.1 保留不同的
  reserved-name 与错误码契约，只共享纯 token 判定。
- 不实现进程池/worktree；不增加 WASM/React 或第二套 daemon 状态。

## 当前边界

独立 parity 代理（`/tmp/pihub-rust-parity.json`）和 Gemini 实际 UI 验收仍是
外部门槛。本文档记录的是可构建、可运行、可隔离测试的本地 development 版本，
不宣称最终验收通过。
