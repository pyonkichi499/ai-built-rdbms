//! 確定・不明なトランザクションの記録と `BTreeMap` のモデル（`m3.md` §7.5）。
//!
//! モデルは「確定したトランザクションを順に適用した状態」。COMMIT を呼んだが `Ok` で戻らなかった
//! トランザクションは「不明」として別に持ち、リカバリ後の内容は「モデルに不明の任意の部分集合を
//! 順に加えたもの」のどれかと一致しなければならない（I4）。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// キー以外の列を文字列にしたもの。
pub(crate) type Row = Vec<String>;
/// 1 テーブルの内容（キーは第 1 列の整数）。
pub(crate) type Table = BTreeMap<i64, Row>;
/// ユーザーテーブル全体（`public` スキーマ）。`SELECT` の結果もこの形に直して比べる。
pub(crate) type Tables = BTreeMap<String, Table>;

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // `Seq` はワークロード 7（R2b）が使う。
pub(crate) enum Op {
    Create(String),
    Drop(String),
    Insert {
        table: String,
        key: i64,
        row: Row,
    },
    Update {
        table: String,
        key: i64,
        row: Row,
    },
    Delete {
        table: String,
        key: i64,
    },
    /// 索引・シーケンスの作成と削除（表は `Create` / `Drop`。M4。I16）。
    Ddl(DdlOp),
    /// シーケンスの確定した払い出しなど（M4。I15）。
    Seq(SeqOp),
}

// ----- M4: カタログ（I16）のモデル ------------------------------------------------

/// カタログにあるべき関係 1 つ（`public`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DdlRel {
    /// `r`（表）/ `i`（索引）/ `S`（シーケンス）。
    pub(crate) kind: char,
    /// 索引なら表、`OWNED BY`（`serial`）のシーケンスなら表。
    pub(crate) parent: Option<String>,
    pub(crate) unique: bool,
    pub(crate) primary: bool,
}

impl DdlRel {
    pub(crate) fn table() -> DdlRel {
        DdlRel {
            kind: 'r',
            parent: None,
            unique: false,
            primary: false,
        }
    }
}

/// 索引・シーケンスの DDL。表の作成・削除は [`Op::Create`] / [`Op::Drop`]（表を落とすと、その索引と
/// 所有するシーケンスも消える）。
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // ワークロード 6〜8（R2b）が使う。
pub(crate) enum DdlOp {
    CreateIndex {
        name: String,
        table: String,
        unique: bool,
        primary: bool,
    },
    DropIndex(String),
    CreateSeq {
        name: String,
        owner: Option<String>,
    },
    DropSeq(String),
}

/// コミットしたカタログの関係の集合。`enforce` が真のときだけ、リカバリ後のカタログとの一致を求める
/// （M3 のワークロードは索引・シーケンスを数えないので偽のまま）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DdlModel {
    pub(crate) enforce: bool,
    pub(crate) rels: BTreeMap<String, DdlRel>,
}

impl DdlModel {
    /// 一致を求める。表の `Create` / `Drop` は `enforce` に関係なく記録している。
    pub(crate) fn enforcing() -> DdlModel {
        DdlModel {
            enforce: true,
            rels: BTreeMap::new(),
        }
    }

    fn create(&mut self, name: &str, rel: DdlRel) {
        if let Some(p) = &rel.parent {
            assert!(
                self.rels.contains_key(p),
                "model: {name} refers to the missing table {p}"
            );
        }
        let prev = self.rels.insert(name.to_owned(), rel);
        assert!(prev.is_none(), "model: relation {name} already exists");
    }

    /// 関係と、それに従属するもの（索引・所有されたシーケンス）を消す。
    fn drop_rel(&mut self, name: &str) {
        assert!(
            self.rels.remove(name).is_some(),
            "model: dropping missing relation {name}"
        );
        self.rels.retain(|_, r| r.parent.as_deref() != Some(name));
    }

    pub(crate) fn apply(&mut self, op: &DdlOp) {
        match op {
            DdlOp::CreateIndex {
                name,
                table,
                unique,
                primary,
            } => self.create(
                name,
                DdlRel {
                    kind: 'i',
                    parent: Some(table.clone()),
                    unique: *unique || *primary,
                    primary: *primary,
                },
            ),
            DdlOp::CreateSeq { name, owner } => self.create(
                name,
                DdlRel {
                    kind: 'S',
                    parent: owner.clone(),
                    unique: false,
                    primary: false,
                },
            ),
            DdlOp::DropIndex(n) | DdlOp::DropSeq(n) => self.drop_rel(n),
        }
    }

