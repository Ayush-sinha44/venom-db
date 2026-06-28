use std::collections::HashSet;

/// Transaction state machine:
///   Active → Committed
///   Active → Aborted
#[derive(Debug, Clone, PartialEq)]
pub enum TxnState {
    Active,
    Committed,
    Aborted,
}

/// A Rid identifies a specific row: which page and which slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rid {
    pub page_id: u32,
    pub slot_id: u16,
}

/// A transaction tracks:
///   - its ID and state
///   - which locks it currently holds
///   - which rows it has modified (for UNDO on abort)
#[derive(Debug)]
pub struct Transaction {
    pub txn_id: u32,
    pub state: TxnState,
    pub shared_locks: HashSet<Rid>,     // rows we hold S-locks on
    pub exclusive_locks: HashSet<Rid>,  // rows we hold X-locks on
    pub undo_log: Vec<UndoEntry>,       // what to reverse on abort
}

/// One entry in the undo log — what a row looked like before we touched it
#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub rid: Rid,
    pub operation: UndoOp,
}

#[derive(Debug, Clone)]
pub enum UndoOp {
    Insert,                  // undo = delete this rid
    Delete { data: Vec<u8> }, // undo = re-insert this data
}

impl Transaction {
    pub fn new(txn_id: u32) -> Self {
        Self {
            txn_id,
            state: TxnState::Active,
            shared_locks: HashSet::new(),
            exclusive_locks: HashSet::new(),
            undo_log: Vec::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.state == TxnState::Active
    }

    pub fn commit(&mut self) {
        self.state = TxnState::Committed;
    }

    pub fn abort(&mut self) {
        self.state = TxnState::Aborted;
    }

    pub fn log_insert(&mut self, rid: Rid) {
        self.undo_log.push(UndoEntry {
            rid,
            operation: UndoOp::Insert,
        });
    }

    pub fn log_delete(&mut self, rid: Rid, data: Vec<u8>) {
        self.undo_log.push(UndoEntry {
            rid,
            operation: UndoOp::Delete { data },
        });
    }
}
