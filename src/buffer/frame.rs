use crate::storage::page::Page;

pub struct Frame {
    pub page: Option<Page>,
    pub pin_count: u32,
    pub is_dirty: bool,
}

impl Frame {
    pub fn new() -> Self {
        Self { page: None, pin_count: 0, is_dirty: false }
    }

    pub fn is_empty(&self) -> bool { self.page.is_none() }

    pub fn page_id(&self) -> Option<u32> {
        self.page.as_ref().map(|p| p.id)
    }
}
