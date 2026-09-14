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
application window above the managed base layer; turning it off lowers them back
below the base layer. System shell surfaces (taskbar, desktop) and tool windows are
never moved.