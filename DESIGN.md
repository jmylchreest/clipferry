# clipferry — Design Document

**A robust, lightweight X11 ⇄ Wayland clipboard bridge in Rust.**

Status: design draft, 2026-07-04. Intended audience: the implementing agent/developer. This document is self-contained — no prior conversation context is required.

---

## 1. Problem statement

Compositors that outsource X11 support to **xwayland-satellite** (niri being the flagship case) have incomplete clipboard integration between the X11 and Wayland worlds. As of xwayland-satellite 0.8.x/0.9.x, copying **out of** an X11 app (games, legacy tools, WeChat/QQ under Wine, etc.) does not reliably reach the Wayland clipboard, and edge cases exist in the other direction. Compositors with integrated Xwayland (Hyprland, GNOME, KDE) bridge selections natively, so this problem is specific to satellite-style setups.

The current community stopgap is **comalot-clipsync** (github.com/123hi123/clipsync): ~480 lines of bash that polls via `clipnotify`/`wl-paste --watch`, eagerly copies clipboard bytes through `xclip`/`wl-copy` pipes, and suppresses sync loops by comparing `xxh128` hashes. It works, but:

- **Eager copying**: every clipboard change pipes the full payload (screenshots can be tens of MB) through multiple processes, twice, even if nothing is ever pasted.
- **Hash-based loop prevention** is racy — it needs `sleep 0.1` calls to dodge self-triggered events, and identical-content copies are indistinguishable from echoes.
- **Process churn**: `wl-paste --watch` forks a new bash per clipboard event; the x2w side runs a `clipnotify` loop spawning `xclip` repeatedly.
- **Per-MIME-type duplicated logic** in shell; fragile special cases (WeChat `application/x-qt-image`, QQ rich text).
- Logs only in Chinese; content *types* and hashes logged (never content — to its credit).

clipferry replaces this with a single, event-driven Rust daemon that owns both selections directly at the protocol level.

## 2. Prior art (what to learn, what to avoid)

| Tool | Language | Approach | Takeaway |
|---|---|---|---|
| comalot-clipsync | bash | poll + hash-compare + eager pipe copy | The MIME quirk list (WeChat/QQ, `x-special/gnome-copied-files` ⇄ `text/uri-list`) is hard-won knowledge — port it. Avoid everything else architecturally. |
| dnut/clipboard-sync | Rust | generic multi-clipboard sync | Closest existing tool; unmaintained since 2023 (v0.2.0), 25 open issues. Broader scope (VNC, multi-seat) than we want. Don't build on it; do skim its issue tracker for edge-case reports. |
| wl-clip-persist | Rust | wlr-data-control, keeps selection alive after source app exits | Good reference for data-control usage in Rust and for coexistence concerns (§10). Not a bridge. |
| cliphist / clipse / clipman | Go/Rust | clipboard **history** managers via data-control | Not bridges, but they will run *alongside* clipferry — coexistence is a hard requirement (§10). |
| xwayland-satellite upstream | Rust | Draft PR #431 "selection: bridge via ext-data-control-v1"; issue #433 proposes using Xwayland's builtin bridge; a full rewrite is in progress (issue #373) | **This problem may eventually be fixed upstream.** clipferry should be honest about being a focused stopgap: small enough to be finished, useful for as long as satellite setups have gaps, and useful beyond niri (any compositor + rootless Xwayland without selection bridging). Watch PR #431; don't replicate its mistakes — read its review comments. |
| Xwayland builtin (`-enable-ei` era selections) | C | Xwayland ≥ 24.1 can bridge selections itself when the compositor implements the right primaries | If satellite adopts this, clipferry's niri use case shrinks. Fine — see project positioning above. |

## 3. Goals

