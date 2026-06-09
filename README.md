# cliphist-tui

Old name: shorinclip

A Wayland clipboard TUI with rich media (images/GIFs/videos, etc.) preview based on `fzf` `wl-clipboard` `cliphist`.

Use `chafa` for image preview, `kitty icat` for GIF preview when using native kitty, `ffmpegthumbnailer` for video thumbnail generation.

## Showcase

- Ctrl+X delete selection

![](pictures/delete-selection.gif)

- Auto refresh when clipboard changed and Alt+X delete all

![](pictures/delete-all-and-instance-refresh.gif)

- Ctrl+E/O open videos or pictures from clipboard manager

![](pictures/open-from-clipboard.gif)

## Installation

```
yay -S cliphist-tui-git
```

For best image preview, a terminal which supports kitty image protocol is needed, such as `kitty` or `ghostty`, or you can let chafa handle image preview (maybe low quality).

For example:

- foot

    ![](pictures/foot.png)

- alacritty

    ![](pictures/alacritty.png)

## Usage

Open cliphist daemon with this command:

```
wl-paste --watch cliphist store
```

Open the TUI with this command: `cliphist-tui` or `shorinclip`.

Then it just works.

Don't forget to set up autostart in your Wayland compositor's config file.

- Niri

    ```
    spawn-at-startup "wl-paste" "--watch" "cliphist" "store"
    ```

- Hyprland

    ```
    exec-once = wl-paste --watch cliphist store
    ```
