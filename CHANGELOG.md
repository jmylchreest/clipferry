# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Backstop mode (default): claims land only in voids — after a copy,
  the other side gets 200 ms to be bridged by anything else before
  clipferry fills the gap; causality pairing prevents re-bridging
  another bridge's answer. Coexists with xwayland-satellite ≥ 0.8's
  builtin sync without ownership fights. `--aggressive-claims` opts
  into immediate claiming with bridge-only re-claims.
- Structured logfmt logging (`level= event= sel= mime= bytes= …`) with
  real journald priorities via validated sd-daemon prefixes; X11 owners
  identified by `WM_CLASS` where available.

- M5: self-applied Landlock sandbox (fs deny-all + read-only Xauthority,
  TCP deny; BestEffort), PR_SET_DUMPABLE=0, `--no-landlock`, hidden
  `--sandbox-selftest` (CI-tested), AUR PKGBUILDs, git tags + GitHub
  releases via release-plz.
- M4: §7 translation table (gnome-copied-files ⇄ uri-list synthesis, Qt
  image quirk), `--sync-mode eager` with `--eager-max-size` (snapshots
  survive source exit, over-cap types degrade to lazy, best-effort mlock),
  `--primary` with 50 ms debounce, `--skip-sensitive` with KDE password
  hint detection; Codecov made informational.
- M3: all MIME types bridged verbatim (§7 pass-through) with INCR in both
  directions; `--transfer-timeout` idle timeout (default: none); X→W reads
  serialized and INCR-drained for single-threaded X11 owners.
- M2: bidirectional text sync — XFIXES selection watching, Wayland data
  source proxying, identity-based loop prevention, startup X11 probe.
- M1: lazy Wayland → X11 text bridging — ext-data-control-v1 (zwlr fallback),
  X11 selection ownership with TARGETS/TIMESTAMP, per-paste transfer threads,
  zeroizing chunk-rope payload buffers, `--oneshot-check` diagnostic.
- Project scaffold: design document, CI pipeline, packaging skeleton.

### Fixed

- Zero-progress waits are bounded at 2 s everywhere (first byte +
  protocol handshakes): an unanswered X11 conversion can no longer
  hang synchronous requestors such as Wine/Proton clipboard reads.
- X→W transfers abort when the owner window is destroyed mid-INCR
  instead of pinning the transfer gate.
- `x-special/gnome-copied-files`-only Wayland offers synthesize
  `text/uri-list` for X11 (cut/copy header handled both directions).
