//! ワークロード（銀行振込、追記ログ、UPDATE の繰り返し、DDL の混在、小さいプール）と、それを
//! 1 スレッドで動かす足場 `Run`（`m3.md` §7.5）。
//!
//! `Run` は SQL を実行しながら、同じ変更を `TxnLog` に記録する。COMMIT が `Ok`（タグが `COMMIT`）で
//! 戻れば「確定」としてモデルへ、COMMIT を呼んだのに `Ok` で戻らなければ「不明」へ入れる。
//! 書き込みを行うセッションは 1 つ（単一ライター。もう 1 つのセッションは読むだけ）。

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use yuzhu_core::error::Severity;
use yuzhu_core::storage::vfs::{FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs};
use yuzhu_core::testing::{QueryOutput, TestClusterOptions, run_sql};
use yuzhu_core::{Cluster, Session, StartupParams};

use crate::model::{Model, Op, Row, Table, Tables, TxnLog};

/// 実装済みのワークロード名。
pub(crate) const WORKLOAD_NAMES: &[&str] =
    &["bank", "applog", "hot_update", "ddl_mix", "small_pool"];

// ----- 乱数 -------------------------------------------------------------------

/// `SplitMix64`。
#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `0..n`。
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0);
        self.next() % n
    }

    pub(crate) fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

// ----- 実行の足場 ----------------------------------------------------------------

/// ワークロードを止めた理由。
#[derive(Debug)]
pub(crate) enum Stop {
    /// 仕掛けた障害が起きた（凍結、または fsync の失敗）。ここでクラッシュさせる。
    Crashed,
    /// 障害が起きていないのにエンジンがエラーや panic を返した（ハーネスが見つけた不具合）。
    Bug(String),
}

pub(crate) type Step = Result<(), Stop>;

/// 書き込み用セッション。
const W: usize = 0;
/// 読み取り用セッション。
const R: usize = 1;

/// どの障害を仕掛けるか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arm {
    /// 起動後の I/O の `n` 番目の直前でディスクを凍結する（`n` は 0 始まり。`n` 回は成功する）。
    CrashAt(u64),
    /// `n` 番目（0 始まり）の fsync を EIO にする（PANIC になるはず）。
    FsyncFailAt(u64),
    /// `op` の `n` 番目（0 始まり）に `kind` の障害を起こす（fsync 以外の障害モデル）。
    FaultAt {
        op: FaultOp,
        n: u64,
        kind: FaultKind,
    },
    /// 仕掛けない（I/O の総数を測る、または終わりまで走らせる）。
    None,
}

/// `Arm::FaultAt` の障害の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FaultKind {
    /// EIO を返す。
    Eio,
    /// 書き込みを途中までにする（ENOSPC 相当）。
    ShortWrite,
    /// 失敗した fsync のデータを忘れる。
    ForgetFsync,
}

impl FaultKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            FaultKind::Eio => "eio",
            FaultKind::ShortWrite => "short",
            FaultKind::ForgetFsync => "forget",
        }
    }

    pub(crate) fn parse(s: &str) -> FaultKind {
        match s {
            "eio" => FaultKind::Eio,
            "short" => FaultKind::ShortWrite,
            "forget" => FaultKind::ForgetFsync,
            _ => panic!("bad fault kind {s:?} (eio, short, forget)"),
        }
    }

    fn effect(self) -> FaultEffect {
        match self {
            FaultKind::Eio => FaultEffect::Error(std::io::ErrorKind::Other),
            FaultKind::ShortWrite => FaultEffect::ShortWrite,
            FaultKind::ForgetFsync => FaultEffect::FsyncFailAndForget,
        }
    }
}

const FAULT_OPS: [(FaultOp, &str); 10] = [
    (FaultOp::Read, "read"),
    (FaultOp::Write, "write"),
    (FaultOp::Sync, "sync"),
    (FaultOp::SyncDir, "syncdir"),
    (FaultOp::Open, "open"),
    (FaultOp::Remove, "remove"),
    (FaultOp::Rename, "rename"),
    (FaultOp::SetLen, "setlen"),
    (FaultOp::CreateDir, "mkdir"),
    (FaultOp::Any, "any"),
];

