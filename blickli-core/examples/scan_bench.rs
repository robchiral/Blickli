//! Benchmark folder scanning and grid-style sorted inserts.
//!
//!     cargo run --release -p blickli-core --example scan_bench -- <folder> [count] [per-folder]
//!
//! Populates `folder` on first run with `count` empty `.jpg` files (default 100,000),
//! split into subfolders of `per-folder` files (default: all in root).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use blickli_core::{Engine, Event, ImageDecoder};

struct NoDecoder;

impl ImageDecoder for NoDecoder {
    type Image = ();
    fn decode(&self, _path: &Path, _max_px: i32) -> Option<()> {
        None
    }
}

fn fill(root: &Path, count: usize, per_folder: usize) {
    let marker = root.join(format!(".bench-{count}-{per_folder}"));
    if marker.exists() {
        return;
    }
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).unwrap();
    let started = Instant::now();
    for n in 0..count {
        let folder = if per_folder >= count {
            root.to_path_buf()
        } else {
            root.join(format!("folder {}", n / per_folder))
        };
        if n % per_folder == 0 {
            std::fs::create_dir_all(&folder).unwrap();
        }
        // Exercise natural sorting and case-insensitive extension matching.
        let ext = if n % 7 == 0 { "JPG" } else { "jpg" };
        std::fs::write(folder.join(format!("IMG_{n}.{ext}")), b"").unwrap();
    }
    std::fs::write(&marker, b"").unwrap();
    eprintln!("filled {count} files in {:.1?}", started.elapsed());
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("folder"));
    let count: usize = args.next().map_or(100_000, |s| s.parse().unwrap());
    let per_folder: usize = args.next().map_or(count, |s| s.parse().unwrap());
    fill(&root, count, per_folder);

    let (engine, events) = Engine::start(NoDecoder, 100, || {});
    let started = Instant::now();
    engine.scan(root, true, |_| false);

    let mut mirror: Vec<std::rc::Rc<blickli_core::FileInfo>> = Vec::new();
    let mut first = None;
    let mut batches = 0usize;
    let mut applying = Duration::ZERO;
    let total = loop {
        match events.recv_timeout(Duration::from_secs(120)).unwrap() {
            Event::FilesInserted { files, .. } => {
                first.get_or_insert_with(|| started.elapsed());
                batches += 1;
                let at = Instant::now();
                blickli_core::apply_inserts(&mut mirror, files, std::rc::Rc::new);
                applying += at.elapsed();
            }
            Event::ScanDone { total, .. } => break total,
            _ => {}
        }
    };
    let done = started.elapsed();
    assert_eq!(total, mirror.len());
    assert!(mirror.windows(2).all(|w| blickli_core::file_info_cmp(
        &w[0],
        &w[1],
        blickli_core::SortOrder::Name,
        false
    )
    .is_lt()));
    println!(
        "{total} files: first batch {:.1?}, done {:.1?}, {batches} batches, \
         mirror inserts {:.1?}",
        first.unwrap_or_default(),
        done,
        applying
    );

    // Benchmark main-thread re-sorting from a worst-case (non-monotonic) order.
    for order in [
        blickli_core::SortOrder::Name,
        blickli_core::SortOrder::Modified,
    ] {
        // Prime stride visits every index once with no existing sorted runs.
        let n = mirror.len();
        let mut files: Vec<_> = (0..n).map(|i| mirror[(i * 7919) % n].clone()).collect();
        let at = Instant::now();
        blickli_core::sort_files(&mut files, |a, b| {
            blickli_core::file_info_cmp(a, b, order, false)
        });
        println!("resort by {order:?}: {:.1?}", at.elapsed());
    }
}