    /// `actual` との差の要約。一致なら `None`。
    pub(crate) fn mismatch(&self, actual: &BTreeMap<String, DdlRel>) -> Option<String> {
        if self.rels == *actual {
            return None;
        }
        let mut out = Vec::new();
        for (n, r) in &self.rels {
            match actual.get(n) {
                None => out.push(format!("{} {n} missing", r.kind)),
                Some(a) if a != r => out.push(format!("{n} expected {r:?} got {a:?}")),
                Some(_) => {}
            }
        }
        for (n, a) in actual {
            if !self.rels.contains_key(n) {
                out.push(format!("{} {n} extra", a.kind));
            }
        }
        let k = out.len();
        out.truncate(6);
        let mut s = out.join(", ");
        if k > 6 {
            let _ = write!(s, ", ... ({k} differences)");
        }
        Some(s)
    }
}

// ----- M4: シーケンス（I15）のモデル ----------------------------------------------

/// 値の最小と最大。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) lo: i64,
    pub(crate) hi: i64,
}

impl Span {
    pub(crate) fn push(this: &mut Option<Span>, v: i64) {
        *this = Some(match *this {
            None => Span { lo: v, hi: v },
            Some(s) => Span {
                lo: s.lo.min(v),
                hi: s.hi.max(v),
            },
        });
    }

    /// 増分 `inc` の向きで「いちばん先まで進んだ値」。
    pub(crate) fn farthest(self, inc: i64) -> i64 {
        if inc >= 0 { self.hi } else { self.lo }
    }
}

/// 1 つのシーケンスについて、払い出した値の記録。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SeqTrack {
    /// 作成がコミットされた（`returned` だけで作った記録は偽）。
    pub(crate) created: bool,
    /// 「次の値はこれより先」と言える値: 確定した `nextval` の結果と `setval` の値。
    pub(crate) above: Option<Span>,
    /// 「次の値はこれ以上（向きは増分）」と言える値: `ALTER SEQUENCE RESTART WITH`。
    pub(crate) restart: Option<Span>,
    /// 確定・不明・ロールバックを含めて、どのセッションにも返した（または設定した）値。
    pub(crate) any: Option<Span>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // ワークロード 7（R2b）が使う。
pub(crate) enum SeqOp {
    /// 作成（同じ名前の記録を捨てる）。
    Create(String),
    Drop(String),
    /// COMMIT された `nextval` の結果。
    Confirm {
        name: String,
        value: i64,
    },
    /// COMMIT された `setval(name, value)`（`is_called = true`）。
    Setval {
        name: String,
        value: i64,
    },
    /// COMMIT された `ALTER SEQUENCE .. RESTART WITH value`。
    Restart {
        name: String,
        value: i64,
    },
    /// COMMIT された `t7` の行の `id`。
    T7Id(i64),
}

/// `serial` の表の名前と、確定した行の `id`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct T7 {
    pub(crate) table: String,
    pub(crate) ids: BTreeSet<i64>,
}

/// I15 のモデル。`enforce` が真のときだけ検査する。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SeqModel {
    pub(crate) enforce: bool,
    pub(crate) seqs: BTreeMap<String, SeqTrack>,
    pub(crate) t7: Option<T7>,
}

impl SeqModel {
    pub(crate) fn enforcing() -> SeqModel {
        SeqModel {
            enforce: true,
            ..SeqModel::default()
        }
    }

    /// 表 `table`（`id serial`）の重複なし検査を有効にする。
    pub(crate) fn track_serial_table(&mut self, table: &str) {
        self.t7 = Some(T7 {
            table: table.into(),
            ids: BTreeSet::new(),
        });
    }

    fn track(&mut self, name: &str) -> &mut SeqTrack {
        self.seqs.entry(name.to_owned()).or_default()
    }

    /// 結果が確定していない払い出し（ロールバック・不明・他セッションの未コミットを含む）。上限だけが
    /// 動く。リカバリの前に `Run.model` へ直接書いてよい（単調）。
    pub(crate) fn returned(&mut self, name: &str, value: i64) {
        Span::push(&mut self.track(name).any, value);
    }

    pub(crate) fn apply(&mut self, op: &SeqOp) {
        match op {
            SeqOp::Create(n) => {
                // 作り直しでも、これまでに返した値（`any`）は捨てない（上限が緩むだけ）。
                let t = self.track(n);
                t.created = true;
                t.above = None;
                t.restart = None;
            }
            SeqOp::Drop(n) => {
                self.seqs.remove(n);
            }
            SeqOp::Confirm { name, value } | SeqOp::Setval { name, value } => {
                let t = self.track(name);
                Span::push(&mut t.above, *value);
                Span::push(&mut t.any, *value);
            }
            SeqOp::Restart { name, value } => {
                let t = self.track(name);
                Span::push(&mut t.restart, *value);
                Span::push(&mut t.any, *value);
            }
            SeqOp::T7Id(id) => {
                if let Some(t7) = &mut self.t7 {
                    t7.ids.insert(*id);
                }
            }
        }
    }
}

