// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Heap regression: the resident corpus must not scale with every eligible
//! file, either cold or while the old publication stays alive during refresh.

use pixel_recall::code_resident::Resident;
use pixel_recall::code_search::VectorCache;
use pixel_recall::embed::{EmbedKind, Embedder};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Meter;
static TEST_LOCK: Mutex<()> = Mutex::new(());
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn allocated(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every operation delegates to System with its original pointer and
// layout; the counters do not allocate or change allocation behavior.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid layout, forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: pointer and layout are forwarded to their original allocator.
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: caller's allocation and requested size are forwarded unchanged.
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            allocated(size);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: Meter = Meter;

struct WideEmbedder;
impl Embedder for WideEmbedder {
    fn model_id(&self) -> &str {
        "memory-fixture"
    }
    fn dims(&self) -> usize {
        16_384
    }
    fn embed_batch(&mut self, texts: &[&str], _: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|_| vec![1.0; self.dims()]).collect())
    }
}

#[test]
fn resident_heap_stays_bounded_on_cold_build_and_full_refresh() {
    const FILES: usize = 256;
    let _guard = TEST_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut previous = None;
    for generation in 0..2 {
        for file in 0..FILES {
            let line = format!("invoice generation{generation} file{file} payment receipt\n");
            let text: String = line.repeat(400).chars().take(16_000).collect();
            std::fs::write(dir.path().join(format!("{file:04}.md")), text).unwrap();
        }
        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
        let resident = Resident::build(
            dir.path(),
            previous.as_ref(),
            &mut WideEmbedder,
            VectorCache::Disabled,
            FILES,
        )
        .unwrap();
        let peak = PEAK.load(Ordering::Relaxed);
        eprintln!(
            "generation={generation} files={} chunks={} resident_bytes={} peak_live_rust_bytes={peak}",
            resident.stats().files,
            resident.stats().chunks,
            resident.stats().bytes
        );
        assert!(
            resident.stats().files > 0 && resident.stats().files < FILES,
            "the default budget must retain useful evidence and exclude the excess"
        );
        let ceiling = if generation == 0 {
            134_217_728
        } else {
            201_326_592
        };
        assert!(
            peak < ceiling,
            "peak Rust heap {peak} >= {ceiling}; corpus or old/new state escaped the budget"
        );
        previous = Some(resident);
    }
}

/// An old cache can be much larger than the admitted corpus. Its unused
/// vectors must never become one whole-segment allocation during loading.
#[test]
fn resident_cache_read_does_not_allocate_the_unselected_segment() {
    const ROWS: usize = 512;
    const DIMS: usize = 16_384;
    let _guard = TEST_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "invoice").unwrap();
    let cache = dir.path().join(".pixel/code-vectors");
    std::fs::create_dir_all(&cache).unwrap();
    let temporary = cache.join("seed");
    let mut file = std::fs::File::create(&temporary).unwrap();
    let mut hash = xxhash_rust::xxh3::Xxh3::new();
    let mut header = Vec::from(&b"PXCVEC01"[..]);
    header.extend_from_slice(&(DIMS as u32).to_le_bytes());
    header.extend_from_slice(&(ROWS as u64).to_le_bytes());
    file.write_all(&header).unwrap();
    hash.update(&header);
    let mut row = vec![0; 16 + 4 * DIMS];
    for key in 0..ROWS {
        row[..16].copy_from_slice(&(key as u128).to_le_bytes());
        file.write_all(&row).unwrap();
        hash.update(&row);
    }
    drop(file);
    drop(row);
    let name = format!("seg-{:032x}.vec", hash.digest128());
    std::fs::rename(temporary, cache.join(&name)).unwrap();
    let namespace = format!(
        "memory-fixture@r{}+c{}",
        pixel_recall::embed::embedder_revision("memory-fixture"),
        pixel_recall::code_search::CHUNKER_VERSION
    );
    let manifest = serde_json::json!({"version":1,"segments":[{
        "file":name,"namespace":namespace,"dim":DIMS,"rows":ROWS
    }]});
    std::fs::write(
        cache.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let resident = Resident::build(
        dir.path(),
        None,
        &mut WideEmbedder,
        VectorCache::Persisted,
        100,
    )
    .unwrap();
    let peak = PEAK.load(Ordering::Relaxed);
    eprintln!(
        "cache_segment_bytes={} peak_live_rust_bytes={peak}",
        20 + ROWS * (16 + 4 * DIMS)
    );
    assert_eq!(resident.stats().files, 1);
    assert_eq!(resident.stats().cached_chunks, 0);
    assert_eq!(resident.stats().embedded_chunks, 1);
    assert_eq!(resident.stats().vector_cache_errors, Vec::<String>::new());
    assert!(peak < 8_388_608, "unused cache segment allocated: {peak}");
}
