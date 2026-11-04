//! Durable run records.
//!
//! Every run leaves a directory:
//!
//! ```text
//! <dir>/<run_id>/
//! ├── events.jsonl   lifecycle events, one per line
//! └── result.json    the RunResult, including the full transcript
//! ```
//!
//! Two files, not one, because they answer different questions. `result.json` is
//! the artifact a caller compares across runs; `events.jsonl` is the timeline,
//! append-friendly and greppable. Neither contains the API key or the request
//! headers — the model configuration is printed separately, redacted.

use crate::error::Result;
use crate::run::{Event, RunResult};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Writes one run and returns the path of `result.json`.
pub fn write_run(
    directory: &Path,
    run_id: &str,
    result: &RunResult,
    events: &[Event],
) -> Result<PathBuf> {
    let run_directory = directory.join(run_id);
    std::fs::create_dir_all(&run_directory)?;

    let events_path = run_directory.join("events.jsonl");
    let mut file = std::fs::File::create(&events_path)?;
    for event in events {
        writeln!(file, "{}", serde_json::to_string(event)?)?;
    }
    file.flush()?;

    let result_path = run_directory.join("result.json");
    std::fs::write(&result_path, serde_json::to_vec_pretty(result)?)?;
    Ok(result_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{Stats, Status, StopReason};

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!("bootidea-{name}-{stamp}"));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    fn result() -> RunResult {
        RunResult {
            run_id: "run-1-2".to_owned(),
            status: Status::Completed,
            stop_reason: StopReason::Completed,
            goal: "goal".to_owned(),
            answer: Some("answer".to_owned()),
            error: None,
            messages: vec![crate::message::Message::user("goal")],
            stats: Stats::default(),
        }
    }

    #[test]
    fn writes_events_and_result() {
        let directory = temp_dir("transcript");
        let events = vec![Event::ModelCall { index: 1 }];
        let path = write_run(&directory, "run-1-2", &result(), &events).expect("write");

        assert!(path.ends_with("result.json"));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("\"answer\": \"answer\""));

        let events_text =
            std::fs::read_to_string(directory.join("run-1-2/events.jsonl")).expect("read events");
        assert_eq!(events_text.lines().count(), 1);
        assert!(events_text.contains("model_call"));

        std::fs::remove_dir_all(&directory).ok();
    }
}
