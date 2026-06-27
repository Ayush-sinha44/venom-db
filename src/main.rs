mod storage;

use storage::disk_manager::DiskManager;
use storage::page::Page;

fn main() -> std::io::Result<()> {
    let mut disk = DiskManager::new("database.db")?;

    // --- Write some pages ---
    println!("=== Writing pages ===");

    let page_id = disk.allocate_page();
    let mut page = Page::new(page_id);

    // Insert some tuples (raw bytes — we'll add schema in a later phase)
    let rows = vec![
        b"Ayush,21,CSE".as_ref(),
        b"Faism,22,ECE".as_ref(),
        b"Alice,20,EEE".as_ref(),
    ];

    for row in &rows {
        match page.insert_tuple(row) {
            Some(slot_id) => println!("Inserted into slot {}: {:?}", slot_id, std::str::from_utf8(row).unwrap()),
            None => println!("Page full!"),
        }
    }

    println!("Free space remaining: {} bytes", page.free_space());
    disk.write_page(&mut page)?;
    println!("Page {} written to disk.\n", page_id);

    // --- Read back from disk ---
    println!("=== Reading page back from disk ===");
    let loaded = disk.read_page(page_id)?;
    println!("Checksum verified ✓");
    println!("Slots on page: {}", loaded.num_slots());

    for slot_id in 0..loaded.num_slots() {
        match loaded.get_tuple(slot_id) {
            Some(data) => println!("Slot {}: {}", slot_id, std::str::from_utf8(data).unwrap()),
            None => println!("Slot {}: (deleted)", slot_id),
        }
    }

    // --- Test delete ---
    println!("\n=== Testing delete ===");
    let mut page2 = disk.read_page(page_id)?;
    page2.delete_tuple(1); // delete "Faism"
    disk.write_page(&mut page2)?;

    let page3 = disk.read_page(page_id)?;
    for slot_id in 0..page3.num_slots() {
        match page3.get_tuple(slot_id) {
            Some(data) => println!("Slot {}: {}", slot_id, std::str::from_utf8(data).unwrap()),
            None => println!("Slot {}: (deleted)", slot_id),
        }
    }

    // --- Stress test: fill a page ---
    println!("\n=== Stress test: filling a page ===");
    let pid2 = disk.allocate_page();
    let mut big_page = Page::new(pid2);
    let mut count = 0u32;

    loop {
        let row = format!("row_{:08}", count);
        match big_page.insert_tuple(row.as_bytes()) {
            Some(_) => count += 1,
            None => break,
        }
    }

    disk.write_page(&mut big_page)?;
    println!("Inserted {} rows before page was full", count);
    println!("Free space after fill: {} bytes", big_page.free_space());

    Ok(())
}
