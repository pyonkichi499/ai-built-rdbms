//! シーケンス（`nextval` / `currval` / `lastval` / `setval`）の実行側（`m4/08-sequence-serial.md` §4.4、§5.1、§5.2）。
//!
//! `SeqSession` はセッションが持つ状態（PostgreSQL の `SeqTableData` と `last_used_seq`）、
//! `SeqRuntime` は文ごとに作る `RuntimeInfo` の 4 つのメソッドの実装本体。

use std::cell::RefCell;
use std::collections::HashMap;

use crate::catalog::{CatalogReader, RelKind, TableDef};
use crate::error::{Error, Result, sqlstate};
use crate::storage::{SequenceHandle, SequenceStore};
use crate::types::Oid;
use crate::wal::Lsn;

#[derive(Clone, Copy, Debug, Default)]
struct SeqEntry {
    /// `currval` が定義済みか（`nextval` か `setval(.., true)` をこのセッションでした）。
    last_valid: bool,
    /// 最後に返した値（`currval`。`lastval` の元）。
    last: i64,
    /// 先取りした最後の値。`last != cached` なら先取りの残りがある。
    cached: i64,
    /// 先取りしたときの `increment`。
    increment: i64,
}

#[derive(Debug, Default)]
pub struct SeqSession {
    entries: HashMap<Oid, SeqEntry>,
    /// `lastval`。
    last_used: Option<Oid>,
    seen_reset_gen: u64,
    /// この文で払い出した値を覆う WAL の最大の LSN。文の終わりに session が `Transaction.wal_flush_upto` へ反映する。
    pending_flush: Lsn,
    /// 文の中で引いた `SequenceHandle`（同じ文のカタログのスナップショットは変わらないので使い回す）。
    handles: HashMap<Oid, SequenceHandle>,
}

impl SeqSession {
    pub fn new() -> SeqSession {
        SeqSession::default()
    }

    /// 文の終わり（成功も失敗も）に session が呼ぶ。`pending_flush` を返して 0 に戻し、`handles` を捨てる。
    pub fn end_statement(&mut self) -> Lsn {
        self.handles.clear();
        std::mem::take(&mut self.pending_flush)
    }

    /// `DISCARD SEQUENCES` 相当（M4 では呼ぶ文がない）。
    pub fn discard(&mut self) {
        self.entries.clear();
        self.last_used = None;
    }

    /// `reset` で世代が進んでいたら全シーケンスの先取り分を捨てる（D8-9）。`currval` の状態は触らない。
    fn sync_generation(&mut self, current: u64) {
        if self.seen_reset_gen != current {
            for e in self.entries.values_mut() {
                e.cached = e.last;
            }
            self.seen_reset_gen = current;
        }
    }

    fn note_flush(&mut self, lsn: Lsn) {
        self.pending_flush = self.pending_flush.max(lsn);
    }
}

/// `def` から `SequenceHandle` を作る。シーケンスでなければ内部エラー。
pub fn handle_from_def(def: &TableDef) -> Result<SequenceHandle> {
    match (def.kind, def.sequence) {
        (RelKind::Sequence, Some(params)) => Ok(SequenceHandle {
            oid: def.oid,
            name: def.name.clone(),
            locator: def.locator,
            params,
        }),
        _ => Err(Error::internal(format!(
            "relation \"{}\" is not a sequence",
            def.name
        ))),
    }
}

/// `RuntimeInfo` の `nextval` / `currval` / `lastval` / `setval` の実装本体。文ごとに session が作る。
pub struct SeqRuntime<'a> {
    pub state: &'a RefCell<SeqSession>,
    pub store: &'a dyn SequenceStore,
    pub catalog: &'a dyn CatalogReader,
    /// 現在のトランザクションが READ ONLY（`transaction_read_only`）。
    pub read_only: bool,
}

impl std::fmt::Debug for SeqRuntime<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeqRuntime")
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

fn undefined_lastval() -> Error {
    Error::new(
        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
        "lastval is not yet defined in this session",
    )
}

