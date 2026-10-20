import { canonicalJson, sha256 } from "./utils.js";

// Additive prompt injection only. Every carrier either exposes an append/extra
// instruction channel or explicitly cannot accept one; the Hub never uses a
// replace flag, so carrier-native constraints stay in force.
export const APPEND_SYSTEM_PROMPT_FLAG = "--append-system-prompt";

// Channel per carrier, each verified against the pinned upstream build:
// - pi:     --append-system-prompt (v0.85.1 docs/usage.md + dist/cli/args.js,
//           repeatable; the value is appended by the resource loader)
// - claude: --append-system-prompt (Claude Code CLI 2.1.280 flag table)
// - codex:  extra developer-layer instructions via
//           `-c developer_instructions="..."`. Verified with
//           `codex debug prompt-input`: the text becomes one additional
//           developer content part while skills/permissions/collaboration and
//           all other developer parts stay byte-identical.
export const CARRIER_PROMPT_SUPPORT = Object.freeze({ pi: "append", claude: "append", codex: "developer" });
export const PROMPT_CHANNELS = Object.freeze({ append: "system-prompt-suffix", developer: "developer-instructions" });

export const PROMPT_PRINCIPLES = Object.freeze([
  Object.freeze({
    id: "first-principles",
    title: "第一性原理",
    text: "先回到问题的基本事实和约束，从最根本的原理推导方案；不要因为现有实现或惯例如此就默认接受。",
  }),
  Object.freeze({
    id: "high-cohesion-low-coupling",
    title: "高内聚低耦合",
    text: "让相关逻辑集中在同一处，模块之间只通过最小、明确、稳定的接口协作；避免隐式共享状态和跨层依赖。",
  }),
  Object.freeze({
    id: "behavior-preserving-refactor",
    title: "保证功能完全不变前提下的局部重构",
    text: "可以在外部行为、接口和可观察结果完全不变的前提下做局部重构；重构与功能变更分开，并给出行为未变的验证。",
  }),
  Object.freeze({
    id: "no-unnecessary-entities",
    title: "如无必要勿增实体",
    text: "不为假设中的未来需求增加抽象、配置、依赖或中间层；现有结构能解决就不新建。",
  }),
]);

export const DEFAULT_PROMPT_PRINCIPLE_IDS = Object.freeze(PROMPT_PRINCIPLES.map((principle) => principle.id));

const MAX_CUSTOM_TEXT = 8_000;
const MAX_PROMPT_TEXT = 20_000;
const PRINCIPLE_BY_ID = new Map(PROMPT_PRINCIPLES.map((principle) => [principle.id, principle]));

function invalid(message) {
  const error = new Error(message);
  error.code = "invalid_prompt_config";
  return error;
}

function normalizeCustomText(value) {
  if (value === undefined || value === null) return "";
  if (typeof value !== "string") throw invalid("promptConfig.customText must be a string");
  const text = value
    .replace(/\r\n?/g, "\n")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "")
    .trim();
  if (text.length > MAX_CUSTOM_TEXT) throw invalid(`promptConfig.customText must be at most ${MAX_CUSTOM_TEXT} characters`);
  return text;
}

function normalizePrincipleIds(value) {
  if (value === undefined || value === null) return [...DEFAULT_PROMPT_PRINCIPLE_IDS];
  if (!Array.isArray(value)) throw invalid("promptConfig.principleIds must be an array of principle ids");
  const selected = new Set();
  for (const entry of value) {
    const id = typeof entry === "string" ? entry.trim() : "";
    if (!PRINCIPLE_BY_ID.has(id)) {
      throw invalid(`unknown prompt principle: ${JSON.stringify(entry)}; known ids: ${[...PRINCIPLE_BY_ID.keys()].join(", ")}`);
    }
    selected.add(id);
  }
  // Canonical order keeps the digest stable regardless of UI ordering.
  return PROMPT_PRINCIPLES.map((principle) => principle.id).filter((id) => selected.has(id));
}

function buildPromptText(principleIds, customText) {
  const blocks = [];
  if (principleIds.length > 0) {
    const lines = principleIds.map((id) => {
      const principle = PRINCIPLE_BY_ID.get(id);
      return `- ${principle.title}：${principle.text}`;
    });
    blocks.push(`### 工作原则\n${lines.join("\n")}`);
  }
  if (customText) blocks.push(`### 用户追加要求\n${customText}`);
  if (blocks.length === 0) return "";
  const header = "以下内容由 pi-subagent-hub 追加在载体原有系统提示末尾，只做追加，不替换也不覆盖载体自带的系统约束；如与原有约束冲突，以载体原有约束为准。";
  return `${header}\n\n${blocks.join("\n\n")}`;
}

