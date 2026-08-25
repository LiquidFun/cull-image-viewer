//! End-to-end check of R1: walk the real library and measure how long each "next
//! image" actually takes once the ring is running.
//!
//! Usage: ringtest <dir> [steps] [radius] [threads] [think_ms]
//!
//! `think_ms` is the pause between keystrokes. Zero means "consume as fast as the
//! decoder can produce", which is producer-limited and therefore measures decode
//! throughput rather than the ring. A realistic human rate shows whether the ring
//! actually gets ahead, which is what R1 asks.

use std::time::{Duration, Instant};

use cull::prefetch::{FileLoader, Prefetcher, State};
use cull::scan;

/// How long we are willing to wait for one image before calling it a stall.
const STALL: Duration = Duration::from_millis(2000);

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .unwrap_or_else(|| "/workspace/2026-08_Norwegen_Lofoten".into());
    let steps: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(40);
    let radius: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let threads: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let think = Duration::from_millis(args.next().and_then(|s| s.parse().ok()).unwrap_or(0));

    let dirs = scan::scan(std::path::Path::new(&dir));
    let groups = scan::flatten(&dirs);
    let paths: Vec<_> = groups
        .iter()
        .filter_map(|g| g.display_path().map(|p| p.to_path_buf()))
        .collect();

    if paths.is_empty() {
        eprintln!("no images found under {dir}");
        std::process::exit(1);
    }
    println!(
        "{} groups, radius {radius}, {} worker threads\n",
        paths.len(),
        if threads == 0 {
            std::thread::available_parallelism().map_or(4, |n| n.get())
        } else {
            threads
        }
    );

    let pf = Prefetcher::new(paths.clone(), radius, threads, FileLoader::default());

    // Start at an offset so the walk stays inside one directory.
    let start = 0usize;
    pf.set_centre(start);

    // Cold start: the first image has nothing prefetched, so time it separately.
    let t0 = Instant::now();
    let cold = wait_for(&pf, start);
    let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("cold start (index {start}): {cold_ms:.1} ms  -> {cold:?}");

    let mut waits = Vec::new();
    let mut instant = 0usize;
    let mut stalls = 0usize;
    let mut bytes = 0u64;

    // Walk forward as fast as the images can be consumed, which is the worst realistic
    // case: no human think-time between keystrokes.
    for step in 1..=steps.min(paths.len() - start - 1) {
        // Time spent looking at the previous image is time the ring gets to work.
        if !think.is_zero() {
            std::thread::sleep(think);
        }
        let idx = start + step;
        let t = Instant::now();

        // Was it already warm the instant we asked for it?
        let pre = pf.state(idx);
        pf.set_centre(idx);
        let st = wait_for(&pf, idx);
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        if matches!(pre, State::Ready | State::Collected) {
            instant += 1;
        }
        if ms > STALL.as_secs_f64() * 1000.0 {
            stalls += 1;
        }
        waits.push(ms);

        // Collect so pixels do not accumulate, mimicking the renderer uploading.
        for (_, img) in pf.take_ready() {
            bytes += img.rgba.len() as u64;
        }

        if st == State::Failed {
            println!("  index {idx} failed: {:?}", pf.failure(idx));
        }
    }

    waits.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| waits[((waits.len() as f64 - 1.0) * q) as usize];

    println!(
        "\n-- navigation latency over {} steps (think time {} ms => {:.1} img/s demanded)",
        waits.len(),
        think.as_millis(),
        if think.is_zero() {
            f64::INFINITY
        } else {
            1000.0 / think.as_millis() as f64
        }
    );
    println!("  already warm on arrival: {instant}/{}", waits.len());
    println!("  median   {:>7.2} ms", pick(0.50));
    println!("  p90      {:>7.2} ms", pick(0.90));
    println!("  p99      {:>7.2} ms", pick(0.99));
    println!("  worst    {:>7.2} ms", pick(1.0));
    println!("  stalls (>{} ms): {stalls}", STALL.as_millis());

    let st = pf.stats();
    println!(
        "\nring stats: decoded {} discarded {} skipped {} failed {}",
        st.decoded, st.discarded, st.skipped, st.failed
    );
    println!("pixels collected: {:.2} GB", bytes as f64 / 1e9);
    println!(
        "\nNote: this sandbox reports {} usable cores, far fewer than the target machine,\n\
         so treat these latencies as a pessimistic bound.",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
}

/// Block until `idx` resolves to a terminal state, or the stall budget expires.
fn wait_for(pf: &Prefetcher, idx: usize) -> State {
    let deadline = Instant::now() + STALL;
    loop {
        match pf.state(idx) {
            State::Ready | State::Collected | State::Failed => return pf.state(idx),
            _ if Instant::now() >= deadline => return pf.state(idx),
            _ => std::thread::sleep(Duration::from_micros(200)),
        }
    }
}
