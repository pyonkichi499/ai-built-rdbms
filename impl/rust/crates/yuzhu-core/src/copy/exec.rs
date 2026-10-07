//! COPY IN の実行状態（行の処理・DEFAULT・NOT NULL・CHECK・挿入。`m4/10-explain-copy-compat.md` §5.5）。
//!
//! 1 行の処理は INSERT と同じ部品を使う（C-10）: NOT NULL と CHECK は `executor::dml::RowChecker`、
//! ヒープと索引への挿入は `executor::dml::insert_with_indexes`。エラーには PostgreSQL の
//! `CopyFromErrorCallback` と同じ CONTEXT を付ける（§5.4）。

use super::text::{self, Field, LineReader};
use super::{BoundCopy, CopyOptions};
use crate::error::{Error, Result, sqlstate};
use crate::executor::ExecCtx;
use crate::executor::dml::{RowChecker, insert_with_indexes};
use crate::executor::eval::eval_const;
use crate::planner::physical::PhysExpr;
use crate::storage::{RelHandle, WriteCtx};
use crate::txn::Transaction;
use crate::types::{Datum, Row, SqlType, io};

/// CONTEXT に出す値・行の最大バイト数（PostgreSQL の `MAX_COPY_DATA_DISPLAY`）。
const MAX_DISPLAY: usize = 100;