impl SeqRuntime<'_> {
    /// `nextval` / `currval` / `setval` が共有する。文のキャッシュ → カタログの順に引く。
    fn handle(&self, oid: Oid) -> Result<SequenceHandle> {
        if let Some(h) = self.state.borrow().handles.get(&oid) {
            return Ok(h.clone());
        }
        let def = self
            .catalog
            .table_by_oid(oid)?
            .ok_or_else(|| Error::internal(format!("could not open relation with OID {oid}")))?;
        if def.kind != RelKind::Sequence {
            let what = if def.kind == RelKind::Index {
                "indexes"
            } else {
                "tables"
            };
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{}\"", def.name),
            )
            .with_detail(format!("This operation is not supported for {what}.")));
        }
        let h = handle_from_def(&def)?;
        self.state.borrow_mut().handles.insert(oid, h.clone());
        Ok(h)
    }

    fn check_writable(&self, func: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::new(
                sqlstate::READ_ONLY_SQL_TRANSACTION,
                format!("cannot execute {func}() in a read-only transaction"),
            ));
        }
        Ok(())
    }

    pub fn nextval(&self, seq: Oid) -> Result<i64> {
        let h = self.handle(seq)?;
        self.check_writable("nextval")?;
        {
            let mut s = self.state.borrow_mut();
            s.sync_generation(self.store.reset_generation());
            let e = s.entries.entry(seq).or_default();
            if e.last != e.cached {
                // 先取りの残り（CACHE）。WAL もラッチも使わない。
                e.last += e.increment;
                let v = e.last;
                s.last_used = Some(seq);
                return Ok(v);
            }
        }
        let cache = u32::try_from(h.params.cache.max(1)).unwrap_or(u32::MAX);
        // 失敗（2200H など）ならセッションの状態は変えない。
        let run = self.store.fetch(&h, cache)?;
        let count = i64::from(run.count.max(1));
        let cached = count
            .checked_sub(1)
            .and_then(|n| n.checked_mul(run.increment))
            .and_then(|d| run.first.checked_add(d))
            .ok_or_else(|| Error::internal("sequence run overflows bigint"))?;
        let mut s = self.state.borrow_mut();
        let e = s.entries.entry(seq).or_default();
        e.increment = run.increment;
        e.last = run.first;
        e.cached = cached;
        e.last_valid = true;
        s.last_used = Some(seq);
        s.note_flush(run.wal_lsn);
        Ok(run.first)
    }

    pub fn currval(&self, seq: Oid) -> Result<i64> {
        let h = self.handle(seq)?;
        match self.state.borrow().entries.get(&seq) {
            Some(e) if e.last_valid => Ok(e.last),
            _ => Err(Error::new(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "currval of sequence \"{}\" is not yet defined in this session",
                    h.name
                ),
            )),
        }
    }

    pub fn lastval(&self) -> Result<i64> {
        let Some(oid) = self.state.borrow().last_used else {
            return Err(undefined_lastval());
        };
        match self.catalog.table_by_oid(oid)? {
            Some(def) if def.kind == RelKind::Sequence => {}
            _ => return Err(undefined_lastval()),
        }
        self.state
            .borrow()
            .entries
            .get(&oid)
            .map(|e| e.last)
            .ok_or_else(undefined_lastval)
    }

    pub fn setval(&self, seq: Oid, value: i64, is_called: bool) -> Result<i64> {
        let h = self.handle(seq)?;
        self.check_writable("setval")?;
        let lsn = self.store.setval(&h, value, is_called)?;
        let mut s = self.state.borrow_mut();
        let e = s.entries.entry(seq).or_default();
        if is_called {
            e.last = value;
            e.last_valid = true;
        }
        // 先取りを捨てる。lastval は変えない（PostgreSQL は setval では last_used_seq を更新しない）。
        e.cached = e.last;
        s.note_flush(lsn);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SequenceParams;
    use crate::catalog::fake::{FakeCatalog, TableBuilder};
    use crate::storage::WriteCtx;
    use crate::storage::sequence::SeqStore;
    use crate::storage::testing::TestStorage;
    use crate::txn::Xid;
    use crate::types::{SqlType, oid};

    fn params(cache: i64, increment: i64, max: i64) -> SequenceParams {
        SequenceParams {
            type_oid: oid::INT8,
            start: if increment > 0 { 1 } else { max },
            increment,
            min: if increment > 0 { 1 } else { i64::MIN },
            max,
            cache,
            cycle: false,
            owned_by: None,
        }
    }

    struct Rig {
        ts: TestStorage,
        store: SeqStore,
        cat: FakeCatalog,
        sess: RefCell<SeqSession>,
    }

    impl Rig {
        fn new() -> Rig {
            let ts = TestStorage::new();
            let store = SeqStore::new(
                std::sync::Arc::clone(ts.pool()),
                std::sync::Arc::clone(ts.smgr()),
                std::sync::Arc::clone(ts.wal()),
            );
            Rig {
                ts,
                store,
                cat: FakeCatalog::new("yuzhu"),
                sess: RefCell::new(SeqSession::new()),
            }
        }

        fn seq(&mut self, name: &str, p: SequenceParams) -> Oid {
            let def = self.cat.add_sequence(name, p);
            self.ts.create_rel(def.locator).unwrap();
            let h = handle_from_def(&def).unwrap();
            self.store
                .init(
                    &WriteCtx {
                        xid: Xid::FIRST_NORMAL,
                        cid: 0,
                    },
                    &h,
                )
                .unwrap();
            def.oid
        }

        fn rt(&self, read_only: bool) -> SeqRuntime<'_> {
            SeqRuntime {
                state: &self.sess,
                store: &self.store,
                catalog: &self.cat,
                read_only,
            }
        }
    }

    fn code<T: std::fmt::Debug>(r: Result<T>) -> (String, String) {
        let e = r.unwrap_err();
        (e.sqlstate.0.to_string(), e.message)
    }

    #[test]
    fn nextval_currval_lastval() {
        let mut r = Rig::new();
        let s = r.seq("s", params(1, 1, i64::MAX));
        let rt = r.rt(false);
        assert_eq!(
            code(rt.currval(s)),
            (
                "55000".into(),
                "currval of sequence \"s\" is not yet defined in this session".into()
            )
        );
        assert_eq!(
            code(rt.lastval()),
            (
                "55000".into(),
                "lastval is not yet defined in this session".into()
            )
        );
        assert_eq!(rt.nextval(s).unwrap(), 1);
        assert_eq!(rt.nextval(s).unwrap(), 2);
        assert_eq!(rt.currval(s).unwrap(), 2);
        assert_eq!(rt.lastval().unwrap(), 2);
        r.ts.assert_clean();
    }

    #[test]
    fn cache_is_consumed_without_the_store() {
        let mut r = Rig::new();
        let s = r.seq("c", params(5, 2, i64::MAX));
        let rt = r.rt(false);
        let before = r.ts.wal().insert_lsn();
        assert_eq!(rt.nextval(s).unwrap(), 1);
        let after_first = r.ts.wal().insert_lsn();
        assert!(after_first > before);
        for expect in [3, 5, 7, 9] {
            assert_eq!(rt.nextval(s).unwrap(), expect);
        }
        assert_eq!(r.ts.wal().insert_lsn(), after_first);
        // 6 個目はストアから: 先取りの最後（9）の次。
        assert_eq!(rt.nextval(s).unwrap(), 11);
        r.ts.assert_clean();
    }

    #[test]
    fn two_sessions_share_the_cache_boundary() {
        let mut r = Rig::new();
        let s = r.seq("c", params(3, 1, i64::MAX));
        let other = RefCell::new(SeqSession::new());
        assert_eq!(r.rt(false).nextval(s).unwrap(), 1);
        let rt2 = SeqRuntime {
            state: &other,
            store: &r.store,
            catalog: &r.cat,
            read_only: false,
        };
        assert_eq!(rt2.nextval(s).unwrap(), 4);
        assert_eq!(r.rt(false).nextval(s).unwrap(), 2);
        assert_eq!(rt2.nextval(s).unwrap(), 5);
    }

    #[test]
    fn setval_semantics() {
        let mut r = Rig::new();
        let s = r.seq("s", params(1, 1, 100));
        let rt = r.rt(false);
        assert_eq!(rt.setval(s, 5, false).unwrap(), 5);
        assert_eq!(code(rt.currval(s)).0, "55000");
        assert_eq!(rt.nextval(s).unwrap(), 5);
        assert_eq!(rt.setval(s, 50, true).unwrap(), 50);
        assert_eq!(rt.currval(s).unwrap(), 50);
        assert_eq!(rt.nextval(s).unwrap(), 51);
        let e = code(rt.setval(s, 101, true));
        assert_eq!(e.0, "22003");
        assert_eq!(
            e.1,
            "setval: value 101 is out of bounds for sequence \"s\" (1..100)"
        );
    }

    #[test]
    fn setval_does_not_define_lastval_but_updates_it() {
        let mut r = Rig::new();
        let a = r.seq("a", params(1, 1, i64::MAX));
        let b = r.seq("b", params(1, 1, i64::MAX));
        let rt = r.rt(false);
        rt.setval(a, 10, true).unwrap();
        assert_eq!(code(rt.lastval()).0, "55000");
        rt.nextval(b).unwrap();
        assert_eq!(rt.lastval().unwrap(), 1);
        rt.setval(b, 40, true).unwrap();
        assert_eq!(rt.lastval().unwrap(), 40);
        assert_eq!(rt.nextval(b).unwrap(), 41);
    }

    #[test]
    fn setval_drops_the_cache() {
        let mut r = Rig::new();
        let s = r.seq("c", params(5, 1, i64::MAX));
        let rt = r.rt(false);
        assert_eq!(rt.nextval(s).unwrap(), 1);
        rt.setval(s, 100, true).unwrap();
        assert_eq!(rt.nextval(s).unwrap(), 101);
    }

    #[test]
    fn read_only_even_with_a_cache_left() {
        let mut r = Rig::new();
        let s = r.seq("c", params(5, 1, i64::MAX));
        assert_eq!(r.rt(false).nextval(s).unwrap(), 1);
        let e = code(r.rt(true).nextval(s));
        assert_eq!(
            e,
            (
                "25006".into(),
                "cannot execute nextval() in a read-only transaction".into()
            )
        );
        assert_eq!(
            code(r.rt(true).setval(s, 3, true)).1,
            "cannot execute setval() in a read-only transaction"
        );
        // currval は read-only でも読める。
        assert_eq!(r.rt(true).currval(s).unwrap(), 1);
    }

    #[test]
    fn limit_error_leaves_the_session_untouched() {
        let mut r = Rig::new();
        let s = r.seq("c3", params(3, 1, 3));
        let rt = r.rt(false);
        assert_eq!(rt.nextval(s).unwrap(), 1);
        assert_eq!(rt.nextval(s).unwrap(), 2);
        assert_eq!(rt.nextval(s).unwrap(), 3);
        let e = code(rt.nextval(s));
        assert_eq!(
            e,
            (
                "2200H".into(),
                "nextval: reached maximum value of sequence \"c3\" (3)".into()
            )
        );
        assert_eq!(rt.currval(s).unwrap(), 3);
    }

    #[test]
    fn handle_errors() {
        let mut r = Rig::new();
        let t = r
            .cat
            .add(&TableBuilder::new("t").column("a", SqlType::INT4));
        let rt = r.rt(false);
        let e = rt.nextval(t.oid).unwrap_err();
        assert_eq!(e.sqlstate.0, "42809");
        assert_eq!(e.message, "cannot open relation \"t\"");
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for tables.")
        );
        let e = rt.currval(99999).unwrap_err();
        assert_eq!(e.sqlstate.0, "XX000");
        assert_eq!(e.message, "could not open relation with OID 99999");
    }

    #[test]
    fn handles_live_for_one_statement() {
        let mut r = Rig::new();
        let s = r.seq("s", params(1, 1, i64::MAX));
        let rt = r.rt(false);
        rt.nextval(s).unwrap();
        assert!(r.sess.borrow().handles.contains_key(&s));
        r.sess.borrow_mut().end_statement();
        assert!(r.sess.borrow().handles.is_empty());
    }

    #[test]
    fn end_statement_returns_the_flush_lsn_and_resets_it() {
        let mut r = Rig::new();
        let s = r.seq("s", params(1, 1, i64::MAX));
        assert_eq!(r.sess.borrow_mut().end_statement(), Lsn(0));
        r.rt(false).nextval(s).unwrap();
        let lsn = r.sess.borrow_mut().end_statement();
        assert!(lsn > Lsn(0));
        assert_eq!(r.sess.borrow_mut().end_statement(), Lsn(0));
    }

    #[test]
    fn reset_generation_drops_the_cache_but_not_currval() {
        let mut r = Rig::new();
        let s = r.seq("c", params(5, 1, i64::MAX));
        assert_eq!(r.rt(false).nextval(s).unwrap(), 1);
        let def = r.cat.table_by_oid(s).unwrap().unwrap();
        let h = handle_from_def(&def).unwrap();
        r.store.reset(&h, &h.params, Some(100)).unwrap();
        let rt = r.rt(false);
        assert_eq!(rt.currval(s).unwrap(), 1);
        assert_eq!(rt.nextval(s).unwrap(), 100);
        assert_eq!(rt.currval(s).unwrap(), 100);
    }

    #[test]
    fn lastval_after_drop_is_undefined() {
        let mut r = Rig::new();
        let s = r.seq("s", params(1, 1, i64::MAX));
        r.rt(false).nextval(s).unwrap();
        r.cat.remove_table(s);
        assert_eq!(code(r.rt(false).lastval()).0, "55000");
    }

    #[test]
    fn discard_clears_the_session() {
        let mut r = Rig::new();
        let s = r.seq("s", params(1, 1, i64::MAX));
        r.rt(false).nextval(s).unwrap();
        r.sess.borrow_mut().discard();
        assert_eq!(code(r.rt(false).currval(s)).0, "55000");
        assert_eq!(code(r.rt(false).lastval()).0, "55000");
    }

    #[test]
    fn handle_from_def_rejects_tables() {
        let mut r = Rig::new();
        let t = r
            .cat
            .add(&TableBuilder::new("t").column("a", SqlType::INT4));
        assert_eq!(handle_from_def(&t).unwrap_err().sqlstate.0, "XX000");
    }
}
