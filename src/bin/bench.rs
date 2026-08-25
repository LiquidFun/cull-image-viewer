//! Decode throughput benchmark on real files.
//!
//! Answers the question REQUIREMENTS.md R1 hinges on: can we keep a +/-10 prefetch
//! ring warm during fast navigation? If parallel throughput comfortably exceeds the
//! rate a human can press a key, the ring can be naive and we can decode at full
//! resolution -- which also removes the need to reload on zoom-in.
//!
//! Usage: bench <dir> [count]

use std::path::{Path, PathBuf};
use std::time::Instant;

use cull::decode::{self, Decoder};
use rayon::prelude::*;

/// One timing run over a set of files.
struct Run {
    label: String,
    /// Wall time for the whole set.
    wall: f64,
    /// Per-image decode times, in milliseconds.
    each: Vec<f64>,
    megapixels: f64,
    images: usize,
}

impl Run {
    fn report(&self) {
        if self.images == 0 {
            println!("{:<34} no images decoded", self.label);
            return;
        }
        let mut sorted = self.each.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];
        let worst = sorted[sorted.len() - 1];
        let per_img = self.wall * 1000.0 / self.images as f64;
        println!(
            "{:<34} {:>7.1} img/s  {:>7.1} MP/s  median {:>6.1} ms  p100 {:>6.1} ms  \
             eff {:>6.1} ms/img",
            self.label,
            self.images as f64 / self.wall,
            self.megapixels / self.wall,
            median,
            worst,
            per_img,
        );
    }
}

fn collect(dir: &Path, ext: &str, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut dirs = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
                out.push(p);
                if out.len() >= limit {
                    return out;
                }
            }
        }
        // Deterministic order so repeated runs hit the same files.
        dirs.sort();
        stack.extend(dirs);
    }
    out.sort();
    out
}

/// Read every file into memory so the timed section measures decode, not I/O.
fn preload(paths: &[PathBuf]) -> Vec<Vec<u8>> {
    paths
        .par_iter()
        .filter_map(|p| std::fs::read(p).ok())
        .collect()
}

fn time_serial(label: &str, blobs: &[Vec<u8>], target: Option<(u32, u32)>, d: Decoder) -> Run {
    let mut each = Vec::new();
    let mut megapixels = 0.0;
    let mut images = 0;
    let start = Instant::now();
    for b in blobs {
        let t = Instant::now();
        match decode::decode(b, target, d) {
            Ok(img) => {
                each.push(t.elapsed().as_secs_f64() * 1000.0);
                megapixels += img.megapixels();
                images += 1;
            }
            Err(e) => eprintln!("  ! {label}: {e}"),
        }
    }
    Run {
        label: label.to_string(),
        wall: start.elapsed().as_secs_f64(),
        each,
        megapixels,
        images,
    }
}

fn time_parallel(label: &str, blobs: &[Vec<u8>], target: Option<(u32, u32)>, d: Decoder) -> Run {
    let start = Instant::now();
    let results: Vec<(f64, f64)> = blobs
        .par_iter()
        .filter_map(|b| {
            let t = Instant::now();
            decode::decode(b, target, d)
                .ok()
                .map(|img| (t.elapsed().as_secs_f64() * 1000.0, img.megapixels()))
        })
        .collect();
    let wall = start.elapsed().as_secs_f64();
    Run {
        label: label.to_string(),
        wall,
        each: results.iter().map(|r| r.0).collect(),
        megapixels: results.iter().map(|r| r.1).sum(),
        images: results.len(),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .unwrap_or_else(|| "/workspace/2026-08_Norwegen_Lofoten".into());
    let count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(24);
    let dir = Path::new(&dir);

    println!("threads: {}", rayon::current_num_threads());

    let jpgs = collect(dir, "JPG", count);
    let arws = collect(dir, "ARW", count);
    println!("files: {} JPG, {} ARW\n", jpgs.len(), arws.len());

    if jpgs.is_empty() && arws.is_empty() {
        eprintln!("no files found under {}", dir.display());
        std::process::exit(1);
    }

    // Report what we are actually decoding, so the numbers are interpretable.
    if let Some(first) = jpgs.first() {
        if let Ok(b) = std::fs::read(first) {
            if let Ok(img) = decode::decode(&b, None, Decoder::Zune) {
                println!(
                    "sample JPG: {}x{} ({:.1} MP) orientation {} icc {} colorspace {:?}",
                    img.width,
                    img.height,
                    img.megapixels(),
                    img.orientation,
                    img.icc.as_ref().map_or(0, |v| v.len()),
                    img.color_space,
                );
            }
        }
    }
    if let Some(first) = arws.first() {
        if let Ok(b) = std::fs::read(first) {
            match decode::decode(&b, None, Decoder::Zune) {
                Ok(img) => println!(
                    "sample ARW preview: {}x{} ({:.1} MP) orientation {}",
                    img.width,
                    img.height,
                    img.megapixels(),
                    img.orientation
                ),
                Err(e) => println!("sample ARW: {e}"),
            }
        }
    }

    // Cold-ish I/O reference: how long does simply reading the bytes take?
    let t = Instant::now();
    let jpg_blobs = preload(&jpgs);
    let read_mb: f64 = jpg_blobs.iter().map(|b| b.len() as f64).sum::<f64>() / 1e6;
    println!(
        "\nread {:.0} MB of JPG in {:.3} s ({:.0} MB/s, page cache warm after this)",
        read_mb,
        t.elapsed().as_secs_f64(),
        read_mb / t.elapsed().as_secs_f64()
    );
    let arw_blobs = preload(&arws);

    // A 4K viewport. Fit-to-window for a 3:2 image on 16:9 is height-limited, so the
    // pixels actually needed are 3240x2160.
    let viewport = Some((3240u32, 2160u32));

    println!("\n-- serial (one image at a time, the latency you feel on a cold slot)");
    time_serial("zune full-res RGBA", &jpg_blobs, None, Decoder::Zune).report();
    time_serial("jpeg-decoder full-res", &jpg_blobs, None, Decoder::JpegDecoder).report();
    time_serial(
        "jpeg-decoder fit-4K (DCT scaled)",
        &jpg_blobs,
        viewport,
        Decoder::JpegDecoder,
    )
    .report();
    time_serial(
        "jpeg-decoder 1/4 scale",
        &jpg_blobs,
        Some((1548, 1032)),
        Decoder::JpegDecoder,
    )
    .report();
    time_serial("ARW preview + zune", &arw_blobs, None, Decoder::Zune).report();

    println!("\n-- parallel (rayon, the rate the prefetch ring can be refilled)");
    time_parallel("zune full-res RGBA", &jpg_blobs, None, Decoder::Zune).report();
    time_parallel("jpeg-decoder full-res", &jpg_blobs, None, Decoder::JpegDecoder).report();
    time_parallel(
        "jpeg-decoder fit-4K (DCT scaled)",
        &jpg_blobs,
        viewport,
        Decoder::JpegDecoder,
    )
    .report();
    time_parallel(
        "jpeg-decoder 1/4 scale",
        &jpg_blobs,
        Some((1548, 1032)),
        Decoder::JpegDecoder,
    )
    .report();
    time_parallel("ARW preview + zune", &arw_blobs, None, Decoder::Zune).report();

    println!(
        "\nVRAM per image at full res: {:.0} MB RGBA; a +/-10 ring holds 21 => {:.1} GB",
        6192.0 * 4128.0 * 4.0 / 1e6,
        21.0 * 6192.0 * 4128.0 * 4.0 / 1e9
    );
}
