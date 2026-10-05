# M4 設計 09: 型・関数・集約・正規表現

`00-contracts.md`（以下「00」）に従う。この章は、numeric・日時（date / timestamp / timestamptz）・`char(n)`（bpchar）・regclass / regtype / int2vector の統合、手書きの正規表現エンジン、集約関数の表、関数と演算子の追加、`generate_series` を、実装者がこの章だけで書ける粒度で定める。担当は T1（numeric）、T2（日時と bpchar）、T3（関数・集約・正規表現・sys・cmp）。

- 正解は PostgreSQL 17。OID と挙動は実機（17.11、`sandbox/pg.sh start`、`TimeZone=Etc/UTC`、`DateStyle=ISO, MDY`、C ロケール、UTF8）で確かめた。確かめていないものは「（未検証）」と書く。
- 既存実装の参照: `impl/rust/crates/yuzhu-numeric`（`Numeric`、`NumericError`、`make_typmod` など）、`impl/rust/crates/yuzhu-datetime`（`Date`、`Timestamp`、`TimestampTz`、`DateTimeEnv`、`ZoneDb`、`DateTimeError`）。どちらも外部依存なしで、PG17 との差分コーパス（numeric 約 3.5 万行、datetime 約 7 万行、TZif の fixtures 付き）を持つ。**この章は両クレートを変更しない**。

---

## 1. 範囲

### 1.1 入れるもの

| 分類 | 内容 |
|---|---|
| 型 | `numeric[(p,s)]`、`char(n)` / `character(n)` / `bpchar`、`date`、`timestamp[(p)]`、`timestamptz[(p)]`、`regclass`、`regtype`、`int2vector`、1 次元の `int2[]`（カタログ列の入出力だけ） |
| 演算子 | numeric の `+ - * / %`・単項 `- +`・比較 6 つ、bpchar の比較 6 つ、date / timestamp / timestamptz の比較 6 つ、`date ± int`・`int + date`・`date - date`、正規表現 `~ ~* !~ !~*`（text / name / bpchar）、bpchar の LIKE 系 |
| 関数 | numeric の `abs sign round trunc ceil ceiling floor mod div scale`、float8 の `round trunc ceil ceiling floor sign`（`round(2)` を PG と同じく float8 にするために必要）、整数の `mod`、`now` `transaction_timestamp` `statement_timestamp` `clock_timestamp`、`date()` `timestamp()` `timestamptz()` の関数形式のキャスト、`length` `char_length` `character_length` `octet_length`（bpchar）、`pg_typeof`、`to_regclass` `to_regtype`、`regclass(text)` |
| 集約 | `AGGREGATES` 43 行（§8） |
| 集合返却 | `generate_series(int4,int4[,int4])`、`generate_series(int8,int8[,int8])`（FROM 句のみ） |
| 日時の SQL 値関数 | `CURRENT_DATE`、`CURRENT_TIMESTAMP[(p)]`、`LOCALTIMESTAMP[(p)]`（`SessionValueKind` に足す。§6.4） |
| 基盤 | `cmp_datum` の拡張、`hash_datum` / `HashKey`、`cmp_with_nulls`、`TypeEnv`、typmod の符号化と適用（`types/typmod.rs`）、`format_type` の拡張 |

### 1.2 入れないもの（実行すると `0A000`、または 42883）

- `interval`、`time`、`timetz`: キーワードは `0A000`（`type interval is not supported yet`）。理由は D-9-1。
- `extract` / `date_part` / `date_trunc` / `age` / `make_*` / `to_char` / `to_timestamp` / `AT TIME ZONE` / `isfinite`: M5。`yuzhu-datetime` に実装（`extract_timestamp`、`date_trunc_timestamp`、`TimestampTz::at_time_zone` など）が揃っているので、M5 では関数の行と本体の橋渡しを書くだけで済む。
- numeric の `sqrt exp ln log power ^`（`^` は行だけ既存のまま本体は `0A000`）、`generate_series(numeric,...)`、`generate_series(timestamp,...,interval)`: M5/M6。
- 正規表現の後方参照（`\1`）と先読み・後読み（`(?=` `(?!` `(?<=` `(?<!`）: `0A000`。理由は D-9-7。
- `SIMILAR TO`、`substring(text from pattern)`、`regexp_*` 関数、`bytea` / `uuid` / 配列の一般型: M5 以降。
- date / timestamp / timestamptz の**型をまたぐ比較演算子**（`date < timestamptz`、`timestamp = timestamptz` など 30 行）: 作らない。暗黙キャストで同じ結果になる（D-9-5）。

---

## 2. 決定（この章で追加で決めたこと）

D-3〜D-5、D-22 は 00 の決定をそのまま実装の形にする。`D-9-n` はこの章の決定。

| # | 決定 | 理由 |
|---|---|---|
| D-3 | numeric は `yuzhu-numeric` を統合する（`Datum::Numeric(Box<Numeric>)`）。ディスク形式は **00 §12.3**（固定 8 バイトヘッダ + 10000 進の桁） | 差分コーパスが既にある。`m5-types-fk.md` §2.1 は PG の short / long ヘッダ形式を提案していたが、タプルヘッダが PG と違う（M2-Q2）時点でサイズ互換の利点がなく、固定ヘッダの方が読み書きが単純。**調査と 00 の食い違いは 00 を採る** |
| D-4 | 日時は `yuzhu-datetime` を統合する。M4 の型は `date` `timestamp` `timestamptz` | pgbench（`mtime timestamp`、`CURRENT_TIMESTAMP`）に必要な最小集合 |
| D-5 | `char(n)` は `Datum::BpChar(String)`（空白で埋めた後の文字列）。比較・ハッシュは末尾の空白を無視 | 変種が型を表す設計（00 §12.2）。`text` との取り違えを型で防ぐ |
| D-22 | 正規表現は手書き（`types/regex.rs`）。NFA シミュレーション（線形時間） | 破滅的バックトラックを避ける。`~` 系は「一致があるか」だけを返すので、最長一致・最短一致の違いは結果に影響しない |
| D-9-1 | `interval` は M5。**M4 では `timestamp - timestamp` / `timestamptz - timestamptz` / `ts ± '1 day'` / `now() - interval '1 day'` は `42883`**（`operator does not exist: timestamp without time zone - timestamp without time zone`、HINT 付き）。`interval` というキーワードは `0A000` | `interval` は 3 つ組の値、128 ビット比較、入力の小数繰り下げ、出力の符号規則、`IntervalStyle` を伴い、`Datum` 変種・ディスク形式・演算子 40 行超を要する。M4 の完了条件（pgbench、psql の `\dt`）は `interval` を使わない。`timestamp - timestamp` の演算子行（OID 2067、1328）を `OPERATORS` に**入れない**ことで 42883 になる。PG では動くので既知の差分。`date - date`（整数を返す）は動く |
| D-9-2 | 型の解決に必要な `pg_cast` と `pg_operator` の行は PG17 と同じ OID で入れる。ただし本体が M5 以降のものは入れない（M2 の `ops::unsupported` の行のうち、interval を含む行はそのまま残す） | 演算子解決の候補を PG に合わせる |
| D-9-3 | 日時リテラル（`'2024-01-01'::date` など）の入力は **アナライザでは評価せず**、`Cast { Literal(Text), InOut }` として残し、プランナの定数畳み込み（`PlanEnv.type_env`）で評価する。numeric・bpchar・int2vector のリテラルは、アナライザが `TypeEnv::default()` で即座に評価する。regclass / regtype のリテラルは、アナライザが `catalog` で OID に畳む | 日時の入力は DateStyle・TimeZone・`now`（トランザクション開始時刻）に依存し、`analyze(stmt, catalog)` の署名には環境がない（00 §5 は署名を変えない）。エラー位置は `Cast` の `span`（リテラルの位置）に付くので PG と同じ位置に出る |
| D-9-4 | タイムゾーンや `now` に依存する型変換は `CastMethod::Env(fn(&[Datum], &TypeEnv<'_>) -> Result<Datum>)`（新設）で表す。`pg_cast` の `provolatile = 's'` のものがこれ。純粋な変換は `CastMethod::Function`。**`Env` の `Cast` と `FnKind::Runtime` の関数は定数畳み込みしない**（stable） | 評価時の `TypeEnv`（`ExecCtx.type_env`）を使う。`apply_cast` は `ctx.type_env` を受け取る（§11 の依頼） |
| D-9-5 | 型をまたぐ日時の比較演算子は作らない。`timestamp < timestamptz` は `timestamp → timestamptz`（暗黙、Env）を挟んだ `timestamptz < timestamptz` に解決される | 本体が `TimeZone` を要し、`BuiltinOperator.func` が純粋関数のため。結果は PG と同じ（無限大と範囲外の境界を除く）。EXPLAIN の表示にキャストが出ること、`timestamp` 列のインデックスを `timestamptz` の値で引けないこと（`ts_col < now()` は全件走査）が差分 |
| D-9-6 | `CURRENT_DATE` / `CURRENT_TIMESTAMP[(p)]` / `LOCALTIMESTAMP[(p)]` は `SessionValueKind` の新変種で表し、`now()` などは `FnKind::Runtime` | EXPLAIN VERBOSE と `pg_get_expr` が PG と同じ綴りを出せる。値は `RuntimeInfo.transaction_timestamp()` から作る |
| D-9-7 | 正規表現は ARE（PG の既定）の部分集合。**後方参照と先読み・後読みは `0A000`** | 後方参照は線形時間で解けない。psql・ORM・pg_dump が生成するパターンは網羅される（§9.2）。バックトラック版の併設は M5 以降の任意項目 |
| D-9-8 | ロケールは C のみ。文字クラス・大文字小文字の同一視・`\w` は ASCII のみ（非 ASCII は `.` と否定クラスにだけ一致する） | PG の C ロケール + UTF8 と同じ（実機で確認: `'é' ~ '\w'` は f、`'é' ~* 'É'` は f、`'é' ~ '.'` は t） |
| D-9-9 | `pg_typeof(x)` はアナライザが `Literal(Datum::Oid(x の型))`（型 regtype）に置き換える。引数は評価しない | `BuiltinFn` が型を受け取らないため。差分: `pg_typeof(1/0)` は PG ではエラー、yuzhu は `integer` |
| D-9-10 | `timestamp(p)` の p が 7 以上は黙って 6 にする（PG は `WARNING: TIMESTAMP(7) precision reduced to maximum allowed, 6`）。p < 0 は構文エラー（パーサ） | アナライザに通知の経路がない。既知の差分 |
| D-9-11 | DEFAULT に `'now'::timestamp` と書いたとき、PG は CREATE TABLE 時点の値で固定するが、yuzhu は保存テキストを毎回評価するので使うたびの時刻になる（Q-006 の帰結）。M4 では**既知の差分として残す** | `m5-types-fk.md` C-7 の選択肢のうち、保存時の畳み込み（deparse が要る）は M5 に回す。実害は小さい |
| D-9-12 | 集約の `min` / `max` は、比較して等しいとき**後に来た値を残す**（PG の `numeric_larger` などが `cmp > 0 ? a : b`、`smaller` が `cmp < 0 ? a : b` のため。実機で `max(1.10, 1.1, 1.100)` = `1.100` を確認） | 表示の違い（`1.10` と `1.1`）が出るので PG に合わせる |

### 2.1 interval を M5 に回す理由と、そのときの見え方

1. 理由は D-9-1 のとおり。`interval` を入れると、`Datum` 変種（`Interval { months, days, micros }`）、比較（1 か月 = 30 日の 128 ビット換算）、ハッシュ、ディスク形式（16 バイト）、入力（`yuzhu-datetime` にある）、`IntervalStyle` の設定、`timestamp ± interval` の演算子とキャスト、`avg(interval)` などの集約が連鎖し、M4 の他の作業（結合・集約・B+Tree）の並列を妨げる。
2. M4 で起きること:

| SQL | M4 の結果 | PG17 |
|---|---|---|
| `SELECT '1 day'::interval` / `interval '1 day'` / `CAST(x AS interval)` | `0A000 type interval is not supported yet` | 動く |
| `SELECT now() - interval '1 day'` | 同上（型名で失敗する） | 動く |
| `SELECT timestamp '2024-01-02' - timestamp '2024-01-01'` | `42883 operator does not exist: timestamp without time zone - timestamp without time zone` + HINT `No operator matches the given name and argument types. You might need to add explicit type casts.` | `1 day` |
| `SELECT ts_col - '1 day'` | `42883 operator does not exist: timestamp without time zone - unknown`（PG は未知リテラルを timestamp として読もうとして `22007`。どちらもエラー） | 22007 |
| `SELECT d1 - d2`（date） | 整数 | 整数 |
| `SELECT current_date - 1` | date | date |

3. M5 で足すもの: `Datum::Interval`、`SqlType` の typmod（範囲 + 精度）、演算子 `1328` `2067` `1327`〜、`interval` の入出力と比較、`extract` など。`yuzhu-datetime::Interval` に実装済み。

---

## 3. 型ごとの統合仕様

### 3.1 共通: OID 定数、`SqlType`、`Datum`

00 §12.1 の表に従い、`types/mod.rs`（担当 A）に次を足す。既存の `builtin::NUMERIC` `DATE` `INTERVAL` は `types::oid` に移す。

```rust
// types/mod.rs の pub mod oid
pub const NUMERIC: Oid = 1700;
pub const BPCHAR: Oid = 1042;
pub const DATE: Oid = 1082;
pub const TIMESTAMP: Oid = 1114;
// TIMESTAMPTZ = 1184 は既存
pub const REGCLASS: Oid = 2205;
pub const REGTYPE: Oid = 2206;
pub const INT2VECTOR: Oid = 22;
pub const ANY: Oid = 2276;          // 疑似型 "any"（count("any")、pg_typeof の引数）

impl SqlType {
    pub const NUMERIC: SqlType = SqlType::of(oid::NUMERIC);
    pub const BPCHAR: SqlType = SqlType::of(oid::BPCHAR);
    pub const DATE: SqlType = SqlType::of(oid::DATE);
    pub const TIMESTAMP: SqlType = SqlType::of(oid::TIMESTAMP);
    pub const TIMESTAMPTZ: SqlType = SqlType::of(oid::TIMESTAMPTZ);
    pub const REGCLASS: SqlType = SqlType::of(oid::REGCLASS);
    pub const REGTYPE: SqlType = SqlType::of(oid::REGTYPE);
    /// `char(n)`。typmod = n + VARHDRSZ
    pub const fn bpchar(n: i32) -> SqlType { SqlType { oid: oid::BPCHAR, typmod: n + VARHDRSZ } }
    pub fn bpchar_len(self) -> Option<i32>;                  // typmod >= VARHDRSZ のとき Some(n)
    /// `timestamp(p)` / `timestamptz(p)`。typmod = p（+4 しない）
    pub const fn timestamp(p: i32) -> SqlType;
    pub const fn timestamptz(p: i32) -> SqlType;
}
```

`Datum`（00 §12.2）に `Numeric(Box<Numeric>)`、`BpChar(String)`、`Date`、`Timestamp`、`TimestampTz`、`Int2Vector(Vec<i16>)` を足す。`regclass` / `regtype` は `Datum::Oid(u32)`。**`Datum::as_str()` は `Text` と `BpChar` の両方で `Some` を返す**（文字列を持つ変種の共通アクセサ。`ops.rs` の `text_arg`、LIKE、正規表現がそのまま BpChar を受けられる）。`Datum::rank()`（型の違う変種の順位）には新しい変種を末尾に足す（`Numeric`=13、`BpChar`=14、`Date`=15、`Timestamp`=16、`TimestampTz`=17、`Int2Vector`=18）。`SqlType::is_string_like()`（`Datum::Text` を持つ型）は変えない（BpChar を含めない）。

### 3.2 型の性質と pg_type の行

`catalog/builtin.rs` の `TYPES` に次を足す（`ty(oid, name, typlen, typbyval, typtype, category, preferred, delim, relid, elem, array_oid, input, output, align, storage, collation)` の引数順）。値は実機の `pg_type` から。

