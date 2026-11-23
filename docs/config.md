# Configuration reference

Lyrachi reads one TOML file and applies it at startup. This page documents
every key, its default, its allowed range, and what happens when a value is
wrong. For a machine-readable version of the same information, run
`lyrachi config schema` (JSON Schema, draft 2020-12).

## File location and precedence

The config file lives at `$XDG_CONFIG_HOME/lyrachi/config.toml` (usually
`~/.config/lyrachi/config.toml`). The `--config <path>` flag points at a
different file, and every other path follows from it: the styles directory is
always the config file's parent plus `styles/`, so `--config ~/test/my.toml`
implies `~/test/styles/`.

Values resolve in this order, later sources winning:

1. Built-in defaults
2. `config.toml`
3. CLI flags (`--offset-ms`, `--offline`, and so on)

A missing config file is not an error and produces no warning. Lyrachi simply
runs on defaults.

## How invalid values are handled

Three behaviors, worth knowing before editing:

- **Unknown or malformed keys discard the whole file.** Every section rejects
  unknown keys. One typo (for example `fulscreen = true`) throws away the
  entire file, including the sections that were valid, and a warning explains
  why at startup. The same applies to enum keys given an invalid string
  (`ui.status_bar`, `terminal.unicode`, `terminal.color_mode`).
- **Out-of-range numbers are clamped, not rejected.** A value outside its
  documented range is pulled to the nearest limit and a warning says so. The
  rest of the file still applies.
- **Some string keys warn per key.** Free-form string values with a known
  vocabulary warn and fall back individually; the rest of the file survives.
  This applies per entry to `media.backends` and `allowed_origins`, but not to
  `providers.order`: one unknown name there reverts the whole order list to
  the default.

Run `lyrachi config check` to see exactly which of these your file triggers.
It validates the config and the active style, prints each warning, and exits
nonzero only when something fails hard: the file cannot be parsed, it cannot
be read, or the active style fails to load. Warnings alone exit 0.

One more edge: a number that does not even fit its field's integer type (for
example `70000` in a 16-bit field, or a negative value in an unsigned field)
fails TOML parsing instead of clamping, and that discards the whole file.
Clamping applies to values that fit the type but exceed the documented range.

## `schema_version`

Optional integer at the root of the file. The only accepted value is `1`
(`lyrachi config print-default` writes it). Any other value warns but the file
still applies; the key exists for future format migrations.

## `[player]`

| Key      | Type   | Default | Description                                                                                                                                                                                                                       |
| -------- | ------ | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `filter` | string | unset   | Only consider media players whose id or name contains this text, case-insensitively (for example `"mpv"`, `"spot"`, `"fir"`). Unset means every player the backend reports is a candidate. Whitespace-only values count as unset. |

## `[sync]`

| Key                | Type    | Default | Range          | Description                                                                                                                         |
| ------------------ | ------- | ------- | -------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `global_offset_ms` | integer | `-300`  | -30000..=30000 | Shifts all lyric timing. Positive values delay lyrics. Applied on top of the per-track offset stored in `offsets.json` (see below). |
| `lookahead_ms`     | integer | `0`     | 0..=5000       | Renders line transitions this many milliseconds early, so the highlight arrives as you start singing rather than after.             |

## `[providers]`

| Key          | Type             | Default                                                       | Description                                                                                                                                                                                                                                                                                                                                                                                                        |
| ------------ | ---------------- | ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `order`      | array of strings | `["local", "betterlyrics", "unison", "lrclib", "lyricsplus"]` | Tiebreak order when two network providers return the same lyric granularity. An empty array or any unknown name warns and reverts the whole list to the default order. Duplicates are dropped silently. Valid names: `local`, `betterlyrics`, `unison`, `lrclib`, `lyricsplus`, plus `ytmusic` when built with the default feature set and `netease`, `qqmusic`, `kugou` when built with `experimental-providers`. |
| `network`    | boolean          | `true`                                                        | Gates every network provider. `false` leaves only the local provider. The `--offline` flag forces this off for one run.                                                                                                                                                                                                                                                                                            |
| `local_dirs` | array of paths   | your music directory                                          | Directories the local provider scans recursively (depth 16) for lyric files. An empty list falls back to the default (the XDG audio directory, else `~/Music`). Setting the key replaces the default list; it does not append. `~` expands to your home directory.                                                                                                                                                 |