pub(crate) fn fault_op_name(op: FaultOp) -> &'static str {
    FAULT_OPS
        .iter()
        .find(|(o, _)| *o == op)
        .map_or("any", |(_, n)| n)
}

pub(crate) fn parse_fault_op(s: &str) -> FaultOp {
    FAULT_OPS
        .iter()
        .find(|(_, n)| *n == s)
        .map_or_else(|| panic!("bad fault op {s:?}"), |(o, _)| *o)
}

impl Arm {
    /// 障害が起きたのにワークロードが最後まで `Ok` で走ったら、エラーを握りつぶした不具合。
    pub(crate) fn must_stop(self) -> bool {
        match self {
            Arm::FsyncFailAt(_) => true,
            Arm::FaultAt { op, kind, .. } => {
                kind == FaultKind::Eio
                    && matches!(op, FaultOp::Sync | FaultOp::SyncDir | FaultOp::Rename)
            }
            Arm::CrashAt(_) | Arm::None => false,
        }
    }
}

pub(crate) struct Run {
    pub(crate) vfs: SimVfs,
    pub(crate) cluster: Arc<Cluster>,
    /// 確定したトランザクションを適用したもの。
    pub(crate) model: Model,
    /// COMMIT を呼んだが結果が分からないトランザクション。
    pub(crate) unknown: Vec<TxnLog>,
    sessions: Vec<Option<Session>>,
    pending: Vec<Option<TxnLog>>,
    arm: Arm,
    fired_base: u64,
    ops_base: u64,
    stmts: u64,
    checkpoint_every: u64,
    user: String,
}

impl Run {
    /// `arm` を仕掛けて、`model` から続ける。
    pub(crate) fn new(
        vfs: SimVfs,
        cluster: Arc<Cluster>,
        opts: &TestClusterOptions,
        model: Model,
        arm: Arm,
        checkpoint_every: u64,
    ) -> Run {
        let rule = |op, nth, effect| FaultPlan {
            rules: vec![FaultRule {
                op,
                path_prefix: None,
                nth: Some(nth),
                probability: None,
                effect,
            }],
        };
        match arm {
            Arm::CrashAt(n) => vfs.set_faults(rule(FaultOp::Any, n + 1, FaultEffect::CrashFreeze)),
            Arm::FsyncFailAt(n) => vfs.set_faults(rule(
                FaultOp::Sync,
                n + 1,
                FaultEffect::Error(std::io::ErrorKind::Other),
            )),
            Arm::FaultAt { op, n, kind } => vfs.set_faults(rule(op, n + 1, kind.effect())),
            Arm::None => {}
        }
        Run {
            fired_base: vfs.stats().faults_fired,
            ops_base: vfs.op_count(),
            vfs,
            cluster,
            model,
            unknown: Vec::new(),
            sessions: vec![None, None],
            pending: vec![None, None],
            arm,
            stmts: 0,
            checkpoint_every,
            user: opts.superuser.clone(),
        }
    }

    /// 仕掛けた障害が起きたか。
    pub(crate) fn fired(&self) -> bool {
        self.vfs.stats().faults_fired > self.fired_base
    }

    /// 障害が起きたら、ワークロードは途中で止まらなければならないか。
    pub(crate) fn arm_must_stop(&self) -> bool {
        self.arm.must_stop()
    }

    /// 開始（`Run::new`）からの I/O の数。
    pub(crate) fn ops_done(&self) -> u64 {
        self.vfs.op_count() - self.ops_base
    }

    /// 開いたままのトランザクションがあれば ROLLBACK する。
    pub(crate) fn rollback_open(&mut self) -> Step {
        if self.pending[W].is_some() {
            self.rollback()?;
        }
        Ok(())
    }

