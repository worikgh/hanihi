//! Detect contradictions between a turn's assistant text and its own tool
//! activity.
//!
//! This is a diagnostic, not a truthfulness checker. It catches two
//! enumerated contradictions and stays silent on everything else. See
//! [`audit_turn`] for the rules and the reason they are narrow.
//!
//! The phrase lists below are intentionally duplicated from
//! `hanihi-eval`'s `audit.rs`. `003` owns the definition; this module copies
//! it so that the off-line eval assertions and the runtime diagnostic agree
//! on what a "capability claim" is. There is no compile-time link between the
//! two crates — if the lists must change, change both in the same commit, and
//! if that starts happening often, promote them into `hanihi-core` and have
//! `hanihi-eval` depend on them.

use rig::completion::message::ToolCall;

/// A capability word that, when negated, constitutes a claim.
const CAPABILITY_TERMS: &[&str] = &[
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
const NEGATION_FRAMES: &[&str] = &[
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
const FAILURE_FRAMES: &[&str] = &[
    "failed",
    "errored",
    "error",
    "is broken",
    "returned an error",
    "did not work",
    "didn't work",
];

/// The enumerated contradictions this audit can detect.
///
/// An enum rather than a bool: more checks are expected to accrete, and a
/// caller that wants to act on one specific kind needs to match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfAuditKind {
    /// Claimed a tool was unavailable with no tool error to cite.
    UnsupportedCapabilityClaim,
    /// Claimed a tool failed, with no failing tool result in the turn.
    UnreportedToolError,
}

impl SelfAuditKind {
    /// A short, stable label for display and logging.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedCapabilityClaim => "unsupported capability claim",
            Self::UnreportedToolError => "unreported tool error",
        }
    }
}

/// A turn whose assistant text made a checkable claim that the turn's own
/// tool activity does not support. Diagnostic only: it never fails a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfAuditFinding {
    /// Which contradiction was detected.
    pub kind: SelfAuditKind,
    /// The offending sentence, verbatim, for the human reading the log.
    pub detail: String,
}

/// Detect contradictions between a turn's final text and its tool activity.
///
/// Pure and total: no panics, no I/O, no model. Returns at most one finding
/// per distinct [`SelfAuditKind`], not one per occurrence — the interesting
/// fact is that the contradiction happened, not how many times.
///
/// Matching rules, deliberately narrow:
///
/// - `text` is split into sentences on `.`/`!`/`?`/newline before matching,
///   so a negation and a capability term in different sentences are not
///   paired. Case-insensitive throughout.
/// - [`SelfAuditKind::UnsupportedCapabilityClaim`]: a negation frame adjacent
///   to a capability term within one sentence, **and** `failures` is empty.
///   A turn with a genuine failure has something to cite, so a claim of
///   unavailability is plausibly grounded and the check stays silent.
/// - [`SelfAuditKind::UnreportedToolError`]: a sentence uses a failure frame
///   and names a tool that was called this turn, **and** that name is absent
///   from `failures`.
///
/// Narrow phrasing means a differently-worded false claim passes. That false
/// negative is accepted: a broad matcher fails honest prose such as "I don't
/// have the file contents yet", and an audit that cries wolf gets ignored.
pub(crate) fn audit_turn(
    text: &str,
    calls: &[ToolCall],
    failures: &[ToolCall],
) -> Vec<SelfAuditFinding> {
    let sentences = sentences(text);
    let mut findings = Vec::new();

    if failures.is_empty() {
        let claim = sentences
            .iter()
            .find(|sentence| is_capability_claim(sentence))
            .map(|sentence| SelfAuditFinding {
                kind: SelfAuditKind::UnsupportedCapabilityClaim,
                detail: sentence.clone(),
            });
        findings.extend(claim);
    }

    if let Some(sentence) = sentences
        .iter()
        .find(|sentence| names_an_unreported_failure(sentence, calls, failures))
    {
        findings.push(SelfAuditFinding {
            kind: SelfAuditKind::UnreportedToolError,
            detail: sentence.clone(),
        });
    }

    findings
}

/// True when `sentence` negates a capability term.
fn is_capability_claim(sentence: &str) -> bool {
    let lowered = sentence.to_lowercase();

    NEGATION_FRAMES.iter().any(|frame| lowered.contains(frame))
        && CAPABILITY_TERMS.iter().any(|term| lowered.contains(term))
}

/// True when `sentence` asserts that a tool called this turn failed, and that
/// tool does not appear in `failures`.
fn names_an_unreported_failure(sentence: &str, calls: &[ToolCall], failures: &[ToolCall]) -> bool {
    let lowered = sentence.to_lowercase();

    if !FAILURE_FRAMES.iter().any(|frame| lowered.contains(frame)) {
        return false;
    }

    calls.iter().any(|call| {
        let name = call.function.name.to_lowercase();
        lowered.contains(&name)
            && !failures
                .iter()
                .any(|failed| failed.function.name.eq_ignore_ascii_case(&name))
    })
}

