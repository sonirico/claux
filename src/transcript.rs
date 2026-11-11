//! What claux reads from a Claude Code transcript: the approximate USD cost,
//! computed from the usage blocks embedded in its assistant turns at
//! standard first-party API rates, and whether the last turn was cut short
//! by the user (Esc), which no hook reports. Parsing is incremental by byte
//! offset per transcript path, so refreshing a multi-MB transcript only
//! re-reads and re-parses the bytes appended since the previous call.
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// Text Claude Code writes as a user entry when a turn is interrupted.
const INTERRUPT_MARKER: &str = "[Request interrupted by user";

/// What a transcript says so far.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Scan {
    /// Accumulated cost in USD since claux started tracking the path.
    pub cost: f64,
    /// The last user or assistant entry is an interrupt marker: the turn
    /// was cut short and the agent is no longer working.
    pub interrupted: bool,
}

struct PathState {
    offset: u64,
    scan: Scan,
}

/// Tracks each transcript path's `Scan`, incrementally.
pub struct TranscriptTracker {
    state: HashMap<String, PathState>,
}

impl TranscriptTracker {
    pub fn new() -> TranscriptTracker {
        TranscriptTracker {
            state: HashMap::new(),
        }
    }

    /// Returns the transcript's `Scan`, or `None` if the file cannot be
    /// read. Only complete lines (terminated by `\n`) are consumed; a
    /// trailing partial line is retried on the next call. A file smaller
    /// than the previously stored offset resets that path's state and
    /// reparses from zero.
    pub fn scan(&mut self, path: &str) -> Option<Scan> {
        let mut file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len();

        let (mut offset, mut scan) = match self.state.get(path) {
            Some(s) => (s.offset, s.scan),
            None => (0, Scan::default()),
        };

        if len < offset {
            offset = 0;
            scan = Scan::default();
        }

        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;

        if let Some(last_newline) = buf.iter().rposition(|&b| b == b'\n') {
            for line in buf[..=last_newline].split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
                    if let Some(cost) = line_cost(&value) {
                        scan.cost += cost;
                    }
                    if let Some(interrupted) = line_interrupted(&value) {
                        scan.interrupted = interrupted;
                    }
                }
            }
            offset += (last_newline + 1) as u64;
        }

        self.state
            .insert(path.to_string(), PathState { offset, scan });
        Some(scan)
    }
}

/// For a user or assistant entry, whether it is an interrupt marker;
/// `None` for every other entry type (system, attachment, ...), which must
/// not clear a pending interrupt.
fn line_interrupted(value: &serde_json::Value) -> Option<bool> {
    let kind = value.get("type")?.as_str()?;
    if kind == "assistant" {
        return Some(false);
    }
    if kind != "user" {
        return None;
    }
    let is_marker =
        |v: &serde_json::Value| v.as_str().is_some_and(|t| t.starts_with(INTERRUPT_MARKER));
    let interrupted = match value.get("message").and_then(|m| m.get("content")) {
        Some(serde_json::Value::String(t)) => t.starts_with(INTERRUPT_MARKER),
        Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|b| {
            b.get("text").is_some_and(is_marker)
                || b.get("content").is_some_and(|c| {
                    is_marker(c)
                        || c.as_array()
                            .is_some_and(|a| a.iter().any(|t| t.get("text").is_some_and(is_marker)))
                })
        }),
        _ => false,
    };
    Some(interrupted)
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

