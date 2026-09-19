# toggle-ignored-window-layer

```
Toggle ignored (unmanaged) windows on the focused monitor above or below the managed
windows

Usage: komorebic.exe toggle-ignored-window-layer

Options:
  -h, --help
          Print help

```

Toggle the ignored (unmanaged) windows on the focused monitor above or below the
managed windows. Turning it on raises every ignored window that looks like a regular
application window (including a fullscreen game such as DFO) above the managed base
layer, and turns the layers back by lowering them below the base layer. System shell
surfaces (taskbar, desktop) and always-on-top widget windows (e.g. a yasb bar with
always_on_top) are never moved, so the bar does not flicker when the layer is
toggled; they are already pinned above everything else)Skip

Raised ignored windows are also activated so that they actually come to the front
(raising with HWND_TOP alone is not enough while a managed window still holds the
foreground).