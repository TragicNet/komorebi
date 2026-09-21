# AGENTS.md

## PROJECT
komorebi is a tiling window manager for Windows 10+ (Rust, Windows-only). It extends
DWM: a third-party hotkey daemon (whkd/AHK) → `komorebic` CLI → Unix domain socket →
the komorebi daemon, which reacts only to WinEvents and socket messages
(`docs/design.md`). It makes minimal OS modifications by default.

Workspace members: `komorebi` (the daemon/WM; nearly all core logic), `komorebic`
(CLI, command defs), `komorebi-bar`, `komorebi-gui`, `komorebi-client`,
`komorebi-layouts`, `komorebi-shortcuts`, `komorebi-themes`, `komorebic-no-console`.

Data model (monitors → workspaces → containers → windows): each window belongs to a
container; windows stack/cycle in a container slot in a ring. Windows live on layers
(tiling / floating / pinned) and in modes (tiled / maximized / monocle / floating).

## DEVELOPMENT
- Toolchain: stable Rust (`rust-toolchain.toml`); formatting via `cargo +nightly fmt`.
- `just` (`justfile`) is the command entry point: `just fmt`, `just build`, `just run <target>`, `just dev`.
- Build: `cargo +stable build --package komorebi --locked --release --no-default-features`.
  Dev builds drop the `schemars` default feature for speed.
- Run locally: `just dev` stops any running komorebi, builds `release-fast`, launches.
  `just debug|info|trace <target>` sets `RUST_LOG`. Only one `komorebi.exe` may run.
- For debugging or verification workflows, load the relevant skill: `win32-debug` (debugging,
  live inspection, crash recovery) or `repo-verify` (verification, frozen surfaces, commits).

## VERIFICATION
- Format: `cargo +nightly fmt` (`just fmt` also runs clippy + prettier on CI YAML).
- Lint: `cargo +stable clippy` must be clean (CI builds with `-Dwarnings`). Auto-fix: `just fix`.
- Tests: `cargo test`. Unit tests live in in-crate `#[cfg(test)]` modules (window_manager.rs,
  process_event.rs, monitor.rs, ...). Only meaningful on Windows. Never claim tests pass
  unless you actually ran `cargo test`.
- CI (`windows.yaml`): fmt --check, clippy, cargo test, cargo-deny on windows-latest.
- Frozen surfaces — no breaking changes: all `komorebic` commands, `komorebi.json`,
  and the app-specific-config schema. Prefer additive changes; `just jsonschema` regenerates schemas.
- Load the `repo-verify` skill before claiming any verification result or changing a frozen surface.

## WINDOWS / WIN32 INVARIANTS (read before touching window-management code)
- Central state is `Arc<Mutex<WindowManager>>` (parking_lot). Every WinEvent is processed
  while holding the lock (process_event.rs); commands acquire via try_lock_for with a 10s
  budget (`LOCK_WAIT_BUDGET`, process_command.rs). Assume the WM lock is held on the main path.
- NEVER block on slow work while holding the WM lock. Synchronous `SetWindowPos` marshals to
  the target window's thread and can hang the whole WM on a hung window. Offload z-order ops
  to `apply_worker` (dedicated FIFO thread); prefer `SWP_ASYNCWINDOWPOS`; probe responsiveness
  before sync calls (`is_window_thread_responding`, `skip_unresponsive_window`).
- Re-entrancy: rule-list globals (`REGEX_IDENTIFIERS`, `FLOATING_APPLICATIONS`, ...) must be
  scoped/dropped before mutation because layer-stack passes re-enter `should_manage()`, which
  re-locks them — holding one across such a pass self-deadlocks (parking_lot mutexes are not re-entrant).
- Window lifecycle: `wm.known_hwnds` (hwnd → (monitor, workspace)) is ownership truth; rebuilt
  only on mapping-changing events and persisted atomically (tmp+rename) to `komorebi.hwnd.json`
  (recover via `komorebic restore-windows`). The `reaper` thread polls every 20ms and removes
  dead windows. `komorebi stop`/Ctrl-C restores all hidden windows.
- Focus / z-order: `raise_window_above_active` climbs a foreground window via a transient
  `HWND_TOPMOST`→`HWND_NOTOPMOST` dance (never leaves sticky TopMost). NEVER use `HWND_TOPMOST`
  for layout positioning (sticky + viral across owned windows). Layer order via `Monitor::enforce_layer_stack`.
- Animations: one render slot per animation key; a newer claim supersedes older ones, stale
  runners are force-released (~min(duration+250ms, 1.5s)); PostRender is never preempted; a
  dispatcher must not touch the window after losing its slot. Shutdown waits for all animations.
- Side-channel threads (border, stackbar, transparency, monitor_reconciliator, reaper, focus,
  theme) each hold the WM lock briefly — keep their handlers short.
- Core files: `window_manager.rs` (orchestration), `workspace.rs` (update/retile),
  `monitor.rs` (layer stack), `window.rs` (per-window ops), `windows_api.rs` (Win32 wrappers),
  `process_event.rs` (event loop), `process_command.rs` (commands), `apply_worker.rs`, `animation/`, `ring.rs`.

## GIT / PR WORKFLOW
- Conventional Commits via Commitizen: use `git cz` (`.czrc`). Commit bodies need ≥1 sentence of rationale.
- One PR = one feature/bug fix; no unrelated changes; no refactors without prior approval.
- Branching: `fix/` and `feature/` off `master`; update PRs with `git rebase master`.
  For multi-feature work use a local `local-trunk` branch rebased off master.

## COMMON DEVELOPMENT WORKFLOWS
- Live inspection, smoke testing, crash recovery, and deadlock/procdump debugging: load the
  `win32-debug` skill before starting.

## AGENT BEHAVIOR
- Read the relevant code path before changing it (event/command → window_manager → workspace → windows_api).
- Think locking/re-entrancy first: the main path runs with the global Mutex held.
- Verify with the project's exact commands; never claim tests/lint passed unless you ran them.
- Load the relevant skill before deep work: `win32-debug` before debugging komorebi behavior,
  `repo-verify` before verification or frozen-surface changes.
- Keep changes minimal and scoped; no drive-by refactors.
- Be careful with git: no commit/push/force-push unless explicitly asked.