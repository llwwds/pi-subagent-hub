# Rust contracts and crate handoff (2.1 → 3.0)

Status: **3.0.0.0 source frozen locally**. Independent behavioral parity, DOM/SSR
and simplicity verdicts are still external gates; this is not a release or
deployment. Frozen candidate: `/tmp/pihub-rust-release-3.0.0.0-2299ad2e/pihub`
`sha256:2299ad2e5777ab9be01d1acd54e32c330f940f46e893e898e2d21473037b7060`
(9304088 bytes), `rust/Cargo.lock` `94d2df8a…`, native workspace
**214 passed / 0 failed**, 0 warnings. Receipt:
`/tmp/pihub-rust-sourcefreeze.json`.
The accepted Node reference is commit `3c17a78777f56092ecc865e4aab377ee425f2e53`
(tag `v2.1.0.0`) at `/tmp/pihub-js21-reference-20261021`. It is read-only.
Production deployment remains `app/releases/2.0.0.0` (still 2.0). An isolation incident stopped its daemon `73547`; it was recovered as daemon `1724`, and the incident is recorded in `/tmp/pihub-dev-isolation-incident.json`. QA uses only private `/tmp` homes and never targets production 1724 for negative tests.

Repository default entry is now native: root `package.json` version `3.0.0`,
`bin.pihub -> bin/pihub` (Rust launcher), and `bin.pihub-legacy -> bin/pihub.js`
(explicit 2.1 reference). The launcher never falls back to Node when the Rust
binary is missing; it prints the Cargo build instruction and exits non-zero.

## Layout and package names

One Cargo workspace at `rust/`, five packages, one shared `Cargo.lock`.

| Directory | Cargo package | Binary/lib | Owner |
| --- | --- | --- | --- |
| `rust/crates/contracts` | `pihub-contracts` | lib | integrator |
| `rust/crates/core` | `pihub-core` | lib | `pihub-rust-core-20261021` |
| `rust/crates/runtime` | `pihub-runtime` | lib | `pihub-rust-runtime-20261021` |
| `rust/crates/app` | `pihub-app` | lib | `pihub-rust-app-20261021` |
| `rust/crates/bin` | `pihub-bin` | binary `pihub` | integrator |

Dependency direction is one-way:

```text
pihub-contracts  <-  pihub-core
                 <-  pihub-runtime
                 <-  pihub-app
core/runtime/app <-  pihub-bin (assembly only)
```

`core`, `runtime` and `app` must not depend on each other. Cross-crate wiring
uses `pihub-contracts` traits and is assembled by `pihub-bin`.

## Shared dependency lock

All versions/features live in `rust/Cargo.toml` under
`[workspace.dependencies]`; `rust/Cargo.lock` is the only lock. Writers must
not run `cargo update`, regenerate/unlock `Cargo.lock`, or add an extra
dependency version/feature locally. If a capability is genuinely missing,
report it to the integrator; do not fork the dependency graph.

Locked highlights:

- `serde` + `serde_json` with `preserve_order` (payload/event key order stays
  insertion-stable; canonical sorting is done explicitly in contracts).
- `tokio` (process/sync/time/net/signal), `async-trait`, `rusqlite` `bundled`,
  `sha2` 0.10, `uuid` v4, `chrono` (single time library).
- `file-rotate` 0.8 for the Hub log family. Chosen for per-Hub-instance
  writers, byte-capacity rotation and retained-file count. The writer is inline
  and synchronous: there is no extra queue and no pressure-drop path. A failed
  write is counted by kind; `stats()` must report the maintained `byKind`
  counters rather than claiming a drop policy that does not exist. Do **not**
  add `flexi_logger` (global singleton) or `tracing-appender` (time-only
  rotation, no bounded byte/file retention).
- App only: `clap`, `axum`, `reqwest`, `askama`.
- `libc` for `kill(pid, 0)` / process group semantics.

No `Clock`/`IdGen` traits: use `chrono::Utc` and `uuid::Uuid` directly, then
normalize timestamps/ids in cross-language comparisons.

## Interface map

All paths are `pihub_contracts::...`.