    /// 開いたままのトランザクションを ROLLBACK してセッションを閉じ、正常停止する。
    /// 障害が起きて止まったなら `Crashed`。
    pub(crate) fn shutdown(&mut self) -> Step {
        self.rollback_open()?;
        self.drop_sessions();
        let cluster = Arc::clone(&self.cluster);
        match catch_unwind(AssertUnwindSafe(|| cluster.shutdown())) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(self.failure("shutdown", &format!("{e:?}"), false)),
            Err(_) => Err(self.failure("shutdown", "panic", true)),
        }
    }

    /// セッションを閉じる（I/O はしない。トランザクションは開いたまま捨てる）。
    pub(crate) fn drop_sessions(&mut self) {
        self.sessions.iter_mut().for_each(|s| *s = None);
    }

    fn failure(&self, what: &str, errors: &str, panicked: bool) -> Stop {
        if self.fired() {
            if matches!(self.arm, Arm::FsyncFailAt(_))
                && !panicked
                && !errors.contains("PANIC")
                && !self.cluster.is_poisoned()
            {
                return Stop::Bug(format!("fsync failure was not a PANIC: {what}: {errors}"));
            }
            Stop::Crashed
        } else {
            Stop::Bug(format!("{what}: {errors}"))
        }
    }

    fn session(&mut self, who: usize) -> Result<&mut Session, Stop> {
        if self.sessions[who].is_none() {
            let cluster = Arc::clone(&self.cluster);
            let params = StartupParams {
                user: self.user.clone(),
                database: "postgres".into(),
                application_name: None,
                options: Vec::new(),
            };
            let made = catch_unwind(AssertUnwindSafe(|| Session::new(cluster, params)));
            match made {
                Ok(Ok(s)) => self.sessions[who] = Some(s),
                Ok(Err(e)) => return Err(self.failure("connect", &format!("{e:?}"), false)),
                Err(_) => return Err(self.failure("connect", "panic", true)),
            }
        }
        Ok(self.sessions[who].as_mut().expect("session was just made"))
    }

    /// SQL を 1 つ実行する。エラーなら（障害のあとなら `Crashed`、そうでなければ `Bug`）。
    pub(crate) fn exec(&mut self, who: usize, sql: &str) -> Result<QueryOutput, Stop> {
        let out = self.exec_no_checkpoint(who, sql)?;
        self.maybe_checkpoint()?;
        Ok(out)
    }

    /// `exec` のうち、チェックポイントを挟まないもの。
    fn exec_no_checkpoint(&mut self, who: usize, sql: &str) -> Result<QueryOutput, Stop> {
        let s = self.session(who)?;
        let r = catch_unwind(AssertUnwindSafe(|| run_sql(s, sql)));
        match r {
            Ok(out) if out.is_ok() => {
                self.stmts += 1;
                Ok(out)
            }
            Ok(out) => {
                let is_panic = out.errors.iter().any(|e| e.severity == Severity::Panic);
                let text = out
                    .errors
                    .iter()
                    .map(|e| format!("{:?} {}: {}", e.severity, e.sqlstate.0, e.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(self.failure(sql, &text, is_panic))
            }
            Err(_) => Err(self.failure(sql, "panic", true)),
        }
    }

    fn maybe_checkpoint(&mut self) -> Step {
        if self.checkpoint_every > 0 && self.stmts.is_multiple_of(self.checkpoint_every) {
            self.checkpoint()?;
        }
        Ok(())
    }

    pub(crate) fn checkpoint(&mut self) -> Step {
        let cluster = Arc::clone(&self.cluster);
        match catch_unwind(AssertUnwindSafe(|| cluster.checkpoint())) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(self.failure("checkpoint", &format!("{e:?}"), false)),
            Err(_) => Err(self.failure("checkpoint", "panic", true)),
        }
    }

    // ----- トランザクション -----

    pub(crate) fn begin(&mut self, label: &str) -> Step {
        assert!(self.pending[W].is_none(), "transaction already open");
        self.exec(W, "BEGIN")?;
        self.pending[W] = Some(TxnLog {
            label: label.into(),
            ops: Vec::new(),
        });
        Ok(())
    }

    /// 書き込み文を実行し、成功したら `op` を記録する。
    pub(crate) fn write(&mut self, sql: &str, op: Op) -> Step {
        self.exec(W, sql)?;
        self.pending[W]
            .as_mut()
            .expect("write outside a transaction")
            .ops
            .push(op);
        Ok(())
    }

    /// COMMIT。`Ok` で戻れば確定、そうでなければ不明。
    pub(crate) fn commit(&mut self) -> Step {
        let log = self.pending[W]
            .take()
            .expect("commit outside a transaction");
        match self.exec_no_checkpoint(W, "COMMIT") {
            Ok(out) => {
                if out.tags.last().map(String::as_str) != Some("COMMIT") {
                    return Err(Stop::Bug(format!(
                        "COMMIT of {} ended with tags {:?}",
                        log.label, out.tags
                    )));
                }
                // COMMIT の応答を受け取ったら、直後のチェックポイントで凍結しても確定として扱う。
                self.model.apply(&log);
                self.maybe_checkpoint()
            }
            Err(e) => {
                self.unknown.push(log);
                Err(e)
            }
        }
    }

    pub(crate) fn rollback(&mut self) -> Step {
        self.pending[W]
            .take()
            .expect("rollback outside a transaction");
        self.exec(W, "ROLLBACK").map(|_| ())
    }

    /// 読み取り用セッションで SELECT する（各行の各列を文字列に直して返す）。
    pub(crate) fn query(&mut self, sql: &str) -> Result<Vec<Vec<String>>, Stop> {
        Ok(self.exec(R, sql)?.text_rows())
    }

    // ----- 変更の部品（SQL とモデルへの記録を一緒にする） -----

    pub(crate) fn create_table(&mut self, table: &str, types: &[&str]) -> Step {
        let cols: Vec<String> = types
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{} {t}", col(i)))
            .collect();
        self.write(
            &format!("CREATE TABLE {table} ({})", cols.join(", ")),
            Op::Create(table.into()),
        )
    }

    pub(crate) fn drop_table(&mut self, table: &str) -> Step {
        self.write(&format!("DROP TABLE {table}"), Op::Drop(table.into()))
    }

    pub(crate) fn insert(&mut self, table: &str, key: i64, row: Row) -> Step {
        let vals: Vec<String> = std::iter::once(key.to_string())
            .chain(row.iter().map(|v| lit(v)))
            .collect();
        self.write(
            &format!("INSERT INTO {table} VALUES ({})", vals.join(", ")),
            Op::Insert {
                table: table.into(),
                key,
                row,
            },
        )
    }

    /// 行の第 1 列以外を丸ごと書き換える。
    pub(crate) fn set_row(&mut self, table: &str, key: i64, row: Row) -> Step {
        let sets: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{} = {}", col(i + 1), lit(v)))
            .collect();
        self.write(
            &format!("UPDATE {table} SET {} WHERE k = {key}", sets.join(", ")),
            Op::Update {
                table: table.into(),
                key,
                row,
            },
        )
    }

    pub(crate) fn delete(&mut self, table: &str, key: i64) -> Step {
        self.write(
            &format!("DELETE FROM {table} WHERE k = {key}"),
            Op::Delete {
                table: table.into(),
                key,
            },
        )
    }

    /// モデル上の 1 行（確定済みの状態）。
    pub(crate) fn model_row(&self, table: &str, key: i64) -> Row {
        self.model
            .row(table, key)
            .unwrap_or_else(|| panic!("model has no {table}[{key}]"))
            .clone()
    }
}

