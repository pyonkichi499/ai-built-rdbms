//! 層 2 のクラッシュ試験: 実プロセスの `kill -9`（`m3.md` §7.6）。
//!
//! 4〜8 本のクライアントが銀行振込と追記ログを流し、ランダムな時刻に
//! `SIGKILL` でサーバを止め、同じデータディレクトリで再起動して不変条件を
//! SQL で確かめる。
//!
//! - I1: `COMMIT` の応答を受け取ったトランザクションは、リカバリ後に必ず見える。
//! - I2/I3: 確認していないトランザクションは、全部見えるか全く見えないかのどちらか
//!   （残高の合計と、ログから導いた残高が一致する。ログは各クライアントの連番の接頭辞になる）。
//! - I10: 確認済みの `CREATE TABLE` + `INSERT` が残る。
//!
//! 長い試験は `#[ignore]`。`cargo test --test crash_kill9 -- --ignored` で流す。
//! 環境変数: `YUZHU_KILL9_ROUNDS`（既定 20、夜間 200）、`YUZHU_KILL9_SEED`（乱数の種）、
//! `YUZHU_KILL9_KEEP=1`（失敗しなくてもデータディレクトリを残す）。
//! `kill -9` ではページキャッシュが残るので、確かめられるのはプロセスのクラッシュだけ
//! （電源断は層 1 の役目）。

#![allow(clippy::print_stderr)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use postgres::{Client, NoTls, SimpleQueryMessage};

const ACCOUNTS: i32 = 8;
const INITIAL: i64 = 1000;
/// `filler` 表の 1 行の長さ（WAL の量を増やす）。
const FILLER_LEN: usize = 1500;

// ---------------------------------------------------------------------------
// 不変条件の検査（純粋関数。単体テストで変異を入れて、ハーネス自体を確かめる）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct LogRow {
    client: i32,
    round: i32,
    seq: i32,
    src: i32,
    dst: i32,
    amt: i64,
}

/// `(client, round)` ごとの、`COMMIT` の応答を受け取った件数。
type Confirmed = BTreeMap<(i32, i32), u32>;

/// 再起動後に SQL で読んだ内容。
#[derive(Debug, Default)]
struct Snapshot {
    accounts: Vec<(i32, i64)>,
    log: Vec<LogRow>,
    /// ラウンドごとの `ddl_<round>` の行数。表がなければ入れない。
    ddl_rows: BTreeMap<i32, usize>,
}

fn check(snap: &Snapshot, confirmed: &Confirmed, ddl_rounds: i32) -> Result<(), String> {
    // 口座は ACCOUNTS 行ちょうど。
    let mut ids: Vec<i32> = snap.accounts.iter().map(|a| a.0).collect();
    ids.sort_unstable();
    if ids != (0..ACCOUNTS).collect::<Vec<_>>() {
        return Err(format!("accounts rows are wrong: ids {ids:?}"));
    }
    // ログ: (client, round) ごとに連番が 0..k で、confirmed <= k <= confirmed + 1。
    let mut seqs: BTreeMap<(i32, i32), Vec<i32>> = BTreeMap::new();
    for r in &snap.log {
        seqs.entry((r.client, r.round)).or_default().push(r.seq);
    }
    for (key, v) in &mut seqs {
        v.sort_unstable();
        let k = u32::try_from(v.len()).map_err(|e| e.to_string())?;
        if v.iter()
            .copied()
            .ne(0..i32::try_from(k).map_err(|e| e.to_string())?)
        {
            return Err(format!(
                "log of client/round {key:?} is not a prefix 0..k (I2/I3): {v:?}"
            ));
        }
        let c = confirmed.get(key).copied().unwrap_or(0);
        if k > c + 1 {
            return Err(format!(
                "client/round {key:?} has {k} log rows but only {c} confirmed commits (I2)"
            ));
        }
    }
    for (key, &c) in confirmed {
        let k = seqs.get(key).map_or(0, Vec::len);
        if u64::try_from(k).unwrap_or(u64::MAX) < u64::from(c) {
            return Err(format!(
                "client/round {key:?}: {c} commits were confirmed but only {k} log rows survived (I1)"
            ));
        }
    }
    // 振込の原子性: 残高 = 初期値 - 出金 + 入金。
    let mut expected: BTreeMap<i32, i64> = (0..ACCOUNTS).map(|i| (i, INITIAL)).collect();
    for r in &snap.log {
        *expected.entry(r.src).or_default() -= r.amt;
        *expected.entry(r.dst).or_default() += r.amt;
    }
    for &(id, balance) in &snap.accounts {
        if expected[&id] != balance {
            return Err(format!(
                "account {id}: balance {balance} but the log implies {} (torn transaction, I2/I3)",
                expected[&id]
            ));
        }
    }
    let total: i64 = snap.accounts.iter().map(|a| a.1).sum();
    if total != INITIAL * i64::from(ACCOUNTS) {
        return Err(format!("total balance {total} changed"));
    }
    // DDL（I10）。
    for r in 0..ddl_rounds {
        match snap.ddl_rows.get(&r) {
            Some(1) => {}
            other => {
                return Err(format!(
                    "ddl_{r} should have exactly 1 row, found {other:?}"
                ));
            }
        }
    }
    Ok(())
}

