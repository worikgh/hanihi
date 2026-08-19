# 004-self-patch

The agent edits a throwaway fixture repo (created by the runner), commits
its change, and verifies the build. This is the self-improvement loop in
miniature — and it exercises the gameability guardrail: the fixture is a
disposable repo, never the hānihi checkout itself.

Assertions: `apply_patch` was called, the build still passes, and the
working tree is clean (`no_diff`) — i.e. the agent committed its change.