/// Split `text` into sentences on `.`, `!`, `?`, and newline, dropping empty
/// fragments and trimming the rest.
///
/// Sentence isolation is what keeps `"I don't have the file contents. I do
/// have write tools."` from registering as a claim.
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
    use serde_json::json;

    /// A tool call with the given name, for the `calls`/`failures` inputs.
    fn call(name: &str) -> ToolCall {
        ToolCall::new(
            format!("{name}-id"),
            rig::completion::message::ToolFunction {
                name: name.to_string(),
                arguments: json!({}),
            },
        )
    }

    fn kinds(findings: &[SelfAuditFinding]) -> Vec<SelfAuditKind> {
        findings.iter().map(|finding| finding.kind).collect()
    }

    #[test]
    fn negated_capability_with_no_failure_is_a_finding() {
        let findings = audit_turn("I don't have the write tools", &[], &[]);

        assert_eq!(
            kinds(&findings),
            vec![SelfAuditKind::UnsupportedCapabilityClaim]
        );
        assert_eq!(findings[0].detail, "I don't have the write tools");
    }

    #[test]
    fn a_real_failure_silences_the_capability_check() {
        let findings = audit_turn(
            "I don't have the write tools",
            &[call("read_file")],
            &[call("read_file")],
        );

        assert!(
            findings.is_empty(),
            "expected no findings, got {findings:?}"
        );
    }

    #[test]
    fn sentence_isolation_keeps_an_honest_answer_clean() {
        let findings = audit_turn(
            "I don't have the file contents. I do have write tools.",
            &[],
            &[],
        );

        assert!(
            findings.is_empty(),
            "expected no findings, got {findings:?}"
        );
    }

    #[test]
    fn cannot_run_shell_is_a_finding() {
        let findings = audit_turn("I cannot run shell commands", &[], &[]);

        assert_eq!(
            kinds(&findings),
            vec![SelfAuditKind::UnsupportedCapabilityClaim]
        );
    }

    #[test]
    fn matcher_is_case_insensitive() {
        let findings = audit_turn("I DO NOT HAVE the write tools", &[], &[]);

        assert_eq!(
            kinds(&findings),
            vec![SelfAuditKind::UnsupportedCapabilityClaim]
        );
    }

    #[test]
    fn a_plain_reading_reports_nothing() {
        let findings = audit_turn("I read the file.", &[call("read_file")], &[]);

        assert!(
            findings.is_empty(),
            "expected no findings, got {findings:?}"
        );
    }

    #[test]
    fn claiming_an_unreported_failure_is_a_finding() {
        let findings = audit_turn("read_file failed", &[call("read_file")], &[]);

        assert_eq!(kinds(&findings), vec![SelfAuditKind::UnreportedToolError]);
        assert_eq!(findings[0].detail, "read_file failed");
    }

    #[test]
    fn a_reported_failure_is_not_a_finding() {
        let findings = audit_turn(
            "read_file failed",
            &[call("read_file")],
            &[call("read_file")],
        );

        assert!(
            findings.is_empty(),
            "expected no findings, got {findings:?}"
        );
    }

    #[test]
    fn a_failure_verb_with_no_tool_named_is_not_a_finding() {
        let findings = audit_turn("something went wrong overall", &[call("read_file")], &[]);

        assert!(
            findings.is_empty(),
            "expected no findings, got {findings:?}"
        );
    }

    #[test]
    fn both_contradictions_are_reported_once_each() {
        let findings = audit_turn(
            "I don't have write tools. Also, read_file failed.",
            &[call("read_file")],
            &[],
        );

        assert_eq!(
            kinds(&findings),
            vec![
                SelfAuditKind::UnsupportedCapabilityClaim,
                SelfAuditKind::UnreportedToolError
            ]
        );
    }

    #[test]
    fn repeated_contradictions_yield_one_finding_per_kind() {
        let findings = audit_turn(
            "I cannot write files. I cannot run shell commands.",
            &[],
            &[],
        );

        assert_eq!(findings.len(), 1, "expected one finding, got {findings:?}");
    }

    #[test]
    fn an_empty_turn_reports_nothing() {
        assert!(audit_turn("", &[], &[]).is_empty());
    }

    #[test]
    fn kind_labels_are_distinct() {
        assert_eq!(
            SelfAuditKind::UnsupportedCapabilityClaim.as_str(),
            "unsupported capability claim"
        );
        assert_eq!(
            SelfAuditKind::UnreportedToolError.as_str(),
            "unreported tool error"
        );
    }

    /// A matcher with an empty phrase list returns `[]` unconditionally and
    /// its check then never fires, so the lists are pinned non-empty.
    #[test]
    fn phrase_lists_are_not_empty() {
        assert!(!CAPABILITY_TERMS.is_empty(), "CAPABILITY_TERMS is empty");
        assert!(!NEGATION_FRAMES.is_empty(), "NEGATION_FRAMES is empty");
        assert!(!FAILURE_FRAMES.is_empty(), "FAILURE_FRAMES is empty");
    }
}
