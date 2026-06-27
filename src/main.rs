
mod storage;
mod buffer;
mod index;
mod recovery;

use recovery::wal::WalManager;

fn main() -> std::io::Result<()> {
    println!("╔══════════════════════════════════════╗");
    println!("║         venom-db  phase 4            ║");
    println!("║    Write-Ahead Log + Recovery        ║");
    println!("╚══════════════════════════════════════╝\n");

    // ─── Demo 1 + 2 + 3: single log showing commit then crash ────────
    let log_path = "venom.log";
    let _ = std::fs::remove_file(log_path);

    println!("━━━ Demo 1: Commit then crash in same log ━━━");
    {
        let mut wal = WalManager::new(log_path)?;

        // txn 1 commits
        let t1 = wal.begin_txn()?;
        println!("txn {} started", t1);
        let slot = wal.log_insert(t1, 0, b"Ayush|CSE|21")?;
        println!("inserted 'Ayush|CSE|21' → page=0 slot={}", slot);
        wal.commit(t1)?;
        println!("txn {} committed + fsynced", t1);

        // txn 2 crashes
        let t2 = wal.begin_txn()?;
        println!("\ntxn {} started", t2);
        wal.log_insert(t2, 1, b"ghost row - should vanish")?;
        println!("inserted 'ghost row' → page=1");
        println!("💥 process dies — no commit written\n");
        // no commit
    }

    println!("━━━ Demo 2: Recovery on restart ━━━");
    {
        let mut wal = WalManager::new(log_path)?;
        let records = wal.read_all_records()?;
        println!("Log has {} records:", records.len());
        for r in &records {
            println!("  LSN {:2} │ {:?}", r.lsn(), r);
        }
        println!();
        let report = wal.recover()?;
        println!("Recovery complete:");
        println!("  REDO: {} operations replayed (committed)", report.redone);
        println!("  UNDO: {} operations rolled back (crashed)", report.undone);
        println!("  Incomplete txns: {:?}\n", report.incomplete_txns);

        match wal.pages.get(&0).and_then(|p| p.get_tuple(0)) {
            Some(d) => println!("  page=0 slot=0 → \"{}\" ✓ (committed)", std::str::from_utf8(d).unwrap()),
            None    => println!("  page=0 slot=0 → missing (bug!)"),
        }
        match wal.pages.get(&1).and_then(|p| p.get_tuple(0)) {
            Some(_) => println!("  page=1 slot=0 → still exists (bug!)"),
            None    => println!("  page=1 slot=0 → correctly absent ✓ (rolled back)"),
        }
    }
    let _ = std::fs::remove_file(log_path);

    // ─── Demo 3: Multiple txns mixed ─────────────────────────────────
    println!("\n━━━ Demo 3: Mixed transactions ━━━");
    let log2 = "venom2.log";
    let _ = std::fs::remove_file(log2);
    {
        let mut wal = WalManager::new(log2)?;
        let t1 = wal.begin_txn()?; wal.log_insert(t1, 0, b"Alice|committed")?; wal.commit(t1)?;
        println!("txn {} (Alice)   → committed", t1);
        let t2 = wal.begin_txn()?; wal.log_insert(t2, 0, b"Bob|committed")?;   wal.commit(t2)?;
        println!("txn {} (Bob)     → committed", t2);
        let t3 = wal.begin_txn()?; wal.log_insert(t3, 0, b"Charlie|CRASHED")?;
        println!("txn {} (Charlie) → 💥 crashed", t3);
    }
    {
        let mut wal = WalManager::new(log2)?;
        let report = wal.recover()?;
        println!("\nRecovery:");
        println!("  REDO {} (Alice + Bob)", report.redone);
        println!("  UNDO {} (Charlie)", report.undone);
        println!("  Incomplete: {:?}", report.incomplete_txns);
    }
    let _ = std::fs::remove_file(log2);

    println!("\n=== Phase 4 complete ===");
    println!("venom-db now survives crashes.");
    Ok(())
}
