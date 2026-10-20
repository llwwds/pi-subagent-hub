import { chmodSync } from "node:fs";
import { join, sep } from "node:path";
import { StringDecoder } from "node:string_decoder";
import { Writable } from "node:stream";
import { createStream } from "rotating-file-stream";
import { ensurePrivateDir, nowIso } from "./utils.js";

// One Hub-level file family, never one file per Agent. Rotated files hold at
// most maxRotatedBytes in total; the current segment is capped by
// maxSegmentBytes. No compression: readers must stay able to tail a segment.
export const DEFAULT_HUB_LOG_LIMITS = Object.freeze({
  fileName: "hub.log",
  maxSegmentBytes: 10 * 1024 * 1024,
  maxRotatedBytes: 50 * 1024 * 1024,
  maxRotatedFiles: 5,
  maxEntryBytes: 16 * 1024,
  maxStringChars: 2048,
  softLimitBytes: 512 * 1024,
  hardLimitBytes: 4 * 1024 * 1024,
  maxStderrLineBytes: 8 * 1024,
});

// Streaming deltas are progress, not history. They must not consume SQLite
// cursors and must not be copied into the Hub log; the final message_end is
// persisted in full by the caller.
//
// Deltas include partial tool output: Pi re-sends the cumulative
// `tool_execution_update.partialResult`, and Codex emits `item.updated` per
// streamed chunk. Boundary events stay semantic and persisted:
// `tool_execution_start`, `tool_execution_end`, `item.completed`,
// `message_end`, errors and terminal states.
export const STREAM_DELTA_TYPES = Object.freeze(new Set([
  "message_update",
  "message_delta",
  "text_delta",
  "thinking_delta",
  "content_block_delta",
  "stream_event",
  "tool_execution_update",
  "item.updated",
]));

// Carrier payloads are wrapped at most once today, but adapters may nest
// wrappers. The bound keeps the walk finite on hostile input, and the visited
// set keeps a cyclic `carrier_event.value` chain from looping forever.
const MAX_CARRIER_UNWRAP_DEPTH = 32;

const MAX_TRACKED_KEYS = 16;

export function resolveHubLogDir(paths) {
  if (!paths?.stateDir) throw new Error("resolveHubLogDir requires paths.stateDir");
  return join(paths.stateDir, "logs");
}

function isDeltaType(type) {
  return typeof type === "string" && STREAM_DELTA_TYPES.has(type);
}

/**
 * True for a streaming delta, including any bounded chain of `carrier_event`
 * wrappers. Anything else is persisted as semantic history.
 */
export function isEphemeralStreamEvent(value) {
  let current = value;
  const visited = new Set();
  for (let depth = 0; depth < MAX_CARRIER_UNWRAP_DEPTH; depth += 1) {
    if (!current || typeof current !== "object") return false;
    if (isDeltaType(current.type)) return true;
    if (current.type !== "carrier_event") return false;
    if (visited.has(current)) return false;
    visited.add(current);
    current = current.value;
  }
  return false;
}

function positiveInt(value, fallback, name) {
  if (value === undefined) return fallback;
  if (!Number.isInteger(value) || value <= 0) throw new Error(`${name} must be a positive integer`);
  return value;
}

function truncateText(text, maxChars) {
  if (typeof text !== "string") return text;
  return text.length > maxChars ? `${text.slice(0, maxChars)}…[truncated ${text.length - maxChars} chars]` : text;
}

function truncateToBytes(text, maxBytes) {
  if (Buffer.byteLength(text) <= maxBytes) return text;
  return Buffer.from(text).subarray(0, Math.max(0, maxBytes)).toString("utf8");
}

function safeStringify(value) {
  try {
    return JSON.stringify(value);
  } catch {
    return null;
  }
}

