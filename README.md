# Tableizer

View structured data files (CSV, TSV) as tables in a cross-platform desktop app for macOS, Windows, and Linux.

## Workspace layout

- [`crates/tableizer-core`](crates/tableizer-core) — the UI-agnostic data engine: byte-faithful
  parsing, out-of-core sparse indexing, streaming search, and external sort. The hard ~80%.
- [`crates/tableizer`](crates/tableizer) — the cross-platform desktop shell
  (`winit` + `wgpu` + `egui`/`eframe`). A thin consumer of the engine's `ViewportSource` trait.

## Build & run

```sh
cargo run --release -p tableizer -- path/to/file.csv   # open a file in the desktop app
cargo test --workspace                                 # run the tests
```

Need a large file to try it on? Generate one:

```sh
cargo run --release -p tableizer-core --example gen_csv -- /tmp/big.csv 5000000
```

## Development

The engine has a fast inner loop via tests (`cargo test -p tableizer-core <name>`). See
[AGENTS.md](AGENTS.md) for conventions (red/green TDD, lints, dependency policy, git rules).

Common tasks are wrapped in a [`justfile`](justfile) (`cargo install just`): run `just` to list recipes —
e.g. `just ci` (format-check + lint + tests, mirrors CI), `just dev <file>` (the UI loop below),
`just gen <file> <rows>` (make test data), `just bench <file>` (time the load path).

For iterating on the **UI**, auto-rebuild and re-run on save with
[`cargo-watch`](https://github.com/watchexec/cargo-watch) — with the app's instant file-open, the
re-run is near-seamless (you lose only scroll position):

```sh
cargo install cargo-watch
cargo watch -x 'run -p tableizer -- /tmp/big.csv'   # debug build = fastest rebuilds, best for UI tweaks
```

Use a `--release` build for performance / frame-rate measurement; the debug build for quick visual tweaks.
(True state-preserving hot reload via a `hot-lib-reloader` split is possible but not currently set up.)

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you shall be dual-licensed as above, without any additional terms.