/// Returns one JSONL entry's USD cost if it is an assistant entry with a
/// `message.usage` object; `None` otherwise.
fn line_cost(value: &serde_json::Value) -> Option<f64> {
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
            path.push(format!(
                "claux-transcript-test-{}-{}",
                std::process::id(),
                name
            ));
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

    fn json(line: &str) -> serde_json::Value {
        serde_json::from_str(line).unwrap()
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
        let cost = line_cost(&json(&line)).unwrap();
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
        let cost = line_cost(&json(&line)).unwrap();
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
        let mut tracker = TranscriptTracker::new();
        let first = tracker.scan(&tmp.path_str()).unwrap().cost;

        tmp.append(&format!("{}\n", line));
        let second = tracker.scan(&tmp.path_str()).unwrap().cost;

        assert!((second - 2.0 * first).abs() < 1e-9);
    }

    #[test]
    fn partial_line_not_consumed() {
        let tmp = TempFile::new("partial");
        let usage = r#"{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":0}"#;
        let line = entry("claude-sonnet-5", usage);

        tmp.write(&format!("{}\n", line));
        let mut tracker = TranscriptTracker::new();
        let first = tracker.scan(&tmp.path_str()).unwrap().cost;

        // Append a second entry WITHOUT a trailing newline: incomplete line.
        tmp.append(&line);
        let second = tracker.scan(&tmp.path_str()).unwrap().cost;
        assert!((second - first).abs() < 1e-9);

        // Complete the line: now it should count.
        tmp.append("\n");
        let third = tracker.scan(&tmp.path_str()).unwrap().cost;
        assert!((third - 2.0 * first).abs() < 1e-9);
    }

    #[test]
    fn truncated_file_resets() {
        let tmp = TempFile::new("truncated");
        let usage = r#"{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":0}"#;
        let line = entry("claude-sonnet-5", usage);

        tmp.write(&format!("{}\n{}\n", line, line));
        let mut tracker = TranscriptTracker::new();
        let two_entries = tracker.scan(&tmp.path_str()).unwrap().cost;

        tmp.write(&format!("{}\n", line));
        let after_truncate = tracker.scan(&tmp.path_str()).unwrap().cost;

        assert!((after_truncate - two_entries / 2.0).abs() < 1e-9);
    }

    #[test]
    fn missing_file_is_none() {
        let mut tmp_path = std::env::temp_dir();
        tmp_path.push(format!(
            "claux-transcript-test-{}-does-not-exist",
            std::process::id()
        ));
        let mut tracker = TranscriptTracker::new();
        assert_eq!(tracker.scan(&tmp_path.to_string_lossy()), None);
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
        let mut tracker = TranscriptTracker::new();
        let cost = tracker.scan(&tmp.path_str()).unwrap().cost;
        let expected = line_cost(&json(&assistant_line)).unwrap();
        assert!((cost - expected).abs() < 1e-9);
    }

    const USER_PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"do it"}}"#;
    const INTERRUPT_TEXT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
    const INTERRUPT_TOOL: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"[Request interrupted by user for tool use]"}]}}"#;
    const SYSTEM_ENTRY: &str = r#"{"type":"system","subtype":"away_summary"}"#;

    fn new_test_scan(name: &str, lines: &[&str]) -> Scan {
        let tmp = TempFile::new(name);
        tmp.write(&format!("{}\n", lines.join("\n")));
        TranscriptTracker::new().scan(&tmp.path_str()).unwrap()
    }

    #[test]
    fn interrupt_text_marks_interrupted() {
        let scan = new_test_scan("int-text", &[USER_PROMPT, INTERRUPT_TEXT]);

        assert!(scan.interrupted);
    }

    #[test]
    fn interrupt_tool_result_marks_interrupted() {
        let scan = new_test_scan("int-tool", &[USER_PROMPT, INTERRUPT_TOOL]);

        assert!(scan.interrupted);
    }

    #[test]
    fn interrupt_survives_system_entries() {
        let scan = new_test_scan("int-system", &[INTERRUPT_TEXT, SYSTEM_ENTRY]);

        assert!(scan.interrupted);
    }

    #[test]
    fn interrupt_cleared_by_next_prompt() {
        let scan = new_test_scan("int-cleared", &[INTERRUPT_TEXT, USER_PROMPT]);

        assert!(!scan.interrupted);
    }

    #[test]
    fn interrupt_cleared_by_assistant_entry() {
        let usage = r#"{"input_tokens":1,"output_tokens":1}"#;
        let assistant = entry("claude-sonnet-5", usage);

        let scan = new_test_scan("int-assistant", &[INTERRUPT_TEXT, &assistant]);

        assert!(!scan.interrupted);
    }
}
