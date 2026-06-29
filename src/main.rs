mod storage;
mod buffer;
mod index;
mod recovery;
mod sql;
mod engine;
mod concurrency;

use std::sync::{Arc, Mutex};
use std::thread;

use concurrency::lock_manager::{LockManager, LockMode};
use concurrency::transaction::{Rid as TxnRid};
use sql::lexer::Lexer;
use sql::parser::Parser;
use engine::executor::{Executor, Row};
use std::io::{self, Write};

// ── REPL helpers ─────────────────────────────────────────────────────────────

fn run(exec: &mut Executor, sql: &str) -> Result<Vec<Row>, String> {
    let trimmed = sql.trim().trim_end_matches(';');
    if trimmed.is_empty() { return Ok(vec![]); }
    let mut lexer = Lexer::new(trimmed);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens);
    let stmt = parser.parse()?;
    exec.execute(stmt)
}

fn print_results(rows: &[Row], exec: &Executor, table: Option<&str>) {
    if rows.is_empty() { println!("(no rows)"); return; }
    let headers: Option<Vec<String>> = table.and_then(|t| {
        exec.catalog.get(t).map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
    });
    if let Some(ref cols) = headers {
        let widths: Vec<usize> = cols.iter().enumerate().map(|(i, col)| {
            let max_val = rows.iter()
                .map(|r| r.get(i).map(|v| v.to_string().len()).unwrap_or(0))
                .max().unwrap_or(0);
            col.len().max(max_val)
        }).collect();
        let header: Vec<String> = cols.iter().enumerate()
            .map(|(i, c)| format!("{:width$}", c, width = widths[i])).collect();
        println!(" {} ", header.join(" │ "));
        let div: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
        println!("─{}─", div.join("─┼─"));
        for row in rows {
            let cells: Vec<String> = row.iter().enumerate()
                .map(|(i, v)| format!("{:width$}", v.to_string(), width = widths.get(i).copied().unwrap_or(0)))
                .collect();
            println!(" {} ", cells.join(" │ "));
        }
    } else {
        for row in rows {
            let cells: Vec<String> = row.iter().map(|v| v.to_string()).collect();
            println!(" {} ", cells.join(" │ "));
        }
    }
    println!("({} row{})", rows.len(), if rows.len() == 1 { "" } else { "s" });
}

fn extract_table(sql: &str) -> Option<&str> {
    let upper = sql.to_uppercase();
    for kw in &["FROM ", "INTO ", "TABLE "] {
        if let Some(pos) = upper.find(kw) {
            let rest = &sql[pos + kw.len()..];
            let end = rest.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(rest.len());
            return Some(&rest[..end]);
        }
    }
    None
}

fn print_help() {
    println!("\n  venom-db SQL shell  (v0.7 — Persistent)");
    println!("  ──────────────────────────────────────────────");
    println!("  SQL:  CREATE TABLE t (col TYPE, ...)");
    println!("        INSERT INTO t VALUES (v1, v2, ...)");
    println!("        SELECT [* | col,...] FROM t [WHERE col op val]");
    println!("        DELETE FROM t [WHERE col op val]");
    println!("        BEGIN / COMMIT / ROLLBACK");
    println!("  Ops:  =  !=  <  >  <=  >=");
    println!("  Meta: \\tables  \\stats  \\help  \\quit");
    println!("  Data: stored in ./venom-data/ (survives restart)\n");
}

fn looks_complete(line: &str) -> bool {
    let u = line.trim().to_uppercase();
    u.starts_with("SELECT") || u.starts_with("INSERT") ||
    u.starts_with("DELETE") || u.starts_with("CREATE") ||
    u.starts_with("BEGIN")  || u.starts_with("COMMIT") ||
    u.starts_with("ROLLBACK")
}

// ── Concurrency demo ─────────────────────────────────────────────────────────

fn run_concurrency_demo() {
    println!("\n━━━ Concurrency Demo ━━━\n");
    let lm = Arc::new(Mutex::new(LockManager::new()));
    let lm1 = Arc::clone(&lm);
    let lm2 = Arc::clone(&lm);
    let row = TxnRid { page_id: 0, slot_id: 0 };

    let t1 = thread::spawn(move || {
        let mut lm = lm1.lock().unwrap();
        match lm.acquire(1, row, LockMode::Exclusive) {
            Ok(true)  => println!("  txn 1: acquired EXCLUSIVE lock on row (0,0) ✓"),
            Ok(false) => println!("  txn 1: waiting for lock on row (0,0)..."),
            Err(e)    => println!("  txn 1: deadlock → {}", e),
        }
        drop(lm);
        thread::sleep(std::time::Duration::from_millis(10));
        let mut lm = lm1.lock().unwrap();
        lm.release_all(1);
        println!("  txn 1: committed, released all locks");
    });

    thread::sleep(std::time::Duration::from_millis(2));

    let t2 = thread::spawn(move || {
        let mut lm = lm2.lock().unwrap();
        match lm.acquire(2, row, LockMode::Shared) {
            Ok(true)  => println!("  txn 2: acquired SHARED lock on row (0,0) ✓"),
            Ok(false) => println!("  txn 2: blocked — row locked by txn 1, queued"),
            Err(e)    => println!("  txn 2: deadlock → {}", e),
        }
    });

    t1.join().unwrap();
    t2.join().unwrap();

    println!("\n  --- Deadlock Detection ---");
    let mut lm = LockManager::new();
    let row_a = TxnRid { page_id: 0, slot_id: 0 };
    let row_b = TxnRid { page_id: 0, slot_id: 1 };
    lm.acquire(1, row_a, LockMode::Exclusive).unwrap();
    println!("  txn 1: locked row A");
    lm.acquire(2, row_b, LockMode::Exclusive).unwrap();
    println!("  txn 2: locked row B");
    lm.acquire(1, row_b, LockMode::Exclusive).unwrap();
    println!("  txn 1: waiting for row B (held by txn 2)...");
    match lm.acquire(2, row_a, LockMode::Exclusive) {
        Err(e) => println!("  txn 2: {} → abort txn 2 ✓", e),
        Ok(_)  => println!("  (no deadlock detected — unexpected)"),
    }
    lm.release_all(1);
    println!("  txn 1: completed after txn 2 aborted");
}

