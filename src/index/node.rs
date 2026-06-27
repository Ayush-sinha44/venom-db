use crate::storage::page::PAGE_SIZE;

/// Order of the B-Tree.
/// Each node holds at most 2*ORDER keys.
/// Each internal node has at most 2*ORDER+1 children.
pub const ORDER: usize = 4;

/// A Rid (Record ID) points to where a row physically lives on disk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rid {
    pub page_id: u32,
    pub slot_id: u16,
}

/// A B-Tree node is either an internal node or a leaf node.
///
/// Internal node:  [child0 | key0 | child1 | key1 | child2 ...]
///                  navigates the tree, holds no row data
///
/// Leaf node:      [key0->rid0 | key1->rid1 | ... | next_leaf]
///                  holds actual row pointers, chained for range scans
#[derive(Debug, Clone)]
pub enum BTreeNode {
    Internal(InternalNode),
    Leaf(LeafNode),
}

#[derive(Debug, Clone)]
pub struct InternalNode {
    pub keys: Vec<i64>,         // separator keys
    pub children: Vec<u32>,     // child page IDs (always keys.len() + 1)
}

#[derive(Debug, Clone)]
pub struct LeafNode {
    pub keys: Vec<i64>,
    pub rids: Vec<Rid>,
    pub next_leaf: Option<u32>, // page_id of right sibling (for range scans)
}

impl InternalNode {
    pub fn new() -> Self {
        Self { keys: Vec::new(), children: Vec::new() }
    }

    pub fn is_full(&self) -> bool {
        self.keys.len() >= 2 * ORDER
    }

    /// Find which child to follow for a given search key.
    pub fn find_child_index(&self, key: i64) -> usize {
        self.keys.partition_point(|&k| k <= key)
    }
}

impl LeafNode {
    pub fn new() -> Self {
        Self { keys: Vec::new(), rids: Vec::new(), next_leaf: None }
    }

    pub fn is_full(&self) -> bool {
        self.keys.len() >= 2 * ORDER
    }

    pub fn find_key_index(&self, key: i64) -> Option<usize> {
        self.keys.iter().position(|&k| k == key)
    }
}

impl BTreeNode {
    /// Serialize a node into PAGE_SIZE bytes for storage on disk.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; PAGE_SIZE];
        match self {
            BTreeNode::Internal(node) => {
                buf[0] = 0; // type marker: 0 = internal
                let n = node.keys.len() as u32;
                buf[1..5].copy_from_slice(&n.to_le_bytes());

                let mut off = 5;
                for &child in &node.children {
                    buf[off..off+4].copy_from_slice(&child.to_le_bytes());
                    off += 4;
                }
                for &key in &node.keys {
                    buf[off..off+8].copy_from_slice(&key.to_le_bytes());
                    off += 8;
                }
            }
            BTreeNode::Leaf(node) => {
                buf[0] = 1; // type marker: 1 = leaf
                let n = node.keys.len() as u32;
                buf[1..5].copy_from_slice(&n.to_le_bytes());

                // next_leaf pointer
                let next = node.next_leaf.unwrap_or(u32::MAX);
                buf[5..9].copy_from_slice(&next.to_le_bytes());

                let mut off = 9;
                for i in 0..node.keys.len() {
                    buf[off..off+8].copy_from_slice(&node.keys[i].to_le_bytes());
                    off += 8;
                    buf[off..off+4].copy_from_slice(&node.rids[i].page_id.to_le_bytes());
                    off += 4;
                    buf[off..off+2].copy_from_slice(&node.rids[i].slot_id.to_le_bytes());
                    off += 2;
                }
            }
        }
        buf
    }

    /// Deserialize a node from raw page bytes.
    pub fn from_bytes(data: &[u8]) -> Self {
        let node_type = data[0];
        let n = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;

        if node_type == 0 {
            // Internal node
            let mut children = Vec::with_capacity(n + 1);
            let mut off = 5;
            for _ in 0..=n {
                children.push(u32::from_le_bytes(data[off..off+4].try_into().unwrap()));
                off += 4;
            }
            let mut keys = Vec::with_capacity(n);
            for _ in 0..n {
                keys.push(i64::from_le_bytes(data[off..off+8].try_into().unwrap()));
                off += 8;
            }
            BTreeNode::Internal(InternalNode { keys, children })
        } else {
            // Leaf node
            let next = u32::from_le_bytes(data[5..9].try_into().unwrap());
            let next_leaf = if next == u32::MAX { None } else { Some(next) };

            let mut off = 9;
            let mut keys = Vec::with_capacity(n);
            let mut rids = Vec::with_capacity(n);
            for _ in 0..n {
                let key = i64::from_le_bytes(data[off..off+8].try_into().unwrap());
                off += 8;
                let page_id = u32::from_le_bytes(data[off..off+4].try_into().unwrap());
                off += 4;
                let slot_id = u16::from_le_bytes(data[off..off+2].try_into().unwrap());
                off += 2;
                keys.push(key);
                rids.push(Rid { page_id, slot_id });
            }
            BTreeNode::Leaf(LeafNode { keys, rids, next_leaf })
        }
    }
}