/// 列名。第 0 列は `k`（キー）、あとは `c1`、`c2`…。
fn col(i: usize) -> String {
    if i == 0 { "k".into() } else { format!("c{i}") }
}

/// SQL のリテラル（整数はそのまま、それ以外は引用符つき）。
fn lit(v: &str) -> String {
    if v.parse::<i64>().is_ok() {
        v.to_owned()
    } else {
        format!("'{}'", v.replace('\'', "''"))
    }
}

/// 長さの決まった詰め物（行を大きくしてページを使い切る）。
fn pad(seed: i64, len: usize) -> String {
    let mut s = format!("p{seed}-");
    while s.len() < len {
        s.push('x');
    }
    s.truncate(len);
    s
}

// ----- ワークロードの定義 --------------------------------------------------------

/// ワークロード 1 つ。`run` は「必要ならセットアップして、`ntx` 個のトランザクションを流し、
/// 最後に 1 つ開いたままにする」。クラッシュ後の続き（I10）でも同じ関数を呼べる。
pub(crate) struct Workload {
    pub(crate) name: &'static str,
    pub(crate) nframes: usize,
    pub(crate) checkpoint_every: u64,
    pub(crate) run: fn(&mut Run, &mut Rng, round: u32, ntx: u32) -> Step,
    /// ワークロード固有の不変条件（I2: 合計が一定、行数が 0 か k など）。
    pub(crate) check: fn(&Tables) -> Result<(), String>,
}

