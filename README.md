# Tableizer

View structured data formats (CSV, TSV, arbitrary separators) as tables — including very large
(multi-TB) files — in a cross-platform desktop app for macOS, Windows, and Linux.

**Status:** early scaffold. See [docs/spec.md](docs/spec.md) for the full specification and roadmap.

## Workspace layout

- [`crates/tableizer-core`](crates/tableizer-core) — the UI-agnostic data engine: byte-faithful
  parsing, out-of-core sparse indexing, streaming search, and external sort. The hard ~80%.
- [`crates/tableizer`](crates/tableizer) — the cross-platform desktop shell
  (`winit` + `wgpu` + `egui`/`eframe`). A thin consumer of the engine's `ViewportSource` trait.

## Build & run

```sh
cargo run -p tableizer   # opens the desktop window
cargo test --workspace       # run the engine tests
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you shall be dual-licensed as above, without any additional terms.
