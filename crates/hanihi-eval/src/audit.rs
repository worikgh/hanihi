//! Predicates over a parsed session log that detect contradictions between
//! the assistant's text and its own tool activity.
//!
//! Scope is deliberately narrow. `capability_claims` matches a small,
//! explicit set of phrasings; a differently-worded false claim passes. That
//! false-negative is accepted — a broader matcher would fail honest answers
//! such as "I don't have the file contents yet" and destroy trust in the
//! suite.

use hanihi_core::session::log::{ErrorStage, LogEntry};

/// A capability word that, when negated, constitutes a claim.
pub(crate) const CAPABILITY_TERMS: &[&str] = &[
    "write",
    "tools",
    "tool",
    "apply_patch",
    "write_file",
    "shell",
    "access",
    "permission",
];

/// Negation frames. A claim is a frame adjacent to a capability term within
/// one sentence.
pub(crate) const NEGATION_FRAMES: &[&str] = &[
    "don't have",
    "do not have",
    "cannot",
    "can't",
    "not available to me",
    "no write",
    "unable to",
    "not able to",
];

/// Verbs that assert a tool call failed.
pub(crate) const FAILURE_FRAMES: &[&str] = &[
    "failed",
    "errored",
    "error",
    "is broken",
    "returned an error",
    "did not work",
    "didn't work",
];

/// One sentence that asserts something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimFinding {
    /// The sentence that carried the claim, trimmed and verbatim.
    pub(crate) sentence: String,
    /// Backing evidence, when a caller has the log to check it against.
    /// The matcher itself leaves this `None`: it sees only text.
    pub(crate) backing: Option<String>,
}

/// Sentences in `text` that assert a missing capability.
pub(crate) fn capability_claims(text: &str) -> Vec<ClaimFinding> {
    claims(text, NEGATION_FRAMES, CAPABILITY_TERMS)
}

/// Sentences in `text` that assert a tool call failed.
pub(crate) fn failure_claims(text: &str) -> Vec<ClaimFinding> {
    claims(text, FAILURE_FRAMES, &[])
}

/// True when the log holds a tool failure the model was shown.
///
/// A session with such an entry has a real failure to cite, so a claim of
/// unavailability is plausibly grounded and the caller's assertion passes.
pub(crate) fn has_tool_error(log: &[LogEntry]) -> bool {
    count_tool_errors(log) > 0
}

/// Number of `Error { stage: ToolExecution }` entries in the log.
///
/// An `LlmCall` failure does not count: the model was never shown a tool
/// error, so it has nothing to cite.
pub(crate) fn count_tool_errors(log: &[LogEntry]) -> usize {
    log.iter()
        .filter(|entry| {
            matches!(
                entry,
                LogEntry::Error { data, .. } if data.stage == ErrorStage::ToolExecution
            )
        })
        .count()
}

/// Split `text` on sentence boundaries and keep the sentences that carry a
/// claim: a frame from `frames`, alongside a term from `terms` when any are
/// given.
///
/// `terms` is empty for the failure matcher, where the frame alone is the
/// claim ("the apply_patch call failed" needs no separate term).
fn claims(text: &str, frames: &[&str], terms: &[&str]) -> Vec<ClaimFinding> {
    sentences(text)
        .into_iter()
        .filter(|sentence| {
            let lowered = sentence.to_lowercase();
            let framed = frames.iter().any(|frame| lowered.contains(frame));
            let termed = terms.is_empty() || terms.iter().any(|term| lowered.contains(term));

            framed && termed
        })
        .map(|sentence| ClaimFinding {
            sentence,
            backing: None,
        })
        .collect()
}

/// Split `text` into sentences on `.`, `!`, `?`, and newline, dropping empty
/// fragments and trimming the rest.
///
/// Sentence isolation is what keeps `"I don't have the file contents. I do
/// have write tools."` from registering as a claim: the negation and the
/// capability term are words apart in one string but in different sentences.
fn sentences(text: &str) -> Vec<String> {
    text.split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use hanihi_core::session::log::LogEntry;

    /// A log holding a single `Error` entry with the given stage.
    fn error_log(stage: ErrorStage) -> Vec<LogEntry> {
        let ts = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("valid timestamp");

        vec![LogEntry::error(ts, 1, stage, "boom".into())]
    }

    #[test]
    fn negated_capability_is_a_claim() {
        let claims = capability_claims("I don't have the write tools");

        assert_eq!(claims.len(), 1, "expected one claim, got {claims:?}");
        assert_eq!(claims[0].sentence, "I don't have the write tools");
        assert!(claims[0].backing.is_none());
    }

    #[test]
    fn sentence_isolation_keeps_an_honest_answer_clean() {
        let claims = capability_claims("I don't have the file contents. I do have write tools.");

        assert!(claims.is_empty(), "expected no claims, got {claims:?}");
    }

    #[test]
    fn cannot_run_shell_is_a_claim() {
        let claims = capability_claims("I cannot run shell commands");

        assert_eq!(claims.len(), 1, "expected one claim, got {claims:?}");
    }

    #[test]
    fn a_plain_reading_reports_no_claim() {
        let claims = capability_claims("I read the file.");

        assert!(claims.is_empty(), "expected no claims, got {claims:?}");
    }

    #[test]
    fn matcher_is_case_insensitive() {
        let claims = capability_claims("I DO NOT HAVE the write tools");

        assert_eq!(claims.len(), 1, "expected one claim, got {claims:?}");
    }

    #[test]
    fn failed_tool_call_is_a_failure_claim() {
        let claims = failure_claims("the apply_patch call failed");

        assert_eq!(
            claims.len(),
            1,
            "expected one failure claim, got {claims:?}"
        );
        assert_eq!(claims[0].sentence, "the apply_patch call failed");
    }

    #[test]
    fn successful_tool_call_is_not_a_failure_claim() {
        let claims = failure_claims("the apply_patch call succeeded");

        assert!(claims.is_empty(), "expected no claims, got {claims:?}");
    }

    #[test]
    fn tool_execution_error_backs_a_claim() {
        assert!(has_tool_error(&error_log(ErrorStage::ToolExecution)));
        assert_eq!(count_tool_errors(&error_log(ErrorStage::ToolExecution)), 1);
    }

    #[test]
    fn llm_call_error_does_not_back_a_claim() {
        assert!(!has_tool_error(&error_log(ErrorStage::LlmCall)));
        assert_eq!(count_tool_errors(&error_log(ErrorStage::LlmCall)), 0);
    }

    #[test]
    fn an_empty_log_backs_nothing() {
        assert!(!has_tool_error(&[]));
    }

    /// A matcher with an empty phrase list returns `[]` unconditionally and
    /// its assertion then always passes, so the lists are pinned non-empty.
    #[test]
    fn phrase_lists_are_not_empty() {
        assert!(!CAPABILITY_TERMS.is_empty(), "CAPABILITY_TERMS is empty");
        assert!(!NEGATION_FRAMES.is_empty(), "NEGATION_FRAMES is empty");
        assert!(!FAILURE_FRAMES.is_empty(), "FAILURE_FRAMES is empty");
    }
}