/// 1 つのトランザクションが行った変更（COMMIT されたら全部効く）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TxnLog {
    pub(crate) label: String,
    pub(crate) ops: Vec<Op>,
}

/// 確定済みの状態。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Model {
    pub(crate) tables: Tables,
    /// 索引・シーケンスを含むカタログの関係（I16）。
    pub(crate) ddl: DdlModel,
    /// シーケンスの払い出し（I15）。
    pub(crate) seq: SeqModel,
}

impl Model {
    /// トランザクション 1 つを適用する。ハーネスが SQL と同時に記録するので、矛盾したら
    /// ハーネスのバグ（panic）。
    pub(crate) fn apply(&mut self, t: &TxnLog) {
        for op in &t.ops {
            self.apply_op(op);
        }
    }

    pub(crate) fn apply_op(&mut self, op: &Op) {
        match op {
            Op::Create(t) => {
                let prev = self.tables.insert(t.clone(), Table::new());
                assert!(prev.is_none(), "model: table {t} already exists");
                self.ddl.create(t, DdlRel::table());
            }
            Op::Drop(t) => {
                assert!(
                    self.tables.remove(t).is_some(),
                    "model: dropping missing table {t}"
                );
                self.ddl.drop_rel(t);
            }
            Op::Insert { table, key, row } => {
                let prev = self.table_mut(table).insert(*key, row.clone());
                assert!(prev.is_none(), "model: duplicate key {key} in {table}");
            }
            Op::Update { table, key, row } => {
                let prev = self.table_mut(table).insert(*key, row.clone());
                assert!(
                    prev.is_some(),
                    "model: updating missing key {key} in {table}"
                );
            }
            Op::Delete { table, key } => {
                assert!(
                    self.table_mut(table).remove(key).is_some(),
                    "model: deleting missing key {key} in {table}"
                );
            }
            Op::Ddl(d) => self.ddl.apply(d),
            Op::Seq(s) => self.seq.apply(s),
        }
    }

    fn table_mut(&mut self, t: &str) -> &mut Table {
        self.tables
            .get_mut(t)
            .unwrap_or_else(|| panic!("model: no table {t}"))
    }

    pub(crate) fn row(&self, table: &str, key: i64) -> Option<&Row> {
        self.tables.get(table)?.get(&key)
    }
}

/// 候補が見つからなかった理由。
#[derive(Debug)]
pub(crate) struct NoCandidate {
    pub(crate) msg: String,
    /// 表の内容だけなら一致する部分集合がある（違うのはカタログの関係だけ。I16）。
    pub(crate) catalog_only: bool,
}

/// 不明なトランザクションの 2^n 通りの部分集合（元の順序を保つ）を試して、`actual` と一致する
/// 最初のものを返す。一致しなければ、全部適用した候補と「確定のみ」の候補との差の要約を返す。
pub(crate) fn find_candidate(
    model: &Model,
    unknown: &[TxnLog],
    actual: &Tables,
) -> Result<(Model, Vec<String>), String> {
    find_candidate_with(model, unknown, actual, None).map_err(|e| e.msg)
}

/// [`find_candidate`] に、カタログの関係（`model.ddl.enforce` のとき）も一致させる版。
/// 表の内容とカタログの両方が同じ部分集合で説明できなければならない（DDL のトランザクションが
/// 原子的に効くことの確認）。
pub(crate) fn find_candidate_with(
    model: &Model,
    unknown: &[TxnLog],
    actual: &Tables,
    actual_ddl: Option<&BTreeMap<String, DdlRel>>,
) -> Result<(Model, Vec<String>), NoCandidate> {
    assert!(unknown.len() <= 10, "too many unknown transactions");
    let mut tables_only: Option<(Model, Vec<String>)> = None;
    for mask in 0u32..(1 << unknown.len()) {
        let mut m = model.clone();
        let mut applied = Vec::new();
        for (i, t) in unknown.iter().enumerate() {
            if mask & (1 << i) != 0 {
                m.apply(t);
                applied.push(t.label.clone());
            }
        }
        if m.tables != *actual {
            continue;
        }
        match actual_ddl {
            Some(a) if m.ddl.enforce && m.ddl.rels != *a => {
                tables_only.get_or_insert((m, applied));
            }
            _ => return Ok((m, applied)),
        }
    }
    if let (Some((m, applied)), Some(a)) = (tables_only, actual_ddl) {
        let why = m.ddl.mismatch(a).unwrap_or_default();
        return Err(NoCandidate {
            msg: format!(
                "the tables match with {applied:?} of the {} unknown transaction(s) applied, but the catalog differs: {why}",
                unknown.len()
            ),
            catalog_only: true,
        });
    }
    let mut all = model.clone();
    for t in unknown {
        all.apply(t);
    }
    let mut msg = format!(
        "no candidate matches ({} unknown transaction(s)); vs committed-only: {}",
        unknown.len(),
        diff(&model.tables, actual)
    );
    if !unknown.is_empty() {
        let _ = write!(
            msg,
            "; vs committed+all-unknown: {}",
            diff(&all.tables, actual)
        );
    }
    Err(NoCandidate {
        msg,
        catalog_only: false,
    })
}