```rust
ty(22,   "int2vector", -1, false, 'b', 'A', false, ',', 0, 21,   1006, ("int2vectorin", 40),    ("int2vectorout", 41),    'i', 'p', 0),
ty(1042, "bpchar",     -1, false, 'b', 'S', false, ',', 0, 0,    1014, ("bpcharin", 1044),       ("bpcharout", 1045),      'i', 'x', 100),
ty(1082, "date",        4, true,  'b', 'D', false, ',', 0, 0,    1182, ("date_in", 1084),        ("date_out", 1085),       'i', 'p', 0),
ty(1114, "timestamp",   8, true,  'b', 'D', false, ',', 0, 0,    1115, ("timestamp_in", 1312),   ("timestamp_out", 1313),  'd', 'p', 0),
ty(1184, "timestamptz", 8, true,  'b', 'D', true,  ',', 0, 0,    1185, ("timestamptz_in", 1150), ("timestamptz_out", 1151),'d', 'p', 0), // 既存
ty(1700, "numeric",    -1, false, 'b', 'N', false, ',', 0, 0,    1231, ("numeric_in", 1701),     ("numeric_out", 1702),    'i', 'm', 0),   // 既存
ty(2205, "regclass",    4, true,  'b', 'N', false, ',', 0, 0,    2210, ("regclassin", 2218),     ("regclassout", 2219),    'i', 'p', 0),
ty(2206, "regtype",     4, true,  'b', 'N', false, ',', 0, 0,    2211, ("regtypein", 2220),      ("regtypeout", 2221),     'i', 'p', 0),
```

- `array_oid` は PG の値を入れる（`rows.rs` が「配列型の行がなければ typarray = 0」にする既存の規則どおり、行は 0 で出る。00 §12.1 の「typarray は 0」と一致）。`_int2`(1005) は既存の行。`int2vector` の `elem` は 21、`typcategory` は `A`。
- `typmodin` / `typmodout`（numeric 2917/2918、bpchar 2913/2914、timestamp 2905/2906、timestamptz 2907/2908）は、`rows.rs` が現在 0 を出す（M2）。**M4 でも 0 のまま**（`format_type` は typmodout を使わず `format_type_name` が持つ）。差分としては `pg_type.typmodin` が 0 のこと。
- `BuiltinType` / `is_supported_type`: `NUMERIC BPCHAR DATE TIMESTAMP TIMESTAMPTZ REGCLASS REGTYPE INT2VECTOR INT2_ARRAY` を足す。`is_null_only_type` から `TIMESTAMPTZ` と `INT2_ARRAY` を外す（`INT2_ARRAY` は `Int2Vector` 変種で値を持つ。`ACLITEM` `ANYARRAY` `TEXT_ARRAY` `OID_ARRAY` `CHAR_ARRAY` は NULL 専用のまま）。
- `analyzer/ddl.rs` の `KNOWN_UNSUPPORTED_TYPES` から `char`（※`"char"` ではなくパーサが作る `bpchar`）`numeric` `decimal` `timestamp` `timestamptz` `date` `regclass` `regtype` を外す。残る: `time` `timetz` `interval`（`0A000`）ほか。
- ストレージの `AttrDesc::from_type`: `type_align` に `TIMESTAMP`（`Double`）を足し、`DATE` `NUMERIC` `BPCHAR` `REGCLASS` `REGTYPE` `INT2VECTOR` は既定の `Int`。`fallback_len_byval` は `TYPES` に行があるので呼ばれない。

### 3.3 typmod の符号化と適用（`types/typmod.rs`、新設、T2）

```rust
// types/typmod.rs
use super::{Datum, Oid, SqlType, TypeEnv};
use crate::error::Result;

/// この型が typmod を持てるか（CoerceTypmod が要りうる型）: varchar, bpchar, numeric, timestamp, timestamptz
pub fn takes_typmod(type_oid: Oid) -> bool;

/// 型名の修飾子（`numeric(10,2)` の [10, 2]）から typmod を作る。アナライザの `resolve_type_name` が呼ぶ。
/// 修飾子なしの呼び出しはしない（SqlType::of(oid) を使う）。エラーは下の表
pub fn typmod_in(type_oid: Oid, mods: &[i64]) -> Result<i32>;

/// 値に typmod を適用する。CoerceTypmod の評価、INSERT / UPDATE の代入、COPY（`input_text_typed` 経由）が使う。
/// `explicit` = 明示キャスト（切り詰めを許す）。typmod < 0 または型が typmod を持たなければ d をそのまま返す
pub fn apply_typmod(d: Datum, ty: SqlType, explicit: bool) -> Result<Datum>;

/// `SqlType` の表示名（typmod つき）: "numeric(10,2)"、"character(3)"、"timestamp(3) without time zone"。format_type と同じ規則
pub fn display_with_typmod(ty: SqlType) -> String;
```

`typmod_in` の規則とエラー（実機で確認）:

| 型 | typmod の値 | エラー |
|---|---|---|
| `numeric` | 無指定 -1。`numeric(p)` = `((p << 16) \| 0) + 4`、`numeric(p,s)` = `((p << 16) \| (s & 0x7ff)) + 4`。実体は `yuzhu_numeric::make_typmod(&[p, s])` | p が 1..1000 の外: `22023` `NUMERIC precision 0 must be between 1 and 1000`。s が -1000..1000 の外: `22023` `NUMERIC scale 1001 must be between -1000 and 1000`。3 個以上: `22023` `invalid NUMERIC type modifier`。**s > p は許す**（PG15 以降。`1::numeric(3,5)` は構文は通り、値が収まらなければ 22003 の DETAIL に `10^-2` と出る） |
| `bpchar` | `char(n)` = n + 4。`char`（長さ省略）= `char(1)`（typmod 5）。`bpchar` と書くと -1 | n < 1: `22023` `length for type char must be at least 1`。n > 10485760（`MaxAttrSize`）: `22023` `length for type char cannot exceed 10485760`。メッセージの型名は常に `char` |
| `timestamp` / `timestamptz` | p そのもの（0..6）。無指定 -1 | p > 6 は 6 にする（D-9-10）。負はパーサの構文エラー |

`apply_typmod` の規則（`yuzhu-numeric` と `yuzhu-datetime` の関数を呼ぶ）:

| 型 | 処理 | エラー |
|---|---|---|
| `numeric` | `Numeric::apply_typmod(typmod)`（s 桁に 0 から遠い方へ丸め、整数部が p - s 桁を超えたらエラー）。**明示と代入で同じ**（varchar と違い切り詰めではない）。NaN は通る。±Infinity は通らない | `22003` `numeric field overflow`、DETAIL `A field with precision 4, scale 2 must round to an absolute value less than 10^2.`（Infinity は `... cannot hold an infinite value.`） |
| `bpchar` | 文字数で数える（バイト数ではない）。n より短ければ**右を空白で埋める**。n より長い: 明示は黙って切り詰め、代入は超過部分がすべて空白なら切り詰め、そうでなければエラー。typmod -1 は何もしない（空白を保持） | `22001` `value too long for type character(3)` |
| `timestamp` / `timestamptz` | `Timestamp::with_typmod(p)` / `TimestampTz::with_typmod(p)`（小数部を p 桁に丸める。infinity はそのまま） | `22008` `timestamp out of range`（丸めで範囲を超えたとき） |
| `varchar` | 既存の `ops::varchar_coerce` | `22001` |

**実機での確認**: `INSERT INTO t(c char(3)) VALUES ('abcdef')` は `22001 value too long for type character(3)`、`'abc  '` は `abc` で入る、`'ab'` は `ab ` で入る（`octet_length` = 3）。`'abcdef'::char(3)` は `abc`。`'é'::char(3)` の `octet_length` は 4（`é` が 2 バイト + 空白 2 個）。`12345.6789::numeric(7,2)` は `12345.68`、`99.5::numeric(2,0)` は 22003。`123.456::numeric(5,-1)` は `120`。

アナライザの `coerce_typmod`（`analyzer/coerce.rs`）は、`target.oid != VARCHAR` の条件を `!typmod::takes_typmod(target.oid)` に変える。`executor/eval.rs` の `coerce_typmod` は `typmod::apply_typmod` を呼ぶだけにする。

### 3.4 入出力（`TypeEnv`、`types/io.rs`）

`TypeEnv`（00 §12.4）に 1 つ足す。**00 への変更提案 P-1**（§13）。

```rust
// types/mod.rs
/// regclass / regtype / regproc の名前と OID の相互変換の口（types は catalog より下の層なので trait で受ける）
pub trait OidNames: std::fmt::Debug {
    /// regclassin: 名前（修飾可・引用符可）から OID。見つからなければ 42P01。数字だけなら OID としてそのまま
    fn class_oid(&self, name: &str) -> Result<Oid>;
    /// regclassout: 検索パスで見えれば修飾なし、見えなければ "schema.name"。存在しなければ None（数字で表示）
    fn class_name(&self, oid: Oid) -> Option<String>;
    /// regtypein: SQL の型名（別名・引用符・typmod 付きを許す）から OID
    fn type_oid(&self, name: &str) -> Result<Oid>;
    /// regtypeout: format_type_be 相当
    fn type_name(&self, oid: Oid) -> Option<String>;
    /// regprocout: 既存の builtin::regproc_name 相当
    fn proc_name(&self, oid: Oid) -> Option<String>;
}
pub struct TypeEnv<'a> {
    pub extra_float_digits: i32,
    pub datetime: Option<yuzhu_datetime::DateTimeEnv<'a>>,
    pub names: Option<&'a dyn OidNames>,        // ★ 追加。Default は None
}
```

`catalog` 側に `pub struct CatalogNames<'a>(pub &'a dyn CatalogReader);`（`impl OidNames`）を置く（担当 T3、`catalog/builtin.rs` の隣の `catalog/names.rs` ではなく `catalog/reader.rs` の末尾でよい）。`class_name` は `CatalogReader::relation_name`（00 §11.2）、`type_name` は `builtin::format_type_name(oid, None)`、`proc_name` は `builtin::regproc_name`、`class_oid` は `relation_kind`（表・インデックス・シーケンス。名前空間は共有）、`type_oid` は §3.9 の手順で解く。

```rust
// types/io.rs（00 §12.4 の署名。ty.typmod は input_text では無視する（M1 からの規則）。typmod は apply_typmod で別に適用する）
pub fn input_text(s: &str, ty: SqlType, env: &TypeEnv<'_>) -> Result<Datum>;
pub fn output_text(d: &Datum, ty: SqlType, env: &TypeEnv<'_>) -> Option<String>;
/// 入力関数 + typmod の適用（代入の意味）。COPY、Extended Query のテキストパラメータ、`'x'::char(3)` 以外の「PG の入力関数が typmod を取る」場面が使う
pub fn input_text_typed(s: &str, ty: SqlType, env: &TypeEnv<'_>) -> Result<Datum>;
/// アナライザが unknown リテラルを即座に評価してよい型か（日時型は false。D-9-3）
pub fn input_is_eager(type_oid: Oid) -> bool;
```

`input_text` の分岐を足す:

| 型 | 入力 | 出力 |
|---|---|---|
| `numeric` | `Numeric::parse(s)`（`NumericError` → `Error`。`22P02 invalid input syntax for type numeric: "x"`、オーバーフロー 22003）。前後の空白可、`NaN` `Infinity` `-Infinity` `inf` 可 | `Display`（常に指数なし、dscale 桁の小数部。`1e100` は `1` の後に 0 が 100 個。`-0.0` は `0.0`） |
| `bpchar` | `Datum::BpChar(s)`（typmod は無視。空白は保持） | パディング済みの文字列そのまま |
| `date` | `Date::parse(s, dt_env)`（`DateStyle` の日付順を使う）。`infinity` `-infinity` `epoch` `today` `tomorrow` `yesterday` `now` を受ける | `Date::format(dt_env)`（`2024-01-31`、BC は ` BC`、`infinity`） |
| `timestamp` | `Timestamp::parse(s, -1, dt_env)`。**含まれるタイムゾーンは黙って無視**（`'2024-01-01 00:00+09'::timestamp` = `2024-01-01 00:00:00`） | `Timestamp::format(dt_env)`（小数部は末尾のゼロを落とし、ゼロなら小数点ごと出さない） |
| `timestamptz` | `TimestampTz::parse(s, -1, dt_env)`。タイムゾーンがなければセッションの `TimeZone` で解釈 | `TimestampTz::format(dt_env)`（`+09`、`+05:30`、`+09:18:59` の形） |
| `regclass` / `regtype` | `names` が必要（`None` なら `Error::internal`）。数字だけの文字列（`'1259'`）は検査なしで OID。`'-'` は 0。それ以外は `OidNames::class_oid` / `type_oid` | `names.class_name(oid)` / `type_name(oid)`。OID が存在しない・`names` が `None` なら数字。0 は `-` |
| `int2vector` | 空白区切りの整数（`1 2 3`）。空文字列は空ベクタ。範囲外は `22003 value "70000" is out of range for type smallint`、不正は `22P02 invalid input syntax for type smallint: "x"`、要素数 > 100 は `22023`（`oidvector` と同じ上限 `OIDVECTOR_MAX`） | 空白区切り（`1 2 3`） |
| `int2[]`（oid 1005） | `{1,2,3}` 形式（既存の `int4_array_in` の流用。NULL 要素は `0A000`、多次元と明示的な次元は `0A000`） | `{1,2,3}` |

日時型の入出力は `dt_env()` を取る: `env.datetime` が `None` なら `Error::internal("date/time input requires a DateTimeEnv")`。`DateTimeError` → `Error` の変換は `types/datetime.rs` の `fn dt_err(e: DateTimeError) -> Error`（`SqlState(e.sqlstate)`、`detail`、`hint` をそのまま写す。`SqlState` は `&'static str` を取るので定数不要）。`NumericError` も同様の `num_err`。

エラー文言（実機で確認、`yuzhu-datetime` / `yuzhu-numeric` が既に一致させている）:

| 状況 | SQLSTATE | 文言 |
|---|---|---|
| `'garbage'::timestamp` | 22007 | `invalid input syntax for type timestamp: "garbage"` |
| `'2024-13-01'::date` | 22008 | `date/time field value out of range: "2024-13-01"`、HINT `Perhaps you need a different "datestyle" setting.` |
| `294277-01-01` | 22008 | `timestamp out of range: "294277-01-01"` |
| `+16` オフセット | 22009 | `time zone displacement out of range: "..."` |
| `'x'::numeric` | 22P02 | `invalid input syntax for type numeric: "x"` |
| `1::int2vector` 要素が範囲外 | 22003 | `value "70000" is out of range for type smallint` |

SQLSTATE 定数（`error.rs`、00 §15.3 に**足りないもの**。**00 への変更提案 P-2**）: `INVALID_DATETIME_FORMAT = "22007"`、`DATETIME_FIELD_OVERFLOW = "22008"`、`INVALID_TIME_ZONE_DISPLACEMENT_VALUE = "22009"`。

### 3.5 ディスク上の形式（00 §12.3 の詳細化）★

ヒープとインデックスのタプル共通。可変長（numeric、bpchar、int2vector）は M2 の varlena（`storage/heap/tuple.rs` の `write_varlena`）: ペイロードが 126 バイト以下なら 1 バイトヘッダ `((len + 1) << 1) | 1`（整列なし）、それ以上なら 4 バイトヘッダ（`typalign` に整列、`(len + 4) << 2` の LE）。固定長は `typalign` に整列して LE。

| 型 | typalign | ペイロード |
|---|---|---|
| `numeric` | `i` | `ndigits: u16`、`weight: i16`、`sign: u16`、`dscale: u16`、`digits: [u16; ndigits]`（すべて LE。各桁 0〜9999）。`sign`: `0x0000` 正、`0x4000` 負、`0xC000` NaN、`0xD000` +Infinity、`0xF000` -Infinity。0 は `ndigits = 0, weight = 0, sign = 0`。特殊値は `ndigits = 0`、`weight = 0`、`dscale = 0`。桁は先頭と末尾のゼロを落とした正規形（`Finite::digits()` がそのまま）。`dscale` は 0..16383、`weight` は i16 の範囲 |
| `bpchar` | `i` | UTF-8 のバイト列（パディング後）。typmod -1 の値は空白を保持したまま |
| `date` | `i` | `i32` LE（2000-01-01 からの日数。`i32::MIN` = `-infinity`、`i32::MAX` = `infinity`） |
| `timestamp` / `timestamptz` | `d` | `i64` LE（2000-01-01 00:00:00 からのマイクロ秒。`timestamptz` は UTC。`i64::MIN` / `i64::MAX` が ∓infinity） |
| `regclass` / `regtype` | `i` | `u32` LE |
| `int2vector` / `int2[]` | `i` | `i16` LE の並び（PG の配列ヘッダは持たない。要素数はペイロード長 / 2） |

バイト例（1 バイトヘッダのとき、先頭の 1 バイトが `((len + 1) << 1) | 1`）:

