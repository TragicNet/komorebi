---
name: win32-debug
description: Deep debugging and live-session troubleshooting for the komorebi Windows window manager (Rust). Covers the event/command flow through window_manager/workspace/windows_api, RUST_LOG workflows, deadlock detection, procdump, window recovery, and the single-instance/live-session caveats. Load before debugging komorebi behavior or diagnosing hangs/crashes.
---

# win32-debug: komorebi debugging runbook

Use this for debugging komorebi behavior (windows not tiling, focus/z-order issues,
appears hung, deadlock, crash recovery) and for inspecting live WM state. It complements
AGENTS.md's WIN32 invariants — the invariants still apply to any code you change.

Companion skill: `repo-verify` for verification claims (fmt/lint/tests/frozen surfaces).

## 1. Read the code path first
Follow the flow before changing anything or drawing conclusions:

event/command → `window_manager.rs` (orchestration) → `workspace.rs` (update/retile)
→ `windows_api.rs` (Win32 wrappers), with `monitor.rs` (layer stack), `window.rs`
(per-window ops), `process_event.rs` (event loop), `process_command.rs` (commands),
`apply_worker.rs`, `animation/`, `ring.rs`.

Keep the locking/re-entrancy rules from AGENTS.md front of mind: the main path holds
the WM lock; side-channel threads hold it only briefly.

## 2. Inspect live state
- `komorebic state` — full `State` as JSON (monitors/workspaces/containers/windows, focus, modes).
- `komorebic check` — configuration/state sanity.
- `komorebic subscribe-socket <name>` / named pipe — stream the events the WM processes.

## 3. Smoke test / iterate locally
- `komorebic stop` (stop any running session) → `just dev` (builds `release-fast`, launches) → drive with `komorebic`/whkd → watch `RUST_LOG=debug just run komorebi` output.
- Log levels: `just debug|info|trace <target>` sets `RUST_LOG` (target = `komorebi`, `komorebic`, ...). `RUST_LOG=debug just run komorebi` works too.

## 4. Single-instance + live-session caveats
- Only one `komorebi.exe` may run at a time. Start it via `just dev` / `just run`, not manually.
- Running a WM session changes live window state (restores hidden windows, retiles, churns
  `komorebi.hwnd.json`). Prefer a disposable session/VM when testing disruptive behaviors.
- `komorebi stop` / Ctrl-C restores all hidden windows before exit.

## 5. Hangs / deadlocks
- Deadlock detection: `just deadlock` — runs `komorebi` with the `deadlock_detection`
  feature (`parking_lot/deadlock_detection`) under `RUST_LOG=trace`; look for the deadlock
  thread-dump backtraces printed on timeout.
- Suspect spots: a layer-stack pass re-entering `should_manage()` while a rule-list global
  (`REGEX_IDENTIFIERS`, `FLOATING_APPLICATIONS`, ...) is held, or slow synchronous work
  (`SetWindowPos` marshaling to a hung window's thread) while holding the WM lock.
- A hung whole-WM usually traces to sync window calls; prefer `SWP_ASYNCWINDOWPOS` and the
  responsiveness probes (`is_window_thread_responding`, `skip_unresponsive_window`).

## 6. Crash dump collection (`procdump`)
- `just procdump` — runs komorebi under procdump. Requires `procdump.exe` (gitignored)
  alongside the repo; do NOT commit it. Use when there is no other way to capture a hang.

## 7. Crash recovery
- `komorebic restore-windows` restores windows from `komorebi.hwnd.json`
  (`wm.known_hwnds` persisted atomically via tmp+rename).
- On exit, state is dumped to `%TEMP%\komorebi.state.json` and auto-applied on next start;
  `--clean-state` skips that restore.

## 8. Verification
Never claim a fix is verified from debugging logs alone — run the project's actual
verification (load the `repo-verify` skill): `cargo +nightly fmt --check`,
`cargo +stable clippy`, `cargo test`.