# Keyboard reference

Every binding Lyrachi ships with, grouped by scope. The scope that is active
wins: when the help overlay, context menu, or search input is open, its keys
take priority over the global ones. `Ctrl+C` is bound to quit in every scope.
Press `?` inside the TUI to see this list rendered live.

## Global

| Key            | Action                                                         |
| -------------- | -------------------------------------------------------------- |
| `?`            | Toggle the help overlay                                        |
| `r`            | Refetch lyrics for the current track                           |
| `f`            | Toggle fullscreen                                              |
| `t`            | Toggle the title bar                                           |
| `s`            | Toggle the status bar                                          |
| `Tab`          | Peek at a hidden status bar (3 seconds)                        |
| `m`            | Cycle display mode: normal, block, kinetic, cylinder, canvas3d |
| `w`            | Toggle word-level highlighting                                 |
| `S`            | Cycle style presets                                            |
| `[`            | Shift timing -100 ms                                           |
| `]`            | Shift timing +100 ms                                           |
| `\`            | Reset the timing offset                                        |
| `-`            | Stage speed -10%                                               |
| `=` / `+`      | Stage speed +10%                                               |
| `0`            | Reset stage speed                                              |
| `{`            | Crawl tilt -5° (canvas3d)                                      |
| `}`            | Crawl tilt +5° (canvas3d)                                      |
| `,`            | Spawn height -2% (canvas3d)                                    |
| `.`            | Spawn height +2% (canvas3d)                                    |
| `<`            | Text zoom -10% (canvas3d)                                      |
| `>`            | Text zoom +10% (canvas3d)                                      |
| `Up` / `Down`  | Scroll unsynced lyrics                                         |
| `c` / `Enter`  | Open the context menu                                          |
| `q` / `Ctrl+C` | Quit                                                           |

Stage keys only have an effect in the modes they tune: speed applies to
cylinder and canvas3d; tilt, spawn height, and zoom apply to canvas3d only.
The help overlay groups them the same way and dims sections that do not apply
to the active mode.

## Context menu

Opened with `c` or `Enter`. Pick a lyric source candidate, edit the search
query, reset the preferred source, or pin a style.

| Key           | Action                      |
| ------------- | --------------------------- |
| `Up` / `Down` | Move the selection          |
| `Enter`       | Select the highlighted item |
| `Esc`         | Close the context menu      |
| `Ctrl+C`      | Quit                        |

## Search input

Opened from the context menu. Two fields: artist and title, switched with
`Tab`, `Up`, or `Down`.

| Key                   | Action                                     |
| --------------------- | ------------------------------------------ |
| `Tab` / `Up` / `Down` | Switch between the artist and title fields |
| printable characters  | Type into the active field                 |
| `Backspace`           | Delete the character before the cursor     |
| `Enter`               | Submit the search                          |
| `Esc`                 | Cancel the search                          |
| `Ctrl+C`              | Quit                                       |

## Help overlay

| Key           | Action                                          |
| ------------- | ----------------------------------------------- |
| `?` / `Esc`   | Close the overlay                               |
| `Up` / `Down` | Scroll the overlay when it exceeds the viewport |
| `Ctrl+C`      | Quit                                            |

The status bar shows a `? help` hint on the right edge at 80 columns or
wider, and idle screens show a `? keybindings` hint.