| 値 | バイト列（16 進） |
|---|---|
| `numeric` `1.5`（digits `[1, 5000]`、weight 0、dscale 1） | `1B 02 00 00 00 00 00 01 00 01 00 88 13`（ペイロード 12 バイト → ヘッダ `(13 << 1) \| 1 = 0x1B`。5000 = `0x1388` → `88 13`） |
| `numeric` `-123.456`（digits `[123, 4560]`、weight 0、sign `0x4000`、dscale 3） | `1B 02 00 00 00 00 40 03 00 7B 00 D0 11` |
| `numeric` `0`（dscale 0） | `13 00 00 00 00 00 00 00 00` |
| `numeric` `NaN` | `13 00 00 00 00 00 C0 00 00` |
| `bpchar` `'ab '`（`char(3)`） | `09 61 62 20` |
| `date` `2024-01-01`（8766 日 = `0x223E`） | `3E 22 00 00` |
| `timestamp` `2000-01-01 00:00:01.5`（1500000 = `0x16E360`） | `60 E3 16 00 00 00 00 00`（先頭が 8 の倍数に整列される） |
| `regclass` `16384` | `00 40 00 00` |
| `int2vector` `{1,2}` | `0B 01 00 02 00` |

エンコード・デコードの関数は `types/numeric.rs` などに置き、`storage/heap/tuple.rs`（担当 H4）は `Kind` の分岐からそれを呼ぶだけにする:

```rust
// types/numeric.rs
pub fn encode_numeric(n: &Numeric, out: &mut Vec<u8>);                 // ペイロードのみ（varlena ヘッダは tuple.rs）
pub fn decode_numeric(b: &[u8]) -> Result<Numeric>;                    // 壊れていたら XX001（Numeric::from_parts は 22P03 を返すので写し替える）
// types/bpchar.rs
pub fn decode_bpchar(b: &[u8]) -> Result<String>;                      // UTF-8 検査（XX001）
```

`tuple.rs` の `Kind` への追加（H4 に依頼）: `Numeric`（varlena）、`BpChar`（varlena）、`Date`（i32、`Int4` と同じ配置）、`Timestamp` / `TimestampTz`（i64、`Int8` と同じ配置）、`Int2Vector`（varlena）。`oid::REGCLASS | oid::REGTYPE` は既存の `Kind::Oid` に足す。`kind_of` の `NullOnly` から `TIMESTAMPTZ` と `INT2_ARRAY` を外す。インデックスタプル（B+Tree）も同じ符号化を使う（比較はデコードして `cmp_datum`）。

**`BT_MAX_ITEM_SIZE`（2704）との関係**: numeric（ndigits が多い）と bpchar（`char(2000)` など）の列をキーにすると超えうる。超えたら 00 §13.5 のとおり `54000`。

### 3.6 キャストの表（`pg_cast`）

`CASTS`（`catalog/builtin.rs`）の追加と変更。`context`: `i` 暗黙、`a` 代入、`e` 明示。`method`: `F` = `CastMethod::Function`（純粋）、`E` = `CastMethod::Env`（TypeEnv を使う）、`B` = `CastMethod::Binary`（同じ `Datum` 変種を再利用）、`bin*` = PG では `'b'`（バイナリ互換）だが `Datum` 変種が変わるので `cast_bin` の形（関数を実行、`pg_method = 'b'`）、`IO` = I/O 変換。castfunc は PG17 の OID。

**numeric**（M2 の `ops::unsupported` を本体に差し替える）

| source → target | ctx | method | castfunc | 本体 |
|---|---|---|---|---|
| int2 → numeric | i | F | 1782 | `Numeric::from(i16)` |
| int4 → numeric | i | F | 1740 | `Numeric::from(i32)` |
| int8 → numeric | i | F | 1781 | `Numeric::from(i64)` |
| float4 → numeric | a | F | 1742 | `Numeric::from_f32`（有効 6 桁に丸める） |
| float8 → numeric | a | F | 1743 | `Numeric::from_f64`（有効 15 桁に丸める。`0.30000000000000004::float8::numeric` = `0.3`） |
| numeric → int2 / int4 / int8 | a | F | 1783 / 1744 / 1779 | `to_i16` / `to_i32` / `to_i64`（0 から遠い方へ丸める。範囲外 `22003 smallint out of range` など。NaN と Infinity は `0A000 cannot convert NaN to integer`） |
| numeric → float4 / float8 | i | F | 1745 / 1746 | `to_f32` / `to_f64`（範囲外 `22003`） |
| numeric → numeric（長さ強制） | i | CoerceTypmod | 1703 | `CASTS` には入れない（varchar と同じ M2 の方針。`pg_cast` にこの行は出ない） |

**bpchar**

| source → target | ctx | method | castfunc | 本体 |
|---|---|---|---|---|
| text → bpchar | i | bin | 0 | `Text(s)` → `BpChar(s)` |
| varchar → bpchar | i | bin | 0 | 同上 |
| bpchar → text | i | F | 401 | 末尾の**空白（U+0020 だけ）**を落とす（`rtrim1`） |
| bpchar → varchar | i | F | 401 | 同上 |
| bpchar → name | i | F | 409 | 末尾の空白を落とし、63 バイトで切る（`truncate_identifier`） |
| name → bpchar | a | F | 408 | `Text` → `BpChar` |
| "char" → bpchar | a | F | 860 | 1 文字（`"char"` の 0 は空文字列） |
| bpchar → "char" | a | F | 944 | 先頭の 1 バイト（`text_to_char` を `as_str` で流用） |
| bool → bpchar | a | F | 2971 | `"true"` / `"false"`（`bool_to_text`）を `BpChar` に包む（typmod は別の CoerceTypmod が切る。`cast(true as char(3))` = `tru`） |
| bpchar → bpchar（長さ強制） | i | CoerceTypmod | 668 | `CASTS` には入れない |

bpchar と他の型（numeric、date など）の変換は、`pg_cast` に行がなく PG の自動 I/O 規則（変換先が文字列カテゴリなら代入以上、変換元が文字列カテゴリなら明示）で通る（既存の `find_coercion_pathway`）。`cast(1.5 as char(3))` = `1.5`、`cast(now() as char(3))` = `202`。

**日時**

| source → target | ctx | method | castfunc | 本体 |
|---|---|---|---|---|
| date → timestamp | i | F | 2024 | `Date::to_timestamp`（範囲外 `22008 date out of range for timestamp`） |
| timestamp → date | a | F | 2029 | `Timestamp::to_date` |
| date → timestamptz | i | **E** | 1174 | `Date::to_timestamptz(tz)` |
| timestamp → timestamptz | i | **E** | 2028 | `Timestamp::to_timestamptz(tz)` |
| timestamptz → timestamp | a | **E** | 2027 | `TimestampTz::to_timestamp(tz)` |
| timestamptz → date | a | **E** | 1178 | `TimestampTz::to_date(tz)` |
| timestamp → timestamp / timestamptz → timestamptz（長さ強制） | i | CoerceTypmod | 1961 / 1967 | `CASTS` には入れない |

pgbench の `INSERT ... VALUES (..., CURRENT_TIMESTAMP)`（timestamptz → timestamp 列）は、代入文脈で上の 4 行目の `E` が使われる。`timestamp → time` などは `time` を持たないので入れない。

**regclass / regtype / int2vector**（M2 の `oid` / `regproc` の行と同じ形）

| source → target | ctx | method | castfunc |
|---|---|---|---|
| int2 → regclass / regtype | i | bin（`ops::int_to_oid`） | 313 |
| int4 → regclass / regtype | i | bin（`ops::int_to_oid`） | 0 |
| int8 → regclass / regtype | i | F（`ops::int8_to_oid`） | 1287 |
| oid → regclass / regtype | i | B | 0 |
| regclass / regtype → oid | i | B | 0 |
| regclass / regtype → int4 | a | bin（`ops::oid_to_int4`） | 0 |
| regclass / regtype → int8 | a | F（`ops::oid_to_int8`） | 1288 |
| text → regclass | i | **E**（`text_to_regclass`） | 1079 |
| varchar → regclass | i | **E** | 1079 |

`text → regclass` の本体は `env.names.class_oid`（`names` が `None` なら `Error::internal`）。名前を持たない `regtype` への `text` キャストは `pg_cast` になく、明示キャストは I/O（`regtypein`）で通る。`name → regclass` も I/O。`regclass → text` は I/O（代入）で、`output_text` が `names` を使うので**実行時にカタログを引く**（`'pg_class'::regclass::text` = `pg_class`、`1259::regclass::text` = `pg_class`、存在しない OID は `99999999`）。

**既存の行への追加**: なし（`bool → text/varchar` などはそのまま）。`unknown → X` は行を持たず、リテラルは §6.1 で直接変換する。

### 3.7 演算子の表（`OPERATORS`、`OPERATOR_META`）

`func` 列は `types/ops.rs`（または `types/numeric.rs`、`types/datetime.rs`、`types/bpchar.rs`、`types/regex.rs`）の関数名。`oprcode` は `OperatorMeta.proc_oid`、`com` と `neg` は `OperatorMeta.com` / `negate`（0 = なし。`rows.rs` が `OPERATORS` にない相手は 0 で書く既存の規則）。**`OPERATOR_MERGE_HASH`（`oprcanmerge` / `oprcanhash`）は M2 の表に既に 1054・1093・1320・1752・2060 が入っているので変更しない。**

#### numeric（T1）

M2 に行がある 1751 1752〜1762 1921 1038 の本体を差し替える（`1038 ^` は `ops::unsupported` のまま。メッセージは `0A000 numeric power is not supported yet`）。

| OID | 演算子 | 左 | 右 | 結果 | 本体 | oprcode | com | neg |
|---|---|---|---|---|---|---|---|---|
| 1752 | `=` | numeric | numeric | bool | `cmp_eq` | 1718 | 1752 | 1753 |
| 1753 | `<>` | numeric | numeric | bool | `cmp_ne` | 1719 | 1753 | 1752 |
| 1754 | `<` | numeric | numeric | bool | `cmp_lt` | 1722 | 1756 | 1757 |
| 1755 | `<=` | numeric | numeric | bool | `cmp_le` | 1723 | 1757 | 1756 |
| 1756 | `>` | numeric | numeric | bool | `cmp_gt` | 1720 | 1754 | 1755 |
| 1757 | `>=` | numeric | numeric | bool | `cmp_ge` | 1721 | 1755 | 1754 |
| 1758 | `+` | numeric | numeric | numeric | `numeric_add` | 1724 | 1758 | 0 |
| 1759 | `-` | numeric | numeric | numeric | `numeric_sub` | 1725 | 0 | 0 |
| 1760 | `*` | numeric | numeric | numeric | `numeric_mul` | 1726 | 1760 | 0 |
| 1761 | `/` | numeric | numeric | numeric | `numeric_div` | 1727 | 0 | 0 |
| 1762 | `%` | numeric | numeric | numeric | `numeric_mod` | 1729 | 0 | 0 |
| 1751 | `-`（前置） | - | numeric | numeric | `numeric_uminus` | 1771 | 0 | 0 |
| 1921 | `+`（前置） | - | numeric | numeric | `identity` | 1915 | 0 | 0 |
| 1038 | `^` | numeric | numeric | numeric | `unsupported` | 1739 | 0 | 0 |

本体の対応（`yuzhu_numeric::Numeric`）: `+` = `checked_add`、`-` = `checked_sub`、`*` = `checked_mul`、`/` = `checked_div`（結果の scale は `select_div_scale`、0 から遠い方へ丸め）、`%` = `checked_rem`（被除数の符号、scale は max(s1, s2)）、単項 `-` = `negate`。ゼロ除算は `22012 division by zero`（`/` と `%`。`'Infinity'::numeric / 0` も）。結果が範囲外は `22003 value overflows numeric format`。比較は `Numeric` の全順序（`-Infinity < 有限 < +Infinity < NaN`、`NaN = NaN`、`1.10 = 1.1`）を `cmp_datum` 経由で。

**整数・浮動小数との混合は演算子の行を作らず、暗黙キャストで決まる**（PG に numeric と整数の混合演算子はない）: `1.5 + 1` は numeric、`1.5 + 1.0::float8` は float8（numeric → float8 が暗黙、float8 → numeric は代入のみ。実機で `pg_typeof` = `double precision`）、`numeric = float8` は float8 比較。M1 の `0A000`（小数リテラルを拒否）をやめる（§6.1）。

#### bpchar（T2）

| OID | 演算子 | 左 | 右 | 本体 | oprcode | com | neg |
|---|---|---|---|---|---|---|---|
| 1054 | `=` | bpchar | bpchar | `cmp_eq` | 1048 | 1054 | 1057 |
| 1057 | `<>` | bpchar | bpchar | `cmp_ne` | 1053 | 1057 | 1054 |
| 1058 | `<` | bpchar | bpchar | `cmp_lt` | 1049 | 1060 | 1061 |
| 1059 | `<=` | bpchar | bpchar | `cmp_le` | 1050 | 1061 | 1060 |
| 1060 | `>` | bpchar | bpchar | `cmp_gt` | 1051 | 1058 | 1059 |
| 1061 | `>=` | bpchar | bpchar | `cmp_ge` | 1052 | 1059 | 1058 |
| 1211 | `~~` | bpchar | text | `bpcharlike` | 1631 | 0 | 1212 |
| 1212 | `!~~` | bpchar | text | `bpcharnlike` | 1632 | 0 | 1211 |
| 1629 | `~~*` | bpchar | text | `bpchariclike` | 1660 | 0 | 1630 |
| 1630 | `!~~*` | bpchar | text | `bpcharicnlike` | 1661 | 0 | 1629 |
| 1055 | `~` | bpchar | text | `bpcharregexeq` | 1658 | 0 | 1056 |
| 1056 | `!~` | bpchar | text | `bpcharregexne` | 1659 | 0 | 1055 |
| 1234 | `~*` | bpchar | text | `bpcharicregexeq` | 1656 | 0 | 1235 |
| 1235 | `!~*` | bpchar | text | `bpcharicregexne` | 1657 | 0 | 1234 |

- 比較は `cmp_datum`（末尾の空白を無視してバイト比較）。`'a'::char(3) < 'a '::char(3)` は偽（同じ値）。
- **`bpchar` と `text` の比較は `text = text` に解決される**（`bpchar → text` が暗黙で rtrim するため。`'abc'::char(5) = 'abc '::text` は偽、実機で確認）。`varchar = bpchar` は完全一致の数で `bpchar = bpchar` になる（`'abc '::varchar = 'abc'::char(5)` は真、実機で確認）。M1 の解決アルゴリズム（`research-pg-types.md` §3.1）が正しく決める。
- **LIKE と正規表現は、パディングした値のまま照合する**（`'a'::char(2) LIKE 'a_'` は真、`'a '::char(2) LIKE 'a'` は偽、`'a'::char(3) ~ 'a$'` は偽、`'a *$'` は真。実機で確認）。`Expr::Like`（00 §6.2）の `expr` が bpchar のとき、アナライザは `text` へのキャストを**挟まない**（挟むと rtrim される）。`Like` の評価は `as_str()` で `Text` と `BpChar` の両方を受ける。
- bpchar の `~<~` 系（`bpchar_pattern_ops`）は作らない（M4 の opclass にない）。

#### text / name の正規表現（T3）

M2 に `~~` 系（`text` の LIKE 演算子 1209 1210 1627 1628）はある。正規表現の演算子を足す。

| OID | 演算子 | 左 | 右 | 本体 | oprcode | neg |
|---|---|---|---|---|---|---|
| 641 | `~` | text | text | `textregexeq` | 1254 | 642 |
| 642 | `!~` | text | text | `textregexne` | 1256 | 641 |
| 1228 | `~*` | text | text | `texticregexeq` | 1238 | 1229 |
| 1229 | `!~*` | text | text | `texticregexne` | 1239 | 1228 |
| 639 | `~` | name | text | `nameregexeq` | 79 | 640 |
| 640 | `!~` | name | text | `nameregexne` | 1252 | 639 |
| 1226 | `~*` | name | text | `nameicregexeq` | 1240 | 1227 |
| 1227 | `!~*` | name | text | `nameicregexne` | 1241 | 1226 |

`com` はすべて 0。本体はすべて `regex_match(subject.as_str(), pattern, icase)` を呼び、`ne` は反転する。NULL は strict 関数として executor が処理する。psql の `OPERATOR(pg_catalog.~)`（`03-parser-analyzer.md` が構文を受け持つ）はこの表の行に解決される。`name` の左辺は `Datum::Text`。

#### date / timestamp / timestamptz（T2）

