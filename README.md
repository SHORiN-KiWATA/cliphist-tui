# shorinclip

My first bash script.

A wayland clipboard TUI based on `fzf` `wl-clipboard` `cliphist`.

Use `chafa` for image preview, `kitty icat` for gif preview when using native kitty, `ffmpegthumbnailer` for video thumb generation.

## Showcase

- kitty

![](pictures/kitty2.png)

- useful features

  - Ctrl+X delete selection

    ![](pictures/delete-selection.gif)

  - Auto refresh when clipboard changed and Alt+X delete all

    ![](pictures/delete-all-and-instance-refresh.gif)

  - Ctrl+E/O open videos or pictures from clipboad manager

    ![](pictures/open-from-clipboard.gif)

## Installation

```
yay -S shorinclip-git
```

For best image preview, a terminal which supports kitty image protocol is needed, such as `kitty` or `ghostty`, or you can let chafa handle image preview (maybe low quality).

For example :

- foot

    ![](pictures/foot.png)

- alacritty

    ![](pictures/alacritty.png)

## Usage

Open cliphist daemon with this command:

```
wl-paste --watch cliphist store
```

Open tui with this command: `shorinclip`

Then it just works.

Don't forget to setup autostart in your wayland compositor's config file.

- Niri

    ```
    spawn-at-startup "wl-paste" "--watch" "cliphist" "store"
    ```

- Hyprland

    ```
    exec-once = wl-paste --watch cliphist store
    ```
