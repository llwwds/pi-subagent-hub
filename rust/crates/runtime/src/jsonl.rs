//! Strict NDJSON reader.
//!
//! Byte-for-byte behaviour of `src/jsonl.js`: split on `\n`, drop a trailing
//! `\r`, skip empty lines, and report a trailing partial record on `finish()`
//! instead of silently accepting it. Chunk boundaries are arbitrary, so the
//! caller feeds raw pipe reads.

use pihub_contracts::HubError;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonlRecord {
    Value(Value),
    /// Malformed JSON or an unterminated record. The raw line is preserved for
    /// the `protocol_error` payload.
    Error { message: String, line: String },
}

#[derive(Debug, Default)]
pub struct JsonlReader {
    pending: Vec<u8>,
}

impl JsonlReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume a raw pipe chunk and return every complete record it contained.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<JsonlRecord> {
        self.pending.extend_from_slice(chunk);
        let mut records = Vec::new();
        let mut start = 0usize;
        while let Some(offset) = self.pending[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let line = &self.pending[start..end];
            start = end + 1;
            self.collect(line, &mut records);
        }
        if start > 0 {
            self.pending.drain(..start);
        }
        records
    }

    /// Flush the remaining bytes. A non-empty remainder is an error, matching
    /// the 2.1 "unterminated JSONL record" behaviour.
    pub fn finish(&mut self) -> Option<JsonlRecord> {
        if self.pending.is_empty() {
            return None;
        }
        let trailing = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        Some(JsonlRecord::Error {
            message: "unterminated JSONL record".to_string(),
            line: trailing,
        })
    }

    pub fn buffered_len(&self) -> usize {
        self.pending.len()
    }

    fn collect(&self, raw: &[u8], records: &mut Vec<JsonlRecord>) {
        let mut line = raw;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(line).into_owned();
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => records.push(JsonlRecord::Value(value)),
            Err(error) => records.push(JsonlRecord::Error {
                message: error.to_string(),
                line: text,
            }),
        }
    }
}

impl JsonlRecord {
    /// Convenience for the request/response correlation path.
    pub fn into_value(self) -> Result<Value, HubError> {
        match self {
            Self::Value(value) => Ok(value),
            Self::Error { message, line } => Err(HubError::new(
                "protocol_error",
                format!("{message}: {line}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonlReader, JsonlRecord};

    #[test]
    fn preserves_unicode_and_survives_arbitrary_chunk_boundaries() {
        let mut reader = JsonlReader::new();
        let payload = "{\"text\":\"a b c\"}\r\n{\"n\":2}\n";
        let bytes = payload.as_bytes();
        let mut records = Vec::new();
        for chunk in [&bytes[..7], &bytes[7..15], &bytes[15..]] {
            records.extend(reader.push(chunk));
        }
        assert_eq!(records.len(), 2);
        assert!(matches!(&records[0], JsonlRecord::Value(value) if value["text"] == "a b c"));
        assert!(matches!(&records[1], JsonlRecord::Value(value) if value["n"] == 2));
        assert!(reader.finish().is_none());
    }

    #[test]
    fn multi_byte_characters_split_across_reads_are_not_corrupted() {
        let text = "{\"emoji\":\"🤖\",\"text\":\"中文\"}\n";
        let bytes = text.as_bytes();
        let mut reader = JsonlReader::new();
        let mut records = Vec::new();
        // Split inside the 4-byte emoji sequence.
        let split = bytes
            .iter()
            .position(|byte| *byte == 0xF0)
            .expect("emoji present")
            + 2;
        records.extend(reader.push(&bytes[..split]));
        records.extend(reader.push(&bytes[split..]));
        assert_eq!(records.len(), 1);
        assert!(matches!(&records[0], JsonlRecord::Value(value)
            if value["emoji"] == "🤖" && value["text"] == "中文"));
    }

    #[test]
    fn reports_malformed_and_unterminated_records() {
        let mut reader = JsonlReader::new();
        let records = reader.push(b"{\"x\":1}\nnot json\n");
        assert_eq!(records.len(), 2);
        assert!(matches!(&records[0], JsonlRecord::Value(_)));
        assert!(matches!(&records[1], JsonlRecord::Error { message, line }
            if line == "not json" && !message.is_empty()));

        let mut trailing = JsonlReader::new();
        assert!(trailing.push(b"{\"x\":1}").is_empty());
        let error = trailing.finish().expect("unterminated record");
        assert!(matches!(error, JsonlRecord::Error { message, .. } if message.contains("unterminated")));

        // Blank lines never produce records.
        let mut blanks = JsonlReader::new();
        assert!(blanks.push(b"\n\r\n\n").is_empty());
        assert!(blanks.finish().is_none());
    }
}