| OID | 演算子 | 型 | 本体 | oprcode | com | neg |
|---|---|---|---|---|---|---|
| 1093 / 1094 / 1095 / 1096 / 1097 / 1098 | `=` `<>` `<` `<=` `>` `>=` | date, date | `cmp_*` | 1086 / 1091 / 1087 / 1088 / 1089 / 1090 | 1093 / 1094 / 1097 / 1098 / 1095 / 1096 | 1094 / 1093 / 1098 / 1097 / 1096 / 1095 |
| 2060 / 2061 / 2062 / 2063 / 2064 / 2065 | 同上 | timestamp, timestamp | `cmp_*` | 2052 / 2053 / 2054 / 2055 / 2057 / 2056 | 2060 / 2061 / 2064 / 2065 / 2062 / 2063 | 2061 / 2060 / 2065 / 2064 / 2063 / 2062 |
| 1320 / 1321 / 1322 / 1323 / 1324 / 1325 | 同上 | timestamptz, timestamptz | `cmp_*` | 1152 / 1153 / 1154 / 1155 / 1157 / 1156 | 1320 / 1321 / 1324 / 1325 / 1322 / 1323 | 1321 / 1320 / 1325 / 1324 / 1323 / 1322 |
| 1100 | `+` | date, int4 → date | `date_pli` | 1141 | 2555 | 0 |
| 2555 | `+` | int4, date → date | `integer_pl_date` | 2550 | 1100 | 0 |
| 1101 | `-` | date, int4 → date | `date_mii` | 1142 | 0 | 0 |
| 1099 | `-` | date, date → int4 | `date_mi` | 1140 | 0 | 0 |

（M2 の 1093〜1098、1099〜1101、2555 は `ops::unsupported` の行。本体を差し替える。）

- 比較は `Date` / `Timestamp` / `TimestampTz` の整数比較（infinity は `i32::MIN/MAX`・`i64::MIN/MAX` なのでそのまま正しく並ぶ）を `cmp_datum` で。
- `date_pli` = `Date::add_days(i32)`、`date_mii` = `sub_days`、`date_mi` = `sub_date`（無限大が絡むと `22008 cannot subtract infinite dates`）。範囲外は `22008 date out of range`。
- **`timestamp - timestamp`（2067）、`timestamptz - timestamptz`（1328）、`date + interval` などの interval を含む行は追加しない**（D-9-1）。M2 にある interval の行（1330〜1338、1583〜1585、1336）は変更しない。
- 型をまたぐ比較（2345〜2350、2358〜2363、2371〜2376、2384〜2389、2534〜2545 の 30 行）は作らない（D-9-5）。ただし `date = timestamp` は両辺が `date → timestamp`（暗黙・純粋）で `timestamp = timestamp` に解決される。

#### 追加の整数・浮動小数の演算子

なし（M2 にある）。ただし `numeric` を導入すると演算子解決の候補が増えるため、次の**解決結果**をテストで固定する（§10）: `1 + 1.5` → numeric、`1.5 + 1.5::float8` → float8、`2 ^ 3` → float8（`965 ^ (float8,float8)`。`2.0 ^ 3` は numeric の `^` に解決されて `0A000`）、`10 / 4.0` → numeric `2.5000000000000000`。

### 3.8 関数の表（`FUNCTIONS`、`PROCS`）

`FUNCTIONS` に足す行（`func(oid, name, args, result, body)` = `FnKind::Pure`、`rt_func` = `FnKind::Runtime`、`ctx_func` = `FnKind::Context`。すべて strict で、`PROCS` の `volatility` は実機の `provolatile`）。

**numeric（T1）**

| OID | 関数 | 引数 → 結果 | 本体 | vol |
|---|---|---|---|---|
| 1705（既存、本体差し替え） | `abs` | numeric → numeric | `Numeric::abs` | i |
| 1706 | `sign` | numeric → numeric | `Numeric::sign`（`sign(0.0)` = `0`） | i |
| 1707 | `round` | numeric, int4 → numeric | `Numeric::round(n)`（0 から遠い方へ。負の n 可、`round(1234.5678, -2)` = `1200`） | i |
| 1708 | `round` | numeric → numeric | `round(0)` | i |
| 1709 | `trunc` | numeric, int4 → numeric | `Numeric::trunc(n)` | i |
| 1710 | `trunc` | numeric → numeric | `trunc(0)` | i |
| 1711 | `ceil` | numeric → numeric | `Numeric::ceil` | i |
| 2167 | `ceiling` | numeric → numeric | 同上 | i |
| 1712 | `floor` | numeric → numeric | `Numeric::floor` | i |
| 1728 | `mod` | numeric, numeric → numeric | `checked_rem` | i |
| 1973 | `div` | numeric, numeric → numeric | `div_trunc` | i |
| 3281 | `scale` | numeric → int4 | `Numeric::scale()`（NaN / Infinity は NULL） | i |

**float8 の丸め（T1）**: `round(2)` は PG では `double precision`（`int4` → `float8` が暗黙で preferred のため `round(float8)` が選ばれる。実機の `pg_typeof(round(2))` = `double precision`、`ceil(2)` も同じ）。**これらを入れないと `round(2)` が numeric を返して黙って PG と違う**ので必須。

| OID | 関数 | 引数 → 結果 | 本体 | vol |
|---|---|---|---|---|
| 1342 | `round` | float8 → float8 | `f64::round_ties_even`（`rint`。`round(2.5::float8)` = `2`） | i |
| 1343 | `trunc` | float8 → float8 | `f64::trunc` | i |
| 2308 | `ceil` | float8 → float8 | `f64::ceil` | i |
| 2320 | `ceiling` | float8 → float8 | 同上 | i |
| 2309 | `floor` | float8 → float8 | `f64::floor` | i |
| 2310 | `sign` | float8 → float8 | -1 / 0 / 1（NaN は NaN） | i |
| 940 / 941 / 947 | `mod` | int2,int2 / int4,int4 / int8,int8 → 同型 | 既存の `int2mod` / `int4mod` / `int8mod` | i |

`mod(5,2)` が `integer`（numeric ではない）になるために整数版が要る。

**日時（T2）**

| OID | 関数 | 引数 → 結果 | 種別 | vol |
|---|---|---|---|---|
| 1299 | `now` | → timestamptz | `rt_func`: `TimestampTz(rt.transaction_timestamp())` | s |
| 2647 | `transaction_timestamp` | → timestamptz | 同上 | s |
| 2648 | `statement_timestamp` | → timestamptz | `rt.statement_timestamp()` | s |
| 2649 | `clock_timestamp` | → timestamptz | `rt.clock_timestamp()` | v |
| 2029 | `date` | timestamp → date | `func`（純粋） | i |
| 1178 | `date` | timestamptz → date | `rt_func`: `to_date(&rt.datetime_env().time_zone)` | s |
| 2024 | `timestamp` | date → timestamp | `func` | i |
| 2027 | `timestamp` | timestamptz → timestamp | `rt_func` | s |
| 1174 | `timestamptz` | date → timestamptz | `rt_func` | s |
| 2028 | `timestamptz` | timestamp → timestamptz | `rt_func` | s |

（`date(x)` 形式は pg_cast の明示キャストと同じ変換。`CASTS` の行の本体とこの関数は `types/datetime.rs` の同じ関数を呼ぶ: `fn ts_to_tstz(d: &Datum, env: &DateTimeEnv) -> Result<Datum>` など。）

`now()` と `transaction_timestamp()` は同じ値（`Transaction.started_at`、00 §14.1）。`RuntimeInfo.datetime_env()`（00 §14.3）と `ExecCtx.type_env.datetime` は、session が同じ設定・`ZoneDb`・`started_at` から作るので同じ値を返す。

**bpchar（T2）**

| OID | 関数 | 引数 → 結果 | 本体 |
|---|---|---|---|
| 1318 | `length` | bpchar → int4 | 末尾の空白を除いた**文字数**（`length('ab   '::char(5))` = 2） |
| 1367 | `character_length` | bpchar → int4 | 同上 |
| 1372 | `char_length` | bpchar → int4 | 同上 |
| 1375 | `octet_length` | bpchar → int4 | **パディング込みのバイト数**（`octet_length('ab '::char(5))` = 5） |

**regclass / regtype / 型名（T3）**

| OID | 関数 | 引数 → 結果 | 種別 |
|---|---|---|---|
| 1079 | `regclass` | text → regclass | `ctx_func`（カタログ。`CatalogNames`） |
| 3495 | `to_regclass` | text → regclass | `ctx_func`（見つからなければ NULL。strict） |
| 3493 | `to_regtype` | text → regtype | `ctx_func` |
| 1619 | `pg_typeof` | "any" → regtype | **アナライザが畳む**（D-9-9）。行は `FUNCTIONS` に非 strict で置き、本体は `Err(Error::internal(..))`（到達しない） |

`pg_typeof` の解決: 引数の型 `"any"`（`oid::ANY`）は任意の型を受ける（解決アルゴリズムの `can_coerce` で `target == oid::ANY` を常に真にする。**N3 でなく T3 が `analyzer/resolve.rs` の該当 1 行を依頼する**）。結果は `Literal(Datum::Oid(arg.ty.oid))`（型は regtype）。`pg_typeof('a')` は `unknown`、`pg_typeof(NULL)` は `unknown`、`pg_typeof(1)` は `integer`、`pg_typeof(pg_typeof(1))` は `regtype`。

**集合返却（T3、§10.1）**: `generate_series` の 4 行（1067 1066 1069 1068）。

`PROCS` の行は、上の `oprcode`、`castfunc`、型の入出力関数（`numeric_in` 1701 … `regtypeout` 2221、`bpcharin` 1044、`date_in` 1084、`timestamp_in` 1312、`int2vectorin` 40）、`FUNCTIONS`、`AGGREGATES`（`prokind = 'a'`、`prosrc = aggregate_dummy`、`proisstrict = false`、`provolatile = 'i'`、`proparallel = 's'`、`prolang = 12`。実機で確認）を参照するすべての OID について必要。**生成方法**: 次の psql（PG17）で行を作り、`BuiltinProc { .. }` に整形するスクリプトを `tools/gen_procs.sh` に置く（T1〜T3 が手で写さない）。

```sql
SELECT oid, proname, proargtypes::oid[], prorettype, proisstrict, provolatile, proparallel, proleakproof, procost, prosrc
FROM pg_proc WHERE oid IN (/* 上の OID の和集合 */) ORDER BY oid;
```

`rows.rs` は `FUNCTIONS` の同じ OID の行が `FnKind::Set` なら `proretset = true` にする（C1 に依頼）。

### 3.9 regclass / regtype（`types/sys.rs`、T3）

- `Datum::Oid(u32)`。`regclass`・`regtype` は `oid` と同じ比較（符号なし）、`regclass = oid` は暗黙キャストで `oid = oid`（607）に解決される。`regclass + 1` は `42883 operator does not exist: regclass + integer`（実機で確認）。
- **リテラルのアナライザでの畳み込み**（D-9-3）: `'pg_class'::regclass` は `coerce_type` で `Literal(Datum::Oid(oid))`（型 regclass）になる。`names` は `CatalogNames(self.catalog)`。評価は `regclass_in`。DEFAULT の `nextval('t_id_seq'::regclass)` も同じ経路（DEFAULT のテキストは使うたびに解析されるので OID が引かれる。PG は作成時に OID を保存するので、シーケンスをリネームしたときの挙動が違う。既知の差分、`08-sequence-serial.md` が扱う）。
- `regclassin` の規則（実機で確認）:

| 入力 | 結果 |
|---|---|
| `'pg_class'` `'PG_CLASS'` `'"pg_class"'` `' pg_class '` | OID 1259（引用符なしは小文字化、前後の空白可） |
| `'pg_catalog.pg_class'` | 修飾名。`db.schema.rel` の 3 要素は `db` が現在の DB のときだけ可 |
| `'1259'` | OID としてそのまま（存在検査なし）。`'99999999'` も可 |
| `'-'`、`0` | OID 0（出力は `-`） |
| `'nosuch'` | `42P01 relation "nosuch" does not exist`。修飾つきは `relation "public.nosuch" does not exist` |
| `''`、`'foo bar'`、`'public.'` | `42602 invalid name syntax` |
| `'a.b.c.d'` | `42601 improper relation name (too many dotted names): a.b.c.d` |

- `regclassout`: 検索パスで見える（`pg_catalog` と `search_path` の名前空間）なら修飾しない。見えなければ `schema.name`。引用が必要な名前（大文字・記号）は `"Foo"` のように引用して出す（`quote_identifier`）。存在しなければ OID の数字。
- `regtypein`: 数字だけは OID。それ以外は SQL の型名として解く: 引用符・大文字小文字・`pg_catalog.` 修飾・別名（`int` `integer` `int4` `bigint` `smallint` `real` `double precision` `boolean` `character varying` `varchar` `char` `character` `bpchar` `decimal` `numeric` `timestamp` `timestamp with time zone` `timestamptz` `date` `text` `name` `oid` `regclass` `regtype` ほか `TYPES` の名前すべて）、`varchar(10)` の修飾子は黙って捨てる（OID だけ返す）、`text[]` の配列は配列型の OID（`TYPES` に行があるものだけ）。見つからなければ `42704 type "nosuch" does not exist`。構文が壊れていれば `42601 invalid type name "..."`（PG は `syntax error` / `trailing junk` などでメッセージが違う。SQLSTATE だけ一致）。実装は `sql::parser` の型名パーサを再利用できるなら使い、なければ小さな手書きで足りる（引用符つきの識別子、空白区切りの語、末尾の `(n)`、末尾の `[]`）。`interval` `time` など `TYPES` に行がある型は OID を返す（実行時に使えないだけ）。
- `regtypeout`: `format_type_be` 相当（`integer`、`character varying`、`"char"`、`character`（bpchar）、`timestamp with time zone`、`text[]`）。
- 出力段（`executor/eval.rs` の `row_to_text`）と `CoerceViaIO`（`regclass::text`）はどちらも `output_text(d, ty, &env)` を呼ぶだけでよくなる（`env.names` が regclass / regtype / regproc を処理する。M2 の `output_text_regproc` は `env.names.proc_name` に吸収して廃止する）。

### 3.10 int2vector と int2[]

`Datum::Int2Vector(Vec<i16>)`。`int2vector`（22）の出力は空白区切り、`int2[]`（1005）は `{1,2}`。どちらも `pg_index.indkey`、`pg_constraint.conkey` などのカタログ列の値として使い、**ユーザーテーブルの列型としては許す**（ディスク形式は §3.5。配列の一般対応は M5 なので、`CREATE TABLE t(a int2[])` は `0A000 array types are not supported yet` のまま。`int2vector` 列の作成は PG も実用上しない）。比較は要素の辞書順（`Vec::cmp`。PG の `array_cmp` と同じ）。`int2vector` の演算子は作らない（PG にも `=` がない）。`array_length` などの配列関数は M5（M2 の `ops::array_unsupported` のまま）。

---

## 4. 型変換の経路（処理の流れ）

### 4.1 unknown リテラル → 型

1. `analyzer/coerce.rs` の `coerce_type`（unknown のリテラルを `target` へ）:
   - `io::input_is_eager(target)` が真（numeric、bpchar、int2vector、整数、float、text 系、oid、regclass / regtype）: `io::input_text(s, SqlType::of(target), &env)` で即座に `Literal` にする。`env` は `TypeEnv { names: Some(&CatalogNames(self.catalog)), ..Default::default() }`。エラーは `.with_span`。
   - 日時型（date、timestamp、timestamptz）: `Cast { expr: Literal(Text) (型 unknown), method: InOut }`（型 target）を作る。
2. プランナの `const_fold`（`04-planner-optimizer.md`）が、`Cast { Literal(Text), InOut }` を `io::input_text(.., env.type_env)` で評価して `Literal` にする。この畳み込みは `PlanEnv.type_env`（`now` = トランザクション開始時刻、`TimeZone`、`DateStyle`）を使う。`'now'` `'today'` `'tomorrow'` `'yesterday'` は `env.now` から。
3. `CoerceTypmod` が上に乗っていれば、`Literal` に対する `apply_typmod` も畳み込む（`'2024-01-01 10:00:00.6'::timestamp(0)` → `2024-01-01 10:00:01`）。
4. DDL（CHECK・DEFAULT）と COPY は `lower_single_rel` + `eval_const` で行ごとに評価する（`EvalCtx` は `type_env` を持つ。`ExecCtx.type_env` から作る）。畳み込みはしない（性能だけの差）。
5. **Extended Query を入れる M5 では、プランをキャッシュするなら日時リテラルの畳み込み結果が `TimeZone` / `DateStyle` に依存することに注意**（M4 は Simple Query だけで毎回計画するので問題ない）。

### 4.2 定数畳み込みの規則（`const_fold` が従う表）

