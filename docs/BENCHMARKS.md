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