pub(crate) fn all() -> Vec<Workload> {
    vec![
        Workload {
            name: "bank",
            nframes: 16,
            checkpoint_every: 9,
            run: bank,
            check: bank_check,
        },
        Workload {
            name: "applog",
            nframes: 16,
            checkpoint_every: 11,
            run: applog,
            check: applog_check,
        },
        Workload {
            name: "hot_update",
            nframes: 16,
            checkpoint_every: 7,
            run: hot_update,
            check: no_check,
        },
        Workload {
            name: "ddl_mix",
            nframes: 16,
            checkpoint_every: 8,
            run: ddl_mix,
            check: no_check,
        },
        Workload {
            name: "small_pool",
            nframes: 8,
            checkpoint_every: 5,
            run: small_pool,
            check: no_check,
        },
        Workload {
            name: WAL_FILL,
            nframes: 16,
            checkpoint_every: 40,
            run: wal_fill,
            check: no_check,
        },
    ]
}

pub(crate) fn by_name(name: &str) -> Workload {
    all()
        .into_iter()
        .find(|w| w.name == name)
        .unwrap_or_else(|| panic!("unknown workload {name}"))
}

#[allow(clippy::unnecessary_wraps)] // `Workload::check` と同じ型にそろえる
fn no_check(_: &Tables) -> Result<(), String> {
    Ok(())
}

fn table_of<'a>(t: &'a Tables, name: &str) -> Option<&'a Table> {
    t.get(name)
}

// (1) 銀行振込: 口座 10 件、合計は常に 10000。

const ACCOUNTS: i64 = 10;
const INITIAL_BALANCE: i64 = 1000;

fn bank_check(t: &Tables) -> Result<(), String> {
    let Some(acct) = table_of(t, "acct") else {
        return Ok(());
    };
    // セットアップのトランザクションは全部か無しか。
    if acct.is_empty() {
        return Ok(());
    }
    let n = i64::try_from(acct.len()).unwrap_or(-1);
    if n != ACCOUNTS {
        return Err(format!("acct has {n} rows, expected {ACCOUNTS}"));
    }
    let total: i64 = acct
        .values()
        .map(|r| r[0].parse::<i64>().unwrap_or(i64::MIN / 100))
        .sum();
    if total != ACCOUNTS * INITIAL_BALANCE {
        return Err(format!(
            "acct balances sum to {total}, expected {}",
            ACCOUNTS * INITIAL_BALANCE
        ));
    }
    Ok(())
}

