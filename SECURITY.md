# Security policy

## Supported versions

Only the latest release receives security fixes.

| Version | Supported |
| ------- | --------- |
| 0.5.x   | yes       |
| older   | no        |

## Reporting a vulnerability

Use GitHub private vulnerability reporting:

https://github.com/ikhsan3adi/lyrachi/security/advisories/new

Please do not open a public issue for something you suspect is a security problem.

Helpful details in a report:

- the version from `lyrachi --version`, your OS, and how you installed Lyrachi
- a minimal reproduction, ideally a file or command
- log output from `LYRACHI_LOG=debug` when the problem involves a media backend or a provider

We aim to acknowledge a report within 7 days and will keep you posted while a fix is in progress. Fixes land in the next patch release when one is feasible.

## Scope

In scope:

- the LRC, ELRC, TTML, KRC, QRC, and YRC parsers and the timeline compiler
- terminal output built from untrusted lyric text or track metadata, including escape sequence injection
- the SQLite cache and the per-track offsets store
- config and style profile parsing, including path handling for the styles directory
- the provider HTTP clients and how they handle responses
- subprocess use: `osascript`, `media-control`, and the headless pipe
- the release pipeline and packaging

Out of scope:

- bugs in terminal emulators
- content that third-party lyric providers return, beyond how Lyrachi parses it
- social engineering

There is no bug bounty program.
