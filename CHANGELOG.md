# Changelog

## 3.0.0.0 - 2026-10-21（Rust 本地 development，未发布/未部署）

- Rewrote the application-owned runtime layer in Rust under `rust/`: CLI, daemon, SQLite store, prompts, skill registry, Skills Manager connection, Pi/Codex/Claude carriers, CC Switch wrapper, loopback HTTP API and Rust SSR dashboard.
- Kept the accepted 2.1 Node implementation only as a read-only behavioral reference; the Rust binary does not invoke the old Hub Node implementation. External Pi/Codex/Claude executables remain external dependencies.
- Added one Cargo workspace with five packages (`pihub-contracts`, `pihub-core`, `pihub-runtime`, `pihub-app`, `pihub-bin`) and a single locked dependency graph.
- Preserved 2.1 contracts: schema/migrations/update allowlist/cursors, 8-delta + 32-wrapper filtering, distinct prompt/skill digests, UTF-16 limits, literal/canonical manager path alias rules, v1/v2 views, daemon state files and graceful stop.
- Added native CLI/daemon integration coverage under a private `/tmp` home; ordinary CLI paths use only `LocalOps` + loopback HTTP, while only the hidden `__daemon` branch constructs core/runtime.
- Switched the repository default `pihub` entry to a Rust launcher (`bin/pihub`) with an explicit `pihub-legacy` Node 2.1 entry; the launcher fails closed with build instructions when no native binary exists and never silently falls back to Node.
- Set npm metadata to `3.0.0`, synchronized `package-lock.json`, and added `docs/releases/v3.0.0.0.md` plus a Cargo-only build/verify entry that uses existing Cargo without machine-specific toolchain paths.
- Scope note: local development only. No GitHub Release, deployment, production switch, remote push or Git tag/commit was performed by this work.

## 2.1.0.0 - 2026-10-20（源码状态，未发布）

- Added multi-carrier support for Pi, Codex CLI, and Claude Code CLI behind one create/prompt/event/stop surface.
- Added a content-addressed Skill registry with frozen per-agent authorization and an opt-in, read-only Skills Manager connection.
- Added append-only System Prompt injection: four preset coding principles (default checked, all-deselectable), custom text, and per-carrier channels (`--append-system-prompt` for Pi/Claude, `-c developer_instructions` for Codex, verified with `codex debug prompt-input`).
- Froze the resolved prompt per agent in `agents.prompt_config_json` plus a private state file; restarts reuse it and `inspect` reports channel, principles, and digest.
- Added `pihub prompts list`, `pihub skills manager status|connect|sync|disconnect`, `pihub agents create --principles/--system-prompt(-file/-stdin)`, `GET /v2/prompts`, and `/v2/skills/manager` routes.
- Replaced per-agent protocol/stderr files with one bounded Hub log family (rotation, size caps, backpressure drop counters) and filtered streaming deltas out of SQLite and logs.
- Added a dashboard System Prompt section and a manual Skills Manager connection panel.
- Kept `/v1`, `pihub spawn`, and manifest v1 response shapes unchanged; `agents` gains an append-only `prompt_config_json` column.
- Scope note: this is a source-state release candidate. No GitHub Release was published. Source and independent deployments do not sync automatically; no existing deployment was replaced or verified against these changes.

## 1.0.0.0 - 2026-09-19

- Added a local CLI and daemon that supervise any number of isolated Pi RPC processes.
- Added independent agent sessions, contexts, logs, lifecycle control, batch manifests, and a unified dashboard.
- Added one shared, file-only AI API configuration contract with private permissions and secret-safe validation.
- Locked Pi Agent source and npm runtime to official release `v0.85.1`.
- Added deterministic runtime bootstrap, test coverage, source/runtime separation, and public release documentation.