| Area | Contract |
| --- | --- |
| Paths/constants | `HubPaths`, `APP_NAME`, `APP_VERSION`, `API_VERSION`, `DEFAULT_TOOLS` |
| Agent | `AgentSpec`, `AgentRecord` (internal/hydrated), `AgentPatch`, `PublicAgent`, `AgentViewV2`, `AgentView`, `SnapshotV1/V2`, `Snapshot` |
| Prompt | `FrozenPromptConfig`, `PromptConfigInput`, `PromptSummary`, `PromptCatalog`, `PromptService` |
| Skill | `SkillPolicy`, `SkillAuthorization`, `SkillGrant`, `SkillRef`, `SkillSummary`, `SkillRegistry` |
| Carrier | `CarrierCapabilities`, `CarrierDiagnostic` |
| Runtime | `RuntimeLaunch`, `RuntimeState`, `RuntimeEvent`, `Runtime`, `RuntimeFactory`, `RuntimeRequestKind` |
| Provider | `Provider`, `ProviderInput`, `ProviderDiagnostics`, `ProviderSnapshot`, `ProviderService` |
| Manager | `ManagerConnection`, `ManagerConnectInput`, `ManagerSyncInput`, `ManagerDisconnectInput`, `ManagerSkillSource` |
| Store | `Store`, `ListAgentsOptions`, `EventQuery`, `NewEvent`, `StoredEvent` |
| Logging | `LogSink`, `LogRecord`, `LogLevel`, `is_ephemeral_stream_event` |
| App boundary | `HubApi`, `SkillManagerAction`, `LocalOps` |
| Wire | `Envelope<T>`, `ErrorBody`, `HubError` |

### Sync/async rule

- `Store`, `PromptService`, `ManagerConnection::resolve_sources` and
  `LogSink::record` are synchronous. Store must not be called while holding an
  async lock; never hold a SQLite statement/transaction across `.await`.
- `SkillRegistry`, `ManagerConnection` mutations/status, `Runtime`,
  `RuntimeFactory` diagnostics, `ProviderService`, `HubApi` and `LocalOps` use
  `#[async_trait]` for object safety. `pihub_contracts::async_trait` is
  re-exported.
- Runtime events use the bounded
  `tokio::sync::mpsc::Sender<RuntimeEvent>` created by core
  (`RUNTIME_EVENT_CHANNEL_CAPACITY = 1024`). Runtime never writes SQLite.

### Internal vs public

`AgentRecord` contains internal paths and frozen authorization. `PublicAgent`
is the v1 wire view; `AgentViewV2::from_record` adds `carrier`, `skillPolicy`,
`skillIds`, `skillDigest` and `promptConfig`. Do not add internal columns to
either public view. v1 detail deliberately has no `promptConfig`, no carrier,
no skill policy and no raw `*_json` columns.

What both public views **do** carry, matching 2.1 `publicAgent()` exactly, is the
hydrated profile projection: `tools`, `skills` and `extensions`. `skills` holds
the frozen grant objects `{id,path,sha256}` when the Agent has a skill
authorization and the profile resources `{path,sha256}` otherwise; there is no
second `grants` field because 2.1 exposes grants only through `skills`,
`skillIds` and `skillPolicy.ids`.

`StoredEvent` keeps both `payload` (parsed) and `payload_json` (the exact stored
text), because 2.1 returns the SQLite row plus the parsed payload and clients
observe both keys. `CarrierDiagnostic.reason` is always serialized, as `null`
when the carrier is available; it is intentionally not `skip_serializing_if`.

## Cross-language invariants to preserve

1. **Prompt digest**
   `sha256(canonicalJson({schemaVersion:1,mode,carrier,principleIds,customText,prompt}))`,
   prefix `sha256:`. Canonical JSON sorts object keys recursively; arrays keep
   order. Use `pihub_contracts::canonical_json` + `sha256_prefixed`.
   `customText` limit 8000 and resolved prompt limit 20000 use UTF-16 code
   units (`utf16_len`), not Rust `chars()`.
2. **Skill authorization digest** is different:
   `sha256(JSON.stringify({mode,grants:[{id,sha256}]}))` in sorted-grant order,
   prefix `sha256:`. Use `skill_authorization_digest`.
3. **Skill package hash** is different again: recursive, names sorted; directory
   header `JSON.stringify(["D", rel])`; file header
   `JSON.stringify(["F", rel, execBit, len])` followed by file bytes. Reject
   symlinks/special files; root `SKILL.md` only; import limits 10 MiB/file,
   50 MiB total, 1000 files; freeze objects `0500`.
