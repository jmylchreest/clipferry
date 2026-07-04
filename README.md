# clipferry

**A robust, lightweight X11 ⇄ Wayland clipboard bridge in Rust.**

[![CI](https://github.com/jmylchreest/clipferry/actions/workflows/ci.yml/badge.svg)](https://github.com/jmylchreest/clipferry/actions/workflows/ci.yml)
[![Snapshot](https://img.shields.io/github/v/release/jmylchreest/clipferry?include_prereleases&label=snapshot)](https://github.com/jmylchreest/clipferry/releases/tag/snapshot)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> **Status: pre-implementation.** The [design](DESIGN.md) is complete; code is landing milestone by milestone (see [Roadmap](#roadmap)). Nothing is usable yet.

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

## Install (planned)

Once released (AUR `clipferry` / `clipferry-git`, crates.io):

```sh
systemctl --user enable --now clipferry.service
```

That's it — no config file, ever. A bare `clipferry` invocation with the standard `$DISPLAY`/`$WAYLAND_DISPLAY` environment is the recommended setup; behaviour tweaks are CLI flags (`--sync-mode eager`, `--primary`, `--skip-sensitive`, …).

Continuous builds from `main` are published to the [snapshot release](https://github.com/jmylchreest/clipferry/releases/tag/snapshot).

## Roadmap

- [x] **M1** — text, one direction (Wayland → X11); proves the lazy proxy model
- [x] **M2** — bidirectional text, ownership-based loop prevention, epoch counter
- [x] **M3** — all-MIME passthrough + INCR in both directions (10 MB PNG test)
- [x] **M4** — translation table, `--sync-mode eager`, `--primary`, `--skip-sensitive`
- [ ] **M5** — Landlock self-sandbox, systemd unit, AUR + crates.io packaging

See [DESIGN.md](DESIGN.md) for the full architecture: broker state machine, lazy vs eager transfer strategy, MIME translation table, memory hygiene, and testing strategy.

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