Note: the printed default template lists `ytmusic` in `order`, but the runtime
default omits it because the provider is behind a compile-time feature. With
the feature on, both behave the same. Built without default features, the
template's `ytmusic` entry warns as an unknown name and the order falls back
to the default, so the effective order is still the same, just with a warning.

## How resolution actually runs

The local provider is checked first, on its own: a local hit ends the lookup
before any network request. A cached answer or a provider you pinned in the
context menu also short-circuits. Otherwise the remaining network providers
race in parallel, the most precise tier wins (syllable over word over line
over plain), and `order` breaks ties within the same tier.

## `[media]`

| Key        | Type             | Default | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ---------- | ---------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `backends` | array of strings | `[]`    | Backend allowlist. Empty means the platform native backend plus any enabled network backend below. Valid names (matched case-insensitively): `mpris`, `mpd`, `webnowplaying`, and, parsed everywhere but only usable on their platforms, `smtc` (Windows 10 1809 or newer) and `applescript` (macOS). The literal name `unsupported` is rejected with a warning. Unknown names and platform-unsupported names each warn and are skipped. When the list is non-empty, Lyrachi also cross-checks it against the `enabled` flags: listing `mpd` or `webnowplaying` without enabling it, or enabling one without listing it, each warns. Duplicates are dropped silently. |

### `[media.mpd]`

Talks to a Music Player Daemon over TCP. Disabled by default.

| Key          | Type    | Default       | Range       | Description                                                          |
| ------------ | ------- | ------------- | ----------- | -------------------------------------------------------------------- |
| `enabled`    | boolean | `false`       |             | Enables the MPD backend.                                             |
| `host`       | string  | `"127.0.0.1"` |             | MPD host. Empty or whitespace-only values warn and keep the default. |
| `port`       | integer | `6600`        | 1..=65535   | MPD port.                                                            |
| `timeout_ms` | integer | `2000`        | 100..=30000 | Connection and command timeout.                                      |

### `[media.webnowplaying]`

Runs a local WebSocket server that the WebNowPlaying browser extension
connects to. Disabled by default.

| Key               | Type             | Default       | Range     | Description                                                                                                                                                                                                      |
| ----------------- | ---------------- | ------------- | --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `enabled`         | boolean          | `false`       |           | Enables the WebSocket bridge.                                                                                                                                                                                    |
| `bind_address`    | string           | `"127.0.0.1"` |           | Bind address. Must parse as an IP address and must be loopback; anything else warns and keeps the default.                                                                                                       |
| `port`            | integer          | `8974`        | 1..=65535 | Listen port.                                                                                                                                                                                                     |
| `allowed_origins` | array of strings | `[]`          |           | Exact extension origins allowed to connect, each starting with `chrome-extension://` or `moz-extension://` followed by the extension ID. Wildcards, slashes, spaces, and http(s) origins are rejected per entry. |
| `token`           | string           | unset         |           | Optional shared secret the extension must present. Whitespace-only values count as unset.                                                                                                                        |

## `[cache]`

| Key          | Type    | Default | Range    | Description                                                                                                          |
| ------------ | ------- | ------- | -------- | -------------------------------------------------------------------------------------------------------------------- |
| `l1_entries` | integer | `64`    | 1..=1024 | Size of the in-memory resolution cache. This is the memory cache only; the on-disk SQLite cache is not configurable. |

## `[ui]`

| Key              | Type        | Default             | Description                                                                                                                                                                                                    |
| ---------------- | ----------- | ------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `fullscreen`     | boolean     | `false`             | Start with the lyrics viewport filling the terminal.                                                                                                                                                           |
| `show_header`    | boolean     | `true`              | Show the track title and artist header.                                                                                                                                                                        |
| `status_bar`     | string enum | `"bottom"`          | `"bottom"`, `"top"`, `"off"`, or `"auto"`. An invalid value discards the whole file (enum parsing).                                                                                                            |
| `display_mode`   | string      | `"normal"`          | One of `"normal"`, `"block"`, `"kinetic"`, `"cylinder"`, `"canvas3d"`. Unknown values warn and keep the default.                                                                                               |
| `word_highlight` | boolean     | `true`              | Word-level karaoke highlighting when the lyric source carries word timing.                                                                                                                                     |
| `fps_active`     | integer     | `60`                | 15..=120. Frame rate while wipe, effect, or countdown animations are running; idle rendering wakes at most about once per second.                                                                              |
| `style`          | string      | `"builtin:default"` | Active style profile: a builtin preset (`builtin:default`, `builtin:dark`, `builtin:aurora`, `builtin:minimal`) or the name of a `.toml` file in the styles directory. Empty values warn and keep the default. |

