//! Toolchain-neutral build/test/lint gates for eval cases.
//!
//! A gate runs a case-authored command in the case's resolved repo
//! directory and reports whether it exited 0. Commands are argv vectors:
//! no shell is involved, so no metacharacters are interpreted.
//!
//! These gates are deliberately *not* routed through the agent's command
//! allowlist. Case authors are trusted, and a gate may legitimately run a
//! command the agent could never have chosen. The eval verifies the
//! artifact, not the agent's process.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Default build gate for cases that do not override it.
///
/// This mirrors the pre-existing behaviour of `build_succeeds`, which ran
/// `cargo check`. Do not "correct" it to `cargo build`: existing cases rely
/// on `check` being cheaper and on its exact diagnostic output.
pub(crate) const DEFAULT_BUILD_COMMAND: &[&str] = &["cargo", "check"];

/// Default test gate for cases that do not override it.
pub(crate) const DEFAULT_TEST_COMMAND: &[&str] = &["cargo", "test"];

/// Default lint gate. Applies only to cases that build with cargo: there is
/// no defensible default for a CMake project, so a non-Cargo case must name
/// its own `lint_command` or the gate is a configuration error.
pub(crate) const DEFAULT_LINT_COMMAND: &[&str] = &["cargo", "clippy", "--", "-D", "warnings"];

/// How much captured output to show in a failure message.
///
/// The whole value of a build gate is the compiler's own text, and a C++
/// diagnostic routinely exceeds a few hundred bytes, so this is generous
/// rather than tight. Still bounded: an unbounded detail would swamp the
/// runner's report.
const MAX_DETAIL_BYTES: usize = 2000;

/// The outcome of running one gate command.
#[derive(Debug)]
pub(crate) struct GateOutcome {
    /// Whether the command exited 0.
    pub(crate) passed: bool,
    /// Human-readable detail: the command description on success, captured
    /// output on failure.
    pub(crate) detail: String,
}

/// Run `argv` in `dir`, capturing output, and report pass/fail.
///
/// `timeout` bounds the command; on expiry the child is killed and the gate
/// fails. Environment is inherited as-is: unlike the agent's `run_command`
/// tool, a gate may legitimately need the ambient toolchain (a C++ compiler
/// on `PATH`, `CMAKE_*` hints). Case authors control what runs, so the
/// scrubbing that protects the agent from itself is not required here.
pub(crate) async fn run_gate(
    dir: &Path,
    argv: &[String],
    description: &str,
    timeout: Duration,
) -> GateOutcome {
    let Some((program, args)) = argv.split_first() else {
        return GateOutcome {
            passed: false,
            detail: format!("{description}: empty command"),
        };
    };

    let child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(output)) if output.status.success() => GateOutcome {
            passed: true,
            detail: format!("{description} exited 0"),
        },
        Ok(Ok(output)) => {
            // Both streams, stderr first. Tools disagree about which one
            // carries the useful text: a compiler writes its diagnostic to
            // stderr, while ctest writes the failing test's output to stdout
            // and only a summary line to stderr. Preferring either one alone
            // discards the diagnostic in one of those cases.
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut parts = Vec::new();
            if !stderr.trim().is_empty() {
                parts.push(stderr.trim());
            }
            if !stdout.trim().is_empty() {
                parts.push(stdout.trim());
            }
            let msg = parts.join("\n");
            GateOutcome {
                passed: false,
                detail: format!("{description} failed: {}", truncate(&msg, MAX_DETAIL_BYTES)),
            }
        }
        Ok(Err(e)) => GateOutcome {
            passed: false,
            detail: format!("spawn {description}: {e}"),
        },
        Err(_elapsed) => GateOutcome {
            passed: false,
            detail: format!("{description} timed out after {timeout:?}"),
        },
    }
}

/// Truncate `s` to at most `max` bytes, appending an ellipsis when cut.
///
/// Truncation is on a char boundary so multi-byte UTF-8 (compiler output
/// routinely contains non-ASCII) cannot panic.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}…", &s[..end])
}

/// Run the configure step, if the case has one, followed by the build step.
///
/// CMake needs two steps where Cargo needs one. A configure failure skips the
/// build and reports the configure output, so the diagnostic names the step
/// that actually broke.
pub(crate) async fn run_configure_and_build(
    dir: &Path,
    configure: Option<&[String]>,
    build: &[String],
    timeout: Duration,
) -> GateOutcome {
    if let Some(configure) = configure {
        let outcome = run_gate(dir, configure, "configure", timeout).await;
        if !outcome.passed {
            return outcome;
        }
    }
    run_gate(dir, build, "build", timeout).await
}

/// Render an argv vector for display in a label or failure message.
pub(crate) fn describe(argv: &[String]) -> String {
    argv.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn run_gate_passes_on_zero_exit() {
        let dir = std::env::temp_dir();
        let outcome = run_gate(&dir, &argv(&["true"]), "probe", Duration::from_secs(10)).await;
        assert!(outcome.passed, "detail: {}", outcome.detail);
        assert!(outcome.detail.contains("exited 0"), "{}", outcome.detail);
    }

    #[tokio::test]
    async fn run_gate_surfaces_captured_output_on_failure() {
        let dir = std::env::temp_dir();
        let outcome = run_gate(
            &dir,
            &argv(&["sh", "-c", "echo 'deliberate failure' >&2; exit 1"]),
            "probe",
            Duration::from_secs(10),
        )
        .await;
        assert!(!outcome.passed);
        assert!(
            outcome.detail.contains("deliberate failure"),
            "failure must surface captured output, got: {}",
            outcome.detail
        );
    }

    #[tokio::test]
    async fn run_gate_fails_on_empty_command() {
        let dir = std::env::temp_dir();
        let outcome = run_gate(&dir, &[], "probe", Duration::from_secs(10)).await;
        assert!(!outcome.passed);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // A multi-byte char straddling the cut must not panic. The cut lands
        // on a char boundary at or below `max`, and the ellipsis is added on
        // top, so the result is at most `max + "…".len()`.
        let s = "é".repeat(400);
        let out = truncate(&s, 300);
        assert!(
            out.len() <= 300 + "…".len(),
            "len {} exceeds max + ellipsis",
            out.len()
        );
        assert!(out.ends_with('…'), "expected an ellipsis, got {out:?}");
        assert!(
            out.is_char_boundary(out.len()),
            "cut must land on a boundary"
        );
    }
}
