# Report 004: Resume-time prompt drift

**Status: open. Not fixed. Recorded here so it can be addressed deliberately.**

## 1. The observation

`session.json` stores the system prompt as a plain string at session creation:

```rust
// crates/hanihi-core/src/session/mod.rs:289 (Session::create)
let meta = serde_json::json!({
    "id": id.to_string(),
    "name": name,
    "created_at": created_at.to_rfc3339(),
    "model": model,
    "system_prompt": system_prompt,
});
```

The CLI resolves *which* string that is before creating the session, in
`main.rs`:

```rust
// crates/hanihi-cli/src/main.rs:295
let task_mode = args.task.is_some();
let base_prompt: String = if task_mode {
    hanihi_core::agent::task_system_prompt(toolchain)   // toolchain-dependent
} else {
    hanihi_core::agent::DEFAULT_SYSTEM_PROMPT.to_string()
};
```

and on resume, when no `--prompt` / `--prompt-file` additions are supplied, the
stored string is replayed verbatim:

```rust
// crates/hanihi-cli/src/main.rs:358
let stored = stored_session_prompt(&working_dir, &session_name)
    .unwrap_or_else(|| base_prompt.to_string());
(stored, false)
```

`base_prompt` is computed on every run — including this one — but it is
**discarded** on the resume path. The `toolchain` value threaded into the
preamble is likewise computed and then unused.

## 2. Consequence

The workflow paragraph the model receives is decided by the repository the
session was **created** in, not the repository it is **running** in. Three
concrete cases:

| Case | Prompt carries | Repository actually is |
|---|---|---|
| Session created before this change, resumed now | Cargo workflow | this repo (Cargo) — correct by luck |
| Session created in this repo, resumed from a CMake checkout | Cargo workflow | CMake — **wrong** |
| Session created in a CMake checkout, resumed here | CMake workflow | this repo (Cargo) — **wrong** |

The failure mode is the exact one plan 022 set out to remove: a model told to
run `cargo fmt`/`cargo clippy` in a repository that has no `cargo`, and no
indication anywhere that the instruction is stale. The change shipped in this
series removes that contradiction on the creation path only.

Note the third row is a *regression relative to pre-change behaviour* in one
narrow sense: before this series, all prompts were the Rust-workflow string, so
a session carried in a CMake checkout was wrong but *predictably* wrong. Now
the stored prompt can be either, and which one it is is not visible from the
resume path.

## 3. Why it was not fixed here

Fixing it requires choosing what `session.json`'s `system_prompt` *means*.
Two candidate designs, with the trade-off that made neither obviously correct
without a decision:

**A. Rebuild the preamble on every resume.** Treat `system_prompt` as a
stored *user addition* rather than a stored prompt: at open time, recompute
`task_system_prompt(toolchain)` and append whatever the user supplied.

- Removes the drift entirely.
- Changes the field's meaning. Every existing `session.json` on disk holds a
  full prompt, base plus additions, with no marker separating them. Splitting
  them retroactively is not possible in general — an old session's
  `system_prompt` is one string and there is no record of where the base
  ended.
- `--prompt` additions would need to be re-applied from somewhere on resume,
  and they currently live only inside that concatenated string.

**B. Detect the mismatch and warn.** Compare the stored prompt against the
prompt this invocation would generate and print a notice when they differ.

- Non-destructive; no schema or semantics change.
- Needs a rule for "matches". Byte comparison fails for any session created
  before this change, and for any session with a `--prompt` addition.
  Detecting "the stored prompt names a tool the current toolchain does not
  have" is looser but is exactly the condition that matters, and it is what
  the acceptance criteria are written in terms of ("no occurrence of `cargo`"
  in a CMake repository).

Neither is a one-line change, and B needs a decoding rule of its own. The plan
explicitly said not to silently change resume semantics, so the change was
left out rather than guessed at.

## 4. The narrower, certain problem

Independent of A or B, there is a straightforward defect: `main.rs` computes
`base_prompt` on every invocation and then throws it away on the resume path.
Whatever is decided about resume semantics, the computed-and-unused value is
dead today. A `--new-prompt` with no additions supplied also silently resets
the prompt to `base_prompt` (line 331), which means "reset to this toolchain's
default" — that path is correct, and it is the one escape hatch that exists
today.

## 5. What to decide

1. Does `session.json`'s `system_prompt` mean "the prompt" (current) or "the
   user's additions to the derived prompt" (design A)?
2. If A: what happens to the sessions already on disk, whose field conflates
   both? A migration marker would be needed, or the field kept as-is and a new
   field introduced alongside it.
3. If B: is the warning about tool names, about the toolchain, or a byte
   comparison, and what does it print?
4. Should a resumed session in a *different* toolchain than its creation repo
   be an error, a warning, or silent?

Recorded, not acted on.
