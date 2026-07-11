# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.2](https://github.com/jmylchreest/clipferry/releases/tag/v0.0.2) - 2026-07-06

### Added

- backstop mode — claims land only in voids (§10.1) ([#14](https://github.com/jmylchreest/clipferry/pull/14))
- level= as a first-class logfmt field ([#12](https://github.com/jmylchreest/clipferry/pull/12))
- logfmt-style structured logging with journald priorities ([#11](https://github.com/jmylchreest/clipferry/pull/11))
- M5 — Landlock sandbox, packaging, release enablement ([#7](https://github.com/jmylchreest/clipferry/pull/7))
- M4 — translation table, eager mode, --primary, --skip-sensitive ([#6](https://github.com/jmylchreest/clipferry/pull/6))
- M3 — all-MIME passthrough and INCR in both directions ([#5](https://github.com/jmylchreest/clipferry/pull/5))
- M2 — bidirectional text sync with loop prevention by identity ([#4](https://github.com/jmylchreest/clipferry/pull/4))
- M1 — lazy Wayland→X11 text bridging ([#3](https://github.com/jmylchreest/clipferry/pull/3))

### Fixed

- refresh stale claims on gap-fill; refuse mimes absent from the live offer ([#19](https://github.com/jmylchreest/clipferry/pull/19))
- causality pairing — a side updating while its fill is pending is the answer, not a new copy ([#15](https://github.com/jmylchreest/clipferry/pull/15))
- bound zero-progress waits at 2s — silence must never reach requestors ([#13](https://github.com/jmylchreest/clipferry/pull/13))
- abort X→W transfers when the owner window is destroyed ([#10](https://github.com/jmylchreest/clipferry/pull/10))
- synthesize text/uri-list for gnome-copied-files-only Wayland offers ([#9](https://github.com/jmylchreest/clipferry/pull/9))

### Other

- release v0.0.1 ([#1](https://github.com/jmylchreest/clipferry/pull/1))
- tighten README — merge quickstart/install, drop completed roadmap ([#17](https://github.com/jmylchreest/clipferry/pull/17))
- quickstart, flags reference with defaults, changelog catch-up ([#16](https://github.com/jmylchreest/clipferry/pull/16))
- bootstrap repository

## [0.0.1](https://github.com/jmylchreest/clipferry/releases/tag/v0.0.1) - 2026-07-04

### Added

- backstop mode — claims land only in voids (§10.1) ([#14](https://github.com/jmylchreest/clipferry/pull/14))
- level= as a first-class logfmt field ([#12](https://github.com/jmylchreest/clipferry/pull/12))
- logfmt-style structured logging with journald priorities ([#11](https://github.com/jmylchreest/clipferry/pull/11))
- M5 — Landlock sandbox, packaging, release enablement ([#7](https://github.com/jmylchreest/clipferry/pull/7))
- M4 — translation table, eager mode, --primary, --skip-sensitive ([#6](https://github.com/jmylchreest/clipferry/pull/6))
- M3 — all-MIME passthrough and INCR in both directions ([#5](https://github.com/jmylchreest/clipferry/pull/5))
- M2 — bidirectional text sync with loop prevention by identity ([#4](https://github.com/jmylchreest/clipferry/pull/4))
- M1 — lazy Wayland→X11 text bridging ([#3](https://github.com/jmylchreest/clipferry/pull/3))

### Fixed

- causality pairing — a side updating while its fill is pending is the answer, not a new copy ([#15](https://github.com/jmylchreest/clipferry/pull/15))
- bound zero-progress waits at 2s — silence must never reach requestors ([#13](https://github.com/jmylchreest/clipferry/pull/13))
- abort X→W transfers when the owner window is destroyed ([#10](https://github.com/jmylchreest/clipferry/pull/10))
- synthesize text/uri-list for gnome-copied-files-only Wayland offers ([#9](https://github.com/jmylchreest/clipferry/pull/9))

### Other

- tighten README — merge quickstart/install, drop completed roadmap ([#17](https://github.com/jmylchreest/clipferry/pull/17))
- quickstart, flags reference with defaults, changelog catch-up ([#16](https://github.com/jmylchreest/clipferry/pull/16))
- bootstrap repository
