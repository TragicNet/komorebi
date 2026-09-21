---
name: repo-verify
description: Verification and compatibility runbook for the komorebi repository (Rust, Windows). Covers the exact CI commands (fmt/clippy/test/cargo-deny), the frozen surfaces that must never break (komorebic commands, komorebi.json, app-specific-config schema) including schema/doc regeneration, and Commitizen commit conventions. Load before claiming any verification result or reviewing changes for this repo.
---

# repo-verify: verification & compatibility runbook

Load this before any verification claim, schema/CLI surface change, or review of changes
for the komorebi repo. Companion skill: `win32-debug` for debugging flow.

## 1. Frozen surfaces — never break (additive changes only)
- All `komorebic` commands: do not rename/remove commands or args, do not reorder or
  change existing flag semantics. New commands/options/additive states are fine.
- `komorebi.json` (main config) and the app-specific-config schema: existing keys,
  defaults, and types must remain parseable.
- `just jsonschema` regenerates the schemas (schema.json, schema.asc.json, schema.bar.json);
  run it and commit the regenerated files when a surface changes.
- `just docgen starlight` regenerates the CLI + schema reference docs; run it for
  komorebic surface changes.

## 2. Verification commands (mirror CI: `.github/workflows/windows.yaml`)
Run the project's exact commands; never claim a result unless you ran it.
- Format: `cargo +nightly fmt` (mutates). CI read-only check: `cargo +nightly fmt --check`.
  `just fmt` = fmt + clippy autofix + prettier on CI YAML.
- Lint: `cargo +stable clippy` must be clean (CI builds with `-Dwarnings`). Autofix: `just fix`.
  Use the `--locked` variant when you want to guarantee Cargo.lock is not rewritten.
- Tests: `cargo test`. Unit tests live in in-crate `#[cfg(test)]` modules
  (window_manager.rs, process_event.rs, monitor.rs, ...). Only meaningful on Windows.
- CI pipeline: fmt --check, clippy, cargo test, cargo-deny on windows-latest.
- Toolchain: stable Rust (`rust-toolchain.toml`); formatting via `cargo +nightly fmt`.

## 3. Commit conventions (Commitizen)
- Conventional Commits via Commitizen: use `git cz` (`.czrc` → cz-conventional-changelog).
- Commit bodies need at least one sentence of rationale.
- One PR = one feature/bug fix; no unrelated changes; no refactors without prior approval.
- Branching: `fix/` and `feature/` off `master`; update PRs with `git rebase master`;
  multi-feature work uses a local `local-trunk` branch rebased off master.

## 4. Review checklist (especially for the reviewer subagent)
- Frozen surfaces: any change to `komorebic` commands, `komorebi.json`, or the
  app-specific-config schema must be additive, schema-regenerated, and doc-gen'd.
- Win32 invariants from AGENTS.md: no blocking on slow work under the WM lock,
  no `HWND_TOPMOST` for layout, re-entrancy/lock-scoping of rule-list globals,
  `known_hwnds` ownership truth + atomic persistence, animation slot discipline.
- Rust hygiene: unchecked Win32 return codes, unsafe blocks/raw pointers,
  path handling, secrets/logging, dependency changes, `-Dwarnings` cleanliness.
- Verification reproducibility: exact commands from §2, not paraphrases.