/// I15（`m4/08` §7.5）: `serial` 列 `log.id` の払い出し。再起動後に `id` の重複がなく、
/// `COMMIT` を確認した `id` がすべて残り、`nextval` が（残っている行と確認済みの `id` の）最大より大きく、
/// 前回の再起動後の `nextval` より大きいこと（払い出しの巻き戻りがない）。
fn check_serial(
    log_ids: &[i64],
    confirmed: &BTreeSet<i64>,
    next: i64,
    prev_next: i64,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for &id in log_ids {
        if !seen.insert(id) {
            return Err(format!("log.id {id} was issued twice (I15)"));
        }
    }
    if let Some(missing) = confirmed.iter().find(|id| !seen.contains(id)) {
        return Err(format!(
            "log.id {missing} was confirmed by COMMIT but is missing after recovery (I1/I15)"
        ));
    }
    let max_seen = seen.iter().next_back().copied().unwrap_or(0);
    if next <= max_seen {
        return Err(format!(
            "nextval after recovery is {next}, not above the largest log.id {max_seen} (I15)"
        ));
    }
    if let Some(&max_confirmed) = confirmed.iter().next_back()
        && next <= max_confirmed
    {
        return Err(format!(
            "nextval after recovery is {next}, not above the largest confirmed id {max_confirmed} (I15)"
        ));
    }
    if next <= prev_next {
        return Err(format!(
            "nextval after recovery is {next}, not above the value {prev_next} issued after the previous recovery (I15)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod serial_check_tests {
    use super::*;

    #[test]
    fn healthy_serial_state_passes() {
        let confirmed = BTreeSet::from([1, 2, 4]);
        check_serial(&[1, 2, 3, 4], &confirmed, 40, 0).unwrap();
    }

    #[test]
    fn duplicate_id_is_detected() {
        let e = check_serial(&[1, 2, 2], &BTreeSet::new(), 40, 0).unwrap_err();
        assert!(e.contains("twice"), "{e}");
    }

    #[test]
    fn lost_confirmed_id_is_detected() {
        let e = check_serial(&[1, 2], &BTreeSet::from([1, 3]), 40, 0).unwrap_err();
        assert!(e.contains("missing"), "{e}");
    }

    #[test]
    fn nextval_not_above_the_maximum_is_detected() {
        let e = check_serial(&[1, 5], &BTreeSet::new(), 5, 0).unwrap_err();
        assert!(e.contains("largest log.id"), "{e}");
        // 確認済みの id が行として残っていても、残っていなくても、nextval は超えていなければならない。
        let e = check_serial(&[1, 2], &BTreeSet::from([2]), 2, 0).unwrap_err();
        assert!(e.contains("largest"), "{e}");
    }

    #[test]
    fn nextval_going_backwards_across_recoveries_is_detected() {
        let e = check_serial(&[1, 2], &BTreeSet::new(), 40, 40).unwrap_err();
        assert!(e.contains("previous recovery"), "{e}");
        check_serial(&[1, 2], &BTreeSet::new(), 41, 40).unwrap();
    }
}
#[cfg(test)]
mod check_tests {
    use super::*;

    fn row(client: i32, seq: i32, src: i32, dst: i32, amt: i64) -> LogRow {
        LogRow {
            client,
            round: 0,
            seq,
            src,
            dst,
            amt,
        }
    }

    fn healthy() -> (Snapshot, Confirmed) {
        let mut accounts: Vec<(i32, i64)> = (0..ACCOUNTS).map(|i| (i, INITIAL)).collect();
        accounts[0].1 -= 5;
        accounts[1].1 += 5;
        accounts[1].1 -= 7;
        accounts[2].1 += 7;
        let snap = Snapshot {
            accounts,
            log: vec![row(0, 0, 0, 1, 5), row(0, 1, 1, 2, 7)],
            ddl_rows: BTreeMap::from([(0, 1)]),
        };
        (snap, Confirmed::from([((0, 0), 2)]))
    }

    #[test]
    fn healthy_state_passes() {
        let (s, c) = healthy();
        check(&s, &c, 1).unwrap();
    }

    #[test]
    fn unconfirmed_but_committed_transaction_passes() {
        let (s, mut c) = healthy();
        c.insert((0, 0), 1);
        check(&s, &c, 1).unwrap();
    }

    #[test]
    fn lost_confirmed_commit_is_detected() {
        let (s, mut c) = healthy();
        c.insert((0, 0), 3);
        assert!(check(&s, &c, 1).unwrap_err().contains("I1"));
        c.insert((0, 0), 2);
        c.insert((9, 0), 1);
        assert!(check(&s, &c, 1).unwrap_err().contains("I1"));
    }

    #[test]
    fn torn_transfer_is_detected() {
        let (mut s, c) = healthy();
        s.accounts[2].1 -= 7; // 入金だけ消えた
        assert!(check(&s, &c, 1).unwrap_err().contains("torn"));
    }

    #[test]
    fn log_with_a_gap_or_duplicate_is_detected() {
        let (mut s, c) = healthy();
        s.log[1].seq = 2;
        assert!(check(&s, &c, 1).unwrap_err().contains("prefix"));
        let (mut s, c) = healthy();
        s.log[1].seq = 0;
        assert!(check(&s, &c, 1).unwrap_err().contains("prefix"));
    }

    #[test]
    fn rows_beyond_the_one_in_flight_are_detected() {
        let (s, mut c) = healthy();
        c.insert((0, 0), 0);
        assert!(check(&s, &c, 1).unwrap_err().contains("only 0 confirmed"));
    }

    #[test]
    fn missing_or_duplicated_ddl_is_detected() {
        let (mut s, c) = healthy();
        assert!(check(&s, &c, 2).unwrap_err().contains("ddl_1"));
        s.ddl_rows.insert(1, 2);
        assert!(check(&s, &c, 2).unwrap_err().contains("ddl_1"));
    }

    #[test]
    fn wrong_account_rows_are_detected() {
        let (mut s, c) = healthy();
        s.accounts.pop();
        assert!(check(&s, &c, 1).unwrap_err().contains("accounts"));
    }
}

// ---------------------------------------------------------------------------
// プロセスの管理
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// `lo..hi` の一様に近い乱数。
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo)
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

fn free_port() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind an ephemeral port");
    l.local_addr().expect("local addr").port()
}

struct Harness {
    dir: PathBuf,
    port: u16,
    child: Option<Child>,
    restarts: u32,
}

impl Harness {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("yuzhu-kill9-{}-{nanos}", std::process::id()));
        let out = Command::new(env!("CARGO_BIN_EXE_yuzhu-initdb"))
            .arg("-D")
            .arg(&dir)
            .args(["--no-sync", "--wal-segment-size", "2"])
            .output()
            .expect("run yuzhu-initdb");
        assert!(
            out.status.success(),
            "initdb failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self {
            dir,
            port: free_port(),
            child: None,
            restarts: 0,
        }
    }

    fn log_path(&self) -> PathBuf {
        self.dir.with_extension("log")
    }

    /// サーバを起動し、接続できるまで待つ（リカバリが終わるまで待ち受けない）。
    fn start(&mut self) -> Client {
        assert!(self.child.is_none(), "server already running");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .expect("open server log");
        let mut child = Command::new(env!("CARGO_BIN_EXE_yuzhu-server"))
            .arg("-D")
            .arg(&self.dir)
            .args(["--port", &self.port.to_string()])
            // WAL の量でチェックポイントを起こし（`checkpoint-timeout` は長くして時間の契機を外す）、
            // 2 MiB のセグメントの切り替えと古いセグメントの削除を、クラッシュの最中に起こす。
            // WAL を増やすために、各トランザクションが大きい行（`filler`）も入れる。
            .args(["--checkpoint-timeout", "30", "--max-wal-size", "4MB"])
            .args(["--shared-buffers", "2MB", "--log-level", "info"])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn yuzhu-server");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!(
                    "server exited during start-up ({status}); log: {}",
                    self.log_path().display()
                );
            }
            match connect(self.port) {
                Ok(c) => {
                    self.child = Some(child);
                    return c;
                }
                Err(e) => {
                    assert!(Instant::now() < deadline, "server did not start: {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// SIGKILL で止めて終了を待つ。
    fn kill9(&mut self) {
        let mut child = self.child.take().expect("server is running");
        child.kill().expect("SIGKILL");
        child.wait().expect("wait");
        self.restarts += 1;
    }

    /// SIGINT（fast shutdown）で止めて、正常終了を確かめる。
    fn stop_fast(&mut self) {
        let mut child = self.child.take().expect("server is running");
        let st = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(st.success(), "kill -INT failed");
        let status = child.wait().expect("wait");
        assert!(status.success(), "fast shutdown exited with {status}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let keep = std::thread::panicking() || std::env::var_os("YUZHU_KILL9_KEEP").is_some();
        if keep {
            eprintln!(
                "kept data directory {} and log {}",
                self.dir.display(),
                self.log_path().display()
            );
        } else {
            let _ = std::fs::remove_dir_all(&self.dir);
            let _ = std::fs::remove_file(self.log_path());
        }
    }
}

fn connect(port: u16) -> Result<Client, postgres::Error> {
    postgres::Config::new()
        .host("127.0.0.1")
        .port(port)
        .user("postgres")
        .dbname("postgres")
        .connect_timeout(Duration::from_secs(5))
        .connect(NoTls)
}

fn rows(c: &mut Client, sql: &str) -> Result<Vec<Vec<String>>, postgres::Error> {
    let mut out = Vec::new();
    for m in c.simple_query(sql)? {
        if let SimpleQueryMessage::Row(r) = m {
            out.push(
                (0..r.len())
                    .map(|i| r.get(i).unwrap_or("").to_owned())
                    .collect(),
            );
        }
    }
    Ok(out)
}

fn int<T: std::str::FromStr>(s: &str) -> T {
    s.parse().unwrap_or_else(|_| panic!("not a number: {s:?}"))
}

fn snapshot(c: &mut Client, ddl_rounds: i32) -> Snapshot {
    let accounts = rows(c, "SELECT id, balance FROM accounts")
        .expect("read accounts")
        .iter()
        .map(|r| (int(&r[0]), int(&r[1])))
        .collect();
    let log = rows(c, "SELECT client, round, seq, src, dst, amt FROM log")
        .expect("read log")
        .iter()
        .map(|r| LogRow {
            client: int(&r[0]),
            round: int(&r[1]),
            seq: int(&r[2]),
            src: int(&r[3]),
            dst: int(&r[4]),
            amt: int(&r[5]),
        })
        .collect();
    let mut ddl_rows = BTreeMap::new();
    for r in 0..ddl_rounds {
        // 表がなければ 42P01 になるので、入れずに検査側で報告する。
        if let Ok(v) = rows(c, &format!("SELECT x FROM ddl_{r}")) {
            ddl_rows.insert(r, v.len());
        }
    }
    Snapshot {
        accounts,
        log,
        ddl_rows,
    }
}

/// 再起動後に `log.id` と `nextval('log_id_seq')` を読んで `check_serial` で検査する。
/// 成功したら、この回で払い出された `nextval` の値を返す。
fn check_serial_after_recovery(
    c: &mut Client,
    confirmed_ids: &BTreeSet<i64>,
    prev_next: i64,
) -> Result<i64, String> {
    let log_ids: Vec<i64> = rows(c, "SELECT id FROM log")
        .map_err(|e| format!("read log.id: {e}"))?
        .iter()
        .map(|r| int(&r[0]))
        .collect();
    let next: i64 = rows(c, "SELECT nextval('log_id_seq')")
        .map_err(|e| format!("nextval after recovery: {e}"))?
        .first()
        .map(|r| int(&r[0]))
        .ok_or("nextval returned no row")?;
    check_serial(&log_ids, confirmed_ids, next, prev_next)?;
    Ok(next)
}

// ---------------------------------------------------------------------------
// ワークロード
// ---------------------------------------------------------------------------

/// 1 本のクライアント。接続が切れるまで、振込 + 追記ログを 1 トランザクションで流す。
/// `COMMIT` の応答を受け取るたびに `confirmed` を増やす。
#[allow(clippy::too_many_arguments)]
fn client_loop(
    port: u16,
    client: i32,
    round: i32,
    seed: u64,
    confirmed: &AtomicU32,
    ids: &Mutex<Vec<i64>>,
    stop: &AtomicBool,
    unexpected: &Mutex<Vec<String>>,
) {
    let Ok(mut c) = connect(port) else { return };
    let mut rng = Rng::new(seed);
    let mut seq = 0i32;
    let filler = "x".repeat(FILLER_LEN);
    while !stop.load(Ordering::Relaxed) {
        let src = i32::try_from(rng.range(0, u64::from(ACCOUNTS.unsigned_abs()))).unwrap_or(0);
        let dst = (src
            + 1
            + i32::try_from(rng.range(0, u64::from((ACCOUNTS - 1).unsigned_abs()))).unwrap_or(0))
            % ACCOUNTS;
        let amt = rng.range(1, 100);
        let mut pending_id = None;
        let statements = [
            "BEGIN".to_owned(),
            format!("UPDATE accounts SET balance = balance - {amt} WHERE id = {src}"),
            format!("UPDATE accounts SET balance = balance + {amt} WHERE id = {dst}"),
            format!(
                "INSERT INTO log (client, round, seq, src, dst, amt) \
                 VALUES ({client}, {round}, {seq}, {src}, {dst}, {amt})"
            ),
            format!("INSERT INTO filler VALUES ({client}, '{filler}')"),
            "SELECT currval('log_id_seq')".to_owned(),
            "COMMIT".to_owned(),
        ];
        for sql in &statements {
            match c.simple_query(sql) {
                Ok(msgs) => {
                    if let Some(id) = msgs.iter().find_map(|m| match m {
                        SimpleQueryMessage::Row(r) => r.get(0).and_then(|v| v.parse::<i64>().ok()),
                        _ => None,
                    }) {
                        pending_id = Some(id);
                    }
                }
                Err(e) => {
                    // 接続が切れたのは想定どおり。SQL のエラーは不具合。
                    if e.as_db_error().is_some() {
                        unexpected
                            .lock()
                            .expect("lock")
                            .push(format!("client {client}: {sql}: {e}"));
                    }
                    return;
                }
            }
        }
        // `COMMIT` の応答を受け取った: 払い出された `id` は必ず残る。
        if let Some(id) = pending_id {
            ids.lock().expect("lock").push(id);
        }
        confirmed.fetch_add(1, Ordering::SeqCst);
        seq += 1;
    }
}

/// DDL を続ける 1 本のクライアント: `churn_<round>_<i>` を作って 1 行入れ、少し前の表を DROP する。
/// kill -9 が DDL の最中に当たるようにする。確認できた CREATE+INSERT / DROP と、応答が分からない
/// 表の名前を `churn` に記録する。
fn ddl_loop(
    port: u16,
    round: i32,
    stop: &AtomicBool,
    churn: &Mutex<Churn>,
    unexpected: &Mutex<Vec<String>>,
) {
    let Ok(mut c) = connect(port) else { return };
    let mut i = 0i32;
    let mut live: Vec<String> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let name = format!("churn_{round}_{i}");
        churn.lock().expect("lock").maybe.push(name.clone());
        let steps = [
            format!("CREATE TABLE {name} (x int)"),
            format!("INSERT INTO {name} VALUES (1)"),
        ];
        for sql in &steps {
            if let Err(e) = c.simple_query(sql) {
                report_db_error(unexpected, "ddl", sql, &e);
                return;
            }
        }
        live.push(name.clone());
        churn.lock().expect("lock").created.push(name);
        if live.len() > 2 {
            let victim = live.remove(0);
            let sql = format!("DROP TABLE {victim}");
            churn
                .lock()
                .expect("lock")
                .maybe_dropped
                .push(victim.clone());
            if let Err(e) = c.simple_query(&sql) {
                report_db_error(unexpected, "ddl", &sql, &e);
                return;
            }
            churn.lock().expect("lock").dropped.push(victim);
        }
        i += 1;
    }
}

fn report_db_error(unexpected: &Mutex<Vec<String>>, who: &str, sql: &str, e: &postgres::Error) {
    // 接続が切れたのは想定どおり。SQL のエラーは不具合。
    if e.as_db_error().is_some() {
        unexpected
            .lock()
            .expect("lock")
            .push(format!("{who}: {sql}: {e}"));
    }
}

/// `ddl_loop` が記録した、`churn_*` 表の確認状況（ラウンドをまたいで積む）。
#[derive(Default)]
struct Churn {
    /// CREATE と INSERT の応答を受け取った表（DROP の応答がなければ、残っていなければならない）。
    created: Vec<String>,
    /// DROP の応答を受け取った表（残ってはならない）。
    dropped: Vec<String>,
    /// DROP を送った表（応答が分からなければ、あってもなくてもよい）。
    maybe_dropped: Vec<String>,
    /// CREATE を送った表（応答が分からなければ、あってもなくてもよい。あれば読めなければならない）。
    maybe: Vec<String>,
}

/// 再起動後の `churn_*` 表の検査。
fn check_churn(c: &mut Client, churn: &Churn) -> Result<(), String> {
    let undefined =
        |e: &postgres::Error| e.as_db_error().is_some_and(|d| d.code().code() == "42P01");
    for name in &churn.maybe {
        let r = rows(c, &format!("SELECT x FROM {name}"));
        let confirmed_alive = churn.created.contains(name) && !churn.maybe_dropped.contains(name);
        let confirmed_dropped = churn.dropped.contains(name);
        match r {
            Ok(v) if confirmed_dropped => {
                return Err(format!(
                    "{name} was dropped (confirmed) but has {} rows",
                    v.len()
                ));
            }
            Ok(v) if v.len() > 1 || v.iter().any(|r| r[0] != "1") => {
                return Err(format!("{name} has unexpected rows {v:?}"));
            }
            Ok(v) if confirmed_alive && v.len() != 1 => {
                return Err(format!(
                    "{name} was created and filled (confirmed) but has {} rows",
                    v.len()
                ));
            }
            Ok(_) => {}
            Err(e) if undefined(&e) => {
                if confirmed_alive {
                    return Err(format!("{name} was created (confirmed) but is missing"));
                }
            }
            Err(e) => return Err(format!("{name} cannot be read after recovery: {e}")),
        }
    }
    Ok(())
}

#[test]
#[ignore = "層 2: 実プロセスの kill -9（夜間ジョブ。YUZHU_KILL9_ROUNDS=200）"]
#[allow(clippy::too_many_lines)]
fn kill9_bank_transfer_and_append_log() {
    let rounds = i32::try_from(env_u64("YUZHU_KILL9_ROUNDS").unwrap_or(20)).expect("rounds");
    let seed = env_u64("YUZHU_KILL9_SEED").unwrap_or_else(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| {
            u64::try_from(d.as_nanos() % u128::from(u64::MAX)).unwrap_or(1)
        })
    });
    eprintln!("kill9: seed {seed}, {rounds} rounds");
    let mut rng = Rng::new(seed);

    let mut h = Harness::new();
    let mut c = h.start();
    c.batch_execute("CREATE TABLE accounts (id int NOT NULL, balance int NOT NULL)")
        .expect("create accounts");
    c.batch_execute(
        "CREATE TABLE log (client int NOT NULL, round int NOT NULL, seq int NOT NULL, \
         src int NOT NULL, dst int NOT NULL, amt int NOT NULL, id serial)",
    )
    .expect("create log");
    c.batch_execute("CREATE TABLE filler (client int NOT NULL, s text NOT NULL)")
        .expect("create filler");
    for id in 0..ACCOUNTS {
        c.batch_execute(&format!("INSERT INTO accounts VALUES ({id}, {INITIAL})"))
            .expect("insert account");
    }

    let mut confirmed = Confirmed::new();
    let mut confirmed_ids = BTreeSet::new();
    let mut prev_next = 0i64;
    let churn = Arc::new(Mutex::new(Churn::default()));
    let mut total_confirmed = 0u64;
    for round in 0..rounds {
        // I10 用: 確認済みの CREATE TABLE + INSERT。
        c.batch_execute(&format!("CREATE TABLE ddl_{round} (x int)"))
            .expect("create ddl table");
        c.batch_execute(&format!("INSERT INTO ddl_{round} VALUES ({round})"))
            .expect("insert into ddl table");
        drop(c);

        let nclients = i32::try_from(rng.range(4, 9)).expect("clients");
        let stop = Arc::new(AtomicBool::new(false));
        let unexpected = Arc::new(Mutex::new(Vec::new()));
        let round_ids = Arc::new(Mutex::new(Vec::<i64>::new()));
        let counters: Vec<Arc<AtomicU32>> =
            (0..nclients).map(|_| Arc::new(AtomicU32::new(0))).collect();
        let threads: Vec<_> = (0..nclients)
            .map(|client| {
                let counter = Arc::clone(&counters[usize::try_from(client).expect("client")]);
                let stop = Arc::clone(&stop);
                let round_ids = Arc::clone(&round_ids);
                let unexpected = Arc::clone(&unexpected);
                let port = h.port;
                let seed = rng.next();
                std::thread::spawn(move || {
                    client_loop(
                        port,
                        client,
                        round,
                        seed,
                        &counter,
                        &round_ids,
                        &stop,
                        &unexpected,
                    );
                })
            })
            .collect();

        let ddl_thread = {
            let stop = Arc::clone(&stop);
            let churn = Arc::clone(&churn);
            let unexpected = Arc::clone(&unexpected);
            let port = h.port;
            std::thread::spawn(move || ddl_loop(port, round, &stop, &churn, &unexpected))
        };
        std::thread::sleep(Duration::from_millis(rng.range(500, 3000)));
        h.kill9();
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().expect("client thread");
        }
        ddl_thread.join().expect("ddl thread");
        assert!(
            unexpected.lock().expect("lock").is_empty(),
            "unexpected SQL errors: {:?}",
            unexpected.lock().expect("lock")
        );
        let mut round_total = 0u32;
        for (client, counter) in counters.iter().enumerate() {
            let n = counter.load(Ordering::SeqCst);
            round_total += n;
            confirmed.insert((i32::try_from(client).expect("client"), round), n);
        }
        total_confirmed += u64::from(round_total);
        confirmed_ids.extend(round_ids.lock().expect("lock").iter().copied());

        c = h.start();
        let snap = snapshot(&mut c, round + 1);
        if let Err(e) = check(&snap, &confirmed, round + 1) {
            panic!(
                "round {round} (seed {seed}): {e}\nserver log: {}",
                h.log_path().display()
            );
        }
        match check_serial_after_recovery(&mut c, &confirmed_ids, prev_next) {
            Ok(next) => prev_next = next,
            Err(e) => panic!(
                "round {round} (seed {seed}): {e}\nserver log: {}",
                h.log_path().display()
            ),
        }
        if let Err(e) = check_churn(&mut c, &churn.lock().expect("lock")) {
            panic!(
                "round {round} (seed {seed}): churn tables: {e}\nserver log: {}",
                h.log_path().display()
            );
        }
        eprintln!(
            "kill9: round {round}: {nclients} clients, {round_total} confirmed commits, {} log rows",
            snap.log.len()
        );
    }
    assert!(
        total_confirmed > 0,
        "no transaction was confirmed in {rounds} rounds; the workload did not run"
    );

    // 正常停止（停止チェックポイント）の後の再起動でも同じ状態が見える。
    drop(c);
    h.stop_fast();
    let mut c = h.start();
    let snap = snapshot(&mut c, rounds);
    check(&snap, &confirmed, rounds).expect("after a clean restart");
    check_churn(&mut c, &churn.lock().expect("lock")).expect("churn tables after a clean restart");
    check_serial_after_recovery(&mut c, &confirmed_ids, prev_next)
        .expect("serial after a clean restart");
    drop(c);
    h.kill9();
    assert_eq!(h.restarts, u32::try_from(rounds).expect("rounds") + 1);
    // WAL がセグメントをまたいだ（切り替えが起きた）こと。
    let newest_segment = std::fs::read_dir(h.dir.join("pg_wal"))
        .expect("read pg_wal")
        .filter_map(|e| e.ok()?.file_name().to_str().map(str::to_owned))
        .filter_map(|n| u64::from_str_radix(&n, 16).ok())
        .max()
        .unwrap_or(0);
    eprintln!("kill9: newest WAL segment number {newest_segment}");
    assert!(
        newest_segment >= 2,
        "the WAL never switched to a second segment"
    );
    let _: &Path = &h.dir;
}
