# Niri Taskbar (for Waybar)

This provides a [Waybar][waybar] taskbar for [Niri][niri].

The main shift from the builtin `wlr/taskbar` module is that windows are always
ordered by workspace index, then window ID (which essentially means that the
windows are ordered by creation time, at least as of Waybar 0.12.0).

![Example screenshot](images/screenshot.png)

## Installation

If you package niri-taskbar for another distro/OS, please let me know and I'll
update the README.

### Arch Linux

@coreymwamba has kindly packaged this as [the `waybar-niri-taskbar` package in
the AUR][aur].

### From source

Users of other distributions and OSes will need to build from source.

At the moment, this needs to built from source.
distro, let me know and I'll update the README.)

### Requirements

- Rust 1.87.0 or later
- Niri (with a version corresponding to the version in the `niri-taskbar` crate
  version; eg `0.4.0+niri-25.11` is specifically for Niri 25.11)
- Gtk+ 3 (including the development package on distros that separate those out)
- Waybar 0.12.0 (or any version that's API compatible with 0.12, which will
  _probably_ include later versions, but I have no actual knowledge there)

### Building

The standard Rust build process should work fine:

```bash
$ cargo build --release
```

This will give you a shared library module at
`target/release/libniri_taskbar.so`. Feel free to move that wherever makes
sense.

## Configuration

This uses the normal configuration for a [CFFI Waybar module][cffi], which in
practice will look something like this:

```jsonc
{
  "modules_left": ["cffi/niri-taskbar"],
  // ...
  "cffi/niri-taskbar": {
    "module_path": "/your/path/to/libniri_taskbar.so",
  },
}
```

### Application highlighting

