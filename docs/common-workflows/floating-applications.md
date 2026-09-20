# Floating Windows

Sometimes you will want a specific application to be managed as a floating window.
You can add rules to enforce this behaviour in the `komorebi.json` configuration file.

```json
{
  "floating_applications": [
    {
      "kind": "Title",
      "id": "Media Player",
      "matching_strategy": "Equals"
    }
  ]
}
```

## Pinning Floating Windows Across Workspaces

You can pin a window so that it stays visible across all workspaces on its
monitor. Pinning works with both floating and tiling windows: a focused tiling
window is floated and pinned on the same command.

Use the `toggle-pin` command to pin or unpin the currently focused window:

```bash
komorebic toggle-pin
```

Unpinning leaves the window on its current workspace, where it behaves like any
other floating window and is hidden when you switch away.

To have a window pinned automatically when it is launched, add the
`pinned` key to one of its `floating_applications` rules:

```json
{
  "floating_applications": [
    {
      "kind": "exe",
      "id": "example.exe",
      "pinned": true
    }
  ]
}
```

### Hiding Pinned Windows On Empty Workspaces

By default a pinned window stays visible across every workspace, even when the
focused workspace has no managed windows in it. To hide pinned windows whenever
the focused workspace is empty instead, enable the `pinning` config key:

```json
{
  "pinning": {
    "hide_on_empty_workspaces": true
  }
}
```

When this is enabled, switching to an empty workspace hides all pinned windows
on the monitor; switching to a workspace with managed windows brings them back.

### Always-on-Top Pinned Windows

A pinned window can be kept always on top, above every other window on its
monitor including tiled windows:

```bash
komorebic toggle-pin-always-on-top
```

Toggling this again returns the window to a normal pinned window. The setting
only applies to pinned windows; use it with a pinned window focused.
