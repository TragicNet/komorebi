# Cycle Focus Across Monitors

By default, `CycleFocusWindow` stays on the currently focused monitor and wraps
within that workspace.

If you want `CycleFocusWindow` to continue onto the next or previous monitor
when you reach the first or last tiled window, enable
`cycle_focus_across_monitors` in `komorebi.json`.

```json
{
  "cycle_focus_across_monitors": true
}
```

When this option is enabled, `komorebi` only wraps across monitors that are on
the tiling layer. Workspaces that are in the floating layer or currently using a
monocle container are skipped.