In addition to [notification support](#notifications), you can highlight
applications based on their app ID and title by configuring application rules in
the Waybar configuration.

For example, to highlight a Signal window that starts with `(1)` (or any other
number), indicating pending notifications, you could configure the taskbar like
so:

```jsonc
{
  "cffi/niri-taskbar": {
    // module_path
    "apps": {
      "signal": [
        {
          "match": "\\([0-9]+\\)$",
          "class": "unread",
        },
      ],
    },
  },
}
```

Each key within the `apps` object is a Wayland app ID, which can have one or
more rules set within it. Each rule must have a `match`, which is a regex that
will be matched against the window title, and a `class`, which is a CSS class
that will be added to the button element if the regex matches.

If more than one rule matches for a single app ID, all matching classes will be
added.

The easiest way to get the app ID for a window is to ask Niri with `niri msg
windows`. Note that app IDs are case sensitive.

### Scrolling anywhere on the bar to cycle windows

You can enable `scroll_windows` to cycle through visible taskbar windows with
the scroll wheel:

```jsonc
{
  "cffi/niri-taskbar": {
    // other settings
    "scroll_windows": true,
    "scroll_scope": "bar",
    "scroll_wrap": false,
    "scroll_reverse": false,
  },
}
```

Options:

- `scroll_scope`: where scroll is captured. `"bar"` uses the full Waybar strip;
  `"taskbar"` limits it to the taskbar module area.
- `scroll_wrap`: whether scrolling past the end jumps back to the beginning.
- `scroll_reverse`: reverses the scroll direction.

This uses the taskbar's current visible window order and is intended for setups
where you want to throw the cursor to the edge of the screen and scroll to
switch windows.

### Multiple outputs

By default, the taskbar will only show applications running on the same output
as the taskbar itself. You can enable the `show_all_outputs` option to show all
applications on all outputs:

```jsonc
{
  "cffi/niri-taskbar": {
    // other settings
    "show_all_outputs": true,
  },
}
```

The taskbar works out which output it is on by matching the bar's monitor
against Niri's output list, and re-checks this whenever outputs are added,
removed, or reconfigured (for example, when a monitor is switched off or changes
mode). Note that multiple output support is still somewhat experimental, and may
have some quirks. Please open an issue with your use case if it's not working as
you expect!

### Showing only the active workspace

By default, the taskbar shows windows from every workspace on its output,
ordered by workspace index. If you would rather it only show the workspace you
are currently looking at, which more closely matches how Niri itself works,
enable the `active_workspace_only` option:

```jsonc
{
  "cffi/niri-taskbar": {
    // other settings
    "active_workspace_only": true,
  },
}
```

The taskbar updates as you switch workspaces. When combined with bar scrolling,
only the windows on the active workspace are cycled through.

You can also have the taskbar slide between workspaces in the same direction
Niri does, by setting `workspace_animation_ms` to the transition duration in
milliseconds (something around 250 is close to Niri's default feel):

```jsonc
{
  "cffi/niri-taskbar": {
    // other settings
    "active_workspace_only": true,
    "workspace_animation_ms": 250,
  },
}
```

### Sliding focus indicator

Instead of styling the focused button directly, the taskbar can draw a pill
underneath it that slides along as the focus moves:

```jsonc
{
  "cffi/niri-taskbar": {
    // other settings
    "focus_indicator": true,
    "focus_indicator_ms": 180,
    "focus_indicator_height": 3,
  },
}
```

`focus_indicator_ms` is how long the slide takes, and `focus_indicator_height`
is the thickness of the pill in pixels. Set the duration to 0 to have it jump
instead of sliding.

The pill is drawn rather than packed into the row, so it can sit between two
buttons while it moves and never disturbs their layout. Style it through the
`indicator` class, which accepts the usual background, border, and border radius
properties, plus horizontal margins to inset it from the edges of the button:

```css
.niri-taskbar .indicator {
  background-color: #00f0f0;
  border-radius: 999px;
  margin: 0 8px 2px 8px;
}
```

Because the pill shows which window is focused, you will usually want to drop
whatever the `focused` class was doing to mark it before.

A second pill can follow the pointer instead of the focus. It grows up from
underneath the button and opens outwards from the middle, then retracts the same
way when the pointer leaves:

```jsonc
{
  "cffi/niri-taskbar": {
    // other settings
    "hover_indicator": true,
    "hover_indicator_ms": 150,
    "hover_indicator_height": 3,
  },
}
```

The two are independent, so either can be used without the other. The hover pill
takes the same `indicator` styling, plus a `hover` class of its own, and is drawn
underneath the focus pill where they overlap:

```css
.niri-taskbar .indicator.hover {
  background-color: rgba(0, 240, 240, 0.35);
}
```

### Notifications

You can enable the `notifications` configuration option to have the taskbar
listen to notifications and attempt to highlight the app that sent the
notification.

Configuration wise:

```jsonc
{
  "cffi/niri-taskbar": {
    // other settings
    "notifications": true,
  },
}
```

Highlighted buttons will gain the `.urgent` CSS class. Default styling is
included, but can be overridden [as described below](#styling).

## Styling

The taskbar uses [the same Gtk styling mechanism as Waybar][style]. The top
level taskbar element is given the class `.niri-taskbar`, and contains `button`
elements within it. The only CSS class that is applied by default is the
`focused` class, which is added to the button for the currently focused window.

The default styling assumes a dark background. It provides a basic hover
effect, and highlights the focused window.

For a light background, something like this is likely good:

```css
.niri-taskbar button:hover {
  background: rgba(0, 0, 0, 0.5);
}

.niri-taskbar button.focused {
  background: rgba(0, 0, 0, 0.3);
}
```

If you apply custom CSS classes using application rules as described above,
then those can be styled in the same way. For instance, with the `unread` class
demonstrated above, you could add a border highlight like so:

```css
.niri-taskbar button {
  /* This is useful to prevent the icon being resized when there's no unread. */
  border-bottom: solid 3px transparent;
}

.niri-taskbar button.unread {
  border-bottom: solid 3px white;
}
```

[aur]: https://aur.archlinux.org/packages/waybar-niri-taskbar
[cffi]: https://github.com/Alexays/Waybar/wiki/Module:-CFFI
[niri]: https://github.com/YaLTeR/niri
[style]: https://github.com/Alexays/Waybar/wiki/Styling
[waybar]: https://github.com/Alexays/Waybar
