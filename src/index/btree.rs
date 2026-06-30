use std::collections::HashMap;
use super::node::{BTreeNode, InternalNode, LeafNode, Rid};

/// The B-Tree index maps i64 keys → Rid (page_id, slot_id).
///
/// All nodes are stored as pages in a dedicated index file.
/// page_id 0 is always the root.
///
/// For simplicity in this phase, we store nodes in a HashMap
/// (in-memory). Phase 4 (WAL) will wire this into the buffer pool.
pub struct BTree {
    nodes: HashMap<u32, BTreeNode>,
    next_page_id: u32,
    pub root_id: u32,
}

/// When a node splits, the split result bubbles up to the parent.
struct SplitResult {
    promoted_key: i64,   // key pushed up to parent
    new_page_id: u32,    // right half of the split
}

impl BTree {
    pub fn new() -> Self {
        let mut tree = Self {
            nodes: HashMap::new(),
            next_page_id: 0,
            root_id: 0,
        };
        // Create empty root leaf
        let root_id = tree.alloc_page(BTreeNode::Leaf(LeafNode::new()));
        tree.root_id = root_id;
        tree
    }

    // --- Public API ---

    /// Search for a key. Returns the Rid if found.
    pub fn search(&self, key: i64) -> Option<Rid> {
        let leaf_id = self.find_leaf(self.root_id, key);
        let node = self.nodes.get(&leaf_id)?;
        if let BTreeNode::Leaf(leaf) = node {
            let idx = leaf.find_key_index(key)?;
            Some(leaf.rids[idx])
        } else {
            None
        }
    }

    /// Insert a key → Rid mapping.
    pub fn insert(&mut self, key: i64, rid: Rid) {
        let root_id = self.root_id;
        if let Some(split) = self.insert_recursive(root_id, key, rid) {
            // Root split — create a new root
            let mut new_root = InternalNode::new();
            new_root.keys.push(split.promoted_key);
            new_root.children.push(self.root_id);
            new_root.children.push(split.new_page_id);
            let new_root_id = self.alloc_page(BTreeNode::Internal(new_root));
            self.root_id = new_root_id;
        }
    }

    /// Delete a key. Marks the entry as removed (no rebalancing for now —
    /// a future compaction pass handles underflow merges).
    pub fn delete(&mut self, key: i64) -> bool {
        let leaf_id = self.find_leaf(self.root_id, key);
        if let Some(BTreeNode::Leaf(leaf)) = self.nodes.get_mut(&leaf_id) {
            if let Some(idx) = leaf.find_key_index(key) {
                leaf.keys.remove(idx);
                leaf.rids.remove(idx);
                return true;
            }
        }
        false
    }

    /// Range scan: returns all Rids where start <= key <= end.
    pub fn range_scan(&self, start: i64, end: i64) -> Vec<Rid> {
        let mut results = Vec::new();

        // Find the leaf where `start` would live
        let mut leaf_id = self.find_leaf(self.root_id, start);

        loop {
            let node = match self.nodes.get(&leaf_id) {
                Some(n) => n,
                None => break,
            };

            if let BTreeNode::Leaf(leaf) = node {
                for (i, &key) in leaf.keys.iter().enumerate() {
                    if key > end { return results; }
                    if key >= start { results.push(leaf.rids[i]); }
                }
                match leaf.next_leaf {
                    Some(next_id) => leaf_id = next_id,
                    None => break,
                }
            } else {
                break;
            }
        }

        results
    }

    /// Print the tree structure (useful for debugging).
    pub fn print_tree(&self) {
        self.print_node(self.root_id, 0);
    }

    // --- Internal helpers ---

    /// Recursively insert. Returns Some(SplitResult) if the node split.
    fn insert_recursive(&mut self, page_id: u32, key: i64, rid: Rid) -> Option<SplitResult> {
        let node = self.nodes.get(&page_id)?.clone();

        match node {
            BTreeNode::Leaf(mut leaf) => {
                // Insert in sorted order
                let pos = leaf.keys.partition_point(|&k| k < key);
                leaf.keys.insert(pos, key);
                leaf.rids.insert(pos, rid);

                if leaf.is_full() {
                    let split = self.split_leaf(page_id, leaf);
                    Some(split)
                } else {
                    self.nodes.insert(page_id, BTreeNode::Leaf(leaf));
                    None
                }
            }

            BTreeNode::Internal(mut internal) => {
                let child_idx = internal.find_child_index(key);
                let child_id = internal.children[child_idx];

                if let Some(split) = self.insert_recursive(child_id, key, rid) {
                    // Insert promoted key into this internal node
                    internal.keys.insert(child_idx, split.promoted_key);
                    internal.children.insert(child_idx + 1, split.new_page_id);

                    if internal.is_full() {
                        let split = self.split_internal(page_id, internal);
                        Some(split)
                    } else {
                        self.nodes.insert(page_id, BTreeNode::Internal(internal));
                        None
                    }
                } else {
                    self.nodes.insert(page_id, BTreeNode::Internal(internal));
                    None
                }
            }
        }
    }

