//! Dump what `cull` extracts from given files. Diagnostic aid for verifying
//! orientation, ICC and RAW preview selection against real files.
//!
//! Usage: probe <file>...

use cull::{decode, tiff};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: probe <file>...");
        std::process::exit(2);
    }

    for path in &args {
        let Ok(bytes) = std::fs::read(path) else {
            println!("{path}: unreadable");
            continue;
        };
        let name = path.rsplit('/').next().unwrap_or(path);
        println!("\n=== {name}  ({:.1} MB)", bytes.len() as f64 / 1e6);

        // Container view: which previews exist, and what the IFDs claim.
        if let Some(info) = tiff::parse(&bytes) {
            println!("  tiff: orientation {} colorspace {:?}", info.orientation, info.color_space);
            for p in &info.previews {
                println!(
                    "    preview {}x{} @{} len {} orient {}",
                    p.width, p.height, p.offset, p.len, p.orientation
                );
            }
        }

        match decode::locate(&bytes) {
            Ok(loc) => println!(
                "  locate: from_raw {} orientation {} colorspace {:?} icc {} B  jpeg {:.2} MB",
                loc.from_raw,
                loc.orientation,
                loc.color_space,
                loc.icc.as_ref().map_or(0, |v| v.len()),
                loc.jpeg.len() as f64 / 1e6,
            ),
            Err(e) => println!("  locate: {e}"),
        }

        match decode::decode(&bytes, None, decode::Decoder::Zune) {
            Ok(img) => println!(
                "  decode: {}x{} ({:.1} MP) native {}x{} denom {} orientation {}",
                img.width,
                img.height,
                img.megapixels(),
                img.native_width,
                img.native_height,
                img.scale_denom,
                img.orientation,
            ),
            Err(e) => println!("  decode: {e}"),
        }
    }
}