function boundValue(value, { maxDepth = 8, maxArray = 50, maxKeys = 50, maxStringChars = 2048 } = {}, seen = new WeakSet(), depth = 0) {
  if (typeof value === "string") return truncateText(value, maxStringChars);
  if (value === null || typeof value !== "object") return value;
  if (seen.has(value)) return "[circular]";
  if (depth >= maxDepth) return "[max-depth]";
  seen.add(value);
  if (Array.isArray(value)) {
    const items = value.slice(0, maxArray).map((item) => boundValue(item, { maxDepth, maxArray, maxKeys, maxStringChars }, seen, depth + 1));
    if (value.length > maxArray) items.push(`[${value.length - maxArray} more items]`);
    return items;
  }
  const result = {};
  const entries = Object.entries(value).slice(0, maxKeys);
  for (const [key, item] of entries) {
    result[key] = boundValue(item, { maxDepth, maxArray, maxKeys, maxStringChars }, seen, depth + 1);
  }
  const remaining = Object.keys(value).length - entries.length;
  if (remaining > 0) result["…"] = `[${remaining} more keys]`;
  return result;
}

function summarizeMessage(message, maxStringChars) {
  if (!message || typeof message !== "object") return { valueType: typeof message };
  const content = Array.isArray(message.content) ? message.content : [];
  const blockTypes = new Set();
  const charsByType = {};
  let toolCalls = 0;
  for (const block of content) {
    const type = typeof block?.type === "string" ? block.type : "unknown";
    blockTypes.add(type);
    if (type === "toolCall" || type === "tool_call" || type === "tool_use") toolCalls += 1;
    const text = typeof block?.text === "string" ? block.text : typeof block?.thinking === "string" ? block.thinking : "";
    if (text) charsByType[type] = (charsByType[type] || 0) + text.length;
  }
  const summary = {
    role: typeof message.role === "string" ? truncateText(message.role, maxStringChars) : null,
    stopReason: typeof message.stopReason === "string" ? truncateText(message.stopReason, maxStringChars) : null,
    contentBlocks: content.length,
    blockTypes: [...blockTypes].slice(0, 12),
    charsByType,
    toolCalls,
  };
  if (typeof message.text === "string") summary.textChars = message.text.length;
  return summary;
}

/**
 * Safe, non-secret view of one protocol value. Text, prompt bodies and tool
 * payloads are reduced to counts; only an explicit debug writer records raw
 * protocol JSON. `direction` is "in", "out" or "error".
 */
export function summarizeProtocol(direction, value, { maxStringChars = 512 } = {}) {
  const summary = { direction: String(direction ?? "unknown") };
  if (!value || typeof value !== "object") {
    summary.valueType = value === null ? "null" : typeof value;
    if (typeof value === "string") summary.chars = value.length;
    return summary;
  }
  const raw = safeStringify(value);
  if (raw !== null) summary.bytes = Buffer.byteLength(raw);
  if (value.type !== undefined) summary.type = truncateText(String(value.type), maxStringChars);
  if (value.id !== undefined) summary.id = truncateText(String(value.id), maxStringChars);
  if (value.command !== undefined) summary.command = truncateText(String(value.command), maxStringChars);
  if (typeof value.success === "boolean") summary.success = value.success;
  if (value.carrier !== undefined) summary.carrier = truncateText(String(value.carrier), maxStringChars);
  summary.keys = Object.keys(value).slice(0, 20);
  if (value.error !== undefined && value.error !== null) summary.error = truncateText(String(value.error), maxStringChars);
  if (value.message !== undefined) summary.message = summarizeMessage(value.message, maxStringChars);
  const inner = value.value;
  if (inner && typeof inner === "object") {
    if (inner.type !== undefined) summary.innerType = truncateText(String(inner.type), maxStringChars);
    summary.innerKeys = Object.keys(inner).slice(0, 20);
    if (inner.message !== undefined) summary.innerMessage = summarizeMessage(inner.message, maxStringChars);
  }
  if (value.assistantMessageEvent && typeof value.assistantMessageEvent === "object") {
    const event = value.assistantMessageEvent;
    if (event.type !== undefined) summary.deltaType = truncateText(String(event.type), maxStringChars);
    const delta = event.delta ?? event.text ?? event.thinking;
    if (typeof delta === "string") summary.deltaChars = delta.length;
  }
  if (value.data && typeof value.data === "object" && !Array.isArray(value.data)) {
    summary.dataKeys = Object.keys(value.data).slice(0, 20);
  }
  return summary;
}

function bumpCounter(map, key) {
  const name = typeof key === "string" && key ? key : "unknown";
  if (!Object.hasOwn(map, name) && Object.keys(map).length >= MAX_TRACKED_KEYS) {
    map.other = (map.other || 0) + 1;
    return;
  }
  map[name] = (map[name] || 0) + 1;
}

