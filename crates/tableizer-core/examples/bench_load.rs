//! Load-path timing harness (perf diagnosis / regression guard for progressive availability).
//!
//! Measures: (A) `open` — should be ~instant regardless of file size; (B) first screen, served by
//! streaming before the index lands; (C) background full-index completion. Usage:
//! `cargo run --release -p tableizer-core --example bench_load -- <path>`

use std::path::Path;
use std::time::{Duration, Instant};

use tableizer_core::{
    CancellationToken, ColumnId, CsvTable, RowCount, RowRange, ViewportRequest, ViewportSource,
    parse::Dialect,
};

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench_load <path>");
    let size = std::fs::metadata(&path).expect("stat").len();

    // A) open = mmap + parse the head for the schema. Must not scan the whole file.
    let t = Instant::now();
    let table = CsvTable::open(Path::new(&path), Dialect::default()).expect("open");
    let open_ms = t.elapsed().as_secs_f64() * 1e3;

    // B) first screen, served by streaming from the head while the index is still building.
    let columns: Vec<ColumnId> = (0..table.schema().columns.len() as u32)
        .map(ColumnId)
        .collect();
    let t = Instant::now();
    let _ = table
        .fetch(
            &ViewportRequest {
                rows: RowRange { start: 0, len: 100 },
                columns: columns.clone(),
            },
            &CancellationToken::new(),
        )
        .expect("fetch head");
    let first_paint_ms = t.elapsed().as_secs_f64() * 1e3;

    // C) background full-index completion (the one-time Tier B cost — no longer blocks the UI).
    let t = Instant::now();
    let rows = loop {
        if let RowCount::Exact(n) = table.row_count() {
            break n;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let index_ms = t.elapsed().as_secs_f64() * 1e3;
    let mbps = (size as f64 / 1e6) / (index_ms / 1e3);

    println!("file {:.0} MB, {rows} rows", size as f64 / 1e6);
    println!(
        "A) open (mmap + head schema):              {open_ms:.2} ms   <- user sees the file here"
    );
    println!("B) first screen (streamed, pre-index):     {first_paint_ms:.2} ms");
    println!(
        "C) background full index completes:        {index_ms:.0} ms  ({mbps:.0} MB/s, off the UI thread)"
    );
}