### `[ui.cylinder]`

Tuning for the cylinder display mode.

| Key             | Type    | Default      | Range    | Description                                                                                                                            |
| --------------- | ------- | ------------ | -------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `annotations`   | string  | `"auto"`     |          | Whether translation, romanization, and background rows ride the cylinder: `"auto"`, `"show"`, or `"hide"`.                             |
| `speed`         | string  | `"constant"` |          | `"constant"` moves every line at one speed; `"line_length"` lets longer lines move faster so each line crosses in about the same time. |
| `speed_pct`     | integer | `100`        | 50..=300 | Speed multiplier on top of the lane speed. Adjustable at runtime with `-` / `=` and reset with `0`.                                    |
| `entry_lead_ms` | integer | `1000`       | 0..=5000 | How early a line starts entering from the right edge before its first word is sung.                                                    |

### `[ui.canvas3d]`

Tuning for the canvas3d display mode.

| Key           | Type    | Default | Range                                                                                                                                                                  | Description                                                                                           |
| ------------- | ------- | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `flip`        | boolean | `false` | Rotate each line in around its center during the last 400 ms before it is sung. When false, lines appear at the horizon and scroll toward the viewer without rotation. |
| `speed_pct`   | integer | `100`   | 50..=300                                                                                                                                                               | Crawl speed multiplier. Adjustable at runtime with `-` / `=` and reset with `0`.                      |
| `tilt_deg`    | integer | `60`    | 35..=60                                                                                                                                                                | Crawl plane tilt. Lower values make horizon text larger and more readable. Adjustable with `{` / `}`. |
| `spawn_y_pct` | integer | `82`    | 50..=95                                                                                                                                                                | Screen height, percent from the top, where new lines appear. Adjustable with `,` / `.`.               |
| `zoom_pct`    | integer | `100`   | 50..=200                                                                                                                                                               | Text scale. Zoom out to fit longer lines. Adjustable with `<` / `>`.                                  |

## `[terminal]`

| Key               | Type        | Default  | Description                                                                                                                                                                                                           |
| ----------------- | ----------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ambiguous_width` | integer     | `1`      | Column width for East Asian Ambiguous characters. Only `1` or `2` is accepted; anything else clamps to `1` with a warning. Raise to `2` if ambiguous characters look squeezed in your terminal.                       |
| `unicode`         | string enum | `"full"` | `"full"` or `"ascii"`. `"ascii"` replaces decorative glyphs (the `·` separator, the `✗` marker) with ASCII stand-ins. An invalid value discards the whole file.                                                       |
| `color_mode`      | string enum | `"auto"` | `"auto"`, `"truecolor"`, `"256"`, or `"16"`. `"auto"` detects capability from `NO_COLOR`, `COLORTERM`, and `TERM`. An invalid value discards the whole file.                                                          |
| `bg_blend_hint`   | string      | unset    | A fallback background color (`#RRGGBB`, `#RGB`, or a named ANSI color) used when blending styles against a terminal background Lyrachi cannot read. `reset` is rejected. Invalid values warn and the key stays unset. |

## Runtime persistence

Toggles you change while the TUI runs are written back into your
`config.toml`: `ui.fullscreen`, `ui.show_header`, `ui.status_bar`,
`ui.display_mode`, `ui.word_highlight`, `ui.cylinder.speed_pct`,
`ui.canvas3d.speed_pct`, `ui.canvas3d.tilt_deg`, `ui.canvas3d.spawn_y_pct`,
`ui.canvas3d.zoom_pct`, and `ui.style`. Other cylinder and canvas3d keys
(`annotations`, `speed`, `entry_lead_ms`, `flip`) are not persisted. The write
goes through `toml_edit`, so your comments and key order survive, and it lands
atomically (temp file, fsync, rename).

## Per-track offsets: `offsets.json`

Per-track offsets live in `$XDG_DATA_HOME/lyrachi/offsets.json` (usually
`~/.local/share/lyrachi/offsets.json`), not in the config file and not in the
SQLite cache. The effective offset for a track is `global_offset_ms` plus the
per-track value. Per-track values use the same ±30000 ms clamp, a value of `0`
deletes the entry, and writes are debounced by 500 ms. Concurrent writers are
serialized through a sibling `offsets.json.lock` file.

