mod storage;
mod buffer;

use buffer::buffer_pool::BufferPool;

fn main() -> std::io::Result<()> {
    let db_path = "venom.db";
    let _ = std::fs::remove_file(db_path); // fresh start

    // Pool with only 4 frames — tiny on purpose to show eviction working
    let mut pool = BufferPool::new(4, db_path)?;

    println!("=== Phase 2: Buffer Pool ===\n");

    // --- Test 1: Create pages and insert tuples ---
    println!("--- Test 1: Insert tuples via buffer pool ---");

    let mut page_ids = Vec::new();

    for i in 0..4 {
        let (page_id, frame_id) = pool.new_page()?;
        {
            let page = pool.get_page_mut(frame_id).unwrap();
            let row = format!("row_from_page_{}", i);
            page.insert_tuple(row.as_bytes());
        }
        pool.unpin(frame_id, true); // dirty = true, we modified it
        page_ids.push(page_id);
        println!("Created page {} with one tuple", page_id);
    }

    pool.flush_all()?;
    println!("All pages flushed to disk.\n");

    // --- Test 2: Fetch pages back (should be cache hits) ---
    println!("--- Test 2: Fetch pages (expect cache hits) ---");

    for &pid in &page_ids {
        let frame_id = pool.fetch_page(pid)?;
        let data = pool.get_page(frame_id)
            .unwrap()
            .get_tuple(0)
            .unwrap()
            .to_vec();
        pool.unpin(frame_id, false);
        println!("Page {}: \"{}\"", pid, std::str::from_utf8(&data).unwrap());
    }

    println!("\nHit rate after fetches: {:.1}%", pool.hit_rate());

    // --- Test 3: Force eviction (pool has 4 frames, add a 5th page) ---
    println!("\n--- Test 3: Force LRU eviction ---");
    println!("Pool size: 4 frames, creating 5th page → must evict LRU");

    let (pid5, fid5) = pool.new_page()?;
    {
        let page = pool.get_page_mut(fid5).unwrap();
        page.insert_tuple(b"i caused an eviction");
    }
    pool.unpin(fid5, true);

    println!("5th page created (page {}), LRU page was evicted to disk", pid5);
    println!("Misses so far: {}", pool.misses);

    // --- Test 4: Access evicted page — must reload from disk ---
    println!("\n--- Test 4: Access evicted page (expect disk reload) ---");

    let misses_before = pool.misses;
    let frame_id = pool.fetch_page(page_ids[0])?;
    let data = pool.get_page(frame_id)
        .unwrap()
        .get_tuple(0)
        .unwrap()
        .to_vec();
    pool.unpin(frame_id, false);

    let reloaded_from_disk = pool.misses > misses_before;
    println!(
        "Page {}: \"{}\" (reloaded from disk: {})",
        page_ids[0],
        std::str::from_utf8(&data).unwrap(),
        reloaded_from_disk
    );

    // --- Final stats ---
    println!("\n=== Buffer Pool Stats ===");
    println!("Total hits:   {}", pool.hits);
    println!("Total misses: {}", pool.misses);
    println!("Hit rate:     {:.1}%", pool.hit_rate());
    println!("Pool size:    {} frames", pool.pool_size());

    pool.flush_all()?;
    println!("\nAll dirty pages flushed. Done.");

    Ok(())
}