| 式 | 畳み込み | 理由 |
|---|---|---|
| `Operator` / `Function(FnKind::Pure)` / `Cast(Function)` で引数がすべて `Literal` | する（エラーも計画時に出る。`SELECT 1/0` は 22012） | immutable |
| `Cast(InOut)` の `Literal` からの変換（日時型含む）、`CoerceTypmod` | する（4.1） | 入力関数は環境を `TypeEnv` で受ける |
| `Cast(Env)`（`date → timestamptz` など）、`Function(FnKind::Runtime)`（`now()` ほか）、`Function(FnKind::Context)`、`SessionValue`（`CURRENT_TIMESTAMP` など） | しない | stable / volatile |
| `nextval` など volatile | しない | |

### 4.3 INSERT / UPDATE の代入キャスト

代入文脈（`CoercionContext::Assignment`）で `pg_cast` の `i` と `a` を使う。`timestamptz` → `timestamp` 列、`float8` → `numeric` 列、`numeric` → `int4` 列は代入で通る。文字列カテゴリへは I/O 変換が代入で通る（`int4` → `char(5)` 列）。通らなければ `42804 column "c" is of type integer but expression is of type text`（既存）。その後の `CoerceTypmod`（explicit = false）が `numeric(p,s)` の丸めと `22003`、`char(n)` の `22001` とパディング、`timestamp(p)` の丸めを行う。

### 4.4 型をまたぐ式の例（解決結果。テストで固定する）

| 式 | 解決 | 結果の型 |
|---|---|---|
| `1.5 + 1` | `numeric + numeric`（int4 → numeric 暗黙） | numeric |
| `1.5 + 1::float8` | `float8 + float8`（numeric → float8 暗黙） | float8 |
| `numeric_col = 2` | `numeric = numeric` | bool |
| `sum(int8_col)` | `sum(int8)` → numeric | numeric |
| `ts_col < now()` | `timestamptz < timestamptz`（`ts_col` に `timestamp → timestamptz`（Env）） | bool |
| `date_col = ts_col` | `timestamp = timestamp`（`date → timestamp` 暗黙） | bool |
| `char_col = 'abc'` | `bpchar = bpchar`（リテラルを bpchar に） | bool |
| `char_col = text_col` | `text = text`（`bpchar → text` で rtrim） | bool |
| `char_col LIKE 'a%'` | bpchar の LIKE（パディング込み） | bool |
| `name_col ~ '^pg_'` | `name ~ text`（639） | bool |
| `c.oid::regclass` | `oid → regclass`（B） | regclass |

---

## 5. 日時の統合（`types/datetime.rs`、T2）

```rust
// types/datetime.rs
use yuzhu_datetime::{Date, Timestamp, TimestampTz, DateTimeEnv, DateTimeError};
pub(crate) fn dt_err(e: DateTimeError) -> Error;
/// env.datetime が None なら Error::internal
pub(crate) fn dt_env<'a>(env: &'a TypeEnv<'a>) -> Result<&'a DateTimeEnv<'a>>;

pub fn date_in(s: &str, env: &TypeEnv<'_>) -> Result<Datum>;
pub fn timestamp_in(s: &str, typmod: i32, env: &TypeEnv<'_>) -> Result<Datum>;
pub fn timestamptz_in(s: &str, typmod: i32, env: &TypeEnv<'_>) -> Result<Datum>;
pub fn date_out(d: &Datum, env: &TypeEnv<'_>) -> Result<String>;     // timestamp_out / timestamptz_out も同様

// キャストと関数の共有本体（引数は非 NULL。D-9-4）
pub fn date_to_timestamp(d: &Datum, env: Option<&DateTimeEnv<'_>>) -> Result<Datum>;     // 純粋（env 不要）
pub fn timestamp_to_date(d: &Datum) -> Result<Datum>;                                     // 純粋
pub fn date_to_timestamptz(d: &Datum, env: &DateTimeEnv<'_>) -> Result<Datum>;           // tz 依存
pub fn timestamp_to_timestamptz(d: &Datum, env: &DateTimeEnv<'_>) -> Result<Datum>;
pub fn timestamptz_to_timestamp(d: &Datum, env: &DateTimeEnv<'_>) -> Result<Datum>;
pub fn timestamptz_to_date(d: &Datum, env: &DateTimeEnv<'_>) -> Result<Datum>;
// CastMethod::Env 用のラッパ（fn(&[Datum], &TypeEnv) -> Result<Datum>）
pub fn cast_date_to_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum>;     // ほか 3 つ
```

`CastMethod` の拡張（`catalog/builtin.rs`、T3。**00 への変更提案 P-3**）:

```rust
pub enum CastMethod {
    Function(BuiltinFn), Binary, InOut,
    /// TypeEnv（TimeZone・now・regclass の名前）を使う変換。stable なので畳み込まない
    Env(fn(&[Datum], &TypeEnv<'_>) -> Result<Datum>),
}
```

`analyzer/coerce.rs` の `find_coercion_pathway`（`CastMethod::Env(_)` は `Pathway::Cast(m)`）、`executor/eval.rs` の `apply_cast`（`Env(f) => f(&[d], ctx.type_env)`、`InOut` も `ctx.type_env` を使う）、`pg_cast.castmethod` の `rows.rs`（`Env` は `'f'`）が影響を受ける。

### 5.1 `TypeEnv` の組み立て

`settings.rs`（担当 S、T2 が書く関数）:

```rust
/// 文ごとに作る。TimeZone の解決結果を持つ（TypeEnv が &TimeZone を借りるため）
pub struct DateTimeSettings { pub time_zone: TimeZone, pub date_style: DateStyle, pub date_order: DateOrder, pub interval_style: IntervalStyle }
impl Settings {
    /// `TimeZone` `DateStyle` `IntervalStyle` の現在値から作る。解決できなければ UTC（保存時に検証済みなので起きない）
    pub fn datetime_settings(&self, zones: &ZoneDb) -> DateTimeSettings;
}
impl DateTimeSettings {
    /// now = Transaction.started_at（PG のエポックからのマイクロ秒）
    pub fn env<'a>(&'a self, zones: &'a ZoneDb, now: i64) -> DateTimeEnv<'a>;
}
```

00 §14.2 の `Settings::type_env(&self, zones)` は、`now` が要るので**引数を足す**（P-4）: `Settings::type_env<'a>(&'a self, ds: &'a DateTimeSettings, zones: &'a ZoneDb, now: i64, names: Option<&'a dyn OidNames>) -> TypeEnv<'a>`。

`SET TimeZone` の検証（`10-explain-copy-compat.md` の S に依頼）: `parse_timezone_setting(zones, value)` を通し、不正なら `22023 invalid value for parameter "TimeZone": "Foo/Bar"`、保存する綴りは `TimeZone::name()`（正式名: `utc` → `UTC`、`asia/tokyo` → `Asia/Tokyo`）。`SET DateStyle` は出力形式 `ISO` だけを受け付ける（`SQL` `Postgres` `German` は `0A000`。日付順 `MDY` `DMY` `YMD` は受け付ける）。`IntervalStyle` は保存するだけ。`DateStyle` / `TimeZone` が変わったら ParameterStatus を送る（既存）。

### 5.2 日時の挙動（PG17 で確認済み。`yuzhu-datetime` が実装）

- 暦は先発グレゴリオ暦、年 0 はなし。範囲は date が 4713-01-01 BC 〜 5874897-12-31、timestamp / timestamptz が 4713 BC 〜 294276 AD。
- DST の境界: 存在しない現地時刻（春）は遷移前のオフセット、重複する現地時刻（秋）は遷移後。
- `timestamptz` の出力オフセットは `+HH`、分があれば `+HH:MM`、秒もあれば `+HH:MM:SS`。
- `now()` はトランザクション開始時刻で固定、`statement_timestamp()` は文の開始、`clock_timestamp()` は呼んだ時点。

### 5.3 tzdata

`ZoneDb::system()`（`/usr/share/zoneinfo` の TZif）を `Cluster` が 1 つ持つ（00 §14.2）。tzdata がない環境では `UTC` と固定オフセットと POSIX 形式だけが使え、地域名は `22023 time zone "Asia/Tokyo" not recognized`（`ZoneDb::without_tzdata()`）。`sandbox/Dockerfile`（`debian:bookworm-slim`）は tzdata を postgresql-17 の依存として現在たまたま含むが、**`apt-get install` の一覧に `tzdata` を明示する**（`11-tests-plan.md` / K に依頼）。GitHub の `ubuntu-latest` には tzdata がある。テストの地域名は `Asia/Tokyo`（DST なし）と `America/New_York`（DST あり）の 2 つだけにする。`yuzhu-datetime/tests/fixtures/zoneinfo`（124KB、21 ファイル）は同クレートの単体テストが使う。

---

## 6. 日時の SQL 値関数と `SessionValueKind`

### 6.1 numeric リテラル（`analyzer/expr.rs`）

`transform_literal`:

- `Literal::Decimal(s)`（`1.5`、`.5`、`1.`、`1e3`、`1.0e-3`）→ `Numeric::parse(s)`、型 `numeric`（typmod -1）。M1 の `0A000` をやめる。`pg_typeof(1.5)` = `numeric`、`1e3` は numeric `1000`、`1.0e-3` は `0.0010`。範囲外は `22003 value overflows numeric format`。
- `Literal::Integer(s)` が `i64` に収まらない → `Numeric::parse(s)`（numeric）。`i32` に収まらないが `i64` に収まる → int8（既存）。
- 負の数の畳み込み（`-2147483648` を int4 にする）はパーサが済ませている（既存）。

### 6.2 ブール以外の定数

`Literal::String` は unknown のまま（既存）。`'1.5'::numeric` は §4.1 の経路。

### 6.3 `SELECT` の出力列名

`SELECT 1.5` の列名は `?column?`、`SELECT current_timestamp` は `current_timestamp`、`SELECT current_date` は `current_date`、`SELECT localtimestamp` は `localtimestamp`（`SessionValueKind::column_name`）。

### 6.4 `SessionValueKind` の追加

```rust
// sql/ast.rs（S1）。Copy / PartialEq / Eq を保つ
pub enum SessionValueKind {
    /* 既存 */
    CurrentDate,
    /// precision = -1 は指定なし（`CURRENT_TIMESTAMP(3)` は 3）
    CurrentTimestamp { precision: i32 },
    LocalTimestamp { precision: i32 },
}
```

パーサ（S1）: `CURRENT_DATE`、`CURRENT_TIMESTAMP [ ( n ) ]`、`LOCALTIMESTAMP [ ( n ) ]` を上に。`CURRENT_TIME`、`LOCALTIME` は `0A000 type time is not supported yet`（現在の `not_supported` の経路のまま）。n が 7 以上は 6（D-9-10）。型と評価（`executor/eval.rs` の `session_value` を `ctx.runtime` を使う形にする。担当 P0 / X）:

| 種別 | 型 | 値 |
|---|---|---|
| `CurrentDate` | date | `TimestampTz(rt.transaction_timestamp()).to_date(&tz)` |
| `CurrentTimestamp { p }` | timestamptz(p)（p < 0 は typmod -1） | `TimestampTz(rt.transaction_timestamp())` を `with_typmod(p)` |
| `LocalTimestamp { p }` | timestamp(p) | `to_timestamp(&tz)` を `with_typmod(p)` |

deparse（`10-explain-copy-compat.md`）は PG と同じ綴り（`CURRENT_DATE`、`CURRENT_TIMESTAMP`、`CURRENT_TIMESTAMP(3)`、`LOCALTIMESTAMP`）で出す。`pg_typeof(current_timestamp(2))` は timestamptz。

---

## 7. `cmp_datum`、`hash_datum`、`cmp_with_nulls`

### 7.1 `cmp_datum` の拡張（`types/datum.rs`、T3）

```rust
// 追加する分岐（NULL は呼び出し側が処理済み。M1 の「NULL は最後」の挙動は残す）
(Numeric(a), Numeric(b)) => a.as_ref().cmp(b.as_ref()),                 // -Inf < 有限 < +Inf < NaN、NaN = NaN、1.10 = 1.1
(BpChar(a), BpChar(b)) => trim_spaces(a).as_bytes().cmp(trim_spaces(b).as_bytes()),   // 末尾の ' ' だけを落とす
(Date(a), Date(b)) => a.0.cmp(&b.0),
(Timestamp(a), Timestamp(b)) => a.0.cmp(&b.0),
(TimestampTz(a), TimestampTz(b)) => a.0.cmp(&b.0),
(Int2Vector(a), Int2Vector(b)) => a.cmp(b),
```

- 変種をまたぐ組（`Timestamp` と `TimestampTz`、`BpChar` と `Text` など）は演算子解決とプランナが型をそろえるので起きない。起きたら `rank()` の順（バグの検出用。M1 から変えない）。
- `trim_spaces` は末尾の U+0020 だけを落とす（タブや改行は落とさない。PG の `bcTruelen`）。
- テキストは C ロケール（バイト比較）。

### 7.2 `hash_datum`（`types/hash.rs`、X2 が書く。仕様はここ）

```rust
pub fn hash_datum(d: &Datum, state: &mut dyn std::hash::Hasher);
pub struct HashKey(pub Vec<Datum>);     // 00 §12.2 のとおり
```

**不変条件: `cmp_datum(a, b) == Equal` ⇒ 同じハッシュ**（同じ型にそろえた値どうし）。変種ごとの規則:

| 変種 | ハッシュに書くもの |
|---|---|
| `Null` | タグ 0 のみ（`HashKey` では NULL どうしを等しいとする） |
| `Bool` | タグ + 1 バイト |
| `Int2` `Int4` `Int8` | タグ `I` + `i64`（幅によらず同じ） |
| `Float4` `Float8` | タグ `F` + `f64` のビット列。**`-0.0` は `0.0` に、すべての NaN は 1 つの標準 NaN に正規化**。`Float4` は `f64` に広げてから |
| `Numeric` | `Numeric` の `Hash` 実装（符号・weight・桁。dscale を含まない。`1.10` と `1.1` が同じ。NaN / ±Infinity はタグのみ）をそのまま使う |
| `Text` | タグ + バイト列 |
| `BpChar` | タグ + **末尾の空白を落としたバイト列** |
| `Date` `Timestamp` `TimestampTz` | タグ + 内部整数 |
| `Oid` `Xid` `Cid` | タグ + `u32`（`Oid` と `Xid` は別タグ） |
| `Char` `Tid` | タグ + 値 |
| `OidVector` `Int2Vector` `Int4Array` | タグ + 要素数 + 各要素 |
| `Void` | タグのみ |

整数と浮動小数のように `cmp_datum` が型をまたいで Equal になるが同じハッシュにならない組は、プランナが型をそろえて作らない（00 §12.2）。**`HashKey` に入れる前に結合キーの NULL を弾く**のは executor（00 §12.2）。

### 7.3 `cmp_with_nulls`（`types/cmp.rs`、T3）

```rust
pub fn cmp_with_nulls(a: &Datum, b: &Datum, descending: bool, nulls_first: bool) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => if nulls_first { Ordering::Less } else { Ordering::Greater },
        (false, true) => if nulls_first { Ordering::Greater } else { Ordering::Less },
        (false, false) => { let o = cmp_datum(a, b); if descending { o.reverse() } else { o } }
    }
}
```

PG の既定は ASC で NULLS LAST、DESC で NULLS FIRST（呼び出し側が `nulls_first` を決める）。DESC は非 NULL の比較だけを反転する。

---

## 8. 集約関数の表（`AGGREGATES`、T3）

00 §11.3 の `BuiltinAggregate { oid, name, args, result, kind }`。**結果型は PG17.11 の `pg_proc` / `pg_aggregate` で再確認した**（`m4-query.md` §5.1 と一致。差分は下の「m4-query §5.1 への追加」）。`ANY` は `oid::ANY`（2276）。

