//! ワークロード（銀行振込、追記ログ、UPDATE の繰り返し、DDL の混在、小さいプール）と、それを
//! 1 スレッドで動かす足場 `Run`（`m3.md` §7.5）。
//!
//! `Run` は SQL を実行しながら、同じ変更を `TxnLog` に記録する。COMMIT が `Ok`（タグが `COMMIT`）で
//! 戻れば「確定」としてモデルへ、COMMIT を呼んだのに `Ok` で戻らなければ「不明」へ入れる。
//! 書き込みを行うセッションは 1 つ（単一ライター。もう 1 つのセッションは読むだけ）。

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use yuzhu_core::error::Severity;
use yuzhu_core::storage::vfs::{FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs};
use yuzhu_core::testing::{QueryOutput, TestClusterOptions, run_sql};
use yuzhu_core::{Cluster, Session, StartupParams};

use crate::model::{DdlOp, Model, Op, Row, SeqOp, Table, Tables, TxnLog};

/// 実装済みのワークロード名。
pub(crate) const WORKLOAD_NAMES: &[&str] =
    &["bank", "applog", "hot_update", "ddl_plain", "small_pool"];

/// M4 のワークロード 6〜8（11 §3.6.1）。
pub(crate) const M4_WORKLOAD_NAMES: &[&str] = &["indexed_table", "sequences", "ddl_mix"];

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
/// 読み取り用セッション（ワークロード 7 では、未コミットの `nextval` を持つ 2 つ目のセッション）。
const R: usize = 1;
/// 3 つ目のセッション（ワークロード 7）。
const X: usize = 2;

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
    /// W 以外のセッションで開いたままのトランザクション（`rollback_open` が閉じる）。
    extra_open: [bool; 3],
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
            sessions: vec![None, None, None],
            pending: vec![None, None, None],
            extra_open: [false; 3],
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
        for who in [R, X] {
            self.close_extra(who)?;
        }
        Ok(())
    }

    /// W 以外のセッション `who` で開いたままのトランザクションを ROLLBACK する。
    pub(crate) fn close_extra(&mut self, who: usize) -> Step {
        if self.extra_open[who] {
            self.extra_open[who] = false;
            self.exec(who, "ROLLBACK")?;
        }
        Ok(())
    }

    /// W 以外のセッション `who` で BEGIN し、開いたままにする（`close_extra` で閉じる）。
    pub(crate) fn begin_extra(&mut self, who: usize) -> Step {
        assert!(
            !self.extra_open[who],
            "session {who} already has a transaction"
        );
        self.exec(who, "BEGIN")?;
        self.extra_open[who] = true;
        Ok(())
    }

    /// 開いているトランザクションの記録へ、SQL を伴わない変更を足す。
    pub(crate) fn record(&mut self, op: Op) {
        self.pending[W]
            .as_mut()
            .expect("record outside a transaction")
            .ops
            .push(op);
    }

    /// 1 行 1 列の整数を返す SQL を、チェックポイントを挟まずに実行して値を返す。
    pub(crate) fn value(&mut self, who: usize, sql: &str) -> Result<i64, Stop> {
        let out = self.exec_no_checkpoint(who, sql)?;
        out.text_rows()
            .first()
            .and_then(|r| r.first())
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| Stop::Bug(format!("{sql}: no integer result")))
    }

    /// `nextval(name)`。値を受け取ったらすぐ「どこかへ返した最大」へ入れる（確定かどうかは呼び出し側）。
    /// チェックポイントは呼び出し側が `maybe_checkpoint` で挟む。
    pub(crate) fn nextval(&mut self, who: usize, name: &str) -> Result<i64, Stop> {
        let v = self.value(who, &format!("SELECT nextval('{name}')"))?;
        self.model.seq.returned(name, v);
        Ok(v)
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
        self.extra_open = [false; 3];
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
    pub(crate) fn exec_no_checkpoint(
        &mut self,
        who: usize,
        sql: &str,
    ) -> Result<QueryOutput, Stop> {
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

    pub(crate) fn maybe_checkpoint(&mut self) -> Step {
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
    if v == "NULL" || v.parse::<i64>().is_ok() {
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
            name: "ddl_plain",
            nframes: 16,
            checkpoint_every: 8,
            run: ddl_plain,
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
            name: "indexed_table",
            nframes: 24,
            checkpoint_every: 9,
            run: indexed_table,
            check: no_check,
        },
        Workload {
            name: "sequences",
            nframes: 16,
            checkpoint_every: 7,
            run: sequences,
            check: no_check,
        },
        Workload {
            name: "ddl_mix",
            nframes: 24,
            checkpoint_every: 6,
            run: ddl_mix,
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

// (4) DDL の混在: CREATE / DROP / ROLLBACK される CREATE と DROP（M3。M4 の `ddl_mix` と区別して `ddl_plain`）。

fn ddl_plain(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
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

// ===== M4 のワークロード 6〜8（11 §3.6.1）=========================================

/// 疑似乱数の 1 つ（`0..n` の `i64`）。
fn below_i64(rng: &mut Rng, n: i64) -> i64 {
    i64::try_from(rng.below(u64::try_from(n).unwrap_or(1))).unwrap_or(0)
}

/// 添字を `0..len` から 1 つ選ぶ。
fn pick_index(rng: &mut Rng, len: usize) -> usize {
    usize::try_from(rng.below(len as u64)).unwrap_or(0)
}

/// 確定したモデルの表のキー（昇順）。
fn model_keys(run: &Run, table: &str) -> Vec<i64> {
    run.model
        .tables
        .get(table)
        .map(|t| t.keys().copied().collect())
        .unwrap_or_default()
}

/// COMMIT か ROLLBACK（`commit_percent` の確率で COMMIT）。最後の 1 つは実行中のまま残す。
fn finish_txn(run: &mut Run, rng: &mut Rng, last: bool, commit_percent: u64) -> Step {
    if last {
        return Ok(());
    }
    if rng.chance(commit_percent) {
        run.commit()
    } else {
        run.rollback()
    }
}

/// 長さの決まった一意な文字列（`tag` が違えば必ず違う）。
fn uniq_text(tag: &str, len: usize) -> String {
    let mut s = format!("u{tag}-");
    while s.len() < len {
        s.push('x');
    }
    s
}

// (6) indexed_table: PK + UNIQUE（700 バイトの s）+ INDEX (v DESC NULLS FIRST)。分割を頻発させる。
// 大きい版（ntx >= 20）は、既存の 450 行以上への CREATE INDEX（索引が 33 ページ以上になり、
// 一括構築の 32 ページごとのレコードを通す）と、その DROP INDEX も行う。

const IX_TABLE: &str = "t";
const IX_BIG_INDEX: &str = "t_big_ix";
const IX_BIG_ROWS: usize = 450;
const IX_S_LEN: usize = 700;

fn ix_s(n: i64) -> String {
    // 連番を散らして、挿入位置をばらけさせる（2^40 の法の奇数倍は全単射）。
    let scattered = n.wrapping_mul(0x9E37_79B1) & ((1 << 40) - 1);
    pad(scattered, IX_S_LEN)
}

fn indexed_table(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    run.model.ddl.enforce = true;
    let big = ntx >= crate::BIG_NTX;
    if !run.model.tables.contains_key(IX_TABLE) {
        run.begin("ix-setup")?;
        run.write(
            "CREATE TABLE t (k int PRIMARY KEY, c1 text, c2 int)",
            Op::Create(IX_TABLE.into()),
        )?;
        run.record(Op::Ddl(DdlOp::CreateIndex {
            name: "t_pkey".into(),
            table: IX_TABLE.into(),
            unique: true,
            primary: true,
        }));
        run.write(
            "CREATE UNIQUE INDEX t_s_ix ON t (c1)",
            Op::Ddl(DdlOp::CreateIndex {
                name: "t_s_ix".into(),
                table: IX_TABLE.into(),
                unique: true,
                primary: false,
            }),
        )?;
        run.write(
            "CREATE INDEX t_v_ix ON t (c2 DESC NULLS FIRST)",
            Op::Ddl(DdlOp::CreateIndex {
                name: "t_v_ix".into(),
                table: IX_TABLE.into(),
                unique: false,
                primary: false,
            }),
        )?;
        run.commit()?;
    }
    let per_txn: i64 = if big { 36 } else { 14 };
    for i in 0..=ntx {
        let last = i == ntx;
        let tag = i64::from(round) * 1000 + i64::from(i);
        let keys = model_keys(run, IX_TABLE);
        let has_big = run.model.ddl.rels.contains_key(IX_BIG_INDEX);
        if big && !last && !has_big && keys.len() >= IX_BIG_ROWS {
            run.begin(&format!("ix-build-{round}-{i}"))?;
            run.write(
                &format!("CREATE INDEX {IX_BIG_INDEX} ON t (c1)"),
                Op::Ddl(DdlOp::CreateIndex {
                    name: IX_BIG_INDEX.into(),
                    table: IX_TABLE.into(),
                    unique: false,
                    primary: false,
                }),
            )?;
            finish_txn(run, rng, last, 85)?;
            continue;
        }
        if big && !last && has_big && rng.chance(12) {
            run.begin(&format!("ix-unbuild-{round}-{i}"))?;
            run.write(
                &format!("DROP INDEX {IX_BIG_INDEX}"),
                Op::Ddl(DdlOp::DropIndex(IX_BIG_INDEX.into())),
            )?;
            finish_txn(run, rng, last, 85)?;
            continue;
        }
        run.begin(&format!("ix-{round}-{i}"))?;
        let mut next_seq = keys.last().map_or(0, |k| k + 1).max(0);
        let mut taken: BTreeSet<i64> = keys.iter().copied().collect();
        // 追加: 連番とランダムなキー。
        for j in 0..per_txn {
            let key = if rng.chance(50) {
                let k = next_seq;
                next_seq += 1;
                k
            } else {
                loop {
                    let k = 10_000_000 + below_i64(rng, 1_000_000);
                    if !taken.contains(&k) {
                        break k;
                    }
                }
            };
            taken.insert(key);
            let v = if rng.chance(15) {
                "NULL".to_owned()
            } else {
                below_i64(rng, 40).to_string()
            };
            run.insert(IX_TABLE, key, vec![ix_s(tag * 100 + j), v])?;
        }
        // 変更: 非キー列、キー、削除。確定済みの行のうち、このトランザクションが触っていないものだけ。
        let n_change = if keys.is_empty() { 0 } else { 3 + rng.below(4) };
        for _ in 0..n_change {
            let k = keys[pick_index(rng, keys.len())];
            if run.pending_touched(IX_TABLE, k) {
                continue;
            }
            let mut row = run.model_row(IX_TABLE, k);
            match rng.below(5) {
                0 | 1 => {
                    row[1] = below_i64(rng, 40).to_string();
                    run.set_row(IX_TABLE, k, row)?;
                }
                2 => {
                    row[0] = ix_s(tag * 100 + 50 + below_i64(rng, 40));
                    if run.model.tables[IX_TABLE].values().any(|r| r[0] == row[0])
                        || run.pending_has_text(IX_TABLE, &row[0])
                    {
                        continue;
                    }
                    run.set_row(IX_TABLE, k, row)?;
                }
                3 => {
                    let new_key = 20_000_000 + tag * 100 + below_i64(rng, 100);
                    if taken.contains(&new_key) {
                        continue;
                    }
                    taken.insert(new_key);
                    run.write(
                        &format!("UPDATE t SET k = {new_key} WHERE k = {k}"),
                        Op::Delete {
                            table: IX_TABLE.into(),
                            key: k,
                        },
                    )?;
                    run.record(Op::Insert {
                        table: IX_TABLE.into(),
                        key: new_key,
                        row,
                    });
                }
                _ => run.delete(IX_TABLE, k)?,
            }
        }
        finish_txn(run, rng, last, 80)?;
    }
    Ok(())
}

impl Run {
    /// 開いているトランザクションが、`table` の列 `c1`（第 1 列）に `text` を入れたか。
    fn pending_has_text(&self, table: &str, text: &str) -> bool {
        self.pending[W].as_ref().is_some_and(|l| {
            l.ops.iter().any(|o| match o {
                Op::Insert { table: t, row, .. } | Op::Update { table: t, row, .. } => {
                    t == table && row.first().is_some_and(|c| c == text)
                }
                _ => false,
            })
        })
    }
}

// (7) sequences: 部品 A〜L（08 §7.4。L は「ページの追い出し」）。

const SEQ_SETUP: &[(&str, &str)] = &[
    ("sq_a", "CREATE SEQUENCE sq_a CACHE 1"),
    ("sq_c", "CREATE SEQUENCE sq_c CACHE 5"),
    ("sq_i", "CREATE SEQUENCE sq_i INCREMENT 3 START 10"),
    ("sq_s", "CREATE SEQUENCE sq_s"),
];

/// 部品の並び（小さい版は 1 周で A〜L を全部通る）。
const SEQ_PLAN: [char; 11] = ['A', 'C', 'B', 'D', 'E', 'G', 'I', 'L', 'J', 'H', 'F'];

/// シーケンスのページをディスクへ書き出す（バッファの追い出しの代わり）。
fn evict_sequence(run: &mut Run, name: &str) -> Step {
    let cluster = Arc::clone(&run.cluster);
    let r = catch_unwind(AssertUnwindSafe(|| {
        let defs = yuzhu_core::testing::user_relation_defs(&cluster, "postgres")?;
        match defs.iter().find(|d| d.name == name) {
            Some(d) => cluster.stack().pool.flush_relation_buffers(d.locator),
            None => Ok(()),
        }
    }));
    match r {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(run.failure("evict", &format!("{e:?}"), false)),
        Err(_) => Err(run.failure("evict", "panic", true)),
    }
}

/// 自動コミットの `nextval` を確定として記録する。
fn auto_nextval(run: &mut Run, who: usize, name: &str) -> Step {
    let v = run.nextval(who, name)?;
    run.model.seq.apply(&SeqOp::Confirm {
        name: name.into(),
        value: v,
    });
    run.maybe_checkpoint()
}

fn sequences(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    if !run.model.seq.enforce {
        run.model.seq.enforce = true;
        run.model.ddl.enforce = true;
        run.model.seq.track_serial_table("t7");
    }
    if !run.model.tables.contains_key("t7") {
        run.begin("seq-setup")?;
        run.write(
            "CREATE TABLE t7 (id serial, who int)",
            Op::Create("t7".into()),
        )?;
        run.record(Op::Ddl(DdlOp::CreateSeq {
            name: "t7_id_seq".into(),
            owner: Some("t7".into()),
        }));
        run.record(Op::Seq(SeqOp::Create("t7_id_seq".into())));
        for (name, sql) in SEQ_SETUP {
            run.write(
                sql,
                Op::Ddl(DdlOp::CreateSeq {
                    name: (*name).into(),
                    owner: None,
                }),
            )?;
            run.record(Op::Seq(SeqOp::Create((*name).into())));
        }
        run.commit()?;
    }
    let rot = pick_index(rng, SEQ_PLAN.len());
    for i in 0..=ntx {
        let last = i == ntx;
        // 他セッションの未コミットのトランザクションは、次の部品の前に閉じる（DDL の待ちを避ける）。
        run.close_extra(R)?;
        let part = if last {
            'F'
        } else if (i as usize) < SEQ_PLAN.len() {
            SEQ_PLAN[(i as usize + rot) % SEQ_PLAN.len()]
        } else {
            SEQ_PLAN[pick_index(rng, SEQ_PLAN.len())]
        };
        let tag = format!("{round}_{i}");
        match part {
            'A' => auto_nextval(run, W, "sq_a")?,
            'B' => {
                auto_nextval(run, W, "sq_c")?;
                auto_nextval(run, W, "sq_i")?;
            }
            'C' => {
                run.begin(&format!("seq-c-{tag}"))?;
                let who = i64::from(round) * 1000 + i64::from(i);
                run.exec(W, &format!("INSERT INTO t7 (who) VALUES ({who})"))?;
                let id = run.value(W, "SELECT currval('t7_id_seq')")?;
                run.model.seq.returned("t7_id_seq", id);
                run.record(Op::Insert {
                    table: "t7".into(),
                    key: id,
                    row: vec![who.to_string()],
                });
                run.record(Op::Seq(SeqOp::Confirm {
                    name: "t7_id_seq".into(),
                    value: id,
                }));
                run.record(Op::Seq(SeqOp::T7Id(id)));
                finish_txn(run, rng, false, 85)?;
            }
            'D' => {
                run.begin(&format!("seq-d-{tag}"))?;
                for _ in 0..=rng.below(3) {
                    let name = ["sq_a", "sq_c", "sq_i"][pick_index(rng, 3)];
                    let v = run.nextval(W, name)?;
                    run.record(Op::Seq(SeqOp::Confirm {
                        name: name.into(),
                        value: v,
                    }));
                }
                run.maybe_checkpoint()?;
                run.commit()?;
            }
            'E' => {
                run.begin(&format!("seq-e-{tag}"))?;
                run.nextval(W, "sq_a")?;
                run.maybe_checkpoint()?;
                run.rollback()?;
            }
            'F' => {
                // 1 つ目のセッションが SEQ_LOG を書いて（コミットせず）開いたまま、2 つ目が余りから払い出す。
                run.begin_extra(R)?;
                run.nextval(R, "sq_a")?;
                auto_nextval(run, X, "sq_a")?;
            }
            'G' => {
                let base = run
                    .model
                    .seq
                    .seqs
                    .get("sq_s")
                    .and_then(|t| t.any)
                    .map_or(0, |s| s.hi);
                let target = base + 1000;
                run.model.seq.returned("sq_s", target);
                // `setval(.., false)` はその場で（新しいファイルを作らずに）(target, is_called = false) に戻す。
                // `ALTER SEQUENCE .. RESTART` は新しい relfilenode に書く（ロールバックで戻る）ので、
                // 「同じページを上向きに書き直す」経路はこちらで試す。
                if rng.chance(50) {
                    run.exec_no_checkpoint(W, &format!("SELECT setval('sq_s', {target})"))?;
                    run.model.seq.apply(&SeqOp::Setval {
                        name: "sq_s".into(),
                        value: target,
                    });
                } else {
                    run.exec_no_checkpoint(W, &format!("SELECT setval('sq_s', {target}, false)"))?;
                    run.model.seq.apply(&SeqOp::Restart {
                        name: "sq_s".into(),
                        value: target,
                    });
                }
                run.maybe_checkpoint()?;
            }
            'H' => run.checkpoint()?,
            'L' => {
                // チェックポイントの後の最初の `nextval` は `SEQ_LOG` を書く。そのページが（追い出しで）ディスクに出た後に
                // 払い出した値は WAL に載らない。REDO が「ページの LSN が新しい」だけで飛ばすと、この値が戻る。
                run.checkpoint()?;
                auto_nextval(run, W, "sq_a")?;
                evict_sequence(run, "sq_a")?;
                auto_nextval(run, W, "sq_a")?;
                auto_nextval(run, W, "sq_a")?;
            }
            'I' => {
                if rng.chance(50) {
                    // 作成して払い出して、ロールバックする（カタログに残らない）。
                    run.begin(&format!("seq-i-{tag}"))?;
                    run.write(
                        "CREATE SEQUENCE tmp_n",
                        Op::Ddl(DdlOp::CreateSeq {
                            name: "tmp_n".into(),
                            owner: None,
                        }),
                    )?;
                    run.nextval(W, "tmp_n")?;
                    run.rollback()?;
                } else {
                    // コミットする作成と、先に作ったものの削除。
                    let old: Option<String> = run
                        .model
                        .ddl
                        .rels
                        .iter()
                        .find(|(n, r)| {
                            r.kind == 'S' && r.parent.is_none() && n.starts_with("tmp_k")
                        })
                        .map(|(n, _)| n.clone());
                    run.begin(&format!("seq-i2-{tag}"))?;
                    if let Some(n) = old {
                        run.write(
                            &format!("DROP SEQUENCE {n}"),
                            Op::Ddl(DdlOp::DropSeq(n.clone())),
                        )?;
                        run.record(Op::Seq(SeqOp::Drop(n)));
                    } else {
                        let n = format!("tmp_k{tag}");
                        run.write(
                            &format!("CREATE SEQUENCE {n}"),
                            Op::Ddl(DdlOp::CreateSeq {
                                name: n.clone(),
                                owner: None,
                            }),
                        )?;
                        run.record(Op::Seq(SeqOp::Create(n)));
                    }
                    run.commit()?;
                }
            }
            _ => {
                // J: 上向きだけの RESTART。
                let base = run
                    .model
                    .seq
                    .seqs
                    .get("sq_a")
                    .and_then(|t| t.any)
                    .map_or(0, |s| s.hi);
                let target = base + 100;
                run.model.seq.returned("sq_a", target);
                run.exec_no_checkpoint(W, &format!("ALTER SEQUENCE sq_a RESTART WITH {target}"))?;
                run.model.seq.apply(&SeqOp::Restart {
                    name: "sq_a".into(),
                    value: target,
                });
                run.maybe_checkpoint()?;
            }
        }
    }
    Ok(())
}

// (8) ddl_mix: CREATE TABLE（PK / UNIQUE / serial）→ INSERT → CREATE INDEX → UPDATE → DROP INDEX →
// TRUNCATE → DROP TABLE、ALTER TABLE ADD PRIMARY KEY を、コミットとロールバックを混ぜて繰り返す。
// 表は `(k int ..., c1 text ..., c2 int | serial)`。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// `k int PRIMARY KEY`
    Pk,
    /// `k int PRIMARY KEY, c1 text UNIQUE`
    PkUnique,
    /// `k int PRIMARY KEY, c2 serial`
    Serial,
    /// PK なし（あとで `ALTER TABLE ADD PRIMARY KEY`）
    NoPk,
}

impl Shape {
    fn create_sql(self, t: &str) -> String {
        match self {
            Shape::Pk => format!("CREATE TABLE {t} (k int PRIMARY KEY, c1 text, c2 int)"),
            Shape::PkUnique => {
                format!("CREATE TABLE {t} (k int PRIMARY KEY, c1 text UNIQUE, c2 int)")
            }
            Shape::Serial => format!("CREATE TABLE {t} (k int PRIMARY KEY, c1 text, c2 serial)"),
            Shape::NoPk => format!("CREATE TABLE {t} (k int, c1 text, c2 int)"),
        }
    }

    /// 表の作成で一緒にできる索引・シーケンス。
    fn created(self, t: &str) -> Vec<DdlOp> {
        let idx = |name: String, primary: bool| DdlOp::CreateIndex {
            name,
            table: t.into(),
            unique: true,
            primary,
        };
        match self {
            Shape::Pk => vec![idx(format!("{t}_pkey"), true)],
            Shape::PkUnique => vec![
                idx(format!("{t}_pkey"), true),
                idx(format!("{t}_c1_key"), false),
            ],
            Shape::Serial => vec![
                idx(format!("{t}_pkey"), true),
                DdlOp::CreateSeq {
                    name: format!("{t}_c2_seq"),
                    owner: Some(t.into()),
                },
            ],
            Shape::NoPk => Vec::new(),
        }
    }
}

/// 確定したモデルの表の形（カタログの関係から割り出す）。
fn shape_of(run: &Run, t: &str) -> Shape {
    let has = |n: String| run.model.ddl.rels.contains_key(&n);
    if has(format!("{t}_c2_seq")) {
        Shape::Serial
    } else if has(format!("{t}_c1_key")) {
        Shape::PkUnique
    } else if has(format!("{t}_pkey")) {
        Shape::Pk
    } else {
        Shape::NoPk
    }
}

/// 表 `t` へ 1 行入れる（モデルにも記録する）。`serial` の列は `currval` で受け取る。
fn dm_insert(run: &mut Run, t: &str, shape: Shape, key: i64, c1: String, c2: i64) -> Step {
    if shape == Shape::Serial {
        run.exec(
            W,
            &format!("INSERT INTO {t} (k, c1) VALUES ({key}, {})", lit(&c1)),
        )?;
        let id = run.value(W, &format!("SELECT currval('{t}_c2_seq')"))?;
        run.record(Op::Insert {
            table: t.into(),
            key,
            row: vec![c1, id.to_string()],
        });
        Ok(())
    } else {
        run.insert(t, key, vec![c1, c2.to_string()])
    }
}

/// 表 `t` の行の `c1` を書き換える（`serial` の列は触らない）。
fn dm_update(run: &mut Run, t: &str, key: i64, c1: String) -> Step {
    let mut row = run.model_row(t, key);
    row[0] = c1;
    run.write(
        &format!("UPDATE {t} SET c1 = {} WHERE k = {key}", lit(&row[0])),
        Op::Update {
            table: t.into(),
            key,
            row,
        },
    )
}

/// 1 回の繰り返しの種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DmAct {
    NewTable,
    FillAndIndex,
    UpdateAndDropIndex,
    Truncate,
    DropTable,
    AddPk,
}

const DM_PLAN: [DmAct; 9] = [
    DmAct::NewTable,
    DmAct::FillAndIndex,
    DmAct::NewTable,
    DmAct::UpdateAndDropIndex,
    DmAct::Truncate,
    DmAct::AddPk,
    DmAct::NewTable,
    DmAct::DropTable,
    DmAct::FillAndIndex,
];

fn ddl_mix(run: &mut Run, rng: &mut Rng, round: u32, ntx: u32) -> Step {
    run.model.ddl.enforce = true;
    let rot = pick_index(rng, DM_PLAN.len());
    for i in 0..=ntx {
        let last = i == ntx;
        let tag = format!("{round}x{i}");
        let tables: Vec<String> = run.model.tables.keys().cloned().collect();
        let mut act = if (i as usize) < DM_PLAN.len() {
            DM_PLAN[(i as usize + rot) % DM_PLAN.len()]
        } else {
            DM_PLAN[pick_index(rng, DM_PLAN.len())]
        };
        // 前提が足りなければ、新しい表を作る。
        let nopk: Vec<&String> = tables
            .iter()
            .filter(|t| shape_of(run, t) == Shape::NoPk)
            .collect();
        if (tables.is_empty() && act != DmAct::NewTable) || (act == DmAct::AddPk && nopk.is_empty())
        {
            act = DmAct::NewTable;
        }
        let commit_percent = 75;
        run.begin(&format!("dm-{tag}"))?;
        match act {
            DmAct::NewTable => {
                let shape =
                    [Shape::Pk, Shape::PkUnique, Shape::Serial, Shape::NoPk][pick_index(rng, 4)];
                let t = format!("d{tag}");
                run.write(&shape.create_sql(&t), Op::Create(t.clone()))?;
                for d in shape.created(&t) {
                    run.record(Op::Ddl(d));
                }
                for j in 0..(3 + below_i64(rng, 4)) {
                    dm_insert(
                        run,
                        &t,
                        shape,
                        j,
                        uniq_text(&format!("{tag}-{j}"), 30),
                        j * 7,
                    )?;
                }
                if rng.chance(50) {
                    let name = format!("{t}_ix0");
                    run.write(
                        &format!("CREATE INDEX {name} ON {t} (c2)"),
                        Op::Ddl(DdlOp::CreateIndex {
                            name,
                            table: t.clone(),
                            unique: false,
                            primary: false,
                        }),
                    )?;
                }
            }
            DmAct::FillAndIndex => {
                let t = tables[pick_index(rng, tables.len())].clone();
                let shape = shape_of(run, &t);
                let base = model_keys(run, &t).last().map_or(0, |k| k + 1);
                for j in 0..(4 + below_i64(rng, 3)) {
                    dm_insert(
                        run,
                        &t,
                        shape,
                        base + j,
                        uniq_text(&format!("{tag}-{j}"), 40),
                        j,
                    )?;
                }
                let unique = rng.chance(40);
                let (name, col) = (format!("{t}_ix{tag}"), if unique { "c1" } else { "c2" });
                run.write(
                    &format!(
                        "CREATE {}INDEX {name} ON {t} ({col})",
                        if unique { "UNIQUE " } else { "" }
                    ),
                    Op::Ddl(DdlOp::CreateIndex {
                        name,
                        table: t.clone(),
                        unique,
                        primary: false,
                    }),
                )?;
            }
            DmAct::UpdateAndDropIndex => {
                let t = tables[pick_index(rng, tables.len())].clone();
                let keys = model_keys(run, &t);
                for (j, k) in keys.iter().take(3).enumerate() {
                    dm_update(run, &t, *k, uniq_text(&format!("{tag}-u{j}"), 36))?;
                }
                let standalone: Vec<String> = run
                    .model
                    .ddl
                    .rels
                    .iter()
                    .filter(|(n, r)| {
                        r.kind == 'i' && n.contains("_ix") && r.parent.as_ref() == Some(&t)
                    })
                    .map(|(n, _)| n.clone())
                    .collect();
                if !standalone.is_empty() {
                    let n = standalone[pick_index(rng, standalone.len())].clone();
                    run.write(
                        &format!("DROP INDEX {n}"),
                        Op::Ddl(DdlOp::DropIndex(n.clone())),
                    )?;
                }
            }
            DmAct::Truncate => {
                let t = tables[pick_index(rng, tables.len())].clone();
                let shape = shape_of(run, &t);
                let keys = model_keys(run, &t);
                run.exec(W, &format!("TRUNCATE {t}"))?;
                for k in keys {
                    run.record(Op::Delete {
                        table: t.clone(),
                        key: k,
                    });
                }
                for j in 0..2 {
                    dm_insert(run, &t, shape, j, uniq_text(&format!("{tag}-t{j}"), 30), j)?;
                }
            }
            DmAct::DropTable => {
                let t = tables[pick_index(rng, tables.len())].clone();
                run.drop_table(&t)?;
            }
            DmAct::AddPk => {
                let t = nopk[pick_index(rng, nopk.len())].clone();
                run.write(
                    &format!("ALTER TABLE {t} ADD PRIMARY KEY (k)"),
                    Op::Ddl(DdlOp::CreateIndex {
                        name: format!("{t}_pkey"),
                        table: t.clone(),
                        unique: true,
                        primary: true,
                    }),
                )?;
            }
        }
        finish_txn(run, rng, last, commit_percent)?;
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
        expected.extend_from_slice(M4_WORKLOAD_NAMES);
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