fn bank(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    if !run.model.tables.contains_key("acct") {
        run.begin("bank-setup")?;
        run.create_table("acct", &["int", "int", "text"])?;
        for k in 0..ACCOUNTS {
            run.insert("acct", k, vec![INITIAL_BALANCE.to_string(), pad(k, 120)])?;
        }
        run.commit()?;
    }
    for i in 0..=ntx {
        let a = i64::try_from(rng.below(10)).unwrap_or(0);
        let b = (a + 1 + i64::try_from(rng.below(9)).unwrap_or(0)) % ACCOUNTS;
        let amount = 1 + i64::try_from(rng.below(50)).unwrap_or(0);
        run.begin(&format!("bank-{round}-{i}"))?;
        for (k, delta) in [(a, -amount), (b, amount)] {
            let mut row = run.model_row("acct", k);
            let bal: i64 = row[0].parse().expect("balance");
            row[0] = (bal + delta).to_string();
            let sql = format!("UPDATE acct SET c1 = c1 + {delta} WHERE k = {k}");
            run.write(
                &sql,
                Op::Update {
                    table: "acct".into(),
                    key: k,
                    row,
                },
            )?;
        }
        if i == ntx {
            // クラッシュの時に実行中のままにする。
            break;
        }
        if rng.chance(30) {
            run.rollback()?;
        } else {
            run.commit()?;
        }
        if i % 3 == 0 {
            let rows = run.query("SELECT c1 FROM acct")?;
            let total: i64 = rows.iter().map(|r| r[0].parse::<i64>().unwrap_or(0)).sum();
            if total != ACCOUNTS * INITIAL_BALANCE {
                return Err(Stop::Bug(format!(
                    "a reader saw a partial transfer: total {total}"
                )));
            }
        }
    }
    Ok(())
}

// (2) 追記ログ: 各トランザクションが同じ tid の行を K 行挿入する。行数は 0 か K。

const LOG_ROWS: i64 = 4;

fn applog_check(t: &Tables) -> Result<(), String> {
    let Some(lg) = table_of(t, "lg") else {
        return Ok(());
    };
    let mut per_tid: BTreeMap<String, i64> = BTreeMap::new();
    for r in lg.values() {
        *per_tid.entry(r[0].clone()).or_default() += 1;
    }
    for (tid, n) in per_tid {
        if n != LOG_ROWS {
            return Err(format!(
                "log txn {tid} has {n} rows, expected 0 or {LOG_ROWS}"
            ));
        }
    }
    Ok(())
}

fn applog(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    if !run.model.tables.contains_key("lg") {
        run.begin("applog-setup")?;
        run.create_table("lg", &["int", "int", "int", "text"])?;
        run.commit()?;
    }
    for i in 0..=ntx {
        let tid = i64::from(round) * 1000 + i64::from(i);
        run.begin(&format!("applog-{tid}"))?;
        for seq in 0..LOG_ROWS {
            run.insert(
                "lg",
                tid * 10 + seq,
                vec![tid.to_string(), seq.to_string(), pad(tid, 60)],
            )?;
        }
        if i == ntx {
            break;
        }
        if rng.chance(25) {
            run.rollback()?;
        } else {
            run.commit()?;
        }
    }
    Ok(())
}

// (3) 少数の行の UPDATE の繰り返し（同じページへの多数の変更とチェックポイント → FPW の経路）。

fn hot_update(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    if !run.model.tables.contains_key("hot") {
        run.begin("hot-setup")?;
        run.create_table("hot", &["int", "int", "text"])?;
        for k in 0..4 {
            run.insert("hot", k, vec!["0".into(), pad(k, 40)])?;
        }
        run.commit()?;
    }
    for i in 0..=ntx {
        run.begin(&format!("hot-{round}-{i}"))?;
        for _ in 0..3 {
            let k = i64::try_from(rng.below(4)).unwrap_or(0);
            let mut row = run.model_row("hot", k);
            // 同じトランザクションの中で同じ行を 2 度更新しても、確定済みのモデルからは
            // 求められないので、この行は 1 トランザクションに 1 度だけ触る。
            let touched = run.pending_touched("hot", k);
            if touched {
                continue;
            }
            let n: i64 = row[0].parse().expect("counter");
            row[0] = (n + 1).to_string();
            run.write(
                &format!("UPDATE hot SET c1 = c1 + 1 WHERE k = {k}"),
                Op::Update {
                    table: "hot".into(),
                    key: k,
                    row,
                },
            )?;
        }
        if i == ntx {
            break;
        }
        if rng.chance(15) {
            run.rollback()?;
        } else {
            run.commit()?;
        }
    }
    Ok(())
}