| OID | 名前 | 引数 | 結果 | `AggKind` |
|---|---|---|---|---|
| 2803 | count | （なし） | int8 | `CountStar` |
| 2147 | count | any | int8 | `Count` |
| 2109 | sum | int2 | **int8** | `SumInt2` |
| 2108 | sum | int4 | **int8** | `SumInt4` |
| 2107 | sum | int8 | **numeric** | `SumInt8` |
| 2110 | sum | float4 | float4 | `SumFloat4` |
| 2111 | sum | float8 | float8 | `SumFloat8` |
| 2114 | sum | numeric | numeric | `SumNumeric` |
| 2102 | avg | int2 | **numeric** | `AvgInt2` |
| 2101 | avg | int4 | **numeric** | `AvgInt4` |
| 2100 | avg | int8 | **numeric** | `AvgInt8` |
| 2104 | avg | float4 | **float8** | `AvgFloat4` |
| 2105 | avg | float8 | float8 | `AvgFloat8` |
| 2103 | avg | numeric | numeric | `AvgNumeric` |
| 2133 / 2117 | min / max | int2 | int2 | `Min` / `Max` |
| 2132 / 2116 | min / max | int4 | int4 | 同上 |
| 2131 / 2115 | min / max | int8 | int8 | 同上 |
| 2134 / 2118 | min / max | oid | oid | 同上 |
| 2135 / 2119 | min / max | float4 | float4 | 同上 |
| 2136 / 2120 | min / max | float8 | float8 | 同上 |
| 2138 / 2122 | min / max | date | date | 同上 |
| 2142 / 2126 | min / max | timestamp | timestamp | 同上 |
| 2143 / 2127 | min / max | timestamptz | timestamptz | 同上 |
| 2145 / 2129 | min / max | text | text | 同上（varchar・name・"char" は text に暗黙変換されて text の行に解決される） |
| 2146 / 2130 | min / max | numeric | numeric | 同上 |
| 2245 / 2244 | min / max | bpchar | bpchar | 同上 |
| 2798 / 2797 | min / max | tid | tid | 同上 |
| 2517 | bool_and | bool | bool | `BoolAnd` |
| 2518 | bool_or | bool | bool | `BoolOr` |
| 2519 | every | bool | bool | `BoolAnd` |

合計 43 行（`count` 2 + `sum` 6 + `avg` 6 + `min` 13 + `max` 13 + bool 3）。**m4-query §5.1 への追加**: min / max の対象型に oid・date・timestamp・timestamptz・bpchar・tid の行を足した（OID は上表。実機の `pg_proc` で確認）。`interval` `time` `timetz` `money` `anyarray` `anyenum` `inet` `pg_lsn` `xid8` の行は作らない。`aggregates_named(name)` は `AGGREGATES` から名前で引く。`BuiltinProc` の行は §3.8 のとおり `AGGREGATES` から `prokind='a'` で生成する。

### 8.1 集約の意味論（`AggState`。X2 が書く。ここは型に関する規則）

PG のトランジション関数と同じ。NULL 入力は飛ばす（strict）。最初の非 NULL で状態を初期化する。**空入力（行なし）の結果**: `count` は 0、それ以外は NULL。

| 集約 | 状態と計算 | 注意 |
|---|---|---|
| `count(*)` / `count(x)` | `i64`（`count(x)` は非 NULL だけ） | `22003 bigint out of range` は実質起きない |
| `sum(int2/int4)` | `i64`。オーバーフローは `22003 bigint out of range` | 結果 int8 |
| `sum(int8)` | `i128` で足し、最後に `Numeric::parse(&sum.to_string())`（dscale 0） | PG の `numeric_poly_sum` と値が同じ。`sum(9223372036854775807, 1)` = `9223372036854775808` |
| `sum(float4)` | `f32` を `float4pl`（オーバーフローは `22003 value out of range: overflow`）で順に足す | 足す順で結果が変わる。slt には誤差の出ない値だけ |
| `sum(float8)` | 同上 `f64` | |
| `sum(numeric)` | `Numeric::checked_add` を順に（dscale は max）。NaN は吸収、`Infinity + -Infinity` は NaN | |
| `avg(int2/int4/int8)` | `(count: i64, sum: i128)`。最後に `Numeric::from(sum) / Numeric::from(count)`（`checked_div`。結果 scale は `select_div_scale`: `avg(1,2)` = `1.5000000000000000`、`avg(1,2,4)` = `2.3333333333333333`、`avg(1,1,1)` = `1.00000000000000000000`） | PG の `int8_avg`（int2 / int4 は int8 で累積）とは値が同じ（int8 を超える累積は到達しない） |
| `avg(float4/float8)` | `(n, 合計 f64)`、結果 `合計 / n`（f64）。float4 は f64 に広げてから足す | 結果 float8 |
| `avg(numeric)` | `sum / Numeric::from(count)`（`checked_div`）。`avg(1.5, 2.25)` = `1.8750000000000000` | 空は NULL |
| `min` / `max` | 最初の非 NULL を状態にし、以降 `cmp_datum` で比べて更新。**等しいときは後に来た値**（D-9-12）。float の NaN は最大（`cmp_datum`）、`-0` と `+0` は等しい | 型は引数の型のまま。bpchar はパディングを保持して返す |
| `bool_and` / `every` | `Option<bool>`、非 NULL を AND | |
| `bool_or` | 同上 OR | |

- `DISTINCT`: `HashSet<HashKey>`（重複判定は `cmp_datum` と `hash_datum`）。numeric で scale の違う等値の値（`1.0` と `1.00`）の代表は先に見た方。PG は整列して先頭を渡すので `sum(DISTINCT)` の dscale が変わりうる。テストは scale をそろえた値を使う。
- `FILTER (WHERE c)`: c が真の行だけを渡す。
- 型の組み合わせのエラー: `sum(bool)` は `42883 function sum(boolean) does not exist`（既存の解決エラー）。

---

## 9. 正規表現エンジン（`types/regex.rs`、T3）

### 9.1 インターフェース

```rust
// types/regex.rs（外部クレートなし、unsafe なし）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RegexFlags { pub icase: bool }          // ~* / !~* のとき true

#[derive(Debug)]
pub struct Regex { /* プログラム（Vec<Inst>）、先頭アンカー、リテラルの高速経路 */ }

impl Regex {
    /// 2201B / 0A000 を返す
    pub fn new(pattern: &str, flags: RegexFlags) -> Result<Regex>;
    /// text のどこかに一致があるか（~ の意味）。PG の REG_NOSUB。線形時間
    pub fn is_match(&self, text: &str) -> bool;
}

/// 演算子の本体が呼ぶ。コンパイル結果をスレッドごとの LRU（32 件。PG の RE_CACHE_SIZE と同じ）に入れる
pub fn regex_match(text: &str, pattern: &str, icase: bool) -> Result<bool>;
```

キャッシュは `thread_local!` の `RefCell<Vec<(RegexKey, Rc<Regex>)>>`（接続ごとに 1 スレッド）。unsafe は不要。

### 9.2 サポートする構文（PG の ARE の部分集合）

PG の既定は ARE（`REG_ADVANCED`）。`~` / `~*` は部分一致（文字列のどこかに一致すればよい）。以下を実機（PG17）で確認した上で範囲を決めた。