class LineSink extends Writable {
  #buffer = "";
  #decoder = new StringDecoder("utf8");
  #finished = false;

  constructor(logger, meta) {
    super({ decodeStrings: false });
    this.logger = logger;
    this.meta = meta;
    this.maxLineBytes = logger.maxStderrLineBytes;
  }

  #emit(text, truncated = false) {
    if (!text) return;
    this.logger.write({
      ...this.meta,
      level: "warn",
      kind: "stderr",
      message: text,
      ...(truncated ? { data: { truncated: true } } : {}),
    }, { droppable: true });
  }

  #drainCompleteLines() {
    let index;
    while ((index = this.#buffer.indexOf("\n")) !== -1) {
      const line = this.#buffer.slice(0, index).replace(/\r$/, "");
      this.#buffer = this.#buffer.slice(index + 1);
      this.#emit(line);
    }
    if (this.#buffer.length > this.maxLineBytes) {
      this.#emit(truncateText(this.#buffer, this.maxLineBytes), true);
      this.#buffer = "";
    }
  }

  _write(chunk, encoding, callback) {
    this.#buffer += Buffer.isBuffer(chunk) ? this.#decoder.write(chunk) : String(chunk);
    this.#drainCompleteLines();
    callback();
  }

  _final(callback) {
    if (this.#finished) return callback();
    this.#finished = true;
    this.#buffer += this.#decoder.end();
    if (this.#buffer) {
      this.#emit(this.#buffer.replace(/\r$/, ""));
      this.#buffer = "";
    }
    callback();
  }
}

/**
 * Bounded, single-writer Hub log. Callers must create exactly one writer per
 * Hub home. Writes go straight to the rotating stream; when the stream is
 * behind, droppable diagnostics are discarded and only counters are kept, so
 * no unbounded in-memory queue can form.
 */
export class HubLogWriter {
  constructor(options = {}) {
    const limits = DEFAULT_HUB_LOG_LIMITS;
    this.fileName = options.fileName ?? limits.fileName;
    if (!/^[A-Za-z0-9._-]+$/.test(this.fileName)) throw new Error("log fileName must be a plain file name");
    this.maxSegmentBytes = positiveInt(options.maxSegmentBytes, limits.maxSegmentBytes, "maxSegmentBytes");
    this.maxRotatedBytes = positiveInt(options.maxRotatedBytes, limits.maxRotatedBytes, "maxRotatedBytes");
    this.maxRotatedFiles = positiveInt(options.maxRotatedFiles, limits.maxRotatedFiles, "maxRotatedFiles");
    this.maxEntryBytes = positiveInt(options.maxEntryBytes, limits.maxEntryBytes, "maxEntryBytes");
    this.maxStringChars = positiveInt(options.maxStringChars, limits.maxStringChars, "maxStringChars");
    this.softLimitBytes = positiveInt(options.softLimitBytes, limits.softLimitBytes, "softLimitBytes");
    this.hardLimitBytes = positiveInt(options.hardLimitBytes, limits.hardLimitBytes, "hardLimitBytes");
    if (this.hardLimitBytes < this.softLimitBytes) throw new Error("hardLimitBytes must be >= softLimitBytes");
    this.maxStderrLineBytes = positiveInt(options.maxStderrLineBytes, limits.maxStderrLineBytes, "maxStderrLineBytes");
    this.debug = Boolean(options.debug);
    this.now = typeof options.now === "function" ? options.now : nowIso;
    this.dir = options.dir ? String(options.dir) : null;
    this.closed = false;
    this.closePromise = null;
    this.streamError = null;
    this.written = 0;
    this.writtenBytes = 0;
    this.dropped = { total: 0, byKind: {}, byReason: {} };
    this.pendingDrops = 0;
    this.pendingDropKinds = {};
    this.#onDrain = () => this.#reportDropped(false);

    const historyName = `.${this.fileName}.history`;
    this.historyPath = this.dir ? join(this.dir, historyName) : null;
    this.filePath = this.dir ? join(this.dir, this.fileName) : null;

    if (options.stream) {
      this.stream = options.stream;
    } else {
      if (!this.dir) throw new Error("HubLogWriter requires a dir or an injected stream");
      ensurePrivateDir(this.dir);
      const dot = this.fileName.lastIndexOf(".");
      const base = dot > 0 ? this.fileName.slice(0, dot) : this.fileName;
      const extension = dot > 0 ? this.fileName.slice(dot) : "";
      const generator = (time, index) => (time ? `${base}.${index ?? 1}${extension}` : this.fileName);
      this.stream = createStream(generator, {
        path: this.dir.endsWith(sep) ? this.dir : `${this.dir}${sep}`,
        size: `${this.maxSegmentBytes}B`,
        maxSize: `${this.maxRotatedBytes}B`,
        maxFiles: this.maxRotatedFiles,
        compress: false,
        mode: 0o600,
        history: historyName,
      });
      const chmodQuiet = (path) => {
        try { chmodSync(path, 0o600); } catch {}
      };
      this.stream.on("open", chmodQuiet);
      this.stream.on("rotated", chmodQuiet);
      this.stream.on("history", () => chmodQuiet(this.historyPath));
    }
    this.stream.on("error", (error) => { this.streamError = error instanceof Error ? error : new Error(String(error)); });
    this.stream.on("drain", this.#onDrain);
  }

  #onDrain;

  #baseRecord(record) {
    const line = {
      ts: this.now(),
      level: record.level || "info",
      kind: record.kind || "note",
    };
    for (const key of ["agentId", "carrier", "source", "direction"]) {
      if (record[key] !== undefined && record[key] !== null) line[key] = String(record[key]);
    }
    if (record.message !== undefined) line.message = record.message;
    if (record.summary !== undefined) line.summary = record.summary;
    if (record.data !== undefined) line.data = record.data;
    if (record.truncated === true) line.truncated = true;
    return line;
  }

