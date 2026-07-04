# clipferry

**A robust, lightweight X11 ⇄ Wayland clipboard bridge in Rust.**

[![CI](https://github.com/jmylchreest/clipferry/actions/workflows/ci.yml/badge.svg)](https://github.com/jmylchreest/clipferry/actions/workflows/ci.yml)
[![Snapshot](https://img.shields.io/github/v/release/jmylchreest/clipferry?include_prereleases&label=snapshot)](https://github.com/jmylchreest/clipferry/releases/tag/snapshot)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> **Status: functional, pre-1.0.** All five design milestones are implemented and live-tested on niri + xwayland-satellite; snapshot builds are usable today.

## Quickstart

```sh
# Arch: build + install from the bundled PKGBUILD (AUR packages land with v0.0.1)
git clone https://github.com/jmylchreest/clipferry
cd clipferry/contrib/aur/clipferry-git && makepkg -si

# any distro
cargo install --git https://github.com/jmylchreest/clipferry
sudo cp contrib/clipferry.service /usr/lib/systemd/user/   # or ~/.config/systemd/user/

# then, either way:
systemctl --user enable --now clipferry.service
clipferry --oneshot-check                  # sanity: both displays reachable?
journalctl --user -u clipferry -f          # watch it work (k=v event log)
```

No configuration, no config file — a bare `clipferry` does the right thing. Flags below tune behaviour.

## Why

Compositors that outsource X11 support to [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite) — [niri](https://github.com/YaLTeR/niri) being the flagship case — have incomplete clipboard integration between the X11 and Wayland worlds: copying out of an X11 app often never reaches the Wayland clipboard. Compositors with integrated Xwayland (Hyprland, GNOME, KDE) don't have this problem.

The community stopgap is a ~480-line bash polling loop that eagerly pipes every clipboard payload through six processes and suppresses sync loops by content hashing. clipferry replaces that with a single event-driven Rust daemon that owns both selections directly at the protocol level:

- **Lazy by default** — payload bytes move only when someone actually pastes. Copying a 50 MB screenshot costs a MIME-list exchange, not two full transfers.
- **Loop prevention by ownership identity**, not content hashing — no sleeps, no races, no false positives on identical content.
- **All MIME types** — images, HTML, `text/uri-list`, plus a small translation table for X11-isms (`x-special/gnome-copied-files`, WeChat/Wine quirks).
- **Sandboxed** — self-applied [Landlock](https://landlock.io/) (no filesystem, no TCP) plus systemd unit hardening. The "is my clipboard daemon exfiltrating passwords" audit is a one-liner.
- **Private** — never logs clipboard content; payload buffers are zeroed at end-of-life (`zeroize`); no history, no disk writes.
- **Small** — one process, one thread, target < 2 MiB stripped / < 5 MiB RSS.

## Why not …?

| Tool | Why not |
|---|---|
| comalot-clipsync | Polling, eager copying, hash-based loop suppression, process churn. Works, but architecturally the thing clipferry replaces. |
| dnut/clipboard-sync | Unmaintained since 2023; much broader scope (VNC, multi-seat). |
| cliphist / clipse / wl-clip-persist | History managers, not bridges — they run happily *alongside* clipferry. |
| Wait for xwayland-satellite to fix it | It may! clipferry is an honest, focused stopgap: small enough to be finished, useful for as long as satellite setups have gaps. |

## Install

From source (AUR packages ship with the first tagged release — PKGBUILDs live in [`contrib/aur/`](contrib/aur)):

```sh
cargo install --git https://github.com/jmylchreest/clipferry
# or grab a continuous build from the snapshot release below
mkdir -p ~/.config/systemd/user && cp contrib/clipferry.service ~/.config/systemd/user/
systemctl --user enable --now clipferry.service
```

That's it — no config file, ever. A bare `clipferry` invocation with the standard `$DISPLAY`/`$WAYLAND_DISPLAY` environment is the recommended setup; behaviour tweaks are CLI flags (`--sync-mode eager`, `--primary`, `--skip-sensitive`, …).

Continuous builds from `main` are published to the [snapshot release](https://github.com/jmylchreest/clipferry/releases/tag/snapshot).

## Flags

Every flag is safe to omit; the defaults are the recommended setup.

| Flag | Default | Effect |
|---|---|---|
| `--sync-mode lazy\|eager` | `lazy` | `lazy`: payload bytes move only when someone pastes. `eager`: snapshot each copy immediately so content survives the source app exiting (recommended alongside heavy clipboard tooling). |
| `--eager-max-size SIZE` | `10M` | Per-type eager snapshot cap (`512K`, `1G`, `0`/`unlimited`). Over-cap types degrade to lazy for that copy. |
| `--primary` | off | Also bridge the PRIMARY (middle-click) selection, with 50 ms debounce. |
| `--skip-sensitive` | off | Don't bridge offers carrying `x-kde-passwordManagerHint` (password managers). Without it they bridge, logged as `sensitive=true` with no type details. |
| `--transfer-timeout SECS` | `0` (none) | *Idle* timeout for in-flight transfers — fires on stalled progress, never on total duration. Independently, the first byte of any transfer is always bounded at 2 s: silence is never forwarded to a waiting application. |
| `--aggressive-claims` | off | Claim selections immediately instead of **backstop mode** (see [Coexistence](#coexistence)), and re-claim when another *bridge* takes our claim — never from a real application. For bridgeless rootless-Xwayland setups. |
| `--no-landlock` | off | Disable the self-applied Landlock sandbox (debugging only; logged loudly). |
| `--oneshot-check` | — | Connect to both displays, print protocols/versions, exit. Support triage. |
| `--log-level LEVEL` | `info` | `error`–`trace`. Logfmt `k=v` output; under systemd, lines carry real journald priorities (`journalctl -p warning` works). Content is never logged at any level. |

## Roadmap

- [x] **M1** — text, one direction (Wayland → X11); proves the lazy proxy model
- [x] **M2** — bidirectional text, ownership-based loop prevention, epoch counter
- [x] **M3** — all-MIME passthrough + INCR in both directions (10 MB PNG test)
- [x] **M4** — translation table, `--sync-mode eager`, `--primary`, `--skip-sensitive`
- [x] **M5** — Landlock self-sandbox, systemd unit, AUR + crates.io packaging

See [DESIGN.md](DESIGN.md) for the full architecture: broker state machine, lazy vs eager transfer strategy, MIME translation table, memory hygiene, and testing strategy.

## Coexistence

clipferry runs in **backstop mode** by default: it watches both clipboards passively and only claims a selection when, ~200 ms after a copy, the other side has not been bridged by anything else (e.g. xwayland-satellite ≥ 0.8's builtin sync, which handles common cases but misses games/Wine and no-X-focus flows). This makes fights with other bridges structurally impossible — clipferry stays idle where your compositor stack already works and fills exactly the gaps. History managers (cliphist/clipse) coexist as before. `--aggressive-claims` restores immediate claiming for bridgeless rootless-Xwayland setups.

## Development

```sh
cargo build            # workspace: crates/clipferry
cargo nextest run      # tests (or plain `cargo test`)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check       # licenses, advisories, bans, sources
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org/); `main` takes squash-merged PRs with required CI.

## License

[MIT](LICENSE)
