# 003-self-build

The agent runs `cargo check` on the hānihi workspace (read-only) via
`run_command` and reports the exit code. Verifies the analysis + build loop
works end-to-end against a real repo.

`repo = "../.."` points at the hānihi checkout (the case lives in
`evals/cases/003-self-build/`). The case is read-only: no write tools are
registered.
