# Benchmarks

All numbers from one Windows 11 machine (32 logical cores), release builds.
Reproduce with `cargo run -p orbit-bench --release` and
`scripts/measure-desktop.ps1`.

## Desktop app (whole process tree, median of 5 runs)

Measured 2026-11-25 on main `25e3dc2`, back to back on an otherwise idle machine:
`scripts/measure-desktop.ps1 -Runs 5`. Orbit opened a seeded data folder
(`cargo run -p orbit-core --example seed`); T3 Code opened its own data.
"Window up" is from process start to a titled main window; idle CPU is the
whole tree over 10 s after an 8 s settle, as a share of one core.

| App | Window up | Processes | Working set | Private | Idle CPU | Executable |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| **Orbit (GPUI Fast)** | 260 ms | 1 | 77 MB | 105 MB | 0.00 % | 43.0 MB |
| Orbit (upstream GPUI) | 232 ms | 1 | 75 MB | 95 MB | 1.56 % | 39.8 MB |
| T3 Code (Electron) | 461 ms | 7 | 1,035 MB | 904 MB | 7.66 % | 234.5 MB |

Against T3 Code, Orbit on GPUI Fast uses 13× less working set, 9× less
private memory, one process instead of seven, a fifth of the disk, and no
measurable CPU at idle; its window is up in 56 % of the time.

GPUI Fast against upstream GPUI: Fast is the one that truly idles (0.00 %
against 1.56 %: upstream keeps redrawing), at the cost of 28 ms more to the
first window and about 10 MB more private memory (its composition surfaces).
T3 Code's idle CPU varies between runs (2.34 % in an earlier session).

### With the background host

Since the engine moved into `orbitd --local` (see [HOST.md](HOST.md)),
measured 2026-11-25 with `-Runs 5` on the same machine, both on one build:

| Engine | Window up | Processes | Working set | Private | Idle CPU |
| --- | ---: | ---: | ---: | ---: | ---: |
| Background host (default) | 238 ms | 2 | 92.8 MB (host 20.3) | 121.5 MB (host 27.6) | 0.00 % |
| In-process (`--in-process`) | 245 ms | 1 | 79.1 MB | 104.1 MB | 0.00 % |

A call from the window to the host takes 30.5 µs p50 over the named pipe
(35 µs over loopback WebSockets, 0.4 µs in-process).

## Engine, protocol, storage, network (`orbit-bench`)

Median of three runs on 2026-11-25, release builds, on the same machine and
day. "Baseline" is main at `da03e0a`, before the feature packages landed;
"Regressed" is `25e3dc2`, after they landed; "Fixed" is `971ff89`, the
regression fixed while still on bitcode; "Now" is the switch to wincode.

| Benchmark | Baseline | Regressed | Fixed (bitcode) | Now (wincode) |
| --- | ---: | ---: | ---: | ---: |
| Link: one streamed token (host → client) | 0.50 M/s | 0.20 M/s | 5.4 M/s | **10.4 M/s** |
| Link: streamed tokens, 512 per frame | 36.6 M/s, 7.3 B/delta | 24.2 M/s, 7.8 B/delta | 35 M/s, 6.7 B/delta | 29 M/s, 8.0 B/delta |
| Link: `Done` reply encode+decode | 4.2 µs | 13.7 µs | 14 ns | 14 ns |
| Engine streaming to UI feed | 2.41 M deltas/s | 2.18 M deltas/s | 2.98 M deltas/s | 2.93 M deltas/s |
| UI state apply | 17 M deltas/s | 17.2 M deltas/s | 21.5 M deltas/s | 21.5 M deltas/s |
| Cold start with 1,000 threads | 8.4 ms | 9.6 ms | 9.6 ms | **5.0 ms** |
| Open a 3,000-item thread | 2.5 ms | 2.9 ms | 2.8 ms | **0.95 ms** |
| In-process command round trip | 0.40 µs p50 | 0.40 µs p50 | 0.40 µs p50 | 0.40 µs p50, 1.1 µs p99 |
| WebSocket round trip (loopback) | 74 µs p50, 161 µs p99 | 107 µs p50, 199 µs p99 | 47 µs p50, 91 µs p99 | **41 µs p50, 80 µs p99** |
| WebSocket streaming | 2.59 M deltas/s | 2.58 M deltas/s | 4.38 M deltas/s | 4.1 M deltas/s |

The switch to wincode (schema 5, protocol 9) halves cold start and makes
opening a thread three times faster: every log record decodes in tens of
nanoseconds instead of microseconds. Streamed tokens travel in runs that
name their thread and item once (`orbit_proto::link::DeltaRun`); otherwise
the ids' varints would cost 18 bytes per token. Data folders from schemas 1
to 4 are read through bitcode and rewritten once (`legacy.rs`).

What the regression was, and the fix:

- **bitcode's per-message cost grows with the type, not the value.** It keeps
  a column per field of every variant and visits all of them on each encode
  and decode. As `Event`, `Command` and `Reply` grew, a one-byte
  `Reply::Done` came to cost 13.7 µs. The link now frames its hot messages
  apart (`orbit_proto::link`): runs of streamed tokens as a small
  `WireDelta` type, and `Done` replies as five bytes. Everything else is still
  a `ServerMsg`, through codecs reused per connection; the server used to
  rebuild them for every reply.
- **Events were 392 bytes.** Every streamed token is moved as an `Event`.
  Its large payloads are now boxed (72 bytes); `Box<T>` encodes exactly like
  `T`, so logs and the wire are unchanged.
- **Two clock reads per token.** The status engine's liveness stamp read
  `Instant::now()` and the wall clock for every delta. The engine now reads
  the clock once per burst of agent events.

Two ordering bugs surfaced while fixing this, and are fixed with tests:

- The server could send a reply before the events its command caused. It now
  sends the queued events first.
- A remote client resolved replies before its consumer had applied the
  events ahead of them. With `RemoteClient::order_replies` (the app and the
  CLI turn it on), a reply resolves only after those events are applied.

## Review sheet (release build, 165 Hz display)

Reproduce: `cargo run -p orbit-core --example seed_review -- <data> <work> --big`, then start Orbit with `ORBIT_DATA=<data>` and `ORBIT_REVIEW_PERF=<log>`, open "Stream the parser" and press Ctrl+R. The log gets load times and frame-time percentiles per 300 frames.

| Measure (802 files, 49,609 changed lines, Rust highlighting on) | Result |
| --- | ---: |
| Fetch (one `git diff`) and parse, off the UI thread | 290 ms |
| Show (tree, diff state, markers) | 2.7 ms |
| Idle frame | p50 6.1 ms (vsync) |
| Wheel scrolling, 120 per 16 ms, 18 s | p50 6.1–9.8 ms, p99 ≤ 13.6 ms, 0 frames over 18 ms |
| Fling, 360 per 10 ms | p50 6.6–8.5 ms, p95 24 ms, 88 of 600 frames over 18 ms |
