use crc32fast::Hasher;

pub const PAGE_SIZE: usize = 4096;

// A SlotId identifies a tuple's position within a page
pub type SlotId = u16;

// Points to a specific tuple across the whole DB: (page, slot)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rid {
    pub page_id: u32,
    pub slot_id: SlotId,
}

// Slot entry: where in the page this tuple lives
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    pub offset: u16,  // byte offset from start of page
    pub length: u16,  // byte length of tuple
    pub active: bool, // false = deleted (tombstone)
}

// Page header: fixed metadata at the very start of the page
#[derive(Debug)]
pub struct PageHeader {
    pub page_id: u32,
    pub num_slots: u16,
    pub free_space_offset: u16, // next free byte from the END (tuples grow backwards)
    pub checksum: u32,
}

impl PageHeader {
    pub const SIZE: usize = 12; // 4 + 2 + 2 + 4 bytes

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.page_id.to_le_bytes());
        buf[4..6].copy_from_slice(&self.num_slots.to_le_bytes());
        buf[6..8].copy_from_slice(&self.free_space_offset.to_le_bytes());
        buf[8..12].copy_from_slice(&self.checksum.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Self {
        Self {
            page_id: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            num_slots: u16::from_le_bytes(buf[4..6].try_into().unwrap()),
            free_space_offset: u16::from_le_bytes(buf[6..8].try_into().unwrap()),
            checksum: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        }
    }
}

// Slot on-disk encoding: 5 bytes per slot
impl Slot {
    pub const SIZE: usize = 5;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..2].copy_from_slice(&self.offset.to_le_bytes());
        buf[2..4].copy_from_slice(&self.length.to_le_bytes());
        buf[4] = self.active as u8;
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Self {
        Self {
            offset: u16::from_le_bytes(buf[0..2].try_into().unwrap()),
            length: u16::from_le_bytes(buf[2..4].try_into().unwrap()),
            active: buf[4] != 0,
        }
    }
}

pub struct Page {
    pub id: u32,
    pub data: [u8; PAGE_SIZE],
    pub dirty: bool,
}

impl Page {
    pub fn new(id: u32) -> Self {
        let mut page = Self {
            id,
            data: [0u8; PAGE_SIZE],
            dirty: true,
        };
        // Initialize header
        let header = PageHeader {
            page_id: id,
            num_slots: 0,
            free_space_offset: PAGE_SIZE as u16, // starts at the end
            checksum: 0,
        };
        page.write_header(&header);
        page
    }

    pub fn from_bytes(id: u32, data: [u8; PAGE_SIZE]) -> Self {
        Self {
            id,
            data,
            dirty: false,
        }
    }

    // --- Header helpers ---

    fn read_header(&self) -> PageHeader {
        PageHeader::from_bytes(&self.data[..PageHeader::SIZE])
    }

    fn write_header(&mut self, header: &PageHeader) {
        self.data[..PageHeader::SIZE].copy_from_slice(&header.to_bytes());
    }

    // --- Slot helpers ---

    fn slot_offset(slot_id: SlotId) -> usize {
        PageHeader::SIZE + slot_id as usize * Slot::SIZE
    }

    fn read_slot(&self, slot_id: SlotId) -> Slot {
        let off = Self::slot_offset(slot_id);
        Slot::from_bytes(&self.data[off..off + Slot::SIZE])
    }

    fn write_slot(&mut self, slot_id: SlotId, slot: &Slot) {
        let off = Self::slot_offset(slot_id);
        self.data[off..off + Slot::SIZE].copy_from_slice(&slot.to_bytes());
    }

    // --- Public API ---

    /// Insert a tuple. Returns the SlotId on success, None if page is full.
    pub fn insert_tuple(&mut self, tuple: &[u8]) -> Option<SlotId> {
        let mut header = self.read_header();

        let tuple_len = tuple.len() as u16;
        let new_free_offset = header.free_space_offset.checked_sub(tuple_len)?;

        // Check there's enough space for the tuple + a new slot entry
        let slot_area_end = PageHeader::SIZE + (header.num_slots as usize + 1) * Slot::SIZE;
        if slot_area_end > new_free_offset as usize {
            return None; // page full
        }

        // Write tuple data at the back
        let tuple_start = new_free_offset as usize;
        self.data[tuple_start..tuple_start + tuple.len()].copy_from_slice(tuple);

        // Write new slot
        let slot_id = header.num_slots;
        self.write_slot(
            slot_id,
            &Slot {
                offset: new_free_offset,
                length: tuple_len,
                active: true,
            },
        );

        // Update header
        header.num_slots += 1;
        header.free_space_offset = new_free_offset;
        self.write_header(&header);

        self.dirty = true;
        Some(slot_id)
    }

    /// Read a tuple by slot ID. Returns None if deleted or invalid.
    pub fn get_tuple(&self, slot_id: SlotId) -> Option<&[u8]> {
        let header = self.read_header();
        if slot_id >= header.num_slots {
            return None;
        }
        let slot = self.read_slot(slot_id);
        if !slot.active {
            return None;
        }
        let start = slot.offset as usize;
        let end = start + slot.length as usize;
        Some(&self.data[start..end])
    }
    pub fn update_tuple(&mut self, slot_id: SlotId, new_data: &[u8]) -> bool {
        let header = self.read_header();
        if slot_id >= header.num_slots {
            return false;
        }
        let slot = self.read_slot(slot_id);
        if !slot.active {
            return false;
        }
        if new_data.len() > slot.length as usize {
            return false;
        }
        let start = slot.offset as usize;
        self.data[start..start + new_data.len()].copy_from_slice(new_data);
        self.dirty = true;
        true
    }

    /// Mark a tuple as deleted (tombstone — space not reclaimed until compaction)
    pub fn delete_tuple(&mut self, slot_id: SlotId) -> bool {
        let header = self.read_header();
        if slot_id >= header.num_slots {
            return false;
        }
        let mut slot = self.read_slot(slot_id);
        if !slot.active {
            return false;
        }
        slot.active = false;
        self.write_slot(slot_id, &slot);
        self.dirty = true;
        true
    }

    /// How many bytes of usable free space remain
    pub fn free_space(&self) -> usize {
        let header = self.read_header();
        let slot_area_end = PageHeader::SIZE + header.num_slots as usize * Slot::SIZE;
        (header.free_space_offset as usize).saturating_sub(slot_area_end)
    }

    pub fn num_slots(&self) -> u16 {
        self.read_header().num_slots
    }

    /// Compute and embed checksum into page data (call before writing to disk)
    pub fn finalize_checksum(&mut self) {
        let mut header = self.read_header();
        header.checksum = 0;
        self.write_header(&header);

        let mut h = Hasher::new();
        h.update(&self.data);
        header.checksum = h.finalize();
        self.write_header(&header);
    }

    /// Verify checksum after reading from disk
    pub fn verify_checksum(&self) -> bool {
        let stored = self.read_header().checksum;
        let mut temp = self.data;
        // zero out checksum field before computing
        temp[8..12].copy_from_slice(&[0u8; 4]);
        let mut h = Hasher::new();
        h.update(&temp);
        h.finalize() == stored
    }
}