  #format(record) {
    const base = this.#baseRecord(record);
    const bounded = boundValue(base, { maxStringChars: this.maxStringChars });
    let text = safeStringify(bounded);
    if (text === null) {
      text = safeStringify({ ts: base.ts, level: base.level, kind: base.kind, message: "unserializable log record" });
    }
    if (Buffer.byteLength(text) > this.maxEntryBytes) {
      const raw = safeStringify(record.data ?? record.message ?? record.summary) ?? "";
      const preview = truncateToBytes(raw, Math.max(256, this.maxEntryBytes - 400));
      const fallback = safeStringify({
        ts: base.ts,
        level: base.level,
        kind: base.kind,
        agentId: base.agentId,
        carrier: base.carrier,
        source: base.source,
        direction: base.direction,
        truncated: true,
        originalBytes: Buffer.byteLength(raw),
        preview,
      }) ?? "";
      text = Buffer.byteLength(fallback) <= this.maxEntryBytes
        ? fallback
        : truncateToBytes(fallback, this.maxEntryBytes);
    }
    return `${text}\n`;
  }

  #countDrop(record, reason) {
    // Only bounded counters are kept; the dropped entry itself is discarded.
    this.dropped.total += 1;
    bumpCounter(this.dropped.byKind, record.kind);
    bumpCounter(this.dropped.byReason, reason);
    this.pendingDrops += 1;
    bumpCounter(this.pendingDropKinds, record.kind);
  }

  #writeLine(line, record, { droppable }) {
    if (this.closed || this.streamError) return false;
    const buffered = this.stream.writableLength;
    const reason = buffered >= this.hardLimitBytes ? "hard_limit" : droppable && buffered >= this.softLimitBytes ? "soft_limit" : null;
    if (reason) {
      this.#countDrop(record, reason);
      return false;
    }
    this.stream.write(line);
    this.written += 1;
    this.writtenBytes += Buffer.byteLength(line);
    this.#reportDropped(false);
    return true;
  }

  #reportDropped(force) {
    if (this.pendingDrops === 0) return;
    if (!force && (this.closed || this.streamError || this.stream.writableLength >= this.softLimitBytes)) return;
    const kinds = this.pendingDropKinds;
    const count = this.pendingDrops;
    this.pendingDrops = 0;
    this.pendingDropKinds = {};
    const line = this.#format({
      level: "warn",
      kind: "log_dropped",
      message: "dropped diagnostics because the log stream was behind",
      data: { count, byKind: kinds, totalDropped: this.dropped.total },
    });
    try {
      this.stream.write(line);
    } catch {
      // The stream is already failing; counters remain available in stats().
    }
  }

  /** Low-level write. Returns false when the entry was dropped. */
  write(record, { droppable = true } = {}) {
    if (!record || typeof record !== "object") throw new TypeError("log record must be an object");
    if (this.closed || this.streamError) return false;
    // Drop before serializing: a stderr flood must not cost CPU per discarded
    // entry, and only bounded counters are kept.
    const buffered = this.stream.writableLength;
    if (buffered >= this.hardLimitBytes) {
      this.#countDrop(record, "hard_limit");
      return false;
    }
    if (droppable && buffered >= this.softLimitBytes) {
      this.#countDrop(record, "soft_limit");
      return false;
    }
    const line = this.#format(record);
    return this.#writeLine(line, record, { droppable: Boolean(droppable) });
  }

  createAgentLogWriter({ agentId, carrier = "pi", source = "agent", debug } = {}) {
    return createAgentLogWriter(this, { agentId, carrier, source, debug });
  }

  stats() {
    return {
      filePath: this.filePath,
      written: this.written,
      writtenBytes: this.writtenBytes,
      bufferedBytes: this.stream.writableLength,
      pendingDrops: this.pendingDrops,
      dropped: {
        total: this.dropped.total,
        byKind: { ...this.dropped.byKind },
        byReason: { ...this.dropped.byReason },
      },
      error: this.streamError ? this.streamError.message : null,
      closed: this.closed,
    };
  }

  async close() {
    if (this.closePromise) return this.closePromise;
    this.closed = true;
    this.#reportDropped(true);
    this.closePromise = new Promise((resolve) => {
      let settled = false;
      const done = () => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        this.stream.off("drain", this.#onDrain);
        resolve();
      };
      const timer = setTimeout(done, 5_000);
      timer.unref?.();
      this.stream.once("finish", done);
      this.stream.once("close", done);
      this.stream.once("error", done);
      try {
        this.stream.end();
      } catch {
        done();
      }
    });
    return this.closePromise;
  }
}