## Styles

Style profiles are TOML files in the styles directory (the `styles/` folder
next to your config file). A profile can inherit from another with
`extends = "name"`, including builtin presets (`extends = "builtin:dark"`).
Constraints:

- Files are capped at 64 KiB.
- `extends` chains are capped at depth 8; cycles are rejected.
- Resolved paths may not escape the styles directory, symlinks included.
- A style name may be given with or without the `.toml` suffix; the loader
  appends it when needed.
- An invalid hot reload keeps the last valid profile and shows a banner
  instead of applying a partial parse.

## Command line

Flags (override the config file for one run):

| Flag                  | Effect                                                                                                               |
| --------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `--config <path>`     | Use this config file; styles are read from its sibling `styles/`.                                                    |
| `--player <name>`     | Same as `[player] filter`.                                                                                           |
| `--local-dir <dir>`   | Same as `[providers] local_dirs`; repeatable, replaces both the default and any `local_dirs` set in the config file. |
| `--offset-ms <ms>`    | Same as `[sync] global_offset_ms`, same clamp.                                                                       |
| `--lookahead-ms <ms>` | Same as `[sync] lookahead_ms`, same clamp.                                                                           |
| `--offline`           | Forces `providers.network` off.                                                                                      |
| `--no-cache`          | Disables the SQLite cache for this run.                                                                              |
| `--log-file <path>`   | Write tracing output to a file.                                                                                      |
| `--dump-events`       | Print media events to stdout, prefixed with a monotonic timestamp in seconds, without starting the TUI.              |

All flags are global: they work after a subcommand too, for example
`lyrachi status --offline`.

Subcommands:

| Command                                               | Effect                                                                                                                |
| ----------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `lyrachi paths`                                       | Print the resolved paths: `config`, `styles`, `offsets`, `cache`, `log`, and `music` (the default music directories). |
| `lyrachi status [--json]`                             | Print current player, playback, lyric source, and provider circuit states without starting the TUI.                   |
| `lyrachi pipe [--format plain\|json] [--max-width N]` | Print the current line for use in status bars and pipes.                                                              |
| `lyrachi config check`                                | Validate the config file and active style; print warnings.                                                            |
| `lyrachi config print-default`                        | Print the commented default `config.toml`.                                                                            |
| `lyrachi config schema [--kind config\|style]`        | Print a JSON Schema (draft 2020-12) for the config or for a style profile.                                            |
| `lyrachi debug smtc`                                  | Dump every Windows SMTC session (AUMID, status, position, timeline age) for debugging.                                |

Setting `LYRACHI_HOME` redirects the whole base directory into one tree:
`config/config.toml`, `data/offsets.json`, `cache/lyrics_v1.db`,
`state/lyrachi.log`, and the `Music` folder all live under it. `--config`
still overrides the config file, and the styles directory stays a sibling of
whatever config file is in effect. An empty value is ignored. This is also
how portable installs work.

## Quick reference: clamps and whole-file discards

Clamped with a warning (value applied at the nearest limit):

| Key                                          | Range                           |
| -------------------------------------------- | ------------------------------- |
| `sync.global_offset_ms`                      | -30000..=30000                  |
| `sync.lookahead_ms`                          | 0..=5000                        |
| `cache.l1_entries`                           | 1..=1024                        |
| `ui.fps_active`                              | 15..=120                        |
| `ui.cylinder.speed_pct`                      | 50..=300                        |
| `ui.cylinder.entry_lead_ms`                  | 0..=5000                        |
| `ui.canvas3d.speed_pct`                      | 50..=300                        |
| `ui.canvas3d.tilt_deg`                       | 35..=60                         |
| `ui.canvas3d.spawn_y_pct`                    | 50..=95                         |
| `ui.canvas3d.zoom_pct`                       | 50..=200                        |
| `media.mpd.port`, `media.webnowplaying.port` | 1..=65535                       |
| `media.mpd.timeout_ms`                       | 100..=30000                     |
| `terminal.ambiguous_width`                   | 1..=2 (anything else becomes 1) |

Whole-file discard triggers:

- Any unknown key in any table (including unknown root-level keys)
- A malformed value for any typed key
- An invalid enum string for `ui.status_bar`, `terminal.unicode`, or
  `terminal.color_mode`