impl Run {
    /// 開いているトランザクションが `table[key]` をすでに変更したか。
    fn pending_touched(&self, table: &str, key: i64) -> bool {
        self.pending[W].as_ref().is_some_and(|l| {
            l.ops.iter().any(|o| match o {
                Op::Insert {
                    table: t, key: k, ..
                }
                | Op::Update {
                    table: t, key: k, ..
                }
                | Op::Delete { table: t, key: k } => t == table && *k == key,
                _ => false,
            })
        })
    }
}

// (4) DDL の混在: CREATE / DROP / ROLLBACK される CREATE と DROP。

fn ddl_mix(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    for i in 0..=ntx {
        let name = format!("d{round}x{i}");
        let existing: Vec<String> = run.model.tables.keys().cloned().collect();
        let pick = |rng: &mut Rng| {
            existing[usize::try_from(rng.below(existing.len() as u64)).unwrap_or(0)].clone()
        };
        run.begin(&format!("ddl-{name}"))?;
        let last = i == ntx;
        match rng.below(5) {
            0 | 1 => {
                run.create_table(&name, &["int", "text"])?;
                for k in 0..2 {
                    run.insert(&name, k, vec![pad(k, 30)])?;
                }
                if last || rng.chance(70) {
                    // 次の分岐で commit / rollback する。
                    if last {
                        break;
                    }
                    run.commit()?;
                } else {
                    run.rollback()?;
                }
            }
            2 if !existing.is_empty() => {
                let t = pick(rng);
                run.drop_table(&t)?;
                if last {
                    break;
                }
                if rng.chance(60) {
                    run.commit()?;
                } else {
                    run.rollback()?;
                }
            }
            _ if !existing.is_empty() => {
                let t = pick(rng);
                let key = 100 + i64::from(round) * 1000 + i64::from(i);
                run.insert(&t, key, vec![pad(key, 30)])?;
                run.set_row(&t, key, vec![pad(key + 1, 30)])?;
                if last {
                    break;
                }
                run.commit()?;
            }
            _ => {
                run.create_table(&name, &["int", "text"])?;
                if last {
                    break;
                }
                run.rollback()?;
            }
        }
    }
    Ok(())
}

// (5) 8 フレームのプールで追い出しと steal を頻発させる。

fn small_pool(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    if !run.model.tables.contains_key("big") {
        run.begin("big-setup")?;
        run.create_table("big", &["int", "int", "text"])?;
        run.commit()?;
    }
    for i in 0..=ntx {
        run.begin(&format!("big-{round}-{i}"))?;
        let base = (i64::from(round) * 1000 + i64::from(i)) * 10;
        for j in 0..4 {
            run.insert("big", base + j, vec!["0".into(), pad(base + j, 300)])?;
        }
        // 古い行も更新して、いろいろなページを触る。
        let keys: Vec<i64> = run
            .model
            .tables
            .get("big")
            .map(|t| t.keys().copied().collect())
            .unwrap_or_default();
        if !keys.is_empty() {
            let k = keys[usize::try_from(rng.below(keys.len() as u64)).unwrap_or(0)];
            let mut row = run.model_row("big", k);
            let n: i64 = row[0].parse().expect("counter");
            row[0] = (n + 1).to_string();
            run.write(
                &format!("UPDATE big SET c1 = c1 + 1 WHERE k = {k}"),
                Op::Update {
                    table: "big".into(),
                    key: k,
                    row,
                },
            )?;
        }
        if rng.chance(30) && !keys.is_empty() {
            let k = keys[usize::try_from(rng.below(keys.len() as u64)).unwrap_or(0)];
            if !run.pending_touched("big", k) {
                run.delete("big", k)?;
            }
        }
        if i == ntx {
            break;
        }
        if rng.chance(20) {
            run.rollback()?;
        } else {
            run.commit()?;
        }
    }
    Ok(())
}

