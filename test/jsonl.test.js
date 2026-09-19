import test from "node:test";
import assert from "node:assert/strict";
import { StrictJsonlParser } from "../src/jsonl.js";

test("strict JSONL parser preserves unicode separators and chunk boundaries", () => {
  const values = [];
  const errors = [];
  const parser = new StrictJsonlParser({ onValue: (value) => values.push(value), onError: (...args) => errors.push(args) });
  const payload = `${JSON.stringify({ text: "a b c" })}\r\n${JSON.stringify({ n: 2 })}\n`;
  const bytes = Buffer.from(payload);
  parser.push(bytes.subarray(0, 7));
  parser.push(bytes.subarray(7, 15));
  parser.push(bytes.subarray(15));
  parser.finish();
  assert.deepEqual(values, [{ text: "a b c" }, { n: 2 }]);
  assert.equal(errors.length, 0);
});

test("strict JSONL parser reports unterminated records", () => {
  const errors = [];
  const parser = new StrictJsonlParser({ onValue() {}, onError: (error, line) => errors.push({ error, line }) });
  parser.push('{"x":1}');
  parser.finish();
  assert.equal(errors.length, 1);
  assert.match(errors[0].error.message, /unterminated/);
});
