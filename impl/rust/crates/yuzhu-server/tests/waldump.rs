//! `yuzhu-waldump` を、initdb したデータディレクトリに対して実行する。

use std::path::PathBuf;
use std::process::{Command, Output};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("yuzhu-{name}-{}-{nanos}", std::process::id()))
}

fn initdb(dir: &PathBuf, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yuzhu-initdb"))
        .arg("-D")
        .arg(dir)
        .arg("--no-sync")
        .args(extra)
        .output()
        .expect("run yuzhu-initdb")
}

fn waldump(dir: &PathBuf, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yuzhu-waldump"))
        .arg("-D")
        .arg(dir)
        .args(extra)
        .output()
        .expect("run yuzhu-waldump")
}

#[test]
fn dumps_the_records_written_by_initdb() {
    let dir = temp_dir("waldump");
    let out = initdb(&dir, &["--wal-segment-size", "2"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = waldump(&dir, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("rmgr: Xlog"), "{text}");
    assert!(text.contains("CHECKPOINT_SHUTDOWN"), "{text}");
    assert!(text.contains("rmgr: Heap"), "{text}");
    assert!(text.contains("lsn: 0/"), "{text}");
    assert!(
        text.lines().last().unwrap().starts_with("end of WAL at 0/"),
        "{text}"
    );

    // --stats prints per-rmgr totals instead of records.
    let out = waldump(&dir, &["--stats"]);
    let stats = String::from_utf8(out.stdout).unwrap();
    assert!(
        stats.contains("Heap") && stats.contains("records:"),
        "{stats}"
    );
    assert!(!stats.contains("lsn:"), "{stats}");

    // --end stops before the given LSN.
    let out = waldump(&dir, &["--end", "0/0"]);
    let none = String::from_utf8(out.stdout).unwrap();
    assert!(none.starts_with("stopped at --end 0/00000000"), "{none}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reports_errors() {
    let dir = temp_dir("waldump-missing");
    let out = waldump(&dir, &[]);
    assert!(!out.status.success());

    let dir = temp_dir("waldump-badlsn");
    assert!(initdb(&dir, &[]).status.success());
    let out = waldump(&dir, &["--start", "nonsense"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid LSN"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn initdb_validates_the_wal_segment_size() {
    for bad in ["0", "1", "3", "1025", "abc"] {
        let dir = temp_dir("initdb-seg");
        let out = initdb(&dir, &["--wal-segment-size", bad]);
        assert!(!out.status.success(), "{bad}");
        assert!(!dir.exists(), "{bad}: nothing must be created");
    }
    let dir = temp_dir("initdb-seg-ok");
    let out = initdb(&dir, &["--wal-segment-size", "4"]);
    assert!(out.status.success());
    let wal = std::fs::read_dir(dir.join("pg_wal")).map(Iterator::count);
    assert!(wal.is_ok_and(|n| n >= 1));
    let _ = std::fs::remove_dir_all(&dir);
}
