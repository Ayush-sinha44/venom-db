use std::collections::VecDeque;

pub struct LruReplacer {
    order: VecDeque<usize>,
    capacity: usize,
}

impl LruReplacer {
    pub fn new(capacity: usize) -> Self {
        Self { order: VecDeque::with_capacity(capacity), capacity }
    }

    pub fn unpin(&mut self, frame_id: usize) {
        if !self.order.contains(&frame_id) {
            self.order.push_back(frame_id);
        }
    }

    pub fn pin(&mut self, frame_id: usize) {
        self.order.retain(|&id| id != frame_id);
    }

    pub fn evict(&mut self) -> Option<usize> {
        self.order.pop_front()
    }

    pub fn record_access(&mut self, frame_id: usize) {
        self.order.retain(|&id| id != frame_id);
        self.order.push_back(frame_id);
    }

    pub fn size(&self) -> usize { self.order.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_eviction() {
        let mut r = LruReplacer::new(3);
        r.unpin(0); r.unpin(1); r.unpin(2);
        assert_eq!(r.evict(), Some(0));
        assert_eq!(r.evict(), Some(1));
        assert_eq!(r.evict(), Some(2));
        assert_eq!(r.evict(), None);
    }

    #[test]
    fn test_record_access_moves_to_back() {
        let mut r = LruReplacer::new(3);
        r.unpin(0); r.unpin(1); r.unpin(2);
        r.record_access(0); // 0 becomes MRU
        assert_eq!(r.evict(), Some(1));
        assert_eq!(r.evict(), Some(2));
        assert_eq!(r.evict(), Some(0));
    }

    #[test]
    fn test_pin_removes_from_candidates() {
        let mut r = LruReplacer::new(3);
        r.unpin(0); r.unpin(1); r.unpin(2);
        r.pin(0); // 0 no longer evictable
        assert_eq!(r.evict(), Some(1));
        assert_eq!(r.evict(), Some(2));
        assert_eq!(r.evict(), None);
    }
}