| 構文 | 対応 | 備考 |
|---|---|---|
| リテラル、`.` | する | `.` は既定で改行にも一致（非改行感応） |
| `^` `$` | する | `$` は文字列の**末尾**だけ（末尾の改行の前には一致しない。実機で確認）。`a^b`、`a$b` のように途中にあっても常にアンカー（一致しない）。`^*` `$*` は `quantifier operand invalid` |
| `\|`（選択） | する | 空の枝を許す（`(\|a)`、`a\|`）。`a\|*` は `quantifier operand invalid` |
| `( )`、`(?: )` | する | 捕獲は使わない（一致の有無だけ）。`()` は空に一致 |
| `* + ?` と遅延版 `*? +? ??` | する | 遅延は結果に影響しないので貪欲と同じに扱う。量指定子の直後の量指定子は `quantifier operand invalid`（`a**`、`a{1,2}{3}`） |
| `{m}` `{m,}` `{m,n}` と遅延版 | する | m, n ≤ 255、m ≤ n（超過は `invalid repetition count(s)`）。**`{` の次が数字でなければ `{` は通常の文字**（`a{x}`、`a{`、`a{,2}` はリテラル。実機で確認）。`{数字` で閉じ括弧がなければ `braces {} not balanced` |
| ブラケット式 `[...]` | する | 範囲 `a-z`、否定 `[^...]`、先頭の `]` と `-` はリテラル、`[:alpha:]` `[:digit:]` `[:alnum:]` `[:upper:]` `[:lower:]` `[:space:]` `[:blank:]` `[:punct:]` `[:print:]` `[:graph:]` `[:cntrl:]` `[:xdigit:]` `[:word:]`（ASCII のみ）、`[.x.]`（1 文字のみ）、`[=x=]`（その文字）、`[\d]` `[\s]` `[\w]`（ブラケット内では `\D` `\S` `\W` は不可）。`[a-\d]` は `invalid character range` |
| `[[:<:]]` `[[:>:]]` | する | `\m` `\M` と同じ |
| エスケープ（文字） | する | `\a`(7) `\b`(8、**後退文字であって単語境界ではない**) `\B`(`\`) `\cX` `\e`(27) `\f` `\n` `\r` `\t` `\v` `\0` `\ooo`（8 進） `\xhhh`（16 進を可能な限り） `\uhhhh` `\Uhhhhhhhh`。英数字以外の `\X` は X そのもの |
| エスケープ（クラス） | する | `\d \D \s \S \w \W`（ASCII。`\w` = `[0-9A-Za-z_]`） |
| エスケープ（制約） | する | `\A`（文字列の先頭）`\Z`（末尾）`\m`（単語の先頭）`\M`（単語の末尾）`\y`（単語境界）`\Y`（単語境界でない）。単語文字は ASCII の英数字と `_` |
| 埋め込みオプション | **先頭の `(?xyz)` だけ** | `i`（大文字小文字を同一視）`c`（区別する）`s`（非改行感応。既定）`n` `m`（改行感応: `.` と `[^…]` が改行に一致せず、`^` `$` が行頭・行末に一致）`p`（`.` と `[^…]` だけ改行感応）`w`（`^` `$` だけ改行感応）`x`（空白と `#` から行末までのコメントを無視。ブラケット内とエスケープされた空白は除く）`q`（以降すべてリテラル）`t`（何もしない）。`b` `e`（BRE / ERE）は `0A000`。他は `invalid embedded option`。**`(?i:a)` のような中間のオプションは PG も `invalid embedded option`**（実機で確認） |
| ディレクタ | する | 先頭の `***:`（ARE）`***=`（以降すべてリテラル）。`***?` は `0A000` |
| 後方参照 `\1`〜`\9` | **`0A000`**（D-9-7） | 存在しないグループ番号は PG と同じ `2201B invalid backreference number`。存在するなら `0A000 regular expression back-references are not supported yet` |
| 先読み `(?=` `(?!`、後読み `(?<=` `(?<!` | **`0A000`** | `0A000 regular expression lookahead/lookbehind constraints are not supported yet` |
| `(?<n>` など未対応の `(?` | `quantifier operand invalid`（PG17 の実機の応答に合わせる） | |

大文字小文字: `~*` と `(?i)` は ASCII のみ（D-9-8）。非 ASCII は区別する（`'é' ~* 'É'` は偽）。

### 9.3 アルゴリズム

1. **構文解析**: 再帰下降で AST にする（`Alt`、`Concat`、`Repeat { min, max: Option<u32>, node }`、`Char`、`Any`、`Class`、`Assert`、`Empty`）。パターンは文字（`char`）の列。オプションとディレクタは先頭で読む。エラーは §9.4 の文言。
2. **コンパイル**: Thompson 型の命令列にする。

```rust
enum Inst {
    Char(char),                       // 大文字小文字を同一視するときは ASCII の小文字と比較する CharIc(char)
    CharIc(char),
    Any,                              // 改行感応のときは '\n' を除く
    Class(Box<CharClass>),            // ranges: Vec<(char, char)>（ソート済み）、negated: bool、nl_sensitive: bool
    Split(usize, usize),
    Jmp(usize),
    Assert(AssertKind),               // Bol, Eol, TextStart, TextEnd, WordStart, WordEnd, WordBoundary, NotWordBoundary
    Match,
}
```

   - `Repeat { min, max }` は `node` を min 個複製し、max が有限なら `(node (node ...)?)?` の入れ子、無限なら末尾に `node*` を足す。
   - プログラムの大きさが `MAX_PROGRAM_SIZE = 200_000` 命令を超えたら `2201B invalid regular expression: regular expression is too complex`（`((a{255}){255}){255}` など）。
3. **照合**: Pike VM の状態集合（疎集合）のシミュレーション。捕獲は持たない。各位置で、(a) まだ一致が見つかっていなければ開始状態（pc = 0）を足す（部分一致のため。先頭が `^` / `\A` の選択肢だけなら位置 0 でだけ足す）、(b) ε 閉包（`Split` / `Jmp` / `Assert`）を前後の文字を見て評価しながら計算（空ループ `(a*)*` は疎集合の訪問済み判定で止まる）、(c) 文字を消費して次の集合へ。`Match` に達したらすぐ `true`。計算量は O(文字数 × 命令数)。**破滅的バックトラックは起きない**（`aaaaaaaaaaaaaaaaaaaaaaaa!` ~ `^(a+)+$` が即座に偽）。
4. **高速経路**: パターンが特殊文字を含まない（`^` `$` も含まない）リテラルなら `str::contains`（icase のときは ASCII 小文字化した上で）。`^literal` は `starts_with`、`literal$` は `ends_with`、`^literal$` は `==`。

### 9.4 エラー（SQLSTATE `2201B`、メッセージは `invalid regular expression: ` + 次。実機で確認）

| 原因 | 文言 |
|---|---|
| `(` `)` の不整合 | `parentheses () not balanced` |
| `[` が閉じない、`[]`、`[a-` | `brackets [] not balanced` |
| `a{2`（閉じ括弧なし） | `braces {} not balanced` |
| `[z-a]`、`[a-\d]` | `invalid character range` |
| `a**`、`*a`、`+`、`?a`、`^*`、`$*`、`a\|*`、`a{1,2}{3}`、`(?<n>a)` | `quantifier operand invalid` |
| `a{2,1}`、`x{256}`、`x{1,256}` | `invalid repetition count(s)` |
| 末尾の `\`、`\q`、`\x`（16 進数字なし） | `invalid escape \ sequence` |
| `[[:foo:]]` | `invalid character class` |
| 複数文字の照合要素 `[[.hyphen.]]` | `invalid collating element` |
| `(?z)`、`(?i`（閉じない） | `invalid embedded option` |
| `\1`、`(a)\2`（存在しないグループ） | `invalid backreference number` |
| プログラムが大きすぎる | `regular expression is too complex` |

未対応の構文は `0A000`（§9.2）。メッセージの行頭に `invalid regular expression: ` を付ける（PG の `errmsg("invalid regular expression: %s", ...)`）。

### 9.5 psql の `\dt` が生成するパターンの網羅テスト

psql 17 の `processSQLNamePattern` は、`*` を `.*`、`?` を `.` に、`$` を `\$` に、`.` で区切って `^(…)$` に包む（引用符つきの名前は正規表現の特殊文字をエスケープ。`E'...'` 形式の文字列で出る）。実機で `psql -E` から取ったもの:

| 入力 | 送られる述語 |
|---|---|
| `\dt` | `n.nspname !~ '^pg_toast'`（ほか `<>` 比較。`~` 系は `!~` だけ） |
| `\dn` | `n.nspname !~ '^pg_'` |
| `\dt foo*` | `c.relname OPERATOR(pg_catalog.~) '^(foo.*)$' COLLATE pg_catalog.default` |
| `\dt pub*.foo?` | `c.relname OPERATOR(pg_catalog.~) '^(foo.)$' ...` と `n.nspname OPERATOR(pg_catalog.~) '^(pub.*)$' ...` |
| `\dt "Foo.Bar"` | `c.relname OPERATOR(pg_catalog.~) E'^(Foo\\.Bar)$' ...` |
| `\dt public.t$` | `E'^(t\\$)$'` と `'^(public)$'` |
| `\dt (x\|y)` | `'^((x\|y))$'` |
| `\dt [a-c]*` | `'^([a-c].*)$'` |
| `\dt a+b` | `'^(a+b)$'` |

テストコーパス `tests/data/regex_psql.tsv`（生成スクリプト `tests/data/gen_regex_corpus.py`、PG17 に問い合わせて期待値を作る）の列は `pattern<TAB>flags<TAB>subject<TAB>expected`（`t` / `f` / `ERR:2201B:...` / `ERR:0A000`）。パターンは次を網羅する（実機で確認済みの例）:

| パターン | 対象 | 結果 |
|---|---|---|
| `^(foo.*)$` | `foo` / `foobar` / `xfoo` | t / t / f |
| `^(foo.)$` | `foox` / `foo` / `fooxy` | t / f / f |
| `^(Foo\.Bar)$` | `Foo.Bar` / `FooxBar` | t / f |
| `^(t\$)$` | `t$` / `t` | t / f |
| `^((x\|y))$` | `x` / `y` / `xy` | t / t / f |
| `^([a-c].*)$` | `apple` / `dog` | t / f |
| `^(a+b)$` | `aaab` / `b` | t / f |
| `^pg_` | `pg_catalog` / `xpg_` | t / f |
| `^(.*)$` | 空文字列 / `anything` | t / t |
| `^(tbl_[0-9]+)$` | `tbl_42` / `tbl_x` | t / f |
| `^(a\|b)$`（`\|` はリテラルの `\|`） | `a\|b` / `a` | t / f |
| `^(\(x\))$` `^(\[x\])$` `^(a\*b)$` `^(a\?b)$` `^(a\+b)$` `^(a\{2\})$` `^(a\^b)$` | 対応するリテラル | すべて t |
| `^(Foo)$` / `^(foo)$` | `foo` / `FOO` | f / f |

加えて、構文網羅（§9.2 の各行、§9.4 の各エラー）、`~*`、`(?i)`、`(?n)`、`(?x)`、改行を含む文字列（`E'a\nb' ~ 'a.b'` は t、`E'a\n' ~ 'a$'` は f、`E'a\nb' ~ '(?n)^b'` は t）、非 ASCII（`'é' ~ '.'` は t、`'é' ~ '\w'` は f）、`bpchar` の主題（パディング込み）、乱数で生成したパターン × 主題の差分（PG17 で期待値を作る。生成器は §9.2 の構文だけから作り、未対応構文は含めない）、ReDoS 型（`^(a+)+$` と `aaaa…!`、`(a*)*b`）が 100 ミリ秒以内、を入れる。単体テスト（`types/regex.rs` の `mod tests`）とコーパステスト（`yuzhu-core/tests/regex_corpus.rs`）の 2 本。

---

## 10. generate_series と `format_type`

### 10.1 `generate_series`（T3、実行は `05-executor.md` の `function_scan.rs`）

```rust
// catalog/mod.rs（担当 A）
pub enum FnKind { Pure(BuiltinFn), Context(..), Runtime(..), /// FROM 句でだけ呼べる集合返却関数
                  Set(SetFn) }
#[derive(Clone, Copy)]
pub struct SetFn {
    /// 引数（評価済み）から行の生成器を作る。引数に NULL があれば 0 行（strict）
    pub begin: fn(&[Datum]) -> Result<Box<dyn SetIter>>,
    /// FROM generate_series(..) の列名と別名の既定（"generate_series"）
    pub column_name: &'static str,
}
pub trait SetIter { fn next(&mut self) -> Result<Option<Datum>>; }
```

`rewind` は引数を再評価して `begin` を呼び直す（`FunctionScan` ノードの責任）。`FnKind::Set` の関数を FROM 句以外（SELECT 句、WHERE など）で呼ぶと、アナライザが `0A000 set-returning functions are only supported in the FROM clause`。

| OID | 関数 | 引数 → 結果 | 本体 |
|---|---|---|---|
| 1067 | `generate_series` | int4, int4 → int4（SETOF） | `series_i4(start, stop, 1)` |
| 1066 | `generate_series` | int4, int4, int4 → int4 | `series_i4` |
| 1069 | `generate_series` | int8, int8 → int8 | `series_i8(start, stop, 1)` |
| 1068 | `generate_series` | int8, int8, int8 → int8 | `series_i8` |

- 規則: `step > 0` なら `cur <= stop` の間、`step < 0` なら `cur >= stop` の間、`cur` を返して `cur += step`。**加算が桁あふれ（`checked_add` が `None`）したら終了**（`generate_series(2147483646, 2147483647)` は 2 行で止まる）。`step = 0` は `22023 step size cannot equal zero`。`start > stop` で `step > 0` は 0 行。
- 結果の列型は引数の型（int4 版は `Datum::Int4`、int8 版は `Datum::Int8`）。`generate_series(1, 3)` は `(int4, int4)` の完全一致、`generate_series(1, 3000000000)` は `(int8, int8)`、`generate_series(1, 10.5)` は `42883`（numeric 版は M5）。
- pgbench（`-I G`）の `insert ... select aid, (aid - 1) / 100000 + 1, 0, '' from generate_series(1, 100000) as aid` が動く（`'' ` は text に解決されて bpchar 列へ代入される）。

### 10.2 `format_type` と `type_display_name`

`catalog/builtin.rs` の `format_type_name(type_oid, typmod: Option<i32>)`（`None` = typmod を渡されていない）の追加分。実機の `format_type` で確認した値:

| 呼び出し | 結果 |
|---|---|
| `format_type(1700, -1)` | `numeric` |
| `format_type(1700, 655366)` | `numeric(10,2)`（`yuzhu_numeric::format_typmod` を使う。**既存の `tmp & 0xffff` は負の scale を壊すので置き換える**: `numeric(10,-2)`） |
| `format_type(1700, 4)` | `numeric(0,0)` |
| `format_type(1042, -1)` | `bpchar`（typmod が渡されて -1） |
| `format_type(1042, NULL)` | `character`（typmod が渡されていない。`regtype` の出力と `pg_typeof` はこちら） |
| `format_type(1042, 5)` | `character(1)` |
| `format_type(1042, 4)` | `character`（n = 0 は括弧なし） |
| `format_type(1114, -1)` | `timestamp without time zone` |
| `format_type(1114, 3)` | `timestamp(3) without time zone` |
| `format_type(1184, -1)` | `timestamp with time zone` |
| `format_type(1184, 0)` | `timestamp(0) with time zone` |
| `format_type(1082, -1)` / `2205` / `2206` / `22` | `date` / `regclass` / `regtype` / `int2vector` |

`type_display_name(oid)`（typmod なし。エラー文言の `type timestamp` などとは別。`invalid input syntax for type …` の型名は PG が `format_type_be` を使わず固定の文字列を書く箇所が多い: numeric は `numeric`、日時は `timestamp` / `timestamp with time zone` / `date`、整数は `integer`、bpchar は `character`）に `NUMERIC → "numeric"`、`BPCHAR → "character"`、`DATE → "date"`、`TIMESTAMP → "timestamp without time zone"`、`TIMESTAMPTZ → "timestamp with time zone"`、`REGCLASS`、`REGTYPE`、`INT2VECTOR` を足す。`types::format_type(ty)` は `typmod::display_with_typmod` に委譲する。

---

## 11. 他章への依頼と契約の接続

| 相手 | 依頼 |
|---|---|
| `02-pipeline-refactor.md`（A、P0） | `TypeEnv.names`（P-1）、`CastMethod::Env`（P-3）、`FnKind::Set`、`SqlType` の新定数、`oid` 定数の移動。`executor/eval.rs` の `apply_cast` が `ctx.type_env` を取り、`CastMethod::Env(f)` と `CoerceViaIO` が使う。`coerce_typmod` が `typmod::apply_typmod` を呼ぶ。`session_value` が `ctx.runtime` を使い、`CurrentDate` / `CurrentTimestamp` / `LocalTimestamp` を評価する |
| `03-parser-analyzer.md`（S1、N1〜N3） | `SessionValueKind` の 3 変種（§6.4）、型名の修飾子（`numeric(p,s)` `char(n)` `timestamp(p)`）を `typmod_in` に渡す、`KNOWN_UNSUPPORTED_TYPES` の整理（§3.2）、`Expr::Like` の bpchar を text に変えない（§3.7）、`pg_typeof` の畳み込み（D-9-9）と `"any"` 引数の受理、`FnKind::Set` を FROM 句以外で拒否（§10.1）、`FROM generate_series(1,5) AS aid` の列名（`SetFn.column_name`）、リテラルの経路（§4.1）、`OPERATOR(pg_catalog.~)` の演算子解決 |
| `04-planner-optimizer.md`（L1、L2） | 定数畳み込みの規則（§4.2）と、日時リテラルの評価に `PlanEnv.type_env` を使うこと。`bpchar` / `numeric` を結合キーにするときに `hash_datum` を使う（§7.2）。`date = timestamp` のような型をまたぐ比較はキャストで型をそろえる |
| `05-executor.md`（X1〜X3） | `hash_datum` / `HashKey`（X2 が実装）、集約の `AggState`（§8.1）、`FunctionScan` と `SetIter`、`eval` の `Cast(Env)`、`EvalCtx.type_env` |
| `06-btree.md`（B2） | `AMPROCS` の比較関数は `cmp_datum`。`opclass` の型: `numeric_ops`（1700）、`bpchar_ops`（1042）、`datetime_ops`（date、timestamp、timestamptz の**同じ型どうし**だけ。型をまたぐ amop / amproc は M4 では入れない。D-9-5）。bpchar のキーは `cmp_datum` が末尾の空白を無視するので、インデックスの比較も同じ。`BT_MAX_ITEM_SIZE` との関係（§3.5） |
| `07-catalog-ddl.md`（C1） | `PROCS` の生成（§3.8）、`proretset`、`pg_type` の行（§3.2）、`pg_index.indkey` / `pg_constraint.conkey` の `Int2Vector` / `int2[]`、`typmodin` は 0 のまま。`pg_cast` の行（§3.6、長さ強制の行は出ない） |
| `10-explain-copy-compat.md`（E1、O1、S） | deparse の日時 SQL 値関数の綴り、regclass リテラルの出力（`'t_id_seq'::regclass`）、`TypeEnv` の組み立て（§5.1）、`SET TimeZone` / `DateStyle` の検証、COPY の列変換は `input_text_typed`（char(n) の空白埋めと timestamp の丸めを含む）、psql の `\dt` が使う `!~` と `OPERATOR(pg_catalog.~)`、pgbench の `char(84)` の COPY（空文字列が 84 個の空白になる） |
| `11-tests-plan.md`（K） | `sandbox/Dockerfile` と CI の tzdata（§5.3）、`tests/slt/m4/types/*`（§12.2）、`TimeZone` を先頭で明示 |

### 11.1 00 への変更提案（統合時に反映）

| # | 提案 | 理由 |
|---|---|---|
| P-1 | `TypeEnv` に `names: Option<&'a dyn OidNames>` を足す（`OidNames: Debug`）。`output_text_regproc` は廃止して `names.proc_name` に吸収する | `regclass::text`、COPY、`||` など executor の出力段以外でも reg* の名前表示が要る。00 §12.4 の「出力段が行う」だけでは `regclass::text` が書けない |
| P-2 | 00 §15.3 に `INVALID_DATETIME_FORMAT = "22007"`、`DATETIME_FIELD_OVERFLOW = "22008"`、`INVALID_TIME_ZONE_DISPLACEMENT_VALUE = "22009"` を足す | 日時の入力エラーに必要（00 にない） |
| P-3 | `CastMethod` に `Env(fn(&[Datum], &TypeEnv<'_>) -> Result<Datum>)` を足す（00 §6.2 の `Cast { method: CastMethod }` の中身。署名は変わらない） | `date → timestamptz` などタイムゾーン依存の変換と `text → regclass` を純粋関数で書けない |
| P-4 | 00 §14.2 の `Settings::type_env(&self, zones)` に `now`、`DateTimeSettings`、`names` を足す（§5.1） | `now`（トランザクション開始時刻）と `TimeZone` の所有が要る |
| P-5 | `FnKind` に `Set(SetFn)` を足す（00 §6.2 の `Function { func }` の中身。署名は変わらない） | `generate_series` を FROM 用の種別として登録する |
| P-6 | `SessionValueKind` に `CurrentDate`、`CurrentTimestamp { precision }`、`LocalTimestamp { precision }` を足す | D-9-6 |
| P-7 | `oid::ANY = 2276` を足す | `count("any")`、`pg_typeof` |

---

## 12. テスト

### 12.1 差分コーパスを活かす

| 対象 | 既存 | M4 での使い方 |
|---|---|---|
| numeric | `yuzhu-numeric/tests/pg_corpus.rs` + `tests/data/pg17_corpus.tsv`（約 3.5 万行: `in` `cast` `add` `sub` `mul` `div` `mod` `divtrunc` `cmp` `round` `trunc` `abs` `sign` `ceil` `floor` `neg` `fromf8` `fromf4` `fromi8` `toi2/4/8` `tof4/8` `send` `typmod`）+ `pg17_regress.tsv`（敵対的レビューで見つけた境界） | **そのまま維持**。T1 は橋渡し（`types/numeric.rs`）に対して、同じコーパスを `yuzhu-core` 側の `numeric_bridge_corpus` テストでも流す（`Datum` 経由の演算子・キャスト・typmod の結果が `Numeric` の直接呼び出しと一致し、エラーの SQLSTATE と文言が同じ） |
| 日時 | `yuzhu-datetime/tests/pg_corpus.rs` + `fixtures/pg17_corpus.tsv`（約 7 万行、TimeZone / DateStyle を変えて、`date_in` `ts_in` `tstz_in` `ts_typmod` `tstz_typmod` `ts_to_tstz` `tstz_to_ts` `tstz_to_date` `date_to_tstz` `date_pl_int` `date_mi_date` など）、`fixtures/zoneinfo` | **そのまま維持**。T2 は橋渡しの差分テスト（`types/datetime.rs` の `Datum` 経由）を、`ts_in` `tstz_in` `date_in` `ts_typmod` `tstz_typmod` と 6 種のキャスト、`date ± int`・`date - date` について流す。interval を含む種別（`iv_*` `*_pl_iv` など）は M4 では流さない |
| bpchar | なし | 新しく生成する: `gen_bpchar_corpus.py`（PG17 に問い合わせる。入力 × typmod × 代入 / 明示 / 比較 / `length` / `octet_length` / LIKE / `~` / `||`）。入力は空白・多バイト文字・境界の長さ |
| 正規表現 | なし | §9.5 のコーパス |
| 集約 | なし | `tests/slt/m4/types/agg_types.slt` と、乱数の値列に対する集約の差分（`gen_agg_corpus.py`。numeric の scale 違い、float の NaN、bpchar の等値の min / max、空入力、NULL だけ） |

### 12.2 `tests/slt/m4/types/*.slt`（PG17 で期待値を確認する。`onlyif yuzhu` は使わない）

先頭で `SET TIME ZONE 'UTC'`、`SET DateStyle = 'ISO, MDY'` を明示する（PG 側の既定に左右されないため）。numeric の結果は文字列で比べる（`T`。sqllogictest の `R` は scale の違いを見落とす）。`now()` `clock_timestamp()` の値は比べず、`IS NOT NULL` と `pg_typeof` で確かめる。

| ファイル | 内容 |
|---|---|
| `numeric_basic.slt` | リテラルの型（`pg_typeof(1.5)` `1e3` `.5` `9223372036854775808`）、四則と scale（`0.1+0.2`=`0.3`、`1.50*2.25`=`3.3750`、`1.0/3`=`0.33333333333333333333`、`100::numeric/3`、`10/4.0`、`7.0%2.5`、`-7.5%2`）、`round` `trunc` `ceil` `floor` `abs` `sign` `mod` `div` `scale`、`round(2)` が float8、`mod(5,2)` が integer、0 から遠い方への丸め、NaN / Infinity、ゼロ除算 `22012`、`1.10 = 1.1`、ORDER BY / GROUP BY / DISTINCT での `1.10` と `1.1`、混合（`1.5 + 1`、`1.5 + 1.0::float8`、`numeric = float8`） |
| `numeric_typmod.slt` | `numeric(5,2)` 列への INSERT（丸め、`22003` と DETAIL、`numeric(3,5)`、`numeric(5,-1)`、NaN と Infinity）、`::numeric(7,2)` の明示キャスト、`typmod_in` のエラー（`numeric(0)` `numeric(1001)` `numeric(5,1001)` `numeric(5,2,1)`）、`format_type`、`\d` 相当の `pg_attribute.atttypmod`（`655366`） |
| `numeric_cast.slt` | `float8 → numeric`（15 桁。`0.30000000000000004::float8::numeric`）、`numeric → int`（丸め、範囲外 `22003`、NaN `0A000`）、text との相互変換、`'x'::numeric` の `22P02` |
| `bpchar.slt` | `char(n)` 列（パディング、`octet_length`、`length`、`'abc  '` が `char(3)` に入る、`'abcd'` は `22001`、`'abcdef'::char(3)` = `abc`、多バイト）、比較（`'a'::char(3) < 'a '::char(3)` が偽、`char = text` が `text = text`、`varchar = char`）、`||` で空白が消える、LIKE と `~` がパディング込み、`IN`、ORDER BY / GROUP BY / DISTINCT / `min` / `max`、UNIQUE（末尾の空白だけが違う値は重複）、`cast(true as char(3))`、`pg_typeof('a'::char)` = `character`、`format_type` |
| `datetime_basic.slt` | date / timestamp / timestamptz の入力と出力（`infinity`、BC、小数秒の丸め、`epoch`）、タイムゾーンつきの入力（`+09`、`Asia/Tokyo`）、比較、ORDER BY、`date ± int`、`date - date`、`min` / `max`、`timestamp(0)` 列への INSERT（丸め）、`timestamp` 列へ `timestamptz` を代入（pgbench の `CURRENT_TIMESTAMP`）、`date(timestamptz)` と `now()::date`、`timestamp - timestamp` が `42883`（`onlyif yuzhu`: PG ではエラーにならない。`skipif yuzhu` 側に期待値 `365 days` を置く）、`interval '1 day'` が `0A000`（同様） |
| `datetime_tz.slt` | `SET TIME ZONE 'Asia/Tokyo'` / `'America/New_York'` での出力、`timestamptz ↔ timestamp` の変換、DST の境界（`2024-03-10 02:30`、`2024-11-03 01:30`）、`SHOW TimeZone`、`SET TIME ZONE 'Foo/Bar'` の `22023`、`SET DateStyle = 'DMY'` と日付の入力、`'now'` `'today'` の型（値は比べない） |
| `datetime_errors.slt` | `22007`（`'garbage'::timestamp`）、`22008`（`'2024-13-01'::date` と HINT、`294277-01-01`）、`22009`（`+16`）、`22023`（未知のゾーン） |
| `regclass.slt` | §3.9 の入力（`'pg_class'` `'PG_CLASS'` `'"pg_class"'` `'1259'` `'99999999'` `'-'` `0`、`'nosuch'` の `42P01`、`''` の `42602`、`'a.b.c.d'`）、`regclass::text`、`oid::regclass`、`regclass = oid`、`regclass + 1` が `42883`、`to_regclass`、`'int4'::regtype` ほか別名、`'nosuch'::regtype` の `42704`、`pg_typeof` の各種、`c.oid = '1259'`（unknown → oid） |
| `regex.slt` | `~` `~*` `!~` `!~*`（text / name / bpchar）、§9.5 の psql パターン、NULL、空のパターン、`2201B` の各エラー、`0A000`（`\1`、`(?=`）、`\dt` 相当の問い合わせ（`pg_class` と `pg_namespace` に対する `relname ~ '^(foo.*)$'` と `nspname !~ '^pg_toast'`） |
| `agg_types.slt` | §8 の全行の結果型（`pg_typeof(sum(int2))` など）と値、空入力、NULL だけ、`avg` の scale（`1.5000000000000000`、`2.3333333333333333`、`1.00000000000000000000`）、`sum(int8)` の `9223372036854775808`、`sum(int4)` の `22003`、`min` / `max` の等値のときの表示（`1.100`）、`count(DISTINCT)`、`FILTER`、`bool_and` / `bool_or` / `every`、`sum(bool)` の `42883` |
| `generate_series.slt` | 正順・逆順・step、`start > stop` で 0 行、step 0 の `22023`、int4 / int8 の端（`2147483646..2147483647`）、`AS g` の列名、`(1, 10.5)` が `42883`（`onlyif yuzhu`）、結合（`generate_series(1,3) a JOIN generate_series(2,4) b ON a = b`）、SELECT 句での使用が `0A000`（`onlyif yuzhu`） |
| `psql_dt.slt` | `\dt` が使う問い合わせ（`pg_class` LEFT JOIN `pg_namespace`、`pg_get_userbyid`、`pg_table_is_visible`、`!~ '^pg_toast'`、`<> 'information_schema'`、`ORDER BY 1,2`）。`10-explain-copy-compat.md` と共同 |

### 12.3 単体テスト

- `types/typmod.rs`: `typmod_in` の全エラー、`apply_typmod` の全分岐（bpchar の切り詰め・パディング・多バイト、numeric、timestamp の丸め）。
- `types/datum.rs`: `cmp_datum` の新分岐（numeric の NaN 最大と `1.10 = 1.1`、bpchar の空白、日時の infinity、`Int2Vector` の辞書順）。
- `types/hash.rs`（X2）: `cmp_datum == Equal` の対（`-0.0` と `0.0`、NaN どうし、`1.10` と `1.1`、`'a '` と `'a'` の bpchar、`Int2(5)` と `Int8(5)`）が同じハッシュ。ランダムな組で「Equal ならハッシュ一致」を 10 万回。
- `types/cmp.rs`: `cmp_with_nulls` の 4 通り × ASC / DESC。
- `types/regex.rs`: §9.4 の全エラー、§9.2 の全行、`MAX_PROGRAM_SIZE`。
- `types/sys.rs`: `regclass_in` / `regtype_in` の全行（`OidNames` のモックで）、`int2vector` の入出力、`int2[]` の入出力。
- `types/io.rs`: 新しい型の入出力のラウンドトリップ（テキスト → `Datum` → テキスト）。
- `storage/heap/tuple.rs`（H4 と共同）: §3.5 のバイト例（numeric `1.5`、`-123.456`、`0`、NaN、bpchar、date、timestamp、regclass、int2vector）を `form_tuple` / `deform_tuple` で往復し、バイト列が表と一致。numeric の `ndigits` が 59 以上（4 バイトヘッダ）と未満（1 バイトヘッダ）の境界。
- `catalog/builtin.rs`: `OPERATORS` の各行の `oprcode`・`com`・`neg` が実機の `pg_operator` と一致するテスト（`tools/gen_procs.sh` と同じ出典から生成した定数表と比べる）。`AGGREGATES` の OID と結果型が実機の `pg_proc` と一致。

### 12.4 差分ランダムテスト（`yuzhu-fuzz-sql`、Z）

numeric（四則と丸め、`numeric(p,s)` 列への INSERT）、bpchar（比較と連結）、日時（比較、`date ± int`）、集約（`sum` `avg` `min` `max` を numeric・float・int・bpchar に）を生成に含める。float の `sum` / `avg` は誤差の出ない値（整数値、2 進で正確な小数）だけを使う。

---

## 13. 実装の分担と工数

| 担当 | 範囲（編集してよいファイル） | 内容 | 日数 |
|---|---|---|---|
| **T1 numeric** | `types/numeric.rs`、`types/ops.rs`（numeric と float8 丸めと整数 `mod` の本体）、`catalog/builtin.rs` の numeric の演算子・キャスト・関数・`mod`・float8 丸めの行 | `Numeric` と `Datum` の橋渡し（演算子 14 行、キャスト 10 行、関数 12 + 6 + 3 行）、`encode_numeric` / `decode_numeric`、`NumericError → Error`、リテラル（`analyzer/expr.rs` の `transform_literal`）、`numeric_bridge_corpus` | 3 |
| **T2 日時と bpchar** | `types/datetime.rs`、`types/bpchar.rs`、`types/typmod.rs`、`types/io.rs` の新しい型の分岐と `input_text_typed`、`settings.rs` の `DateTimeSettings`（S と共同）、日時と bpchar の `catalog/builtin.rs` の行 | 入出力（`TypeEnv`）、キャスト 4 + 10 行（`CastMethod::Env` の使い手）、演算子（date 10、timestamp 6、timestamptz 6、bpchar 14）、関数（`now` 系 4、`date()` 系 6、bpchar 4）、`apply_typmod` / `typmod_in`、`CURRENT_DATE` 系の評価（P0 と共同）、橋渡しの差分テスト、`gen_bpchar_corpus.py` | 4 |
| **T3 関数・集約・正規表現** | `types/regex.rs`、`types/sys.rs`（regclass / regtype / int2vector）、`types/cmp.rs`、`types/datum.rs` の `cmp_datum` の拡張、`catalog/builtin.rs` の `AGGREGATES`・`generate_series`・`regclass` 系関数・`CatalogNames`、`tools/gen_procs.sh` | 正規表現エンジンとコーパス（約 2.5 日）、`AGGREGATES` 43 行と `PROCS` の生成（0.5 日）、regclass / regtype（1 日）、`cmp_datum` / `cmp_with_nulls`（0.5 日）、`generate_series`（0.5 日） | 5 |

依存: T1・T2・T3 はいずれも A（型と `Datum` 変種、`TypeEnv`、`CastMethod::Env`、`FnKind::Set`）の後で、互いに独立（共有するのは `catalog/builtin.rs` の表への行の追加だけなので、**最初に A が表の新しい行の場所（空のコメント区画）を作ってから分岐する**と衝突しない）。X2 は T1 の `Numeric` の橋渡し（`SumNumeric` など）に依存する。H4 の `tuple.rs` の `Kind` の追加は T1・T2 の `encode_*` / `decode_*` の署名だけに依存する（スタブで先行できる）。

### 13.1 並列化の注意

- `analyzer/expr.rs`（リテラル）、`analyzer/coerce.rs`（`coerce_type` の日時の分岐、`coerce_typmod`）、`analyzer/ddl.rs`（`resolve_type_name`）、`executor/eval.rs`（`apply_cast`、`session_value`）は P0 / N 担当の範囲。T1・T2 は**呼び出す関数を先に用意し**、P0 のマージ後に、これらのファイルの該当箇所の変更を P0 / N の担当者へ依頼する（00 §17 の「他の担当の範囲は自分では直さない」）。
- `error.rs` の SQLSTATE の追記は例外として T2 が行ってよい（P-2）。

---

## 14. 未検証の点

| 項目 | 内容 |
|---|---|
| tzdata の有無 | `sandbox` と CI で tzdata を明示インストールする（§5.3）。tzdata の版（2026b）と PG17 が使う版の違いでゾーンの境界が変わりうる（テストは `Asia/Tokyo` と `America/New_York` の最近の年だけ） |
| 型をまたぐ日時の比較の OID | 2345〜2350、2358〜2363、2371〜2376、2384〜2389、2534〜2545（実機で確認したが M4 では作らない。M5 で `BuiltinOperator` に tz 依存の種別を足すとき使う） |
| `pg_typeof` の評価 | `pg_typeof(x)` が `x` を評価するかどうか（PG は式を計画時に畳むのでエラーが出る）。D-9-9 の差分 |
| `regtype` の入力の文法 | `'foo bar'::regtype` などの構文エラーの文言と SQLSTATE（PG はパーサのエラーをそのまま返す） |
| numeric の `^` の本体 | M4 は `0A000`。`2 ^ 3`（float8）と `2.0 ^ 3`（numeric → `0A000`）の差は PG と違う（PG は numeric の累乗を返す） |
| 日時の `WARNING`（精度の丸め） | D-9-10 |
| `min` / `max` の等値のときの代表（float の `-0` と `+0`、bpchar のパディングの違い）の細部 | 実機で numeric（`1.100`）と float（`-0`）を確認。bpchar は `||` 経由の確認のみ |
| `Datum::as_str` を BpChar に広げた影響 | `Datum::Text` だけを想定している箇所（`planner/mod.rs`、`executor/nodes/distinct.rs`、`catalog/rows.rs`、`catalog/store.rs`）の洗い出しは A / P0 の最初の作業 |
| 正規表現の ARE の細部 | `\xhhh` の桁数、`[[.x.]]` の名前つき照合要素、`(?x)` の `#` コメントの境界は実機の差分生成器で確かめる（§9.5 のコーパスに含める） |
| 計画のキャッシュ（M5） | 日時リテラルの畳み込み結果が `TimeZone` / `DateStyle` に依存する（§4.1） |

---

## 15. 確認事項

`11-tests-plan.md` が `M4-Q` の通し番号に振り直す。★はディスク形式に関わるもの。

### [09-Q1] ★ numeric のディスク形式は 00 §12.3 の固定ヘッダ（調査の PG 形式ではなく）

- **仮決め**: `ndigits` `weight` `sign` `dscale` の各 `u16` / `i16` + 桁（§3.5）。`m5-types-fk.md` §2.1 の PG の short / long ヘッダ形式は採らない。
- **理由**: M2-Q2 でタプルヘッダが PG と違い、サイズ互換の意味がない。固定ヘッダの方が読み書きとテストが単純で、`Numeric::from_parts` / `Finite::weight()` / `dscale()` / `digits()` で組み立てられる。
- **変えたい場合**: PG 形式にすると `encode_numeric` / `decode_numeric` の書き直し（約 0.5 日）と、バイト例・`tuple.rs` のテストの更新。M2-Q20 により既存データディレクトリの互換は不要。

### [09-Q2] interval を M5 に回し、`timestamp - timestamp` を `42883` にする

- **仮決め**: D-9-1。`interval` キーワードは `0A000`、`timestamp - timestamp` と `ts ± '1 day'` は `42883`。`date - date` と `date ± int` は動く。
- **理由**: M4 の完了条件（pgbench、`\dt`）が interval を使わず、`interval` の連鎖（`Datum`、比較、ハッシュ、ディスク形式、`IntervalStyle`、演算子、集約）が大きい。
- **変えたい場合**: `Datum::Interval`（16 バイト）、typmod、演算子約 20 行、キャスト、`min`/`max`/`sum`/`avg(interval)`、入出力を T2 に足す（約 +3 日）。`yuzhu-datetime::Interval` は実装済みなので橋渡しが主。`extract` / `date_part` / `date_trunc` も同時に入れると約 +4 日。

### [09-Q3] 日時リテラルはアナライザでなくプランナの畳み込みで評価する

- **仮決め**: D-9-3（`Cast { Literal, InOut }` として残す）。
- **理由**: `analyze(stmt, catalog)` の署名（00 §5）に `TypeEnv` がない。署名を変えると全担当に波及する。
- **変えたい場合**: `analyze` に `&TypeEnv` を足す（呼び出し箇所は `session.rs` と `testing.rs` だけ）と、リテラルを即座に評価できる（エラーが解析時に出る。実装は簡単になるが 00 の署名変更）。

### [09-Q4] 型をまたぐ日時の比較演算子を作らない

- **仮決め**: D-9-5。`timestamp < timestamptz` は `timestamptz < timestamptz` + 暗黙キャスト。
- **理由**: `BuiltinOperator.func` が純粋関数。
- **変えたい場合**: `BuiltinOperator` に `OpFn::{Pure, Env}` を足し 30 行を入れる。インデックスが `ts_col < now()` で使えるようになる（`06-btree.md` の `datetime_ops` に型をまたぐ amop / amproc も要る。約 +1.5 日）。

### [09-Q5] 正規表現の後方参照と先読みは `0A000`

- **仮決め**: D-9-7。NFA の線形時間のみ。
- **理由**: 後方参照は線形時間で解けない。psql・pg_dump・ORM のパターンは使わない。
- **変えたい場合**: ステップ数の上限（例 1000 万）付きのバックトラック版を併設し、`\1` と先読みを持つパターンだけをそちらで実行する（約 +2 日。超過は `2201B regular expression is too complex`）。

### [09-Q6] `pg_typeof` は引数を評価しない

- **仮決め**: D-9-9。アナライザが型の定数にする。
- **理由**: `BuiltinFn` が型を受け取らない。
- **変えたい場合**: 引数式を 2 引数目の `Literal(型 OID)` つきで評価する形にする（`pg_typeof(1/0)` もエラーになる。約 0.5 日）。

### [09-Q7] DEFAULT の `'now'::timestamp` は毎回評価される

- **仮決め**: D-9-11（既知の差分）。
- **理由**: 保存時に畳むには deparse した定数を保存形式に書き戻す必要がある（Q-006）。
- **変えたい場合**: `07-catalog-ddl.md` の CREATE TABLE が、DEFAULT の式から `'now'` 系の日時リテラルのキャストを畳み、`deparse` したテキストを保存する（約 +1 日）。

### [09-Q8] `TypeEnv.names` で reg* の名前表示を一本化する（00 の P-1）

- **仮決め**: `TypeEnv` に `names: Option<&dyn OidNames>` を足し、`output_text_regproc` を廃止する。
- **理由**: `regclass::text` が出力段以外でも動く必要がある（`\d` の `c.oid::regclass::text`、ORM）。
- **変えたい場合**: 出力段だけで reg* を表示する（00 §12.4 のまま）と、`regclass::text`（`CoerceViaIO`）が数字になる。`\dt` の完了条件には影響しないが、`\d tbl`（任意）と ORM が壊れる。

### [09-Q9] `round(float8)` などの float8 版を入れる

- **仮決め**: §3.8。`round` `trunc` `ceil` `ceiling` `floor` `sign` の float8 版と整数の `mod` を入れる。
- **理由**: `round(2)` が PG では `double precision`。numeric 版だけだと黙って numeric を返す。
- **変えたい場合**: 入れない場合、`round(2)` は numeric（PG と型が違う）。入れる費用は約 0.3 日。

### [09-Q10] `timestamp(p)` の p が 7 以上のときの `WARNING` を出さない

- **仮決め**: D-9-10。
- **理由**: アナライザに通知の経路がない。
- **変えたい場合**: アナライザの出力に `warnings: Vec<Notice>` を足し、session が送る（`03-parser-analyzer.md` と `10-explain-copy-compat.md`、約 0.5 日）。