4. **Streaming filter** exactly 8 delta types; `carrier_event.value` unwrapped
   up to 32 layers; filtered events neither persist nor consume a cursor.
   Unknown types persist.
5. **Migrations/update allowlist** follow 2.1: append-only columns `carrier`,
   `skill_authorization_json`, `prompt_config_json`; updates only
   `name,status,desired_state,pid,session_file,last_error,provider,model,thinking,updated_at`.
   `agent_id`-scoped event queries use `cursor ASC`, limit clamp `1..=2000`
   (default 500). `mark_interrupted` maps all active states to `crashed`,
   `pid=NULL`, fixed "daemon restarted..." message.
6. **Intentional entry-point differences**: skill-policy normalization in the
   registry and the Skills Manager connection is deliberately not unified.
   The registry path keeps `assertId` semantics (reserved-name rejection,
   `invalid_skill_id`) and the manager path keeps `SKILL_ID` +
   `invalid_skill_policy`; share only the pure token predicate, not error
   codes or reserved-name policy. Skill ids are ASCII, so both sortings agree.
7. **Path safety**: allow system ancestors such as `/tmp` → `/private/tmp`
   by checking both literal and canonical roots; still reject every symlink
   segment inside the manager library; resolve sources to canonical absolute
   directories.
8. **v1/v2 payloads**: v1 legacy-only (`skill_authorization_json IS NULL`);
   v2 detail `includeText=true`, v2 list/snapshot summary only. Manager
   `status/connect/sync/disconnect` keep the exact 2.1 JSON object in
   `serde_json::Value`; do not invent a reduced shape.

## Build and test environment

Scripts use the existing Cargo installation; no machine-specific toolchain
path is committed. Optionally point at an existing toolchain for one command:

```bash
export PIHUB_RUST_TOOLCHAIN=/path/to/toolchain/bin   # optional; process-local
export CARGO_HOME="${CARGO_HOME:-${TMPDIR:-/tmp}/pihub-cargo-home}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/pihub-rust-target}"
export CARGO_BUILD_JOBS=2
cd rust
cargo check -p pihub-contracts --locked
cargo test  -p <your-package> --locked
# or the full isolated entry:
rust/scripts/verify.sh
```

`rust/scripts/verify.sh` and `rust/scripts/build-release.sh` fail with an
instruction if Cargo is absent; they never install a toolchain or change PATH.
Build artifacts stay under the explicit/`TMPDIR` target, never in the sync tree.

### Launcher app root

`bin/pihub` knows the real repository root, so it exports `PIHUB_APP_ROOT` for
the binary it starts **only when the variable is unset**. An out-of-tree build
(`CARGO_TARGET_DIR=/tmp/...`) otherwise leaves the binary with no
`vendor/pi.lock.json` above it and it would derive `appRoot` from
`<target>/release`, making `config paths`, `doctor` (`pi-lock`, `pi-commit`,
`pi-cli`) and `vendor/pi` lookups report the build directory instead of the
deployment. An explicit `PIHUB_APP_ROOT` always wins; when it is unset the
launcher passes the repository root unconditionally — the app root does not
depend on whether `vendor/` has been downloaded, so a missing vendor is reported
as a missing item at the correct root instead of silently diagnosing the
binary's parent directory. `PIHUB_HOME` is never set or changed by the launcher,
and no cwd or device path is guessed. Deployment layout remains
`<app_root>/bin/pihub`.

Tests use private `/tmp` homes/ports/DBs/fixtures, bounded timeouts and
`finally`-style cleanup (`Drop`/explicit close). Do not force-exit to hide
leaked children. Do not touch production state/DB, global Skills Manager,
`vendor/pi`, or any Git state.

## Handoff checks for writers

- Only edit your own crate directory and its `Cargo.toml` **inside** the
  workspace (package section only; shared dependency versions stay in root).
- Start from the contract types; do not duplicate them in your crate.
- Report completion in `/tmp/pihub-rust-{core,runtime,app}.json` with
  `phase=implemented`, commands/results, and any contract change request.
- If a contract is insufficient, report the exact type/method and minimal fix;
  the integrator owns contract changes. Do not edit `contracts`, `bin`, root
  `Cargo.toml`/`Cargo.lock`, or another writer's crate.
