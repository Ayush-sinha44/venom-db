use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use super::page::{Page, PAGE_SIZE};

pub struct DiskManager {
    file: File,
    num_pages: u32,
}

impl DiskManager {
    pub fn new(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        let num_pages = (file.metadata()?.len() / PAGE_SIZE as u64) as u32;

        Ok(Self { file, num_pages })
    }

    /// Allocate a new page ID (doesn't write anything yet)
    pub fn allocate_page(&mut self) -> u32 {
        let id = self.num_pages;
        self.num_pages += 1;
        id
    }

    /// Write a page to disk at the correct offset
    pub fn write_page(&mut self, page: &mut Page) -> std::io::Result<()> {
        page.finalize_checksum();
        let offset = page.id as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        self.file.flush()?;
        Ok(())
    }

    /// Read a page from disk by its ID
    pub fn read_page(&mut self, page_id: u32) -> std::io::Result<Page> {
        let offset = page_id as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;

        let mut data = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut data)?;

        let page = Page::from_bytes(page_id, data);

        if !page.verify_checksum() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Checksum mismatch on page {}", page_id),
            ));
        }

        Ok(page)
    }

    pub fn num_pages(&self) -> u32 {
        self.num_pages
    }
}
