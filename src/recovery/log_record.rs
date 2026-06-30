/// Every operation that touches data gets a log record written BEFORE
/// the actual page is modified. This is the Write-Ahead Log guarantee.
///
/// LSN = Log Sequence Number. Monotonically increasing u64.
/// Every record has one. Pages store the LSN of the last record
/// that modified them — used during recovery to avoid replaying
/// records that are already reflected on disk.

#[derive(Debug, Clone, PartialEq)]
pub enum LogRecord {
    /// Transaction started
    Begin { lsn: u64, txn_id: u32 },

    /// A tuple was inserted
    Insert {
        lsn: u64,
        txn_id: u32,
        page_id: u32,
        slot_id: u16,
        data: Vec<u8>, // the tuple bytes (for REDO)
    },

    /// A tuple was deleted
    Delete {
        lsn: u64,
        txn_id: u32,
        page_id: u32,
        slot_id: u16,
        old_data: Vec<u8>, // the tuple bytes before deletion (for UNDO)
    },

    /// A tuple was updated
    Update {
        lsn: u64,
        txn_id: u32,
        page_id: u32,
        slot_id: u16,
        old_data: Vec<u8>, // before (for UNDO)
        new_data: Vec<u8>, // after  (for REDO)
    },

    /// Transaction committed — durable after this is fsynced
    Commit { lsn: u64, txn_id: u32 },

    /// Transaction aborted — all its changes must be undone
    Abort { lsn: u64, txn_id: u32 },

    /// Checkpoint: all pages up to this point are safely on disk.
    /// Recovery can start from here instead of the beginning of the log.
    Checkpoint { lsn: u64 },
}

impl LogRecord {
    pub fn lsn(&self) -> u64 {
        match self {
            Self::Begin { lsn, .. } => *lsn,
            Self::Insert { lsn, .. } => *lsn,
            Self::Delete { lsn, .. } => *lsn,
            Self::Update { lsn, .. } => *lsn,
            Self::Commit { lsn, .. } => *lsn,
            Self::Abort { lsn, .. } => *lsn,
            Self::Checkpoint { lsn, .. } => *lsn,
        }
    }

    pub fn txn_id(&self) -> Option<u32> {
        match self {
            Self::Begin { txn_id, .. } => Some(*txn_id),
            Self::Insert { txn_id, .. } => Some(*txn_id),
            Self::Delete { txn_id, .. } => Some(*txn_id),
            Self::Update { txn_id, .. } => Some(*txn_id),
            Self::Commit { txn_id, .. } => Some(*txn_id),
            Self::Abort { txn_id, .. } => Some(*txn_id),
            Self::Checkpoint { .. } => None,
        }
    }

    // --- Serialization ---
    // Format: [type: u8][lsn: u8x8][payload...]
    // Each variant has a unique type byte.

    const TYPE_BEGIN: u8 = 0;
    const TYPE_INSERT: u8 = 1;
    const TYPE_DELETE: u8 = 2;
    const TYPE_UPDATE: u8 = 3;
    const TYPE_COMMIT: u8 = 4;
    const TYPE_ABORT: u8 = 5;
    const TYPE_CHECKPOINT: u8 = 6;

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        match self {
            Self::Begin { lsn, txn_id } => {
                buf.push(Self::TYPE_BEGIN);
                buf.extend(&lsn.to_le_bytes());
                buf.extend(&txn_id.to_le_bytes());
            }
            Self::Insert {
                lsn,
                txn_id,
                page_id,
                slot_id,
                data,
            } => {
                buf.push(Self::TYPE_INSERT);
                buf.extend(&lsn.to_le_bytes());
                buf.extend(&txn_id.to_le_bytes());
                buf.extend(&page_id.to_le_bytes());
                buf.extend(&slot_id.to_le_bytes());
                buf.extend(&(data.len() as u32).to_le_bytes());
                buf.extend(data);
            }
            Self::Delete {
                lsn,
                txn_id,
                page_id,
                slot_id,
                old_data,
            } => {
                buf.push(Self::TYPE_DELETE);
                buf.extend(&lsn.to_le_bytes());
                buf.extend(&txn_id.to_le_bytes());
                buf.extend(&page_id.to_le_bytes());
                buf.extend(&slot_id.to_le_bytes());
                buf.extend(&(old_data.len() as u32).to_le_bytes());
                buf.extend(old_data);
            }
            Self::Update {
                lsn,
                txn_id,
                page_id,
                slot_id,
                old_data,
                new_data,
            } => {
                buf.push(Self::TYPE_UPDATE);
                buf.extend(&lsn.to_le_bytes());
                buf.extend(&txn_id.to_le_bytes());
                buf.extend(&page_id.to_le_bytes());
                buf.extend(&slot_id.to_le_bytes());
                buf.extend(&(old_data.len() as u32).to_le_bytes());
                buf.extend(old_data);
                buf.extend(&(new_data.len() as u32).to_le_bytes());
                buf.extend(new_data);
            }
            Self::Commit { lsn, txn_id } => {
                buf.push(Self::TYPE_COMMIT);
                buf.extend(&lsn.to_le_bytes());
                buf.extend(&txn_id.to_le_bytes());
            }
            Self::Abort { lsn, txn_id } => {
                buf.push(Self::TYPE_ABORT);
                buf.extend(&lsn.to_le_bytes());
                buf.extend(&txn_id.to_le_bytes());
            }
            Self::Checkpoint { lsn } => {
                buf.push(Self::TYPE_CHECKPOINT);
                buf.extend(&lsn.to_le_bytes());
            }
        }