export function createHubLogWriter(options = {}) {
  return new HubLogWriter(options);
}

/**
 * Light adapter scoped to one Agent. It owns no file and no queue: protocol
 * lines and stderr chunks are forwarded to the single Hub writer tagged with
 * agentId/carrier/source. Raw protocol JSON is only written when debug is
 * explicitly enabled on the adapter or the Hub writer.
 */
export function createAgentLogWriter(logger, { agentId, carrier = "pi", source = "agent", debug } = {}) {
  if (!logger || typeof logger.write !== "function") throw new TypeError("createAgentLogWriter requires a HubLogWriter");
  if (typeof agentId !== "string" || !agentId) throw new TypeError("createAgentLogWriter requires agentId");
  const meta = { agentId, carrier: String(carrier || "pi"), source: String(source || "agent") };
  const debugEnabled = debug === undefined ? Boolean(logger.debug) : Boolean(debug);
  const stderr = new LineSink(logger, meta);
  let ended = false;

  return {
    ...meta,
    protocol(direction, value) {
      const normalized = direction === "in" || direction === "out" || direction === "error" ? direction : String(direction ?? "unknown");
      if (debugEnabled) {
        logger.write({ ...meta, level: "debug", kind: "protocol_raw", direction: normalized, data: value }, { droppable: true });
        return;
      }
      logger.write({
        ...meta,
        level: "debug",
        kind: "protocol",
        direction: normalized,
        summary: summarizeProtocol(normalized, value, { maxStringChars: logger.maxStringChars }),
      }, { droppable: true });
    },
    note(message, data, level = "info") {
      logger.write({ ...meta, level, kind: "agent_note", message, data }, { droppable: level !== "error" });
    },
    stderr,
    end() {
      if (ended) return Promise.resolve();
      ended = true;
      if (!stderr.writableEnded) stderr.end();
      if (stderr.writableFinished) return Promise.resolve();
      return new Promise((resolve) => {
        stderr.once("finish", resolve);
        stderr.once("close", resolve);
      });
    },
  };
}