1. **Bidirectional sync** of the CLIPBOARD selection between X11 (via `$DISPLAY`, i.e. xwayland-satellite) and Wayland (via `$WAYLAND_DISPLAY`).
2. **Lazy, zero-copy-until-paste**: never transfer clipboard payload bytes unless a client actually requests them.
3. **Deterministic loop prevention by ownership**, not content hashing.
4. **All MIME types**, not just text — images, `text/uri-list`, HTML, arbitrary types — with a small translation table for X11-isms.
5. **Single static-ish binary**, one process, one thread (event loop over two fds), target < 2 MiB stripped, < 5 MiB RSS.
6. **Never log clipboard content.** Metadata only (MIME lists, sizes, direction). English log messages.
7. **Resilient**: survives compositor restart of xwayland-satellite, `$DISPLAY` appearing late, Wayland socket loss (exit clean, let systemd restart).
8. Ship with a systemd user unit; package for AUR + crates.io.

### Non-goals

- Clipboard **history** (cliphist's job).
- Network/multi-machine sync (deskflow's job).
- PRIMARY selection sync by default (middle-click paste semantics differ; ship behind an opt-in flag `--primary`).
- Windows/macOS. Linux only.
- Config file. None, ever — CLI flags with sensible defaults (plus standard `$WAYLAND_DISPLAY`/`$DISPLAY` env). Every flag must be safe to omit; a bare `clipferry` invocation is the recommended setup. If a config file ever feels necessary, the tool has grown too much.

## 4. Architecture

```
                ┌────────────────────────────────────────────┐
                │                clipferry (1 process)       │
                │                                            │
 X11 socket ────┤  x11 backend        core         wayland   ├──── Wayland socket
 ($DISPLAY)     │  (x11rb)         ┌────────┐      backend   │  ($WAYLAND_DISPLAY)
                │  XFIXES notify ─▶│ Broker │◀─ data-control │
                │  selection owner │ state  │   ext-data-    │
                │  INCR handling   └────────┘   control-v1   │
                │                                            │
                │        single epoll/calloop event loop     │
                └────────────────────────────────────────────┘
```

### 4.1 Core model: the Broker

One state machine with a single invariant: **at any time, each side's selection is owned either by a real application or by clipferry acting as a proxy for the other side.**

States per selection (CLIPBOARD, and optionally PRIMARY):

- `Idle` — neither side has a selection (startup, or selection cleared).
- `X11App { targets }` — an X11 app owns the X11 selection; clipferry owns the **Wayland** selection, offering translated MIME types, proxying reads on demand.
- `WaylandApp { mime_types }` — mirror image.

Transitions are driven by exactly two event kinds:

1. **X11 `XFixesSelectionNotify`** → if the new owner is *not* clipferry's own window: fetch `TARGETS`, translate, claim the Wayland selection with a new data source offering those types. If the new owner *is* clipferry: ignore (this is our own proxy claim echoing back — this is the loop prevention, see §4.3).
2. **Wayland `data_control_device::selection(offer)`** → if the offer is *not* from clipferry's own data source: read the offer's MIME list, translate, claim the X11 selection. If it is our own: ignore.

**Startup: fill only the missing side.** On connect, query both selections (X11: `GetSelectionOwner` on CLIPBOARD; Wayland: data-control delivers the current selection — or null — on bind). If exactly one side is owned, proxy it to the empty side as if the ownership event had just fired. If **both** sides are owned, touch nothing — clobbering either would destroy real user data; the sides converge at the next copy. Neither owned → `Idle`. This rule is also what makes the crash-only restart policy (§4.4) clean: a restarted clipferry rebuilds truth from the live selections without disturbing them.

### 4.2 Lazy proxying (the key improvement over bash)

When clipferry owns the Wayland selection as a proxy and a Wayland client pastes:

- We receive `data_source::send(mime_type, fd)`.
- We initiate an X11 `ConvertSelection` for the corresponding target, stream the reply (including INCR chunks) into the fd, close it.
- No caching. Payload bytes only ever move when a paste actually happens, and they stream through a fixed-size buffer (64 KiB), never fully materialized in memory.

Mirror image for X11 paste (`SelectionRequest` → `wl_data_offer::receive`-equivalent on the data-control offer → stream, using INCR if the payload exceeds the max-request size).

**Consequence:** copying a 50 MB screenshot costs clipferry a MIME-list exchange (~bytes). The bash tool copies it twice through 6 processes immediately.

**Timeout:** `--transfer-timeout SECS`, default `0` = no timeout. When set, it is an **idle** timeout — it fires only when a transfer makes no progress for N seconds, never on total duration, so a slow-but-alive INCR transfer of a large payload is never killed mid-flight. **"Infinite" governs progress, not zero-progress silence**: independent of this flag, the first byte of any transfer and every protocol handshake is bounded at 2 s — a local source that produces nothing at all is dead or wedged, and an X11 conversion left unanswered hangs synchronous requestors (Wine's clipboard thread re-imports on every owner change and blocks apps in `GetClipboardData`). A prompt refusal is always safe; silence never is. On expiry: close the fd / reply with an empty property, log at WARN. Infinite-by-default is safe because transfers die with their consumer: when the pasting client closes its end, our write fails (EPIPE) and the transfer thread exits. The residual hang — an X11 owner that never answers `ConvertSelection` — leaks one waiting thread, bounded by concurrent pastes to that dead owner; users who care set a timeout.

### 4.2.1 Sync mode is configurable: `--sync-mode lazy|eager` (default: `lazy`)

Lazy is not always the right trade-off, so the transfer strategy is a config, not a constant:

- **`lazy`** (default): as described above — bytes move only on paste. Note that a clipboard history manager reading the offer *is* a paste from clipferry's perspective: the read routes through the proxy and triggers a real fetch from the source app. Lazy therefore still feeds cliphist et al. — but every reader hits the source app again, and the data **dies with the source**: X11 selections vanish when their owning app exits, so lazy content is only available while the source app lives.
- **`eager`**: on every claim, clipferry immediately fetches the payload for each offered MIME type (bounded by `--eager-max-size`, default 10 MiB per type, `0`/`unlimited` = no cap; over-cap types degrade to lazy for that type, logged at DEBUG) and serves all subsequent reads from its in-memory snapshot. This costs one transfer per copy even if never pasted, but: (a) content survives source-app exit — the classic "copied from an app, closed it, paste is empty" problem; (b) history managers snapshot exactly once from clipferry's buffer instead of re-reading the source app; (c) repeated pastes don't re-disturb fragile X11 sources (some Wine apps misbehave on repeated ConvertSelection).

Eager mode's snapshot is held in memory only, replaced on the next claim, and never written to disk (§8 still holds). A reasonable middle ground for most users running a history manager on the Wayland side: `--sync-mode eager` with the default size cap — small/text payloads get durability, giant images stay lazy. Users who want everything durable regardless of size run `--eager-max-size unlimited` and accept transient RSS spikes up to the payload size (one snapshot at a time; replaced on next claim). The size value parses human suffixes (`10M`, `512K`, `1G`) plus the literals `0` and `unlimited`; note the source side cannot always be trusted to declare size up front (Wayland offers don't carry a size), so the cap is enforced while streaming — abort the snapshot and degrade that type to lazy the moment the running total crosses the cap.

The broker treats this as a per-claim strategy decision, so the state machine is identical in both modes; only the "where do reads come from" arm differs. Implement lazy first (M1–M3), add eager in M4 — the snapshot path reuses the same transfer code with a memory sink instead of a paste fd.

### 4.3 Loop prevention by identity

Because clipferry is a *single process owning both proxy ends*, self-triggered events are identifiable by construction:

- X11 side: `XFixesSelectionNotify.owner == our_window_id` → ours, skip.
- Wayland side: data-control emits our own source back; track the currently-claimed source object identity → ours, skip.

No hashing, no sleeps, no races on identical content. An epoch counter (u64, incremented per legitimate claim) guards against late-arriving events from a superseded state.

### 4.4 One event loop

Use **calloop** (the Wayland ecosystem's event loop, already a dependency of wayland-rs users) with the X11 connection's raw fd registered as a source. Single thread. Blocking transfers (paste streaming) must not block the loop: do transfers on a small on-demand thread (`std::thread::spawn` per active transfer is acceptable — paste frequency is human-scale) or with non-blocking fd writes driven by the loop. **Decision: spawn-per-transfer**, it is simpler, correct, and the count is bounded by concurrent pastes (≈1).

**Panic policy: crash-only.** `panic = "abort"` in the release profile — and that is the robustness design, not a size optimization (though it also drops unwind tables): a panic is by definition an invariant violation, after which the broker state cannot be trusted, and unwinding past it is how a clipboard bridge *silently* stops syncing — the worst failure mode. Abort → systemd restarts us in ~1 s → the startup fill-the-missing-side rule (§4.1) rebuilds truth from the live selections. All *expected* failures (protocol errors, EPIPE, timeouts, malformed offers) are `Result`s and must never panic; `unwrap` is lint-banned in production paths. Two consequences, both handled: (1) abort skips `Drop`, so `Zeroizing` buffers are not zeroed on the way down — closed by the kernel, which zeroes pages before reuse by another process, and by `PR_SET_DUMPABLE=0` (§8.1), so the dying image is never written anywhere; (2) a deterministic content-triggered panic would crash-loop, so the unit uses exponential restart backoff (`RestartSec=1`, `RestartSteps=5`, `RestartMaxDelaySec=30`, `StartLimitIntervalSec=0` — never give up permanently; a dead bridge is worse than a slow retry, and the journal makes a loop visible). Dev/test profiles keep default unwinding, so `cargo test` is unaffected.

## 5. Wayland backend

- **Protocol: `ext-data-control-v1`** (wayland-protocols ≥ 1.39, supported by niri) with **fallback to `zwlr_data_control_manager_v1`** (older compositors, Sway ≤ some versions, Hyprland). Bind whichever the compositor advertises; prefer ext.
- **Crates**: `wayland-client` + `wayland-protocols` + `wayland-protocols-wlr` (all from the smithay wayland-rs family). Do **not** use `wl-clipboard-rs` — it's a CLI-oriented convenience layer that connects per-operation; we need one long-lived connection with our own state machine. It *is* a good code reference for data-control quirks.
- No surface, no rendering, no seat input — data-control is designed exactly for focus-less clipboard managers. GNOME doesn't implement it, which is fine: GNOME isn't a target (it has integrated Xwayland).
- Handle `finished`/protocol errors by exiting nonzero; systemd restarts us.

## 6. X11 backend

- **Crate: `x11rb`** (pure Rust, no Xlib link). Extensions: **XFIXES** (mandatory — `SelectSelectionInput` for `SetSelectionOwnerNotify`), core selection machinery.
- One invisible 1×1 unmapped window as selection owner / property target.
- **Serve `TARGETS`, `TIMESTAMP`** (and refuse `MULTIPLE` initially; add if a real app needs it — log requests for it at INFO so we find out).
- **INCR support is mandatory in both directions** (large images will exceed max-request-size). This is the fiddliest part of the whole project; budget test time accordingly.
- When reading from an X11 app: `ConvertSelection` → wait for `SelectionNotify` → read property → if type is `INCR`, loop on `PropertyNotify` deletes.
- xwayland-satellite starts Xwayland lazily / may restart. On startup, if `$DISPLAY` connect fails: retry with backoff (500 ms → cap 5 s), log once at INFO, not per-attempt. If the X connection drops at runtime: exit(0-with-restart semantics? No —) exit nonzero, `Restart=always` handles it. Keep it simple; no in-process reconnect state surgery.

## 7. MIME / target translation table

Bidirectional, small, data-driven (a `const` table + fall-through rule):

| X11 target | Wayland MIME | Notes |
|---|---|---|
| `UTF8_STRING`, `text/plain;charset=utf-8` | `text/plain;charset=utf-8` | Also advertise legacy `STRING`/`TEXT` on the X11 side when proxying text; convert to UTF-8. |
| `image/png`, `image/jpeg`, `image/*` | identical | Pass through byte-identical. |
| `text/html` | `text/html` | Pass through. |
| `text/uri-list` | `text/uri-list` | Pass through. |
| `x-special/gnome-copied-files` | `text/uri-list` | X→W: strip the leading `copy\n` line. W→X: additionally offer `x-special/gnome-copied-files` synthesized as `copy\n` + URIs. Port of the bash tool's file-manager interop. |
| `application/x-qt-image` (alongside `text/uri-list`) | `text/uri-list` | WeChat/Wine quirk: prefer the uri-list, normalize `file://` prefixes. Port from bash tool. |
| `TARGETS`, `TIMESTAMP`, `MULTIPLE`, `SAVE_TARGETS`, `DELETE` | — | X11 protocol machinery; never forwarded as content types. |
| anything else | pass through verbatim | Unknown types are forwarded untranslated — clipferry is a pipe, not a censor. |

## 8. Sensitive content

- **Never log payload bytes, at any log level.** Log: direction, MIME list, byte counts, durations.
- Honor **`x-kde-passwordManagerHint`**: when the offer includes it with value `secret`, still sync (the user copied a password *because they want to paste it*, possibly into an X11 app) but forward the hint type as well, and log the event as `[sensitive]` without MIME details. Add `--skip-sensitive` for users who want such offers not bridged at all.
- No history, no disk writes, no cache files. Everything is in-memory MIME lists.

### 8.1 Memory hygiene: payload buffers are zeroed at end-of-life

Clipboard bytes pass through (lazy) or rest in (eager) clipferry's memory. The contract: **every buffer that ever held payload bytes is zeroed the moment its role ends**, so stale clipboard content never lingers in freed heap, swap, or a core dump.

**Buffer lifetimes and zero points:**

| Buffer | Lives | Zeroed when |
|---|---|---|
| Lazy streaming buffer (64 KiB, per transfer) | duration of one paste transfer | after the transfer completes, **and on every abort path** (timeout, closed fd, protocol error) |
| Eager snapshot (per MIME type) | while it is the current selection — that's its purpose; it cannot be zeroed "after sync" | when replaced by the next claim, when the selection goes `Idle` (source cleared / owner exited and we drop the proxy), and on clean shutdown |
| INCR reassembly state | duration of one chunked transfer | same as streaming buffer, including partial-transfer aborts |

**Implementation:** the `zeroize` crate, with one sharp edge designed around: `Zeroizing<Vec<u8>>` zeroes only the buffer's *final* allocation — a `Vec` that grows reallocates, and the old blocks return to the allocator **un-zeroed**, exactly the freed-heap leak this section exists to prevent. Payload therefore never lives in a growable `Vec`. Payload storage is a **chunk rope**: `Vec<Zeroizing<Box<[u8]>>>` of fixed 64 KiB chunks. Bytes are read directly into a fresh chunk, chunks are never reallocated or moved, and each zeroes on drop; the outer index Vec holds only pointers and lengths (not secret) and may realloc freely. The rope composes with everything else here: INCR data arrives pre-chunked anyway, the `--eager-max-size` cap is enforced per-chunk while streaming, and `mlock` is applied per-chunk as allocated. Where a contiguous slice is unavoidable (an X11 non-INCR `ChangeProperty` needs one buffer, ≤ max-request-size), copy into a temporary `Zeroizing` buffer for the call and let it drop.

The invariant remains **type-level**, not call-site discipline: no function in the codebase may accept or return clipboard payload as a bare `Vec<u8>`/`String`, so abort paths get zeroing for free from drop semantics. Enforce in code review; a grep for `Vec<u8>` in the broker/transfer modules should hit nothing payload-carrying.

**Side channels closed while buffers are legitimately alive:**

- `prctl(PR_SET_DUMPABLE, 0)` at startup: no core dumps, and unprivileged processes cannot ptrace-attach. (Root can always read process memory; that is out of any userspace tool's threat model.)
- Best-effort `mlock` on eager snapshots up to `RLIMIT_MEMLOCK` (keeps them out of swap); degrade silently to `madvise(MADV_DONTDUMP)` beyond the limit — relevant for `--eager-max-size unlimited`. Log the degradation once at DEBUG.

**Honest scope:** clipferry can only destroy *its own* copies. The content still exists in the source app, the destination clipboard, and any history manager the user runs — zeroing here closes clipferry as a disclosure vector, it does not shrink the clipboard's inherent exposure (§8 intro).

### 8.2 In-process sandboxing: Landlock (self-applied, unprivileged)

The systemd hardening in §9 only applies when clipferry runs under the unit. Landlock is applied by the process itself, so the guarantee travels with the binary — shell runs, non-systemd distros, other people's session managers. Use the **`landlock` crate** (rust-landlock) with `CompatLevel::BestEffort` so older kernels degrade gracefully instead of failing.

**Lock sequence (order matters — Landlock does not affect already-open fds):**

1. Parse args, resolve `$WAYLAND_DISPLAY` / `$DISPLAY`.
2. Establish both connections (including the X11 startup retry loop — Xauthority is read here).
3. Apply one ruleset and enforce:
   - **Filesystem:** handle all fs access rights, add **one rule** — read-only access to the Xauthority file (`$XAUTHORITY` / `~/.Xauthority`) → everything else denied. stderr/stdout and the two display sockets are already-open fds and keep working. The single exception exists because transfers run on per-transfer X11 connections (§4.4/§6) that re-authenticate after the lock; a read-only auth cookie is not an exfiltration vector, and no write access exists anywhere. There is no config file (§3 non-goals) and no cache.
   - **Network:** handle `bind_tcp` + `connect_tcp`, add zero rules → all TCP denied (ABI v4, kernel ≥ 6.7). Unix sockets are unaffected — exactly right for us.
4. Log the achieved ABI level once at INFO (e.g. `landlock: enforced (ABI v4, fs+tcp)` / `landlock: unavailable, relying on systemd hardening`).

**Honest scope:** Landlock cannot block UDP or raw sockets (as of ABI v6). "Provably no network" therefore remains a layered claim: Landlock kills TCP + all filesystem access from inside; `RestrictAddressFamilies=AF_UNIX` in the unit kills every other family from outside. Do not add a seccomp socket-family filter in v1 — it's the natural third layer but costs a dependency and arch-specific fiddliness; revisit if anyone demonstrates a use. Post-lock reconnect is a non-issue by design: any display disconnect exits the process, and the restarted process re-locks after reconnecting (§6).

`--no-landlock` flag for debugging (logged loudly at WARN). Integration test: a hidden `--sandbox-selftest` mode that, after locking, asserts `open("/etc/passwd")` and a TCP connect both fail, then exits 0.

## 9. CLI, logging, unit

```
clipferry [--sync-mode lazy|eager] [--eager-max-size BYTES] [--primary] [--skip-sensitive]
          [--transfer-timeout SECS] [--log-level LEVEL] [--oneshot-check] [--no-landlock]
```

- `--oneshot-check`: connect to both displays, print detected protocols/versions as a diagnostic, exit. (Support triage tool.)
- Logging: `log` + a minimal stderr logger (or `env_logger` with default filter `info`) — **not** `tracing` (dependency weight for no benefit here). journald picks up stderr via the unit. English messages; if anyone ever wants i18n, log messages are ~20 call sites behind one macro.
- systemd user unit (installed to `/usr/lib/systemd/user/clipferry.service`):

```ini
[Unit]
Description=clipferry — X11 <-> Wayland clipboard bridge
After=graphical-session.target
PartOf=graphical-session.target
StartLimitIntervalSec=0
# Only needed on satellite-style compositors; users gate per-session, e.g.:
# ConditionEnvironment=XDG_CURRENT_DESKTOP=niri  (as a drop-in, not shipped)

[Service]
Type=simple
ExecStart=/usr/bin/clipferry
Restart=always
RestartSec=1
RestartSteps=5
RestartMaxDelaySec=30
# Hardening — we need only the two sockets and stderr:
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=true
NoNewPrivileges=true
RestrictAddressFamilies=AF_UNIX

[Install]
WantedBy=graphical-session.target
```

The unit ships in the package (AUR PKGBUILD installs it; `cargo install` users get it from the repo's `contrib/` dir) and is user-scoped — activation is `systemctl --user enable --now clipferry.service`, no root, one unit per graphical session. The README's install section must show exactly that one command. For users running both niri and Hyprland sessions (where Hyprland doesn't need clipferry), document the drop-in pattern: a user-local override at `~/.config/systemd/user/clipferry.service.d/*.conf` with `ConditionEnvironment=XDG_CURRENT_DESKTOP=niri` — do not ship the condition in the packaged unit, since niri isn't the only satellite-style target.

The hardening block is a genuine differentiator over the bash tool and costs nothing: combined with the self-applied Landlock sandbox (§8.1), the daemon provably cannot write the filesystem or open network sockets — systemd blocks all non-unix address families from outside, Landlock denies all filesystem access and TCP from inside, and each layer holds when the other is absent. It makes the "is my clipboard daemon exfiltrating passwords" audit a one-liner.

## 10. Coexistence with clipboard managers

cliphist / wl-clip-persist / clipse also use data-control on the same compositor. Rules:

- Multiple data-control **readers** are fine by protocol design.
- Writer fights: when a history manager *re-sets* the selection (wl-clip-persist does this when a source app exits), clipferry sees a legitimate new Wayland selection and re-proxies to X11 — correct behavior, no special casing.
- The failure mode to test: clipferry claims Wayland selection (proxying X11) → history manager immediately re-claims to persist it → clipferry sees non-self owner, claims X11 side… which it already represents. The epoch counter + "content owner is on the X11 side already, new Wayland owner offers identical type list" — **do not** try to detect this by content. Instead: accept the re-claim as the new truth (the history manager is now the source; reads route through it). This converges and cannot loop because each claim transfers ownership to a real client on exactly one side.

## 10.1 Coexistence with other bridges: backstop mode (field-derived, 2026-07-04)

Live findings on niri + xwayland-satellite 0.8.1 (Xwayland ≥ 24.1 builtin selection bridge, activated by satellite's 0.8 clipboard overhaul):

- The builtin bridge is real but partial: it misses exactly the historic gaps (games/Wine, no-X-focus flows) while actively mirroring what it can. Its X11 claims come from the WM check window (`_NET_SUPPORTING_WM_CHECK`); its Wayland mirror offers carry X11 protocol atoms (`TARGETS`, `TIMESTAMP`) verbatim as MIME strings.
- Bridge-vs-bridge invalidates §10's convergence argument (which assumes the other claimant is a real client): naive mutual mirroring produced ownership ping-pong at machine speed, cancelled real sources mid-claim, and — because Wine's clipboard machinery is synchronous and re-imports on every ownership change — hung Proton games. An unanswered X11 conversion is the single worst output this daemon can produce (see §4.2 zero-progress bound).

**Resolution: claims land only in voids.** Selections cannot be provided without exclusive ownership (both protocols are ownership-based), but contention can be made structurally impossible:

- Watch both sides passively (XFIXES + data-control cost nothing and claim nothing).
- After a copy on one side, wait `GAP_WINDOW` (200 ms) for the other side to update by itself. If any other actor bridged it — do nothing. If it stayed silent — fill the gap (fetch targets / claim, eager snapshot recommended so chains built on top of us terminate in real bytes).
- Generation counters per side invalidate stale fills; a fill also stands down if we already provide the target side.
- Another bridge's artifacts are never proxied: X11 claims by the WM check window and Wayland offers with the protocol-atom fingerprint are tracked as changes but never bridged back (bridging a bridge loops).
- `--aggressive-claims` opts into the pre-backstop behavior — immediate claims plus a once-per-epoch re-claim when a *bridge* (never a real application) takes our claim — for bridgeless rootless-Xwayland setups where the 200 ms readiness latency is unwanted.

Consequences: at most **one** ownership change per copy reaches X11 requestors; clipferry is idle wherever the platform bridge works and active exactly where it does not.

## 11. Dependency budget

`x11rb`, `wayland-client`, `wayland-protocols`, `wayland-protocols-wlr`, `calloop`, `landlock`, `zeroize`, `log`, + a tiny args parser (`lexopt` or hand-rolled; **not** `clap` — binary size). Target: `cargo build --release` with `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true` → expect ≈ 1–1.5 MiB.

## 12. Testing strategy

- **Unit**: MIME translation table (pure functions, table-driven tests); broker state machine transitions with mocked events (design the broker as sans-IO: events in, commands out — this makes it fully unit-testable without any display server).
- **Integration** (CI-able on Linux): run a headless compositor that speaks data-control (`sway --headless` or `labwc` under `WLR_BACKENDS=headless`) + `Xwayland`/`xwayland-satellite`; drive with `wl-copy`/`wl-paste`/`xclip`; assert round-trips for: UTF-8 text (incl. multi-byte), 10 MB PNG (forces INCR), `text/uri-list` ⇄ `gnome-copied-files`, rapid alternating copies (loop test: 100 alternating copies must produce exactly 100 claims per side, zero extra), self-echo (copy identical content twice — must still count as two claims, no suppression by content).
- **Soak**: leave running with a script copying every 100 ms for an hour; RSS must be flat (in eager mode: flat at baseline + one snapshot, i.e. old snapshots are provably dropped on replacement).
- **Eager-specific**: copy from an X11 app, kill the app, paste on Wayland — must succeed in eager mode, must fail gracefully (empty paste, WARN log) in lazy mode; a >`--eager-max-size` payload must degrade to lazy for that type and still paste while the source lives.
- **Memory hygiene**: unit-verify the type invariant (payload types are `Zeroizing`, drop zeroes — testable directly by inspecting a buffer through a raw pointer after drop in a controlled test); assert `/proc/self/status` shows `CoreDumping`-safe state (dumpable=0) in the sandbox selftest.

## 13. Milestones

1. **M1 — text, one direction** (W→X): wayland data-control read + X11 selection ownership + lazy send. Prove the proxy model.
2. **M2 — bidirectional text** + loop prevention + epoch counter. The 100-alternating-copies test passes.
3. **M3 — all MIME passthrough + INCR** both directions. The 10 MB PNG test passes.
4. **M4 — translation table** (uri-list/gnome-copied-files/WeChat quirks), `--sync-mode eager` + `--eager-max-size`, `--primary`, `--skip-sensitive`.
5. **M5 — hardening, unit, packaging**: Landlock self-sandbox (§8.1) + `--sandbox-selftest`, systemd unit, AUR `clipferry` + `clipferry-git` PKGBUILDs, crates.io publish, README with the "why not just use X" table from §2.

## 14. Open questions (implementer may decide)

- calloop vs. hand-rolled `poll(2)` over two fds: calloop is idiomatic; a hand-rolled loop drops a dependency. Either acceptable.
- Whether to advertise `SAVE_TARGETS` (clipboard-manager persistence protocol) on the X11 side — probably no (out of scope, history managers' job).
- PRIMARY selection semantics under proxying (highlight-drag generates high-frequency owner changes; may need debounce ~50 ms if `--primary` is on).

## 15. Name

**clipferry** — verified available 2026-07-04: crates.io (404), AUR (0 packages), GitHub (0 repos with that name). The metaphor is exact: a ferry carries cargo between two shores on demand; it doesn't warehouse it.