        // Prepend total record length so we can read records back one at a time
        let mut framed = (buf.len() as u32).to_le_bytes().to_vec();
        framed.extend(buf);
        framed
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.is_empty() {
            return None;
        }
        let record_type = buf[0];
        let mut off = 1;

        macro_rules! read_u16 {
            () => {{
                let v = u16::from_le_bytes(buf[off..off + 2].try_into().ok()?);
                #[allow(unused_assignments)]
                {
                    off += 2;
                }
                v
            }};
        }
        macro_rules! read_u32 {
            () => {{
                let v = u32::from_le_bytes(buf[off..off + 4].try_into().ok()?);
                #[allow(unused_assignments)]
                {
                    off += 4;
                }
                v
            }};
        }
        macro_rules! read_u64 {
            () => {{
                let v = u64::from_le_bytes(buf[off..off + 8].try_into().ok()?);
                #[allow(unused_assignments)]
                {
                    off += 8;
                }
                v
            }};
        }
        macro_rules! read_bytes {
            () => {{
                let len = read_u32!() as usize;
                let v = buf[off..off + len].to_vec();
                #[allow(unused_assignments)]
                {
                    off += len;
                }
                v
            }};
        }

        Some(match record_type {
            Self::TYPE_BEGIN => {
                let lsn = read_u64!();
                let txn_id = read_u32!();
                Self::Begin { lsn, txn_id }
            }
            Self::TYPE_INSERT => {
                let lsn = read_u64!();
                let txn_id = read_u32!();
                let page_id = read_u32!();
                let slot_id = read_u16!();
                let data = read_bytes!();
                Self::Insert {
                    lsn,
                    txn_id,
                    page_id,
                    slot_id,
                    data,
                }
            }
            Self::TYPE_DELETE => {
                let lsn = read_u64!();
                let txn_id = read_u32!();
                let page_id = read_u32!();
                let slot_id = read_u16!();
                let old_data = read_bytes!();
                Self::Delete {
                    lsn,
                    txn_id,
                    page_id,
                    slot_id,
                    old_data,
                }
            }
            Self::TYPE_UPDATE => {
                let lsn = read_u64!();
                let txn_id = read_u32!();
                let page_id = read_u32!();
                let slot_id = read_u16!();
                let old_data = read_bytes!();
                let new_data = read_bytes!();
                Self::Update {
                    lsn,
                    txn_id,
                    page_id,
                    slot_id,
                    old_data,
                    new_data,
                }
            }
            Self::TYPE_COMMIT => {
                let lsn = read_u64!();
                let txn_id = read_u32!();
                Self::Commit { lsn, txn_id }
            }
            Self::TYPE_ABORT => {
                let lsn = read_u64!();
                let txn_id = read_u32!();
                Self::Abort { lsn, txn_id }
            }
            Self::TYPE_CHECKPOINT => {
                let lsn = read_u64!();
                Self::Checkpoint { lsn }
            }
            _ => return None,
        })
    }
}
