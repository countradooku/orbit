# bitcode or wincode for Orbit's messages (2026-11-25)

Orbit encodes everything with bitcode: the host link, the event log and
thread timelines. bitcode's per-message cost grows with the size of the
type, not the value (see `docs/BENCHMARKS.md`). This compares it with
wincode on Orbit's own messages.

## Candidates

From [rust_serialization_benchmark](https://github.com/djkoloski/rust_serialization_benchmark)
(updated 2026-11-05) and crates.io on 2026-11-25:

| Crate | Latest release | Standing in the benchmark |
| --- | --- | --- |
| wincode 0.6.3 | 2026-10-13 | Most consistent speed: top 2 to serialize and deserialize on log, mesh and mk48; mid-pack size. Built by Anza (Agave); RustSec names it the replacement for bincode (RUSTSEC-2025-0141). |
| bitcode 0.6.9 | 2025-12-18 (commits to 2026-06) | Smallest output on log, minecraft and mk48; fastest on minecraft. |
| rkyv 0.8.18 | 2026-08-05 | Zero-copy access; best for reading large stored data in place, heavy for small messages. |
| postcard 1.2.0 | 2026-11-21 | Very small output; slower to deserialize. |
| savefile 0.20.6 | 2026-10-19 | Fast, little used (about 10k recent downloads). |

bincode, musli and speedy are not in the benchmark; bincode is flagged as
unmaintained and speedy's last release was January 2024.

## Measured on Orbit's messages

Branch `experiment/wincode`: every protocol type also derives wincode behind
a `wincode` feature, and
`cargo run --release -p orbit-proto --features wincode --example codecs`
times encode+decode with reused buffers. Release build, Windows 11,
32 threads; two runs agree within a few percent.

| Message | bitcode | wincode (bincode layout) | wincode (varint, `u8` tags) |
| --- | ---: | ---: | ---: |
| Streamed token (`Envelope`) | 4.8 µs, 14 B | 60 ns, 45 B | 65 ns, 11 B |
| 512 tokens (`ServerMsg::Events`) | 30 µs, 3.4 KB | 27 µs, 22.5 KB | 29 µs, 5.5 KB |
| Call: rename a thread | 4.1 µs, 20 B | 78 ns, 41 B | 85 ns, 18 B |
| Reply: `Done` | 12.4 µs, 5 B | 21 ns, 16 B | 22 ns, 4 B |
| Thread record (event log) | 5.0 µs, 78 B | 162 ns, 197 B | 190 ns, 66 B |
| Timeline, 3,000 items | 1.3 ms, 2.15 MB | 1.3 ms, 2.30 MB | 0.85 ms, 2.17 MB |
| Snapshot, 500 threads | 50 µs, 20.7 KB | 66 µs, 94.8 KB | 81 µs, 34.3 KB |

## Reading it

- **Single messages:** wincode is 25–600× faster (60–190 ns against 4–12 µs).
  With varint integers and `u8` tags it is also smaller than bitcode. These
  are most of what Orbit sends: calls, replies, state changes, log records.
- **Large batches:** even in speed. bitcode is about 40 % smaller on token
  batches (6.7 against 11 bytes per token) and on snapshots, where its
  columnar layout compresses repeated fields.
- **Timelines:** wincode (varint) is about 1.5× faster at the same size.
- **Snapshots:** bitcode is 1.6× faster. A snapshot is sent once per
  connection, so 30 µs there matters less than 4 µs on every call.

The link fix already in main (`orbit_proto::link`) routes the two hottest
shapes (token runs, `Done` replies) around bitcode's per-message cost. wincode
would remove that cost for every other message too, including calls (4 µs →
85 ns) and each durable log write (5 µs → 190 ns).

## If Orbit switches

- Use the varint configuration with `u8` enum tags (no Orbit enum has more
  than 256 variants).
- Raise wincode's preallocation limit, 4 MiB by default, to the frame limit
  (64 MiB): a long thread's timeline can exceed 4 MiB, and decoding would
  fail.
- Durable logs: a new schema version written with wincode; logs from older
  versions keep being read through bitcode in `legacy.rs`, and are rewritten
  once.
- Keep the link's kind byte, so the frame format can change again later.
