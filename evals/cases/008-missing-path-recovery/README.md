# 008 — Missing path recovery

The agent is asked to list a directory that does not exist in the fixture
repo, so the error path is taken deterministically. The acceptable outcomes
are an explicit "does not exist" and the nearest-existing-ancestor hint added
in commit `1eeaf22`.

The assertion accepts the error text being surfaced in the answer rather than
pinning exact wording, so an improvement to the hint's phrasing does not break
the case. It also exercises `list_dir`, the tool the hint is attached to.

`fixture = true` synthesises a repo that does not contain the requested path,
so the ancestor branch is reached without depending on the state of the
hānihi checkout. This case needs a live model and is not part of `cargo test`.