/// 長い文字列を先頭 100 バイト（文字境界）に切って `...` を付ける（`limit_printout_length`）。
fn limit_printout(s: &str) -> String {
    if s.len() <= MAX_DISPLAY {
        return s.to_owned();
    }
    let mut end = MAX_DISPLAY;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// 進行中の COPY FROM STDIN。session の `CopyState` が持つ。
#[derive(Debug)]
pub struct CopyIn {
    table_name: String,
    rel: RelHandle,
    /// 入力のフィールド i が入る列（`attnum - 1`）。
    columns: Vec<usize>,
    /// 入力のフィールドの列名（フィールド数の検査用）。
    column_names: Vec<String>,
    col_types: Vec<SqlType>,
    col_defaults: Vec<Option<PhysExpr>>,
    checker: RowChecker,
    options: CopyOptions,
    freeze: bool,
    reader: LineReader,
    /// `header` の最初の行をまだ捨てていない。
    header_pending: bool,
    rows: u64,
    /// 行の読み取りバッファ（行ごとの確保を避ける）。
    line: Vec<u8>,
}

impl CopyIn {
    /// `RelHandle` などを組み立てる。
    pub fn new(b: &BoundCopy) -> Result<CopyIn> {
        if b.columns.is_empty() && !b.table.columns.is_empty() {
            return Err(Error::internal("COPY with no target columns"));
        }
        let table = &b.table;
        let column_names = b
            .columns
            .iter()
            .map(|&i| {
                table
                    .columns
                    .get(i)
                    .map(|c| c.name.clone())
                    .ok_or_else(|| Error::internal(format!("COPY column {i} out of range")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(CopyIn {
            table_name: table.name.clone(),
            rel: RelHandle::from_table(table),
            columns: b.columns.clone(),
            column_names,
            col_types: b.col_types.clone(),
            col_defaults: b.defaults.clone(),
            checker: RowChecker::new(
                table.oid,
                table.name.clone(),
                b.not_null.clone(),
                b.checks.clone(),
            ),
            header_pending: b.options.header,
            freeze: b.options.freeze,
            options: b.options.clone(),
            reader: LineReader::new(),
            rows: 0,
            line: Vec::new(),
        })
    }

    /// `CopyInResponse` に載せる列数。
    pub fn ncols(&self) -> usize {
        self.columns.len()
    }

    /// これまでに挿入した行数。
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// 行の読み取りと `CopyFail` 用の CONTEXT（`COPY {table}, line {n}`。n = 読み終えた行数 + 1）。
    pub fn read_context(&self) -> String {
        format!("COPY {}, line {}", self.table_name, self.reader.line_no())
    }

    /// `FREEZE` の前提（対象の表がこのトランザクションで作成・TRUNCATE されている）の検査（10 §5.6）。
    /// 通れば凍結せず通常の xmin で挿入する。
    pub fn check_freeze(&self, txn: &Transaction) -> Result<()> {
        if self.freeze && !txn.pending_creates.contains(&self.rel.locator) {
            return Err(Error::new(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot perform COPY FREEZE because the table was not created or truncated in the current subtransaction",
            ));
        }
        Ok(())
    }

    /// 1 つの `CopyData`。完成した行を処理して挿入する。
    pub fn push_data(&mut self, chunk: &[u8], ctx: &mut ExecCtx<'_>, w: &WriteCtx) -> Result<()> {
        self.reader.push(chunk);
        self.drain(false, ctx, w)
    }

    /// `CopyDone`: 未完の最後の行を処理して、挿入した総行数を返す。
    pub fn finish(&mut self, ctx: &mut ExecCtx<'_>, w: &WriteCtx) -> Result<u64> {
        self.drain(true, ctx, w)?;
        Ok(self.rows)
    }

    /// `CopyFail`: `57014`（CONTEXT は `COPY {table}, line {n}`）。
    pub fn fail(&self, message: &str) -> Error {
        let msg = if message.is_empty() {
            "COPY from stdin failed".to_owned()
        } else {
            format!("COPY from stdin failed: {message}")
        };
        Error::new(sqlstate::QUERY_CANCELED, msg).with_context(self.read_context())
    }

    /// 取り出せる行をすべて処理する。
    fn drain(&mut self, at_eof: bool, ctx: &mut ExecCtx<'_>, w: &WriteCtx) -> Result<()> {
        let mut line = std::mem::take(&mut self.line);
        let r = self.drain_lines(at_eof, &mut line, ctx, w);
        self.line = line;
        r
    }

    fn drain_lines(
        &mut self,
        at_eof: bool,
        line: &mut Vec<u8>,
        ctx: &mut ExecCtx<'_>,
        w: &WriteCtx,
    ) -> Result<()> {
        loop {
            ctx.check_interrupts()?;
            let line_no = self.reader.line_no();
            let got = self
                .reader
                .next_line_into(at_eof, line)
                .map_err(|e| e.with_context(format!("COPY {}, line {line_no}", self.table_name)))?;
            if !got {
                return Ok(());
            }
            if std::mem::take(&mut self.header_pending) {
                continue;
            }
            self.process_line(line, line_no, ctx, w)?;
            self.rows += 1;
        }
    }

    fn line_context(&self, line_no: u64, line: &[u8]) -> String {
        format!(
            "COPY {}, line {line_no}: \"{}\"",
            self.table_name,
            limit_printout(&String::from_utf8_lossy(line))
        )
    }

    /// 1 行を行バッファに復元して、NOT NULL・CHECK を検査して挿入する。
    fn process_line(
        &self,
        line: &[u8],
        line_no: u64,
        ctx: &mut ExecCtx<'_>,
        w: &WriteCtx,
    ) -> Result<()> {
        let in_line = |e: Error| e.with_context(self.line_context(line_no, line));
        let fields = text::split_fields(line, self.options.delimiter);
        let names = self
            .column_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        // PG: 余りのフィールドは変換の前に、足りないフィールドはその列に着いた時点で報告する。
        if fields.len() > names.len() {
            text::check_field_count(fields.len(), &names).map_err(in_line)?;
        }

        let mut row: Row = vec![Datum::Null; self.col_types.len()];
        let mut provided = vec![false; self.col_types.len()];
        for (i, &col) in self.columns.iter().enumerate() {
            let Some(raw) = fields.get(i) else {
                return Err(in_line(
                    text::check_field_count(i, &names).expect_err("fewer fields than columns"),
                ));
            };
            provided[col] = true;
            let field = raw.classify(
                &self.options.null_string,
                self.options.default_string.as_deref(),
            );
            row[col] = match field {
                Field::Null => Datum::Null,
                Field::Default => self.eval_default(col, ctx).map_err(in_line)?,
                Field::Value(bytes) => self.convert(col, &bytes, line_no, ctx)?,
            };
        }
        for col in 0..row.len() {
            if !provided[col] {
                row[col] = self.eval_default(col, ctx).map_err(in_line)?;
            }
        }
        self.checker.check(ctx, &row).map_err(in_line)?;
        // PG 17 は行の挿入・インデックス登録のエラーには行の内容を付けない。
        insert_with_indexes(ctx, &self.rel, w, &row)
            .map_err(|e| e.with_context(format!("COPY {}, line {line_no}", self.table_name)))?;
        Ok(())
    }

    /// 列の DEFAULT（なければ NULL）。
    fn eval_default(&self, col: usize, ctx: &ExecCtx<'_>) -> Result<Datum> {
        match self.col_defaults.get(col).and_then(Option::as_ref) {
            Some(e) => eval_const(e, &Row::new(), &ctx.eval_ctx()),
            None => Ok(Datum::Null),
        }
    }

    /// 復元したバイト列を列の型の値にする（UTF-8 の検査、入力関数、typmod）。
    fn convert(&self, col: usize, bytes: &[u8], line_no: u64, ctx: &ExecCtx<'_>) -> Result<Datum> {
        let in_column = |e: Error| {
            e.with_context(format!(
                "COPY {}, line {line_no}, column {}: \"{}\"",
                self.table_name,
                self.rel_column_name(col),
                limit_printout(&String::from_utf8_lossy(bytes))
            ))
        };
        text::validate_utf8(bytes).map_err(in_column)?;
        let s = std::str::from_utf8(bytes)
            .map_err(|_| in_column(Error::internal("invalid UTF-8 after validation")))?;
        io::input_text_typed(s, self.col_types[col], ctx.type_env).map_err(in_column)
    }

    fn rel_column_name(&self, col: usize) -> &str {
        self.columns
            .iter()
            .position(|&c| c == col)
            .and_then(|i| self.column_names.get(i))
            .map_or("?", String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::table_def;
    use crate::catalog::{CheckDef, ColumnDef};
    use crate::error::sqlstate;
    use crate::executor::eval::tests::{GT, col, int, op};
    use crate::executor::nodes::test_util::{Fixture, index_on};
    use crate::planner::physical::PhysCheck;
    use crate::txn::Xid;
    use std::sync::Arc;

    const T: u32 = 16384;
    const PK: u32 = 100;

    fn def() -> crate::catalog::TableDef {
        let c = |name: &str, attnum, ty, not_null| ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null,
            default: None,
            identity: None,
        };
        table_def(
            T,
            "cp",
            vec![
                c("a", 1, SqlType::INT4, true),
                c("b", 2, SqlType::TEXT, false),
                c("c", 3, SqlType::INT4, false),
            ],
            vec![CheckDef {
                name: "cp_c_check".into(),
                expr_sql: "c < 100".into(),
                no_inherit: false,
            }],
        )
    }

    fn fixture() -> Fixture {
        let mut f = Fixture::new();
        f.catalog.put_table(Arc::new(def()));
        f
    }

    fn bound(columns: Vec<usize>, options: CopyOptions) -> BoundCopy {
        let table = Arc::new(def());
        BoundCopy {
            col_types: table.columns.iter().map(|c| c.ty).collect(),
            not_null: table.columns.iter().map(|c| c.not_null).collect(),
            defaults: vec![None, None, Some(int(42))],
            checks: vec![PhysCheck {
                name: "cp_c_check".into(),
                // c < 100  ==  100 > c
                expr: op(&GT, int(100), col(2, SqlType::INT4)),
            }],
            table,
            columns,
            options,
        }
    }

    fn copy_in(options: CopyOptions) -> CopyIn {
        CopyIn::new(&bound(vec![0, 1, 2], options)).unwrap()
    }

    fn w() -> WriteCtx {
        WriteCtx {
            xid: Xid(3),
            cid: 0,
        }
    }

    fn feed(f: &mut Fixture, c: &mut CopyIn, chunks: &[&[u8]]) -> Result<u64> {
        let mut ctx = f.ctx();
        for ch in chunks {
            c.push_data(ch, &mut ctx, &w())?;
        }
        c.finish(&mut ctx, &w())
    }

    fn run(input: &[u8]) -> (Fixture, Result<u64>) {
        let mut f = fixture();
        let r = feed(&mut f, &mut copy_in(CopyOptions::default()), &[input]);
        (f, r)
    }

    fn i4(n: i32) -> Datum {
        Datum::Int4(n)
    }
    fn tx(s: &str) -> Datum {
        Datum::Text(s.into())
    }

    #[test]
    fn inserts_rows_split_across_chunks() {
        let mut f = fixture();
        let mut c = copy_in(CopyOptions::default());
        let n = feed(
            &mut f,
            &mut c,
            &[b"1\tx\t5\n2\ty", b"\t6\n", b"3\tz\t7", b"\n\\.\n"],
        )
        .unwrap();
        assert_eq!(n, 3);
        assert_eq!(c.rows(), 3);
        assert_eq!(
            f.storage.rows(T),
            vec![
                vec![i4(1), tx("x"), i4(5)],
                vec![i4(2), tx("y"), i4(6)],
                vec![i4(3), tx("z"), i4(7)],
            ]
        );
    }

    #[test]
    fn last_line_without_terminator_and_empty_input() {
        let (f, r) = run(b"2\tx\t5");
        assert_eq!(r.unwrap(), 1);
        assert_eq!(f.storage.rows(T).len(), 1);
        let (f, r) = run(b"");
        assert_eq!(r.unwrap(), 0);
        assert!(f.storage.rows(T).is_empty());
        let (_, r) = run(b"\\.\n");
        assert_eq!(r.unwrap(), 0);
    }

    #[test]
    fn restores_escapes_nulls_and_stops_at_end_marker() {
        let (f, r) = run(b"1\ta\\tb\\x41\\101\\q\t\\N\n2\t\\\\N\t\\N\n3\tq\t9\\.\n4\tnot\t1\n");
        assert_eq!(r.unwrap(), 3);
        assert_eq!(
            f.storage.rows(T),
            vec![
                vec![i4(1), tx("a\tbAAq"), Datum::Null],
                vec![i4(2), tx("\\N"), Datum::Null],
                vec![i4(3), tx("q"), i4(9)],
            ]
        );
    }

    #[test]
    fn column_list_and_defaults() {
        let mut f = fixture();
        let mut c = CopyIn::new(&bound(vec![1, 0], CopyOptions::default())).unwrap();
        assert_eq!(c.ncols(), 2);
        feed(&mut f, &mut c, &[b"x\t1\ny\t2\n"]).unwrap();
        // c は列リストにないので DEFAULT（42）。
        assert_eq!(
            f.storage.rows(T),
            vec![vec![i4(1), tx("x"), i4(42)], vec![i4(2), tx("y"), i4(42)]]
        );
    }

    #[test]
    fn default_option_uses_the_column_default() {
        let opts = CopyOptions {
            default_string: Some(b"D".to_vec()),
            ..CopyOptions::default()
        };
        let mut f = fixture();
        let mut c = copy_in(opts);
        feed(&mut f, &mut c, &[b"1\tD\tD\n2\tD\t7\n"]).unwrap();
        // b には DEFAULT がないので NULL。c は 42。
        assert_eq!(
            f.storage.rows(T),
            vec![
                vec![i4(1), Datum::Null, i4(42)],
                vec![i4(2), Datum::Null, i4(7)],
            ]
        );
    }

    #[test]
    fn custom_delimiter_null_and_header() {
        let opts = CopyOptions {
            delimiter: b'|',
            null_string: b"NA".to_vec(),
            header: true,
            ..CopyOptions::default()
        };
        let mut f = fixture();
        let mut c = copy_in(opts);
        feed(&mut f, &mut c, &[b"a|b|c\n1|NA|3\n2|x\\|y|NA\n"]).unwrap();
        assert_eq!(
            f.storage.rows(T),
            vec![
                vec![i4(1), Datum::Null, i4(3)],
                vec![i4(2), tx("x|y"), Datum::Null],
            ]
        );
    }

    #[test]
    fn header_line_is_counted_in_line_numbers() {
        let opts = CopyOptions {
            header: true,
            ..CopyOptions::default()
        };
        let mut f = fixture();
        let e = feed(&mut f, &mut copy_in(opts), &[b"h\n1\ty\n"]).unwrap_err();
        assert_eq!(e.context().unwrap(), "COPY cp, line 2: \"1\ty\"");
    }

    fn ctx_of(e: &Error) -> &str {
        e.context().expect("context")
    }

    #[test]
    fn field_count_errors() {
        let (_, r) = run(b"3\ty\n");
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::BAD_COPY_FILE_FORMAT);
        assert_eq!(e.message, "missing data for column \"c\"");
        assert_eq!(ctx_of(&e), "COPY cp, line 1: \"3\ty\"");
        let (_, r) = run(b"3\ty\t1\t9\n");
        let e = r.unwrap_err();
        assert_eq!(e.message, "extra data after last expected column");
        assert_eq!(ctx_of(&e), "COPY cp, line 1: \"3\ty\t1\t9\"");
    }

    #[test]
    fn earlier_invalid_column_wins_over_missing_data() {
        let (_, r) = run(b"x\ty\n");
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION);
        assert_eq!(ctx_of(&e), "COPY cp, line 1, column a: \"x\"");
        // 余りのフィールドは変換より先に報告される。
        let (_, r) = run(b"x\ty\t1\t9\n");
        assert_eq!(
            r.unwrap_err().message,
            "extra data after last expected column"
        );
    }
    #[test]
    fn input_function_error_has_column_context() {
        let (_, r) = run(b"1\tx\t1\n2\ty\tabc\n");
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION);
        assert_eq!(e.message, "invalid input syntax for type integer: \"abc\"");
        assert_eq!(ctx_of(&e), "COPY cp, line 2, column c: \"abc\"");
    }

    #[test]
    fn invalid_utf8_and_nul() {
        let (_, r) = run(b"1\t\xff\t1\n");
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert!(ctx_of(&e).starts_with("COPY cp, line 1, column b: "));
        let (_, r) = run(b"1\ta\\0b\t1\n");
        assert_eq!(
            r.unwrap_err().sqlstate,
            sqlstate::CHARACTER_NOT_IN_REPERTOIRE
        );
    }

    #[test]
    fn not_null_and_check_violations() {
        let (f, r) = run(b"\\N\ty\t1\n");
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NOT_NULL_VIOLATION);
        assert_eq!(
            e.message,
            "null value in column \"a\" of relation \"cp\" violates not-null constraint"
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("Failing row contains (null, y, 1).")
        );
        assert_eq!(ctx_of(&e), "COPY cp, line 1: \"\\N\ty\t1\"");
        assert!(f.storage.rows(T).is_empty());

        let (_, r) = run(b"1\tok\t1\n5\ty\t500\n");
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::CHECK_VIOLATION);
        assert_eq!(
            e.message,
            "new row for relation \"cp\" violates check constraint \"cp_c_check\""
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("Failing row contains (5, y, 500).")
        );
        assert_eq!(ctx_of(&e), "COPY cp, line 2: \"5\ty\t500\"");
    }

    #[test]
    fn unique_violation_context_has_no_line_content() {
        let mut f = fixture();
        let mut c = copy_in(CopyOptions::default());
        c.rel.indexes = Arc::from(vec![index_on(
            PK,
            "cp_pkey",
            "cp",
            true,
            &[(1, "a", SqlType::INT4)],
        )]);
        let e = feed(&mut f, &mut c, &[b"1\tx\t1\n2\ty\t2\n1\tz\t3\n"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNIQUE_VIOLATION);
        assert_eq!(e.detail.as_deref(), Some("Key (a)=(1) already exists."));
        assert_eq!(ctx_of(&e), "COPY cp, line 3");
    }

    #[test]
    fn read_errors_have_a_plain_context() {
        let (_, r) = run(b"1\tx\t1\n22\tx\t5\\.x\n");
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::BAD_COPY_FILE_FORMAT);
        assert_eq!(e.message, "end-of-copy marker corrupt");
        assert_eq!(ctx_of(&e), "COPY cp, line 2");
        let (_, r) = run(b"1\tx\t1\r\n2\tx\n");
        let e = r.unwrap_err();
        assert_eq!(e.message, "literal newline found in data");
        assert_eq!(ctx_of(&e), "COPY cp, line 2");
    }

    #[test]
    fn long_values_are_clipped_in_context() {
        let long = "x".repeat(150);
        let (_, r) = run(format!("1\ty\t{long}\n").as_bytes());
        let e = r.unwrap_err();
        assert_eq!(
            ctx_of(&e),
            format!("COPY cp, line 1, column c: \"{}...\"", "x".repeat(100))
        );
        // 文字境界で切る（3 バイト文字 34 個 = 102 バイト → 99 バイト）。
        let s = "あ".repeat(34);
        assert_eq!(limit_printout(&s), format!("{}...", "あ".repeat(33)));
        assert_eq!(limit_printout("short"), "short");
    }

    #[test]
    fn fail_has_57014_and_context() {
        let mut f = fixture();
        let mut c = copy_in(CopyOptions::default());
        {
            let mut ctx = f.ctx();
            c.push_data(b"1\tx\t1\n", &mut ctx, &w()).unwrap();
        }
        let e = c.fail("client aborted");
        assert_eq!(e.sqlstate, sqlstate::QUERY_CANCELED);
        assert_eq!(e.message, "COPY from stdin failed: client aborted");
        assert_eq!(ctx_of(&e), "COPY cp, line 2");
        assert_eq!(c.fail("").message, "COPY from stdin failed");
    }

    #[test]
    fn freeze_requires_a_table_created_in_this_transaction() {
        let mut f = fixture();
        let opts = CopyOptions {
            freeze: true,
            ..CopyOptions::default()
        };
        let c = copy_in(opts);
        let e = c.check_freeze(&f.txn).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
        assert_eq!(
            e.message,
            "cannot perform COPY FREEZE because the table was not created or truncated in the current subtransaction"
        );
        f.txn.pending_creates.push(c.rel.locator);
        c.check_freeze(&f.txn).unwrap();
        // FREEZE でなければ検査しない。
        let plain = copy_in(CopyOptions::default());
        let f2 = fixture();
        plain.check_freeze(&f2.txn).unwrap();
    }

    #[test]
    fn interrupts_stop_the_copy() {
        let mut f = fixture();
        let mut c = copy_in(CopyOptions::default());
        f.interrupts.request_cancel();
        let e = feed(&mut f, &mut c, &[b"1\tx\t1\n"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::QUERY_CANCELED);
        assert!(f.storage.rows(T).is_empty());
    }

    #[test]
    fn many_rows_keep_memory_flat() {
        let mut f = fixture();
        let mut c = copy_in(CopyOptions::default());
        let mut ctx = f.ctx();
        for i in 0..2000 {
            c.push_data(format!("{i}\tv\t1\n").as_bytes(), &mut ctx, &w())
                .unwrap();
        }
        assert_eq!(c.finish(&mut ctx, &w()).unwrap(), 2000);
    }
}
