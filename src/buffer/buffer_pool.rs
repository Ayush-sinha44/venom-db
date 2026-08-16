use super::frame::Frame;
use super::lru_replacer::LruReplacer;
use crate::storage::disk_manager::DiskManager;
use crate::storage::page::Page;
use std::collections::HashMap;

/// The Buffer Pool is the heart of the database's memory management.
///
/// It maintains a fixed array of Frames in RAM. When a page is requested:
///   1. If it's already in a frame (cache hit)  → return it directly
///   2. If not (cache miss) → load from disk into a free or evicted frame
///
/// "Pinning" a page means marking it in-use so the eviction policy
/// won't throw it out while someone is reading/writing it.
/// Always unpin when done, or the pool fills up and deadlocks.
pub struct BufferPool {
    frames: Vec<Frame>,
    page_table: HashMap<u32, usize>, // page_id → frame_index
    replacer: LruReplacer,
    disk: DiskManager,
    pool_size: usize,

    // stats
    pub hits: u64,
    pub misses: u64,
}

impl BufferPool {
    pub fn new(pool_size: usize, db_path: &str) -> std::io::Result<Self> {
        let mut frames = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            frames.push(Frame::new());
        }

        Ok(Self {
            frames,
            page_table: HashMap::new(),
            replacer: LruReplacer::new(pool_size),
            disk: DiskManager::new(db_path)?,
            pool_size,
            hits: 0,
            misses: 0,
        })
    }

    /// Allocate a brand new page (on disk + in pool).
    /// Returns (page_id, frame_index). Caller must unpin when done.
    pub fn new_page(&mut self) -> std::io::Result<(u32, usize)> {
        let frame_id = self.find_free_frame()?;
        let page_id = self.disk.allocate_page();
        let page = Page::new(page_id);

        self.frames[frame_id].page = Some(page);
        self.frames[frame_id].pin_count = 1;
        self.frames[frame_id].is_dirty = true;

        self.page_table.insert(page_id, frame_id);
        self.replacer.pin(frame_id);

        Ok((page_id, frame_id))
    }

    /// Fetch a page into the pool (or return it if already cached).
    /// Returns frame_index. Caller must unpin when done.
    pub fn fetch_page(&mut self, page_id: u32) -> std::io::Result<usize> {
        // Cache hit
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            self.frames[frame_id].pin_count += 1;
            self.replacer.pin(frame_id);
            self.replacer.record_access(frame_id);
            self.hits += 1;
            return Ok(frame_id);
        }

        // Cache miss — load from disk
        self.misses += 1;
        let frame_id = self.find_free_frame()?;
        let page = self.disk.read_page(page_id)?;

        self.frames[frame_id].page = Some(page);
        self.frames[frame_id].pin_count = 1;
        self.frames[frame_id].is_dirty = false;

        self.page_table.insert(page_id, frame_id);
        self.replacer.pin(frame_id);

        Ok(frame_id)
    }

    /// Get a reference to the page in a frame.
    pub fn get_page(&self, frame_id: usize) -> Option<&Page> {
        self.frames[frame_id].page.as_ref()
    }

    /// Get a mutable reference to the page in a frame.
    pub fn get_page_mut(&mut self, frame_id: usize) -> Option<&mut Page> {
        self.frames[frame_id].page.as_mut()
    }

    /// Unpin a frame when you're done using it.
    /// Set is_dirty=true if you modified the page.
    pub fn unpin(&mut self, frame_id: usize, is_dirty: bool) {
        if self.frames[frame_id].pin_count == 0 {
            return; // already unpinned, ignore
        }

        self.frames[frame_id].pin_count -= 1;
        if is_dirty {
            self.frames[frame_id].is_dirty = true;
        }

        if self.frames[frame_id].pin_count == 0 {
            self.replacer.unpin(frame_id);
        }
    }

    /// Flush a specific page to disk immediately.
    pub fn flush_page(&mut self, page_id: u32) -> std::io::Result<()> {
        if let Some(&frame_id) = self.page_table.get(&page_id)
            && self.frames[frame_id].is_dirty {
                if let Some(page) = self.frames[frame_id].page.as_mut() {
                    self.disk.write_page(page)?;
                }
                self.frames[frame_id].is_dirty = false;
            }
        Ok(())
    }

    /// Flush all dirty pages to disk.
    pub fn flush_all(&mut self) -> std::io::Result<()> {
        let page_ids: Vec<u32> = self.page_table.keys().cloned().collect();
        for page_id in page_ids {
            self.flush_page(page_id)?;
        }
        Ok(())
    }

    /// Cache hit rate as a percentage.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        self.hits as f64 / total as f64 * 100.0
    }

    pub fn pool_size(&self) -> usize {
        self.pool_size
    }

    // --- Internal helpers ---

    /// Find a frame to use — either empty or evict the LRU victim.
    fn find_free_frame(&mut self) -> std::io::Result<usize> {
        // First: look for an empty frame
        for i in 0..self.pool_size {
            if self.frames[i].is_empty() {
                return Ok(i);
            }
        }

        // No empty frames — evict via LRU
        let victim_id = self.replacer.evict().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "Buffer pool full: all pages are pinned",
            )
        })?;

        // If the victim is dirty, flush it to disk before evicting
        if self.frames[victim_id].is_dirty
            && let Some(page) = self.frames[victim_id].page.as_mut() {
                self.disk.write_page(page)?;
            }

        // Remove from page table
        if let Some(old_page_id) = self.frames[victim_id].page.as_ref().map(|p| p.id) {
            self.page_table.remove(&old_page_id);
        }

        // Clear the frame
        self.frames[victim_id].page = None;
        self.frames[victim_id].pin_count = 0;
        self.frames[victim_id].is_dirty = false;

        Ok(victim_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_page_and_fetch() -> std::io::Result<()> {
        let path = "/tmp/test_bp.db";
        let _ = std::fs::remove_file(path);
        let mut pool = BufferPool::new(4, path)?;

        // Create a new page and write a tuple
        let (page_id, frame_id) = pool.new_page()?;
        {
            let page = pool.get_page_mut(frame_id).unwrap();
            page.insert_tuple(b"hello from buffer pool");
        }
        pool.unpin(frame_id, true);
        pool.flush_page(page_id)?;

        // Fetch it back
        let frame_id2 = pool.fetch_page(page_id)?;
        let data = pool
            .get_page(frame_id2)
            .unwrap()
            .get_tuple(0)
            .unwrap()
            .to_vec();
        pool.unpin(frame_id2, false);

        assert_eq!(data, b"hello from buffer pool");
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn test_lru_eviction() -> std::io::Result<()> {
        let path = "/tmp/test_evict.db";
        let _ = std::fs::remove_file(path);

        // Pool with only 3 frames
        let mut pool = BufferPool::new(3, path)?;

        // Fill all 3 frames
        let (pid0, fid0) = pool.new_page()?;
        let (pid1, fid1) = pool.new_page()?;
        let (pid2, fid2) = pool.new_page()?;

        // Unpin all — now all are eviction candidates
        pool.unpin(fid0, false);
        pool.unpin(fid1, false);
        pool.unpin(fid2, false);

        // Allocating a 4th page should evict page 0 (LRU)
        let (_pid3, _fid3) = pool.new_page()?;

        // page 0 should no longer be in page_table
        assert!(!pool.page_table.contains_key(&pid0));
        // pages 1 and 2 still in pool
        assert!(pool.page_table.contains_key(&pid1));
        assert!(pool.page_table.contains_key(&pid2));

        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn test_hit_rate() -> std::io::Result<()> {
        let path = "/tmp/test_hitrate.db";
        let _ = std::fs::remove_file(path);
        let mut pool = BufferPool::new(4, path)?;

        let (page_id, frame_id) = pool.new_page()?;
        pool.unpin(frame_id, false);

        // Fetch the same page 9 more times — all hits
        for _ in 0..9 {
            let fid = pool.fetch_page(page_id)?;
            pool.unpin(fid, false);
        }

        // 1 miss (initial new_page doesn't count as fetch)
        // 9 hits
        println!("Hit rate: {:.1}%", pool.hit_rate());
        assert!(pool.hit_rate() > 80.0);

        let _ = std::fs::remove_file(path);
        Ok(())
    }
    #[test]
    fn test_dirty_page_flushed_before_eviction() -> std::io::Result<()> {
        let path = "/tmp/test_dirty_evict.db";
        let _ = std::fs::remove_file(path);

        let mut pool = BufferPool::new(2, path)?; // tiny pool forces eviction

        let (pid0, fid0) = pool.new_page()?;
        {
            let page = pool.get_page_mut(fid0).unwrap();
            page.insert_tuple(b"dirty data");
        }
        pool.unpin(fid0, true); // mark dirty, do NOT flush manually

        let (_pid1, fid1) = pool.new_page()?;
        pool.unpin(fid1, false);

        let (_pid2, fid2) = pool.new_page()?; // forces eviction of pid0 (LRU)
        pool.unpin(fid2, false);

        // pid0 should have been auto-flushed to disk during eviction
        let fid_reload = pool.fetch_page(pid0)?;
        let data = pool
            .get_page(fid_reload)
            .unwrap()
            .get_tuple(0)
            .unwrap()
            .to_vec();
        pool.unpin(fid_reload, false);

        assert_eq!(data, b"dirty data");

        let _ = std::fs::remove_file(path);
        Ok(())
    }
}
