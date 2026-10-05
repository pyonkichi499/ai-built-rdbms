//! 確定・不明なトランザクションの記録と `BTreeMap` のモデル（`m3.md` §7.5）。
//!
//! モデルは「確定したトランザクションを順に適用した状態」。COMMIT を呼んだが `Ok` で戻らなかった
//! トランザクションは「不明」として別に持ち、リカバリ後の内容は「モデルに不明の任意の部分集合を
//! 順に加えたもの」のどれかと一致しなければならない（I4）。

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// キー以外の列を文字列にしたもの。
pub(crate) type Row = Vec<String>;
/// 1 テーブルの内容（キーは第 1 列の整数）。
pub(crate) type Table = BTreeMap<i64, Row>;
/// ユーザーテーブル全体（`public` スキーマ）。`SELECT` の結果もこの形に直して比べる。
pub(crate) type Tables = BTreeMap<String, Table>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Create(String),
    Drop(String),
    Insert { table: String, key: i64, row: Row },
    Update { table: String, key: i64, row: Row },
    Delete { table: String, key: i64 },
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
            }
            Op::Drop(t) => {
                assert!(
                    self.tables.remove(t).is_some(),
                    "model: dropping missing table {t}"
                );
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

/// 不明なトランザクションの 2^n 通りの部分集合（元の順序を保つ）を試して、`actual` と一致する
/// 最初のものを返す。一致しなければ、全部適用した候補と「確定のみ」の候補との差の要約を返す。
pub(crate) fn find_candidate(
    model: &Model,
    unknown: &[TxnLog],
    actual: &Tables,
) -> Result<(Model, Vec<String>), String> {
    assert!(unknown.len() <= 10, "too many unknown transactions");
    for mask in 0u32..(1 << unknown.len()) {
        let mut m = model.clone();
        let mut applied = Vec::new();
        for (i, t) in unknown.iter().enumerate() {
            if mask & (1 << i) != 0 {
                m.apply(t);
                applied.push(t.label.clone());
            }
        }
        if m.tables == *actual {
            return Ok((m, applied));
        }
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
    Err(msg)
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
