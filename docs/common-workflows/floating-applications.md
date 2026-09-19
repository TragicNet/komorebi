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

You can pin a floating window so that it stays visible across all workspaces on its
monitor. Only floating windows can be pinned; tiling windows are never pinned.

Use the `toggle-pin` command to pin or unpin the currently focused floating window:

```bash
komorebic toggle-pin
```

Unpinning leaves the window on its current workspace, where it behaves like any
other floating window and is hidden when you switch away.

To have a floating window pinned automatically when it is launched, add the
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
