# 006-cpp-build

The agent makes a change to a real C++ (CMake) project and the eval verifies
the result compiles and its tests still pass. This is the C++ counterpart of
`004-self-patch`: same loop, different toolchain.

## Layout

```
fixture/
├── CMakeLists.txt      project(cpp_fixture CXX), enable_testing(), add_test
├── include/foo.hpp     declares fixture::add(int, int)
├── src/foo.cpp         defines fixture::add(int, int)  (the library target)
└── src/foo_test.cpp    asserts add(2,3)==5 and add(-1,1)==0, exits non-zero otherwise
```

`foo_test` links the `foo` library and is registered with
`add_test(NAME foo_add COMMAND foo_test)`, so `ctest` has exactly one test to
run. The test exits non-zero on a failed assertion, which is what makes
`tests_pass` meaningfully red when the library is deliberately broken.

## Gates

Unlike a Rust case, the commands are supplied per case rather than defaulted:

```toml
repo = "fixture"
configure_command = ["cmake", "-B", "build"]
build_command = ["cmake", "--build", "build"]
test_command = ["ctest", "--test-dir", "build", "--output-on-failure"]
```

`build_succeeds` runs `configure_command` first and only then `build_command`.
A configure failure skips the build and reports the configure output, so the
diagnostic names the step that actually broke.

`ctest` fails if the test binary is missing, but `add_test`'s target must
already exist before the test is registered, so the ordering above is also
what makes `tests_pass` meaningful rather than vacuous.

## No `lint_clean` here

This case deliberately omits `lint_clean`. `clang-tidy` needs a
`compile_commands.json` produced by a successful configure, and there is no
canonical C++ equivalent of `clippy` that is always available. Adding a
`lint_command` would mean either a tool that may not exist on the machine or a
no-op that reports coverage the case does not have. The honest gate set for
C++ is `build_succeeds` + `tests_pass`, which is what this case uses.

## Requirements

`cmake` and a C++ compiler (g++ or clang++) must be on `PATH`. The eval's gate
tests in `crates/hanihi-eval` fail loudly, naming the missing tool, rather
than silently skipping, if either is absent.

## Note on the asymmetry

The eval gates are case-authored commands executed directly by the runner;
they do not go through the agent's `run_command` allowlist. So this case can
pass its build gate even if the agent could not have run `cmake` itself. The
eval verifies the **artifact**, not the agent's process.
