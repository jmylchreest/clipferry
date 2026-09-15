# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- W→X claims capture the payload before taking the X11 selection, so
  they no longer depend on the Wayland source outliving the claim.
  Bytes still move only on paste for X→W, and for the W→X copies the
  backstop never claims — which is most of them — but a claim we do
  make now costs one transfer. `--eager-max-size` bounds the capture;
  over-cap types are dropped and logged at WARN, since here "degrade to
  lazy" means the type is lost. Pastes arriving mid-capture are held
  and replayed rather than refused (`event=park` / `event=unpark`).

### Fixed

- Screenshots (and any other Wayland copy) no longer need to be made
  twice before an X11 or Xwayland app can paste them. Two defects, both
  triggered by the same 0.3 ms echo:
  - Taking the X11 selection makes xwayland-satellite republish that
    claim onto the Wayland clipboard, which cancels the source being
    proxied — `wl-copy` exits and the content is gone before the first
    paste arrives (`event=paste reason=empty-source`). Lazy proxying
    cannot survive that, so the W→X path now captures first (above).
  - That republished claim was not recognised as our own: satellite
    re-exports our X11 `TARGETS` list minus `TARGETS` itself, so the
    mirror comes back carrying `TIMESTAMP` — a *superset* of what we
    advertised, which failed the subset test. Content types are now
    compared with X11 protocol atoms filtered out of both sides, and
    any such atom fingerprints the offer as a bridge mirror outright
    (`event=coexist action=observe-own-claim`). The mirror fingerprint
    likewise no longer demands both `TARGETS` and `TIMESTAMP`, which
    satellite's mirrors never satisfy.
- Copies made while an X11 app holds the display no longer need to be
  made twice before they can be pasted. When the backstop filled the
  X11 gap, the Xwayland WM mirrored that proxy claim straight back as a
  Wayland offer (~0.3 ms later); clipferry read it as a fresh copy and
  destroyed the offer it was proxying, cancelling the real source — so
  the whole chain, including a paste already streaming, dead-ended on
  nothing. Our own claim is now recognised by identity and observed
  rather than bridged (`event=coexist action=observe-own-claim`).
- A Wayland source that goes away mid-transfer is refused instead of
  being served as a successful empty payload: requestors were caching
  that emptiness as the clipboard contents rather than retrying or
  falling back to another target (`event=paste reason=empty-source`).

## [0.0.2] - 2026-07-06

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