/// 差の要約（先頭の数件）。`expected` にあって `actual` に無いものは "missing"、逆は "extra"。
pub(crate) fn diff(expected: &Tables, actual: &Tables) -> String {
    let mut out = Vec::new();
    for (t, rows) in expected {
        let Some(arows) = actual.get(t) else {
            out.push(format!("table {t} missing"));
            continue;
        };
        for (k, r) in rows {
            match arows.get(k) {
                None => out.push(format!("{t}[{k}] missing")),
                Some(a) if a != r => out.push(format!("{t}[{k}] expected {r:?} got {a:?}")),
                Some(_) => {}
            }
        }
        for k in arows.keys().filter(|k| !rows.contains_key(k)) {
            out.push(format!("{t}[{k}] extra"));
        }
    }
    for t in actual.keys().filter(|t| !expected.contains_key(*t)) {
        out.push(format!("table {t} extra"));
    }
    if out.is_empty() {
        return "no difference".into();
    }
    let n = out.len();
    out.truncate(6);
    let mut s = out.join(", ");
    if n > 6 {
        let _ = write!(s, ", ... ({n} differences)");
    }
    s
}

/// `actual` に「あるべきなのに無い」ものがあるか（永続性 I1 の違反の目印）。
pub(crate) fn has_missing(expected: &Tables, actual: &Tables) -> bool {
    expected.iter().any(|(t, rows)| {
        actual
            .get(t)
            .is_none_or(|a| rows.iter().any(|(k, r)| a.get(k) != Some(r)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(t: &str, k: i64, v: &str) -> Op {
        Op::Insert {
            table: t.into(),
            key: k,
            row: vec![v.into()],
        }
    }

    fn log(label: &str, ops: Vec<Op>) -> TxnLog {
        TxnLog {
            label: label.into(),
            ops,
        }
    }

    #[test]
    fn apply_tracks_ddl_and_dml() {
        let mut m = Model::default();
        m.apply(&log(
            "a",
            vec![
                Op::Create("t".into()),
                ins("t", 1, "x"),
                ins("t", 2, "y"),
                Op::Update {
                    table: "t".into(),
                    key: 1,
                    row: vec!["z".into()],
                },
                Op::Delete {
                    table: "t".into(),
                    key: 2,
                },
            ],
        ));
        assert_eq!(m.row("t", 1), Some(&vec!["z".to_string()]));
        assert_eq!(m.row("t", 2), None);
        m.apply(&log("b", vec![Op::Drop("t".into())]));
        assert!(m.tables.is_empty());
    }

    #[test]
    #[should_panic(expected = "duplicate key")]
    fn duplicate_insert_is_a_harness_bug() {
        let mut m = Model::default();
        m.apply(&log(
            "a",
            vec![Op::Create("t".into()), ins("t", 1, "x"), ins("t", 1, "y")],
        ));
    }

    #[test]
    fn candidates_are_ordered_subsets() {
        let mut m = Model::default();
        m.apply(&log("c", vec![Op::Create("t".into())]));
        let unknown = vec![
            log("u1", vec![ins("t", 1, "a")]),
            log("u2", vec![ins("t", 2, "b")]),
        ];
        // 4 subsets are all acceptable.
        for mask in 0u32..4 {
            let mut want = m.clone();
            for (i, u) in unknown.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    want.apply(u);
                }
            }
            let (got, applied) = find_candidate(&m, &unknown, &want.tables).unwrap();
            assert_eq!(got, want);
            assert_eq!(applied.len(), mask.count_ones() as usize);
        }
        // A state no subset explains is reported with a diff.
        let mut bad = m.clone();
        bad.apply(&log("x", vec![ins("t", 9, "q")]));
        let e = find_candidate(&m, &unknown, &bad.tables).unwrap_err();
        assert!(e.contains("t[9] extra"), "{e}");
    }

    #[test]
    fn diff_and_missing() {
        let mut a = Tables::new();
        a.insert("t".into(), Table::from([(1, vec!["x".to_string()])]));
        let mut b = a.clone();
        assert_eq!(diff(&a, &b), "no difference");
        assert!(!has_missing(&a, &b));
        b.get_mut("t").unwrap().insert(2, vec!["y".into()]);
        assert!(diff(&a, &b).contains("t[2] extra"));
        assert!(!has_missing(&a, &b));
        b.get_mut("t").unwrap().remove(&1);
        assert!(has_missing(&a, &b));
        b.remove("t");
        assert!(diff(&a, &b).contains("table t missing"));
    }
}
