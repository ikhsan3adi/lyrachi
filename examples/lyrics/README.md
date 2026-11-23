# Sample lyrics for Lyrachi

These files are fictional and exist to exercise every annotation kind and line
role the semantic model supports. Copy one next to an audio file (or into a
music directory listed in `providers.local_dirs`) and Lyrachi's local provider
will pick it up.

## showcase.ttml

The only format that can express every annotation kind explicitly.

| Construct | Result |
|---|---|
| `<span ttm:role="x-translation">` | Translation annotation |
| `<span ttm:role="x-roman">` | Romanization annotation |
| Two lines whose starts are within 20 ms, same script | Secondary annotation |
| `<span ttm:role="x-bg">` inside a line with main text | Background annotation |
| `ttm:role="x-bg"` on the `<p>` itself | BackgroundVocal line |
| `<ttm:agent>` declarations; lines with a non-primary agent | DuetSecondary line |
| A gap longer than 8 s between two lines | InstrumentalBreak line |
| Adjacent spans with no whitespace between them | Syllable-level timing |
| Spaced spans | Word-level timing |

## showcase.elrc

Shows what the LRC family can do through the normalizer alone.

| Construct | Result |
|---|---|
| `<mm:ss.xx>` tags inside a line, spaced | Word-level timing |
| Adjacent tags with no space (`ne<00:16.20>on`) | Syllable-level timing |
| Trailing tag after the last word | Explicit word end |
| `[00:10.00]Paper lanterns drift` + `[00:10.01]Faroles de papel` | Secondary annotation (same script) |
| `[00:12.00]Moonlight...` + `[00:12.01]月光` | Translation annotation (different script) |
| `[00:14.00]夜光` + `[00:14.01]Yakou` | Romanization annotation (CJK line + ASCII line) |
| A gap longer than 8 s between two lines | InstrumentalBreak line |

ELRC has no syntax for duet singers, background vocals, or inline annotations;
those need TTML (or a provider that marks them, like LyricsPlus or QRC singer
prefixes).

## Timing rules worth knowing

- The 20 ms window is `ANNOTATION_WINDOW_MS` in `src/core/normalize.rs`; the
  first follower within the window of a line's start is absorbed as an
  annotation, and its kind is decided by script comparison.
- The 8 s gap is `INSTRUMENTAL_GAP_MS` in the same file; the break spans from
  the previous line's end to the next line's start.
- The local provider accepts `.elrc`, `.lrc`, `.ttml`, `.krc`, `.qrc`, and
  `.yrc` sidecars, capped at 256 KiB.
