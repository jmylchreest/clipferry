# clipferry

**A robust, lightweight X11 ⇄ Wayland clipboard bridge in Rust.**

[![CI](https://github.com/jmylchreest/clipferry/actions/workflows/ci.yml/badge.svg)](https://github.com/jmylchreest/clipferry/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/jmylchreest/clipferry?label=release)](https://github.com/jmylchreest/clipferry/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## Quickstart

```sh
# Arch
paru -S clipferry        # or clipferry-git for main

# any distro
cargo install --git https://github.com/jmylchreest/clipferry
sudo cp contrib/clipferry.service /usr/lib/systemd/user/

# then:
systemctl --user enable --now clipferry.service
clipferry --oneshot-check    # sanity: both displays reachable?
```

No config file, ever — a bare `clipferry` does the right thing. Continuous builds from `main` are on the [snapshot release](https://github.com/jmylchreest/clipferry/releases/tag/snapshot); watch it work with `journalctl --user -u clipferry -f`.

## Why

Compositors that outsource X11 to [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite) ([niri](https://github.com/YaLTeR/niri) being the flagship case) have incomplete clipboard integration — most visibly, copying out of X11 apps (games, Wine) doesn't reach the Wayland clipboard. clipferry is a single event-driven daemon that fills those gaps at the protocol level:

- **Lazy** — payload bytes move only when someone pastes. A 50 MB screenshot copy costs a MIME-list exchange, not two transfers.
- **All MIME types** — images, HTML, `text/uri-list`, arbitrary types, plus a small translation table for X11-isms (`x-special/gnome-copied-files`, WeChat/Wine quirks) and INCR in both directions.
- **Loop prevention by ownership identity** — no content hashing, no sleeps, no races.
- **Sandboxed & private** — self-applied [Landlock](https://landlock.io/) (no filesystem, no TCP) plus systemd hardening; content is never logged; payload buffers are zeroed at end-of-life.
- **Small** — one process, ~1 MiB binary, <5 MiB RSS.

## Flags

Every flag is safe to omit; the defaults are the recommended setup.

| Flag | Default | Effect |
|---|---|---|
| `--sync-mode lazy\|eager` | `lazy` | `eager` snapshots each copy immediately so content survives the source app exiting. |
| `--eager-max-size SIZE` | `10M` | Per-type eager snapshot cap (`512K`, `1G`, `0`/`unlimited`). Over-cap types degrade to lazy. |
| `--primary` | off | Also bridge the PRIMARY (middle-click) selection, debounced. |
| `--skip-sensitive` | off | Don't bridge password-manager-hinted offers; otherwise they bridge with types suppressed from logs. |
| `--transfer-timeout SECS` | `0` (none) | *Idle* timeout for stalled transfers. The first byte is always bounded at 2 s regardless — silence is never forwarded to a waiting application. |
| `--aggressive-claims` | off | Claim immediately instead of backstopping (see below), and re-claim when another *bridge* takes our claim — never from real applications. |
| `--no-landlock` | off | Disable the sandbox (debugging only). |
| `--oneshot-check` | — | Print detected protocols/versions and exit. |
| `--log-level LEVEL` | `info` | `error`–`trace`, logfmt `k=v` output with real journald priorities. |

## Coexistence

By default clipferry runs in **backstop mode**: it watches both clipboards passively and claims a selection only when, ~200 ms after a copy, nothing else has bridged the other side. Fights with other bridges (e.g. satellite ≥ 0.8's builtin sync) are structurally impossible — clipferry stays idle where your stack works and fills exactly the gaps. Clipboard history managers (cliphist, clipse) coexist as designed.

## Alternatives

| Tool | Trade-off |
|---|---|
| comalot-clipsync | Bash polling + eager pipe copies + hash-based loop suppression. |
| dnut/clipboard-sync | Unmaintained since 2023; much broader scope. |
| xwayland-satellite builtin | Handles common cases; misses games/Wine and no-X-focus flows — clipferry backstops it. |

## Development

```sh
cargo nextest run                            # or plain `cargo test`
cargo clippy --all-targets -- -D warnings
cargo deny check
```

Architecture, invariants, and the testing strategy live in [DESIGN.md](DESIGN.md). Conventional Commits; squash-merged PRs with required CI; releases via release-plz.

## License

[MIT](LICENSE)
