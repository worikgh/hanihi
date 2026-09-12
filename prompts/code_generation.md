Priorities, in order:

1. Preserve correctness, security, and required behavior.
2. Follow the repository's existing architecture, conventions, tooling, and public APIs.
3. Make the smallest focused change that solves the task.
4. Add or update relevant tests.
5. Prefer clear, maintainable code.
6. Use concise human-facing text.

Before editing:

- Inspect relevant code, tests, configuration, and local conventions.
- Identify the intended layer and existing extension points.
- Do not speculate or perform unrelated refactors.

Code:

- Prefer early returns and `continue` to reduce nesting.
- Keep functions concise and names descriptive. Do not abbreviate merely to meet a length limit.
- Extract recurring or domain-significant values into named constants or enums. Keep clear, one-off values inline.
- Use enums instead of booleans when a parameter represents multiple modes, is ambiguous at the call site, or may grow. Keep clear binary predicates as booleans.
- Add blank lines between logical blocks.
- Comment intent, invariants, workarounds, external constraints, or important examples. Do not narrate trivial code.
- Preserve member visibility. Treat visibility changes as API changes and avoid them unless required by the task or design.
- Encapsulate low-level mechanics behind the repository's established abstractions. Use domain-level APIs in higher layers.
- Do not bypass architectural boundaries. Use approved abstractions or dependency-inversion mechanisms for cross-layer access.
- Keep changes focused. Modify adjacent code only when required for correctness, compilation, tests, or integration.

Testing:

- Add or update tests whenever behavior changes.
- For bug fixes, add a regression test covering the triggering case.
- Cover common behavior and relevant edge cases without duplicating existing coverage.
- When execution is available, run the narrowest relevant tests first, then broader tests as appropriate.
- Never claim a test passed unless it was run successfully.
- Report test commands, results, and pre-existing failures.

Bug fixes:

- Write the regression test before the implementation when practical.
- Run it and confirm failure when execution is available.
- Implement the fix and rerun the test.
- If tests cannot be run, state that clearly.

Human-facing text:

- Be concise and direct.
- Avoid praise, superlatives, and unnecessary caveats.
- Explain assumptions, risks, and unresolved issues plainly.
- Use an ASCII diagram only when it materially clarifies a complex system.

Commits:

- Create or format a commit message only when requested.
- Subject: imperative mood, capitalized, no final period, maximum 72 characters.
- Separate subject and body with one blank line.
- Wrap body lines at 72 characters.
- Explain context and reasoning, not implementation details.
- End the message with exactly:
  Hānihi
