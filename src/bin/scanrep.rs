//! Report what the scanner makes of a real directory tree. Verifies grouping against
//! actual data rather than only synthetic tests.
//!
//! Usage: scanrep <dir>

use std::collections::BTreeMap;

use cull::scan;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/workspace/2026-08_Norwegen_Lofoten".into());
    let root = std::path::Path::new(&dir);

    let t = std::time::Instant::now();
    let dirs = scan::scan(root);
    let groups = scan::flatten(&dirs);
    let elapsed = t.elapsed();

    println!(
        "scanned {} in {:.3} s: {} dirs, {} groups",
        root.display(),
        elapsed.as_secs_f64(),
        dirs.len(),
        groups.len()
    );

    // Distribution of group shapes, to catch mis-grouping at a glance.
    let mut shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut raw_only = 0;
    let mut no_display = 0;
    let mut total_files = 0;

    for g in &groups {
        let mut exts: Vec<&str> = g.members.iter().map(|m| m.ext.as_str()).collect();
        exts.sort_unstable();
        *shapes.entry(exts.join("+")).or_default() += 1;
        if g.is_raw_only() {
            raw_only += 1;
        }
        if g.display_path().is_none() {
            no_display += 1;
        }
        total_files += g.members.len();
    }

    println!("\ngroup shapes (extension multiset -> count):");
    for (shape, n) in &shapes {
        println!("  {shape:<28} {n:>6}");
    }

    println!("\ntotals: {total_files} files in {} groups", groups.len());
    println!("raw-only groups (display via embedded preview): {raw_only}");
    println!("groups with no displayable file: {no_display}   <- must be 0");

    println!("\nper-directory:");
    for d in &dirs {
        println!(
            "  {:<52} {:>5} groups",
            d.path
                .strip_prefix(root)
                .unwrap_or(&d.path)
                .display()
                .to_string(),
            d.groups.len()
        );
    }

    // Sizes and times come from the background stat pass at runtime (R23); run it inline
    // here and report what it cost, since that is the number that dominates a cold launch.
    let meta = scan::MetaStore::new();
    let paths: Vec<_> = groups.iter().flat_map(|g| g.member_paths().cloned()).collect();
    let t = std::time::Instant::now();
    meta.fill(&paths);
    println!("\nstat pass: {} files in {:?}", paths.len(), t.elapsed());

    println!("\nsidebar row preview (as the tree renders them):");
    for g in groups.iter().take(6) {
        println!(
            "  {:<20} {:<8}{:>9}  {}",
            g.stem,
            g.kind(),
            meta.group_bytes(g).map_or_else(|| "-".into(), scan::format_size),
            meta.group_modified(g)
                .map(scan::format_time)
                .unwrap_or_else(|| "-".into()),
        );
    }

    println!("\nfirst 3 groups in navigation order:");
    for g in groups.iter().take(3) {
        println!("  {} ({} files)", g.stem, g.members.len());
        for m in &g.members {
            println!(
                "      {:<10} {:?}  {}",
                m.ext,
                m.role,
                m.path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        println!(
            "      display -> {}",
            g.display_path()
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                .unwrap_or_default()
        );
    }
}