export function listPromptPrinciples() {
  return PROMPT_PRINCIPLES.map(({ id, title, text }) => ({ id, title, text }));
}

export function carrierPromptSupport(carrier) {
  return CARRIER_PROMPT_SUPPORT[carrier] || null;
}

export function assertPromptSupported(carrier, promptConfig) {
  if (!promptConfig || promptConfig.mode !== "append") return;
  if (!carrierPromptSupport(carrier)) {
    const error = invalid(`carrier ${carrier} cannot inject a system prompt; clear the system prompt or choose another carrier`);
    error.code = "prompt_not_supported";
    throw error;
  }
}

/**
 * Resolve user input into the frozen configuration recorded for one Agent.
 * `promptConfig` accepts `{ principleIds?: string[], customText?: string }`.
 *
 * Carriers without append support (Codex today) must stay usable with default
 * payloads, so the "four principles checked" default only applies where the
 * text can actually be appended. An explicit `principleIds` or non-empty
 * `customText` is a real injection request and is rejected for such carriers.
 */
export function resolvePromptConfig(input, { carrier = "pi", resolvedAt = new Date().toISOString() } = {}) {
  const source = input === undefined || input === null ? {} : input;
  if (typeof source !== "object" || Array.isArray(source)) throw invalid("promptConfig must be an object");
  const customText = normalizeCustomText(source.customText);
  const explicitSelection = source.principleIds !== undefined || customText !== "";
  const defaultIds = carrierPromptSupport(carrier) ? [...DEFAULT_PROMPT_PRINCIPLE_IDS] : [];
  const principleIds = explicitSelection
    ? normalizePrincipleIds(source.principleIds === undefined && customText !== "" ? [] : source.principleIds)
    : defaultIds;
  const prompt = buildPromptText(principleIds, customText);
  if (prompt.length > MAX_PROMPT_TEXT) throw invalid(`resolved system prompt must be at most ${MAX_PROMPT_TEXT} characters`);
  const mode = prompt ? "append" : "none";
  const support = carrierPromptSupport(carrier);
  const digest = `sha256:${sha256(canonicalJson({ schemaVersion: 1, mode, carrier, principleIds, customText, prompt }))}`;
  const frozen = {
    schemaVersion: 1, mode, carrier, channel: support, support, principleIds, customText, prompt, digest, resolvedAt,
  };
  assertPromptSupported(carrier, frozen);
  return frozen;
}

/** Read a frozen record coming from the store column or the private state file. */
export function readFrozenPromptConfig(value) {
  let parsed = value;
  if (typeof parsed === "string") {
    if (!parsed.trim()) return null;
    try { parsed = JSON.parse(parsed); } catch { return null; }
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;
  if (parsed.schemaVersion !== 1 || typeof parsed.prompt !== "string"
    || typeof parsed.digest !== "string" || !Array.isArray(parsed.principleIds)) return null;
  return parsed;
}

/** Public, non-secret view. Text is opt-in because it can be long. */
export function promptSummary(frozen, { includeText = false } = {}) {
  if (!frozen) return null;
  const summary = {
    mode: frozen.mode,
    carrier: frozen.carrier || null,
    // How the text is delivered: "append" = system prompt suffix,
    // "developer" = extra developer-layer instructions.
    channel: frozen.channel ?? frozen.support ?? carrierPromptSupport(frozen.carrier) ?? null,
    support: frozen.support ?? carrierPromptSupport(frozen.carrier) ?? null,
    principleIds: [...frozen.principleIds],
    customTextChars: (frozen.customText || "").length,
    promptChars: frozen.prompt.length,
    digest: frozen.digest,
    resolvedAt: frozen.resolvedAt || null,
  };
  if (includeText) {
    summary.customText = frozen.customText || "";
    summary.prompt = frozen.prompt;
  }
  return summary;
}

/** Launch arguments for an injectable carrier; empty when nothing is injected. */
export function promptLaunchArgs(carrier, frozen) {
  if (!frozen || frozen.mode !== "append" || !frozen.prompt) return [];
  assertPromptSupported(carrier, frozen);
  const channel = carrierPromptSupport(carrier);
  // One argv element, never a shell string: newlines and quotes stay literal.
  if (channel === "append") return [APPEND_SYSTEM_PROMPT_FLAG, frozen.prompt];
  if (channel === "developer") {
    // Codex parses `-c` values as TOML; a JSON string is a valid TOML basic
    // string, so quoting/escaping round-trips exactly for multi-line text.
    return ["-c", `developer_instructions=${JSON.stringify(frozen.prompt)}`];
  }
  return [];
}
