# Changelog

All notable changes to Ratatex are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-07-29

### Added

- In-process TeX parsing, layout, embedded fonts, and supersampled PNG
  rasterization through RaTeX, with no system TeX toolchain or subprocess.
- Asynchronous bounded rendering with memory and content-addressed disk caches.
- Kitty graphics uploads, virtual placements, and Unicode-placeholder Ratatui
  cells with tmux passthrough.
- Display-math extraction that ignores fenced and inline code.
- Streaming display-math healing for progressive rendering before delimiters
  close.
- Signed clipping that preserves Kitty tile coordinates while formulas scroll.
- Compact LaTeX source fallback for native terminal selection and copying.

[0.1.0]: https://github.com/gakonst/ratatex/releases/tag/v0.1.0
