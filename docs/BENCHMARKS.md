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

## Engine, protocol, storage, network (`orbit-bench`)

Median of three runs on 2026-11-25 (main `25e3dc2`), with the first
recorded run (2026-11-24) for comparison.

| Benchmark | Now | First run |
| --- | ---: | ---: |
| Delta codec, 512-delta wire frames | 24.2 M/s, 7.8 B/delta | 39.8 M/s, 7.3 B/delta |
| Engine streaming to UI feed | 2.18 M deltas/s | ~3.0 M deltas/s |
| UI state apply | 17.2 M deltas/s | ~17 M deltas/s |
| Cold start with 1,000 threads | 9.6 ms | 7 ms |
| Open a 3,000-item thread | 2.9 ms | 2.5 ms |
| In-process command round trip | 0.40 µs p50, 1.1 µs p99 | 0.4 µs p50 |
| WebSocket round trip (loopback) | 107 µs p50, 199 µs p99 | 63 µs p50, 141 µs p99 |
| WebSocket streaming | 2.58 M deltas/s | 3.6 M deltas/s |

**Open regression.** Codec throughput, engine streaming, WebSocket round
trip and streaming all got 25–70 % slower while the feature packages
landed. Likely causes, not yet confirmed: the `Event`/`Command` enums grew
(more variants and larger payloads, so a bigger tag and 0.5 more bytes per
delta), per-delta liveness bookkeeping in the status engine, and the CLI's
own WebSocket framing (`orbit-net/src/ws_lite.rs`) replacing sockudo-ws's
on the client. Still far beyond what an agent produces (a fast agent streams
hundreds of deltas per second), but it should be won back.

## Review sheet (release build, 165 Hz display)

Reproduce: `cargo run -p orbit-core --example seed_review -- <data> <work> --big`, then start Orbit with `ORBIT_DATA=<data>` and `ORBIT_REVIEW_PERF=<log>`, open "Stream the parser" and press Ctrl+R. The log gets load times and frame-time percentiles per 300 frames.

| Measure (802 files, 49,609 changed lines, Rust highlighting on) | Result |
| --- | ---: |
| Fetch (one `git diff`) and parse, off the UI thread | 290 ms |
| Show (tree, diff state, markers) | 2.7 ms |
| Idle frame | p50 6.1 ms (vsync) |
| Wheel scrolling, 120 per 16 ms, 18 s | p50 6.1–9.8 ms, p99 ≤ 13.6 ms, 0 frames over 18 ms |
| Fling, 360 per 10 ms | p50 6.6–8.5 ms, p95 24 ms, 88 of 600 frames over 18 ms |
