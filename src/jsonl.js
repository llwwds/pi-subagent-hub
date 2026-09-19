export class StrictJsonlParser {
  #buffer = Buffer.alloc(0);
  #onValue;
  #onError;

  constructor({ onValue, onError }) {
    this.#onValue = onValue;
    this.#onError = onError;
  }

  push(chunk) {
    const next = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    this.#buffer = Buffer.concat([this.#buffer, next]);
    let index;
    while ((index = this.#buffer.indexOf(0x0a)) !== -1) {
      let line = this.#buffer.subarray(0, index);
      this.#buffer = this.#buffer.subarray(index + 1);
      if (line.at(-1) === 0x0d) line = line.subarray(0, -1);
      if (line.length === 0) continue;
      try {
        this.#onValue(JSON.parse(line.toString("utf8")));
      } catch (error) {
        this.#onError(error, line.toString("utf8"));
      }
    }
  }

  finish() {
    if (this.#buffer.length === 0) return;
    const trailing = this.#buffer.toString("utf8");
    this.#buffer = Buffer.alloc(0);
    this.#onError(new Error("unterminated JSONL record"), trailing);
  }
}
