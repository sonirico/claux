//! Approximate USD cost of a Claude Code transcript, computed from the usage
//! blocks embedded in its assistant turns at standard first-party API rates.
//! Parsing is incremental by byte offset per transcript path, so refreshing
//! the cost on a multi-MB transcript only re-reads and re-parses the bytes
//! appended since the previous call.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

struct PathState {
    offset: u64,
    total: f64,
}

/// Tracks accumulated USD cost per transcript path, incrementally.
pub struct CostTracker {
    state: HashMap<String, PathState>,
}

impl CostTracker {
    pub fn new() -> CostTracker {
        CostTracker {
            state: HashMap::new(),
        }
    }

    /// Returns the transcript's accumulated cost in USD since claux started
    /// tracking it, or `None` if the file cannot be read. Only complete
    /// lines (terminated by `\n`) are consumed; a trailing partial line is
    /// retried on the next call. A file smaller than the previously stored
    /// offset resets that path's state and reparses from zero.
    pub fn cost_for(&mut self, path: &str) -> Option<f64> {
        let mut file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len();

        let (mut offset, mut total) = match self.state.get(path) {
            Some(s) => (s.offset, s.total),
            None => (0, 0.0),
        };

        if len < offset {
            offset = 0;
            total = 0.0;
        }

        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;

        if let Some(last_newline) = buf.iter().rposition(|&b| b == b'\n') {
            for line in buf[..=last_newline].split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                if let Ok(text) = std::str::from_utf8(line) {
                    if let Some(cost) = line_cost(text) {
                        total += cost;
                    }
                }
            }
            offset += (last_newline + 1) as u64;
        }

        self.state
            .insert(path.to_string(), PathState { offset, total });
        Some(total)
    }
}

/// (input, output) USD per 1M tokens, by substring match on the model id.
fn model_rates(model: &str) -> (f64, f64) {
    if model.contains("fable") || model.contains("mythos") {
        (10.0, 50.0)
    } else if model.contains("opus") {
        (5.0, 25.0)
    } else if model.contains("sonnet") {
        (3.0, 15.0)
    } else if model.contains("haiku") {
        (1.0, 5.0)
    } else {
        (5.0, 25.0)
    }
}