// ── main ─────────────────────────────────────────────────────────────────────

fn main() {
    let data_dir = "./venom-data";

    println!("╔══════════════════════════════════════════╗");
    println!("║           venom-db  v0.7                 ║");
    println!("║   Persistent Storage + WAL Recovery      ║");
    println!("║   type \\help for usage, \\quit to exit    ║");
    println!("╚══════════════════════════════════════════╝");

    // Open database — creates data dir if it doesn't exist
    let mut exec = match Executor::open(data_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to open database: {}", e);
            std::process::exit(1);
        }
    };

    // Run WAL recovery before accepting queries
    match exec.recover() {
        Ok(info) => {
            if info.tables_loaded > 0 || info.redone > 0 {
                println!("\n  Recovery complete:");
                println!("    tables loaded : {}", info.tables_loaded);
                println!("    WAL records redone : {}", info.redone);
                println!("    WAL records undone : {}", info.undone);
            } else {
                println!("\n  Fresh database — no data to recover.");
            }
        }
        Err(e) => {
            eprintln!("Recovery failed: {}", e);
            std::process::exit(1);
        }
    }

    run_concurrency_demo();

    println!("\n━━━ Interactive SQL Shell ━━━");
    println!("  Data directory: {}", data_dir);
    println!("  All changes are durable — data survives \\quit and restart.\n");

    let mut lm = LockManager::new();
    let mut current_txn: Option<u32> = None;
    let mut next_txn_id: u32 = 1;
    let mut input_buf = String::new();

    loop {
        if input_buf.trim().is_empty() {
            if let Some(id) = current_txn {
                print!("venom-db [txn {}]> ", id);
            } else {
                print!("venom-db> ");
            }
        } else {
            print!("       -> ");
        }
        io::stdout().flush().unwrap();

        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => { eprintln!("read error: {}", e); break; }
        }

        let trimmed = line.trim();

        match trimmed.to_uppercase().as_str() {
            "\\QUIT" | "\\Q" | "EXIT" | "QUIT" => { println!("bye."); break; }
            "\\HELP" | "\\H" => { print_help(); continue; }
            "\\TABLES" => {
                let tables: Vec<String> = exec.catalog.tables.keys().cloned().collect();
                if tables.is_empty() { println!("  (no tables)"); }
                else { for t in tables { println!("  {}", t); } }
                continue;
            }
            "\\STATS" => {
                println!("  buffer pool hit rate: {:.1}%", exec.hit_rate());
                continue;
            }
            _ => {}
        }

        // Transaction control
        let upper = trimmed.to_uppercase();
        if upper == "BEGIN" || upper == "BEGIN;" {
            if current_txn.is_some() {
                println!("  already in a transaction — COMMIT or ROLLBACK first");
            } else {
                current_txn = Some(next_txn_id);
                next_txn_id += 1;
                println!("  transaction {} started", current_txn.unwrap());
            }
            continue;
        }
        if upper == "COMMIT" || upper == "COMMIT;" {
            if let Some(id) = current_txn.take() {
                lm.release_all(id);
                println!("  transaction {} committed", id);
            } else {
                println!("  no active transaction");
            }
            continue;
        }
        if upper == "ROLLBACK" || upper == "ROLLBACK;" {
            if let Some(id) = current_txn.take() {
                lm.release_all(id);
                println!("  transaction {} rolled back", id);
            } else {
                println!("  no active transaction");
            }
            continue;
        }

        input_buf.push_str(trimmed);
        input_buf.push(' ');

        if !trimmed.ends_with(';') && !looks_complete(trimmed) {
            continue;
        }

        let sql = input_buf.trim().trim_end_matches(';').trim().to_string();
        input_buf.clear();
        if sql.is_empty() { continue; }

        let table_hint = extract_table(&sql).map(|s| s.to_string());

        // Acquire lock if in explicit transaction
        if let Some(txn_id) = current_txn {
            let is_write = sql.to_uppercase().starts_with("INSERT")
                || sql.to_uppercase().starts_with("DELETE");
            let mode = if is_write { LockMode::Exclusive } else { LockMode::Shared };
            let table_rid = TxnRid { page_id: u32::MAX, slot_id: 0 };
            match lm.acquire(txn_id, table_rid, mode) {
                Ok(true)  => {}
                Ok(false) => { println!("  waiting for lock..."); }
                Err(e)    => { println!("  {}", e); continue; }
            }
        }

        match run(&mut exec, &sql) {
            Ok(rows) => {
                if rows.is_empty() {
                    let u = sql.to_uppercase();
                    if u.starts_with("CREATE")      { println!("  Table created."); }
                    else if u.starts_with("INSERT") { println!("  1 row inserted."); }
                    else if u.starts_with("DELETE") { println!("  Rows deleted."); }
                } else {
                    println!();
                    print_results(&rows, &exec, table_hint.as_deref());
                }
            }
            Err(e) => eprintln!("  Error: {}", e),
        }
    }
}
