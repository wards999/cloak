//! Full-file vs ends-only encryption — the whole reason for the header/footer
//! design. We time encrypting the same buffers both ways across a range of file
//! sizes and print MB/s plus the ratio.
//!
//! This is a dependency-free benchmark (no criterion): `cargo bench` builds it
//! with `harness = false` and runs `main`. Numbers are indicative, not rigorous
//! microbenchmarks — they exist to show *where the two strategies diverge*.

use cloak::crypto::{transform_ends, transform_whole, KEY_LEN, NONCE_LEN};
use std::time::Instant;

const REGION: u64 = 64 * 1024; // 64 KiB protected at each end

fn make_buf(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + 7) as u8).collect()
}

fn time_it(iters: u32, mut f: impl FnMut()) -> f64 {
    // Warm up once (touch pages, prime caches).
    f();
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_secs_f64() / iters as f64
}

fn main() {
    let key = [0x42u8; KEY_LEN];
    let nonce = [0x24u8; NONCE_LEN];

    let sizes = [
        ("64 KiB", 64 * 1024usize),
        ("1 MiB", 1024 * 1024),
        ("16 MiB", 16 * 1024 * 1024),
        ("256 MiB", 256 * 1024 * 1024),
        ("1 GiB", 1024 * 1024 * 1024),
    ];

    println!(
        "{:>8} | {:>12} | {:>12} | {:>10} | {:>18}",
        "size", "full (ms)", "ends (ms)", "speedup", "ends throughput"
    );
    println!("{}", "-".repeat(72));

    for (label, size) in sizes {
        // Fewer iterations for the big buffers so the bench stays quick.
        let iters = if size <= 16 * 1024 * 1024 { 20 } else { 3 };
        let mut buf = make_buf(size);

        let full = time_it(iters, || transform_whole(&key, &nonce, &mut buf));
        let ends = time_it(iters, || transform_ends(&key, &nonce, &mut buf, REGION));

        let speedup = full / ends;
        // Ends only ever touches 2*REGION bytes regardless of size.
        let ends_bytes = (2 * REGION).min(size as u64) as f64;
        let ends_mb_s = (ends_bytes / (1024.0 * 1024.0)) / ends;

        println!(
            "{:>8} | {:>12.3} | {:>12.3} | {:>9.1}x | {:>12.0} MB/s",
            label,
            full * 1000.0,
            ends * 1000.0,
            speedup,
            ends_mb_s
        );
    }

    println!();
    println!("Takeaway: full-file cost grows with size; ends-only is flat (it always");
    println!("touches 2 x {} KiB). The gap is the price a determined examiner could", REGION / 1024);
    println!("still read out of the plaintext middle — see README threat model.");
}