// (6) WAL を 2 MiB のセグメントの外まで進める: 大きい行を大量に挿入する。セグメントの切り替え、
// 新セグメントの作成（tmp → sync_all → rename → sync_dir）、チェックポイントでの古いセグメントの
// 削除がクラッシュ点に入る。

/// `wal_fill` の名前（`WORKLOAD_NAMES` の他のワークロードと違い、I/O が多いので総当たりの試験には入れない）。
pub(crate) const WAL_FILL: &str = "wal_fill";

fn wal_fill(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    if !run.model.tables.contains_key("wf") {
        run.begin("wf-setup")?;
        run.create_table("wf", &["int", "text"])?;
        run.commit()?;
    }
    for i in 0..=ntx {
        run.begin(&format!("wf-{round}-{i}"))?;
        let base = (i64::from(round) * 1000 + i64::from(i)) * 10;
        for j in 0..6 {
            run.insert("wf", base + j, vec![pad(base + j, 2500)])?;
        }
        // 古い行を 1 つ消す（モデル上の確定済みの行のうち、このトランザクションが触っていないもの）。
        let keys: Vec<i64> = run
            .model
            .tables
            .get("wf")
            .map(|t| t.keys().copied().collect())
            .unwrap_or_default();
        if !keys.is_empty() && rng.chance(50) {
            let k = keys[usize::try_from(rng.below(keys.len() as u64)).unwrap_or(0)];
            if !run.pending_touched("wf", k) {
                run.delete("wf", k)?;
            }
        }
        if i == ntx {
            break;
        }
        if rng.chance(15) {
            run.rollback()?;
        } else {
            run.commit()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_bounded() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            let x = a.below(10);
            assert_eq!(x, b.below(10));
            assert!(x < 10);
        }
        assert_ne!(Rng::new(1).next(), Rng::new(2).next());
    }

    #[test]
    fn literals_and_padding() {
        assert_eq!(lit("12"), "12");
        assert_eq!(lit("-3"), "-3");
        assert_eq!(lit("a'b"), "'a''b'");
        assert_eq!(pad(3, 10).len(), 10);
        assert_eq!(col(0), "k");
        assert_eq!(col(2), "c2");
    }

    #[test]
    fn registry_matches_names() {
        let names: Vec<&str> = all().iter().map(|w| w.name).collect();
        let mut expected = WORKLOAD_NAMES.to_vec();
        expected.push(WAL_FILL);
        assert_eq!(names, expected);
    }

    #[test]
    fn bank_check_detects_a_lost_update() {
        let mut t = Tables::new();
        let rows: Table = (0..ACCOUNTS)
            .map(|k| (k, vec![INITIAL_BALANCE.to_string(), "p".to_string()]))
            .collect();
        t.insert("acct".into(), rows);
        assert!(bank_check(&t).is_ok());
        t.get_mut("acct").unwrap().get_mut(&3).unwrap()[0] = "990".into();
        assert!(bank_check(&t).is_err());
    }

    #[test]
    fn applog_check_detects_partial_transactions() {
        let mut t = Tables::new();
        let mut rows = Table::new();
        for seq in 0..LOG_ROWS {
            rows.insert(seq, vec!["1".into(), seq.to_string(), "p".into()]);
        }
        t.insert("lg".into(), rows);
        assert!(applog_check(&t).is_ok());
        t.get_mut("lg").unwrap().remove(&0);
        assert!(applog_check(&t).is_err());
    }
}
