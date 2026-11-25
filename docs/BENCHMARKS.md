# Benchmarks

All numbers from one Windows 11 machine (32 logical cores), release builds.
Reproduce with `cargo run -p orbit-bench --release` and
`scripts/measure-desktop.ps1`.

## Desktop app (whole process tree, median of runs)

| App | Window up | Processes | Working set | Private | Idle CPU | Executable |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| T3 Code (Electron) | 505 ms | 7 | 994 MB | 857 MB | 2.34 % | 234.5 MB |

## Engine, protocol, storage, network (`orbit-bench`)

| Benchmark | Result |
| --- | ---: |
| Delta codec, 512-delta wire frames | 39.8 M/s, 7.3 B/delta |
| Engine streaming to UI feed | ~3.0 M deltas/s |
| UI state apply | ~17 M deltas/s |
| Cold start with 1,000 threads | 7 ms |
| Open a 3,000-item thread | 2.5 ms |
| In-process command round trip | 0.4 µs p50 |
| WebSocket round trip (loopback) | 63 µs p50, 141 µs p99 |
| WebSocket streaming | 3.6 M deltas/s |

## Review sheet (release build, 165 Hz display)

Reproduce: `cargo run -p orbit-core --example seed_review -- <data> <work> --big`, then start Orbit with `ORBIT_DATA=<data>` and `ORBIT_REVIEW_PERF=<log>`, open "Stream the parser" and press Ctrl+R. The log gets load times and frame-time percentiles per 300 frames.

| Measure (802 files, 49,609 changed lines, Rust highlighting on) | Result |
| --- | ---: |
| Fetch (one `git diff`) and parse, off the UI thread | 290 ms |
| Show (tree, diff state, markers) | 2.7 ms |
| Idle frame | p50 6.1 ms (vsync) |
| Wheel scrolling, 120 per 16 ms, 18 s | p50 6.1–9.8 ms, p99 ≤ 13.6 ms, 0 frames over 18 ms |
| Fling, 360 per 10 ms | p50 6.6–8.5 ms, p95 24 ms, 88 of 600 frames over 18 ms |
