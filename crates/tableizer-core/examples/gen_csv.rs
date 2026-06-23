//! Generate a synthetic CSV for manual and performance testing (e.g. feeding the grid spike a
//! multi-GB file — never check such files into the repo).
//!
//! Usage: `cargo run --release -p tableizer-core --example gen_csv -- <path> <rows>`

use std::fs::File;
use std::io::{BufWriter, Write};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: gen_csv <path> <rows>");
    let rows: u64 = args
        .next()
        .expect("usage: gen_csv <path> <rows>")
        .parse()
        .expect("rows must be a number");

    let file = File::create(&path).expect("create output file");
    let mut out = BufWriter::new(file);
    writeln!(out, "id,name,email,score,note").expect("write header");
    for i in 0..rows {
        if i == 0 {
            // The first data row carries a quoted embedded newline, exercising the index on real input.
            writeln!(out, "{i},\"multi\nline\",a@example.com,0,first").expect("write row");
        } else {
            writeln!(out, "{i},name{i},user{i}@example.com,{},note {i}", i % 100)
                .expect("write row");
        }
    }
    out.flush().expect("flush");
    println!("wrote {rows} rows to {path}");
}