    /// Split a full leaf node into two. Returns the promoted key and new right sibling.
    fn split_leaf(&mut self, left_id: u32, mut left: LeafNode) -> SplitResult {
        let mid = left.keys.len() / 2;

        let mut right = LeafNode::new();
        right.keys = left.keys.split_off(mid);
        right.rids = left.rids.split_off(mid);

        // Chain: left → right → (old right sibling)
        right.next_leaf = left.next_leaf;
        let right_id = self.alloc_page(BTreeNode::Leaf(right));
        left.next_leaf = Some(right_id);

        let promoted_key = self.nodes
            .get(&right_id)
            .and_then(|n| if let BTreeNode::Leaf(l) = n { l.keys.first().copied() } else { None })
            .unwrap();

        self.nodes.insert(left_id, BTreeNode::Leaf(left));

        SplitResult { promoted_key, new_page_id: right_id }
    }

    /// Split a full internal node into two.
    fn split_internal(&mut self, left_id: u32, mut left: InternalNode) -> SplitResult {
        let mid = left.keys.len() / 2;
        let promoted_key = left.keys[mid];

        let mut right = InternalNode::new();
        right.keys = left.keys.split_off(mid + 1);
        right.children = left.children.split_off(mid + 1);
        left.keys.pop(); // remove the promoted key from left

        let right_id = self.alloc_page(BTreeNode::Internal(right));
        self.nodes.insert(left_id, BTreeNode::Internal(left));

        SplitResult { promoted_key, new_page_id: right_id }
    }

    /// Traverse internal nodes to find which leaf a key belongs in.
    fn find_leaf(&self, mut page_id: u32, key: i64) -> u32 {
        loop {
            match self.nodes.get(&page_id) {
                Some(BTreeNode::Internal(node)) => {
                    let idx = node.find_child_index(key);
                    page_id = node.children[idx];
                }
                _ => return page_id,
            }
        }
    }

    fn alloc_page(&mut self, node: BTreeNode) -> u32 {
        let id = self.next_page_id;
        self.next_page_id += 1;
        self.nodes.insert(id, node);
        id
    }

    fn print_node(&self, page_id: u32, depth: usize) {
        let indent = "  ".repeat(depth);
        match self.nodes.get(&page_id) {
            Some(BTreeNode::Internal(node)) => {
                println!("{}[Internal] keys: {:?}", indent, node.keys);
                for &child in &node.children {
                    self.print_node(child, depth + 1);
                }
            }
            Some(BTreeNode::Leaf(node)) => {
                println!("{}[Leaf] keys: {:?} next→{:?}", indent, node.keys, node.next_leaf);
            }
            None => println!("{}[missing node {}]", indent, page_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(n: u32) -> Rid { Rid { page_id: n, slot_id: 0 } }

    #[test]
    fn test_insert_and_search() {
        let mut tree = BTree::new();
        for i in [10, 20, 5, 15, 30, 25, 1, 8] {
            tree.insert(i, rid(i as u32));
        }
        assert_eq!(tree.search(10), Some(rid(10)));
        assert_eq!(tree.search(1),  Some(rid(1)));
        assert_eq!(tree.search(30), Some(rid(30)));
        assert_eq!(tree.search(99), None);
    }

    #[test]
    fn test_range_scan() {
        let mut tree = BTree::new();
        for i in 1..=20 {
            tree.insert(i, rid(i as u32));
        }
        let results = tree.range_scan(5, 10);
        let keys: Vec<u32> = results.iter().map(|r| r.page_id).collect();
        assert_eq!(keys, vec![5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn test_delete() {
        let mut tree = BTree::new();
        for i in [10, 20, 30] {
            tree.insert(i, rid(i as u32));
        }
        assert!(tree.delete(20));
        assert_eq!(tree.search(20), None);
        assert_eq!(tree.search(10), Some(rid(10)));
        assert_eq!(tree.search(30), Some(rid(30)));
    }

    #[test]
    fn test_large_insert_forces_splits() {
        let mut tree = BTree::new();
        // Insert enough keys to force multiple splits
        for i in 0..100 {
            tree.insert(i, rid(i as u32));
        }
        // All keys must still be searchable after splits
        for i in 0..100 {
            assert_eq!(tree.search(i), Some(rid(i as u32)),
                "key {} not found after splits", i);
        }
    }

    #[test]
    fn test_range_scan_across_leaves() {
        let mut tree = BTree::new();
        for i in 0..50 {
            tree.insert(i, rid(i as u32));
        }
        let results = tree.range_scan(10, 20);
        assert_eq!(results.len(), 11); // 10..=20
        let keys: Vec<i64> = {
            let leaf_id = tree.find_leaf(tree.root_id, 10);
            if let Some(super::super::node::BTreeNode::Leaf(l)) = tree.nodes.get(&leaf_id) {
                l.keys.iter().filter(|&&k| k >= 10 && k <= 20).copied().collect()
            } else { vec![] }
        };
        // just verify count is right
        assert_eq!(results.len(), 11);
    }

    #[test]
    fn test_serialization_roundtrip() {
        use super::super::node::BTreeNode;
        let mut leaf = LeafNode::new();
        leaf.keys = vec![1, 2, 3];
        leaf.rids = vec![rid(1), rid(2), rid(3)];
        leaf.next_leaf = Some(42);

        let node = BTreeNode::Leaf(leaf);
        let bytes = node.to_bytes();
        let recovered = BTreeNode::from_bytes(&bytes);

        if let BTreeNode::Leaf(l) = recovered {
            assert_eq!(l.keys, vec![1, 2, 3]);
            assert_eq!(l.next_leaf, Some(42));
        } else {
            panic!("wrong node type after deserialization");
        }
    }
}