/// Parses one JSONL line and returns its USD cost if it is a valid
/// assistant entry with a `message.usage` object; `None` otherwise.
fn line_cost(text: &str) -> Option<f64> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage")?.as_object()?;

    let model = message.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let (i, o) = model_rates(model);

    let num = |key: &str| -> f64 { usage.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0) };

    let input_tokens = num("input_tokens");
    let output_tokens = num("output_tokens");
    let cache_read_input_tokens = num("cache_read_input_tokens");

    let (ephemeral_5m, ephemeral_1h) = match usage.get("cache_creation").and_then(|v| v.as_object())
    {
        Some(cache_creation) => {
            let get = |key: &str| -> f64 {
                cache_creation
                    .get(key)
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            };
            (
                get("ephemeral_5m_input_tokens"),
                get("ephemeral_1h_input_tokens"),
            )
        }
        None => (num("cache_creation_input_tokens"), 0.0),
    };

    Some(
        (input_tokens * i
            + output_tokens * o
            + 0.1 * i * cache_read_input_tokens
            + 1.25 * i * ephemeral_5m
            + 2.0 * i * ephemeral_1h)
            / 1e6,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn new(name: &str) -> TempFile {
            let mut path = std::env::temp_dir();
            path.push(format!("claux-cost-test-{}-{}", std::process::id(), name));
            TempFile { path }
        }

        fn path_str(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }

        fn write(&self, contents: &str) {
            let mut f = File::create(&self.path).unwrap();
            f.write_all(contents.as_bytes()).unwrap();
        }

        fn append(&self, contents: &str) {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap();
            f.write_all(contents.as_bytes()).unwrap();
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn entry(model: &str, usage: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"model":"{}","usage":{}}}}}"#,
            model, usage
        )
    }

    #[test]
    fn rates_by_model_family() {
        assert_eq!(model_rates("claude-fable-5"), (10.0, 50.0));
        assert_eq!(model_rates("claude-mythos-1"), (10.0, 50.0));
        assert_eq!(model_rates("claude-opus-4-1"), (5.0, 25.0));
        assert_eq!(model_rates("claude-sonnet-5"), (3.0, 15.0));
        assert_eq!(model_rates("claude-haiku-4-5"), (1.0, 5.0));
        assert_eq!(model_rates("some-unknown-model"), (5.0, 25.0));
    }

    #[test]
    fn entry_cost_formula() {
        let usage = r#"{"input_tokens":100,"output_tokens":200,"cache_read_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":400,"ephemeral_1h_input_tokens":500}}"#;
        let line = entry("claude-sonnet-5", usage);
        let cost = line_cost(&line).unwrap();
        let i = 3.0;
        let o = 15.0;
        let expected =
            (100.0 * i + 200.0 * o + 0.1 * i * 300.0 + 1.25 * i * 400.0 + 2.0 * i * 500.0) / 1e6;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn entry_cost_without_cache_creation_object() {
        let usage = r#"{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":0,"cache_creation_input_tokens":50}"#;
        let line = entry("claude-opus-4-1", usage);
        let cost = line_cost(&line).unwrap();
        let i = 5.0;
        let o = 25.0;
        let expected = (10.0 * i + 20.0 * o + 1.25 * i * 50.0) / 1e6;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn incremental_parse_only_new_bytes() {
        let tmp = TempFile::new("incremental");
        let usage = r#"{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":0}"#;
        let line = entry("claude-sonnet-5", usage);

        tmp.write(&format!("{}\n", line));
        let mut tracker = CostTracker::new();
        let first = tracker.cost_for(&tmp.path_str()).unwrap();

        tmp.append(&format!("{}\n", line));
        let second = tracker.cost_for(&tmp.path_str()).unwrap();

        assert!((second - 2.0 * first).abs() < 1e-9);
    }

    #[test]
    fn partial_line_not_consumed() {
        let tmp = TempFile::new("partial");
        let usage = r#"{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":0}"#;
        let line = entry("claude-sonnet-5", usage);

        tmp.write(&format!("{}\n", line));
        let mut tracker = CostTracker::new();
        let first = tracker.cost_for(&tmp.path_str()).unwrap();

        // Append a second entry WITHOUT a trailing newline: incomplete line.
        tmp.append(&line);
        let second = tracker.cost_for(&tmp.path_str()).unwrap();
        assert!((second - first).abs() < 1e-9);

        // Complete the line: now it should count.
        tmp.append("\n");
        let third = tracker.cost_for(&tmp.path_str()).unwrap();
        assert!((third - 2.0 * first).abs() < 1e-9);
    }

    #[test]
    fn truncated_file_resets() {
        let tmp = TempFile::new("truncated");
        let usage = r#"{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":0}"#;
        let line = entry("claude-sonnet-5", usage);

        tmp.write(&format!("{}\n{}\n", line, line));
        let mut tracker = CostTracker::new();
        let two_entries = tracker.cost_for(&tmp.path_str()).unwrap();

        tmp.write(&format!("{}\n", line));
        let after_truncate = tracker.cost_for(&tmp.path_str()).unwrap();

        assert!((after_truncate - two_entries / 2.0).abs() < 1e-9);
    }

    #[test]
    fn missing_file_is_none() {
        let mut tmp_path = std::env::temp_dir();
        tmp_path.push(format!(
            "claux-cost-test-{}-does-not-exist",
            std::process::id()
        ));
        let mut tracker = CostTracker::new();
        assert_eq!(tracker.cost_for(&tmp_path.to_string_lossy()), None);
    }

    #[test]
    fn skips_non_assistant_and_garbage() {
        let tmp = TempFile::new("garbage");
        let usage = r#"{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":0}"#;
        let assistant_line = entry("claude-sonnet-5", usage);
        let user_line = r#"{"type":"user","message":{"model":"claude-sonnet-5","usage":{"input_tokens":999,"output_tokens":999}}}"#;
        let garbage_line = "not json at all";

        tmp.write(&format!(
            "{}\n{}\n{}\n",
            assistant_line, user_line, garbage_line
        ));
        let mut tracker = CostTracker::new();
        let cost = tracker.cost_for(&tmp.path_str()).unwrap();
        let expected = line_cost(&assistant_line).unwrap();
        assert!((cost - expected).abs() < 1e-9);
    }
}
