# yuzhu M5 基本設計 05: 型の枠組み・M4 の穴埋め・bytea・uuid・配列

M5 の型のうち、**日時（interval・time・タイムゾーン）以外のすべて**を受け持つ章です。内容は 4 つあります。

1. 型の枠組み: `pg_type` の `typreceive` / `typsend` / `typmodin` / `typmodout` / `typarray` / `typelem` / `typsubscript` と、対応する `pg_proc` の行（M2-Q13 の持ち越し）。キャスト・比較・ハッシュ・演算子クラスを足すときの決まり。ポリモーフィック型（`anyarray` など）の解決。
2. M4 が統合した numeric と `char(n)`（bpchar）の穴埋め（PostgreSQL との差の洗い出しの手順と、M5 で埋めるもの）。
3. `bytea`、`uuid`、配列（1 次元）の新規実装。
4. 全型のバイナリ形式の表（`send` / `recv`。例つき）と、PostgreSQL との差分コーパス試験の作り方。

- 略号は **TY**（作業パッケージ TY-1〜TY-7）。契約は `spec/design/m5/00-contracts.md`（以下「00」）に従う。00 と食い違う点は §11 に書いた（章の中で黙って変えない）。
- 前提: **M4 が numeric・`char(n)`・date・timestamp・timestamptz・regclass・regtype・int2vector を統合済み**（`spec/design/m4/00-contracts.md`、`spec/design/m4/09-types-functions.md`。以下「M4-09」）。M5 はそれらを作り直さず、穴だけ埋める。M4 の章 06・07 などはまだ並行して書かれているので、確定したら §9 の突き合わせ項目から確かめ直す。
- 日時（`interval`、`time`、`DateStyle`、`TimeZone`）は `06-types-datetime.md`、バイナリの振り分けとメッセージは `04-extended-query.md` が持つ。この章は**全型のバイナリ形式の表（§6.4）を定義**するが、日時の型の `send` / `recv` の**実装**は 06 章。
- 調査: `spec/research/m5-types-fk.md`（§2 numeric、§4 char(n)、§5 bytea、§6 uuid、§7 配列、§10 カタログ）、`pg-compat-tools.md` §3.1（psql `\d tbl`）。調査と実機・M4 の食い違いは §0 の決定表に書いた。
- 正解は PostgreSQL 17。根拠の記号は 00 §9 と同じ（【確認】実機 17.11（`sandbox/pg.sh start`、C ロケール、UTF8）またはソース REL_17_STABLE で確かめた、【記憶】未照合、【提案】yuzhu への推奨）。`PG:<path>` は `https://github.com/postgres/postgres/blob/REL_17_STABLE/<path>`。「（未検証）」は確かめていない。この章の固定値（バイト列・OID・エラー文言）は、特に断らない限り**実機で確かめた**。

---

## 0. 決定（この章で扱う論点。調査間の食い違いも）

00 の D20（バイナリ形式）、D22（numeric・日時の統合は M4）、D25（配列）、D43（`Datum::BpChar`）をそのまま実装の形にする。`TY-Dn` はこの章の決定。

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| TY-D1 | 配列の `Datum` | `Int4Array`（M3）と `Int2Vector`（M4 の `int2[]`）を残す／`Datum::Array(Box<ArrayValue>)` に一本化（00 §4.9、D25） | **一本化する。`Int4Array` は削除、`int2[]`（1005）の値は `Array`。`Datum::Int2Vector` は `int2vector`（22）の値としてだけ残す**（`OidVector` も `oidvector` のまま）。`anyarray` を取る関数が `int2vector` / `oidvector` を受けたときは、下限 0 の `ArrayValue` に読み替える（§5.3 `array_view`） | PostgreSQL では `int2vector` が `int2[]` と同じ表現（下限 0 の配列）。実機で `('1 2'::int2vector)[1]` = `2`、`('1 2'::int2vector)::int2[]` = `[0:1]={1,2}`（【確認】）。psql の `\d tbl` が `prattrs[s]` と `generate_series(0, array_upper(prattrs::int2[], 1))` を使うのでこの 0 始まりが必要 |
| TY-D2 | 次元数 | 多次元まで作る／1 次元だけ（D25） | **値の表現（`dims: Vec<ArrayDim>`）とディスク形式は PostgreSQL の `ArrayType` と同じ多次元対応。M5 の入口（入力・コンストラクタ・バイナリ受信）は 1 次元だけ許し、2〜6 次元は `0A000`（`multidimensional arrays are not supported yet`）、7 次元以上は PostgreSQL と同じ `54000`。定数 `M5_MAX_NDIM = 1` の 1 行で広げられるように、パーサ・出力・`array_eq`・バイナリは一般の次元数で書く** | 00 D25 のとおり。多次元を許す費用は小さい（入出力の一般形は PG のアルゴリズムの移植で、1 次元専用に書く方がむしろ手間）が、添字・`ARRAY[[...]]`・スライスの意味が絡むので M6 |
| TY-D3 | 配列のディスク形式 | yuzhu 独自（`int2vector` と同じ平坦な並び）／PG の `ArrayType` と同じ | **PG と同じ（§3.3）。タプルの varlena のペイロード = `ArrayType` の `vl_len_` を除いた部分。`dataoffset` の意味も PG のまま（4 バイトヘッダの像を基準にした位置）。要素は常に 4 バイトヘッダ（varlena のとき）と `typalign` の整列で並べる。読み出しは 1 バイトヘッダの要素も受ける** | 実機の `heap_page_items` のバイトと完全に一致する（§3.3 の例は実機から取った）。M6 でユーザーテーブルの配列列を許すときにディスク形式を変えずに済む |
| TY-D4 | ユーザーテーブルの配列列 | 許す／`0A000`（D25、M5-Q8） | **`0A000 array types are not supported yet`（M4 の `int2[]` と同じ文言）。システムカタログ（OID < 16384）の列と、問い合わせの途中の値としては配列を使える** | 比較・ハッシュ・B+Tree の opclass（`array_ops`）が M6 のため。DDL の検査は `analyzer/ddl.rs` の `resolve_type_name`（§11 の依頼）が `types::array::is_array_type` で行う |
| TY-D5 | `text` → `bytea` のキャスト | 調査: 「`text → bytea` のキャストはない」（m5-types-fk §5.2）／実機 | **`pg_cast` の行は作らず、既存の自動 I/O 変換の経路（`find_coercion_pathway` の `CoerceViaIo`）に任せる。実機は `'ab'::text::bytea` = `\x6162`（`byteain` を通す。`'\x41'::text::bytea` = `\x41`）、`'\x41'::bytea::text` = `\x41`（代入）、`1::bytea` は `42846 cannot cast type integer to bytea`（【確認】）** | 調査の記述は誤り（`convert_to` は別の手段）。`bytea` を `is_supported_type` に足せば、M4 のアナライザが同じ結果を出す |
| TY-D6 | `bytea_output` | `hex` だけ／`hex` と `escape` | **両方。`TypeEnv` に `bytea_output: ByteaOutput` を足す（§4.2）。設定値は `hex` / `escape`。不正は `22023 invalid value for parameter "bytea_output": "foo"` + HINT `Available values: escape, hex.`（【確認】）** | 調査の推奨どおり（安い）。pgAdmin が接続時に SET する（pg-compat §2.6） |
| TY-D7 | uuid の乱数 | 暗号クレートを直接／`yuzhu-auth` | **`yuzhu_auth::random::fill`（00 §4.11、D29）。version 4（`byte[6] = (b & 0x0f) \| 0x40`、`byte[8] = (b & 0x3f) \| 0x80`）。失敗は `XX000 could not generate random values`** | PostgreSQL の `gen_random_uuid`（PG:src/backend/utils/adt/uuid.c）と同じ |
| TY-D8 | `min` / `max` / `sum` の uuid・bytea 版 | 作る／作らない | **作らない**（PG17 にも `min(uuid)` `min(bytea)` はない。`pg_proc` の検索で 0 行【確認】）。`string_agg(bytea, bytea)` も M6 | M4 の `AGGREGATES` に行を足さずに済む |
| TY-D9 | numeric のバイナリ形式 | 調査 §2.8（`int16 ndigits, weight, uint16 sign, int16 dscale, digits`）／実機 | **`yuzhu_numeric::Numeric::to_binary` / `from_binary` をそのまま使う**（実装済み。調査の記述と一致し、さらに**`±Infinity` の dscale は 32 を送る**という PG の癖（`NUMERIC_DSCALE` が特殊値のヘッダの下位ビットを読む。実機: `numeric_send('Infinity')` = `\x00000000d0000020`【確認】）まで再現している）。受信の検査（`invalid sign` / `invalid scale` / `invalid digit` の `22P03`）も同じ文言（【確認】） | クレートが PG17 の `numeric_send` / `numeric_recv`（PG:src/backend/utils/adt/numeric.c）と同じ。M5 の作業は橋渡しと固定値のテストだけ |
| TY-D10 | 全型のバイナリ形式 | 型ごとに各担当が決める（00 D20）／この章で表を一本化 | **§6.4 の表を正とする**（全型・send と recv・バイト例。日時の 5 型も含む）。日時の実装は 06 章だが、固定値は同じ表を単体テストに使う | 表が 2 か所にあると食い違う。実機の `*_send` 関数で取った値 |
| TY-D11 | `pg_type` の列の埋め方 | `BuiltinType` に recv / send / typmodin / typmodout の列を足す／別表 | **別表 `TYPE_PROCS`（`catalog/builtin/type_io.rs` ★）。`BuiltinType` と `ty(..)` の引数は変えない。配列型の行は要素型の行から機械的に作る（§5.1）。`typarray` は PG の値をそのまま出す（M2-Q13 の差（2）が消える）** | `ty(..)` の引数が既に 16 個で、F0 の分割後に複数担当が触る。別表なら衝突しない |
| TY-D12 | ポリモーフィック型 | M2 の `anynonarray` だけ／PG の 5 つ（`anyelement`、`anyarray`、`anynonarray`、`anycompatible`、`anycompatiblearray`） | **5 つ。解決は `analyzer/polymorphic.rs` ★（§5.4）。`anyenum`・`anyrange` などは作らない** | 配列関数（`array_length` など）と `\|\|` に要る |
| TY-D13 | 配列の式の木 | 関数で表す（`array_ctor` などの関数行）／`ExprKind` に変種を足す | **変種を足す（§4.4）: `ArrayCtor`、`ArraySubscript`、`ArrayCoerce`、`ScalarArrayOp`、`SubLinkKind::Array`。F0 がまとめて足し、TY が評価と変換を実装する**（§11 の依頼 1） | 可変個の引数・結果型が引数から決まる・遅延評価（`ANY` の打ち切り）は、`BuiltinFunction`（`args: &'static [Oid]`、純粋関数）では表せない。`pg_proc` に行が出てしまうのも避ける |
| TY-D14 | カットライン（00 §1.3: `ARRAY(SELECT)` と添字） | 00 のとおり落とす／**落とさない** | **推奨: 落とさない（TY-5 の必須）。** psql 17 の `\d tbl` は、**どんな表でも**（`pg_policy` が空でも）`array(select rolname from pg_roles where oid = any (pol.polroles) order by 1)` と `prattrs[s]` を含む問い合わせを送る。解析が通らないと `\d tbl` が何も表示しない（実機の `psql -E` で 12 本を確認、§6.5）。落とすなら `\d tbl` を M5 の完了条件から外す必要がある | `pg-compat-tools.md` §3.1 の要点。確認事項 M5-TY-Q1 |
| TY-D15 | 数値関数のうち M6 に回すもの（00 D23） | 行を作らない／行だけ作って本体は `0A000` | **`sqrt`・`exp`・`ln`・`log`・`log10`・`power`・`pow` の numeric 版は、行（OID は PG17。§6.6）だけ入れ、本体は `0A000 numeric <name> is not supported yet`（M4 の `^` と同じ）** | 行がないと、numeric の引数が暗黙で float8 に変わり `sqrt(2.0)` が **numeric ではなく float8 を黙って返す**（PG は numeric）。エラーの方が安全 |
| TY-D17 | SELECT 句の集合返却関数（SRF）の最小対応（00 D25 は「SELECT 句の SRF は M6」。psql の `\d tbl` の `Referenced by` が `pg_partition_ancestors` を SELECT 句で使う。07 §7.3・§6.8 の完了条件。レビュー対応 R-28） | M6 に回す（FK のある表の `\d tbl` が M5 では動かない）／**SELECT 句に SRF が 1 つだけで、ほかの出力列も FROM 句もない `SELECT srf(args)` を `SELECT * FROM srf(args)` に書き換える最小の対応を M5 に足す** | **足す（TY-5c。+0.5 日）**。解析段階の書き換え（`analyzer/polymorphic.rs` の隣の `analyzer/srf_rewrite.rs` ★。M4 の `FnKind::Set` と FROM 句の `FunctionScan` を使う）。対象は `pg_partition_ancestors(regclass)`（パーティションでない表には 0 行。`pg_proc` の行と `FnKind::Set` の実体を TY-5c が足す）、`unnest`・`generate_subscripts` が使える形。複数の SRF、SRF と他の列の併用、WHERE の SRF は M4 のとおり `0A000`。psql の問い合わせは `confrelid IN (SELECT pg_partition_ancestors('..') UNION ALL VALUES (..))` で、副問い合わせの腕がこの形に当たる | 00 D25 に例外を足した。足さないと FK を持つ表の `\d tbl` が動かず、07 の完了条件 9（`\d p` / `\d c` の一致）が達成できない。10 KD-14 もこの形に合わせた |
| TY-D16 | 差分コーパスの形式 | 数値・日時と同じ TSV（`gen_corpus.py`）／式を並べたファイルから sqllogictest を生成 | **新しい型（bytea・uuid・配列・numeric と bpchar の穴）は後者（§7.2）。生成器は psql と PL/pgSQL（サンドボックスに Python がないため）。numeric・日時の既存の TSV コーパスはそのまま維持する** | 式をそのまま PG と yuzhu の両方で流せ（`tests/run.sh --target pg` で PG 自身も通る）、橋渡しの層だけでなく解析・演算子解決・出力も一度に確かめられる |

**調査と M4 の食い違い（まとめ）**:

- `m5-types-fk.md` §2.1 は numeric のディスク形式を PG の short / long ヘッダにするよう提案。M4-09 D-3 / 09-Q1 が **固定 8 バイトヘッダ**（00 §12.3）を採った。**M4 を採る（変えない）**。この章は numeric のディスク形式に触れない。
- `m5-types-fk.md` §7.2 の配列の最小実装は `Datum::Array(Box<ArrayValue { elem_type, items }>)`（次元なし）。00 §4.9 が `dims` を足した形を決めた。**00 を採る**。
- `m5-types-fk.md` §5.2「`text → bytea` のキャストはない」は実機と違う（TY-D5）。
- `m5-types-fk.md` §7.2 の「`=` 比較」: 配列の `<` `<=` `>` `>=` と `ORDER BY` / `GROUP BY` は M6（00 §4.9）。`cmp_datum` は全順序で書く（§5.1）が、アナライザは配列の `ORDER BY` を `42883`（`could not identify an ordering operator for type integer[]`）にする（PG は動く。既知の差）。

---

## 1. 範囲

### 1.1 M5 で作るもの

| 分類 | 内容 | WP |
|---|---|---|
| 枠組み | `TYPE_PROCS`（全型の `typreceive` / `typsend` / `typmodin` / `typmodout` / `typsubscript`）、配列型の行（全スカラー型）、`typarray` の埋め直し、`pg_proc` の行、`bytea` / `uuid` / `anyelement` / `anycompatible` / `anycompatiblearray` / `_cstring` / `_record` の `pg_type` の行、`bpchar` / `numeric` の `typmodin` / `typmodout` の本体、関数形式のキャスト（`int4(numeric)` など）、`bytea_ops` / `uuid_ops` の opclass、`cmp_datum` / `hash_datum` の新しい変種 | TY-1 |
| numeric | M4 との差の洗い出し（§5.2）と穴埋め: `gcd` `lcm` `min_scale` `trim_scale` `width_bucket` `factorial` `@`、`sqrt` 系の行（本体 `0A000`）、関数形式のキャスト、`numeric` のバイナリ（橋渡し） | TY-2 |
| bpchar | 差の洗い出しと穴埋め: 関数形式のキャスト、`bpchar` のバイナリ、配列の要素としての扱い（`'{"ab  "}'::char(4)[]`） | TY-3 |
| bytea | 入力（hex・escape）、出力（`bytea_output`）、`=` `<>` `<` `<=` `>` `>=` `\|\|`、関数（`length` `octet_length` `bit_length` `substr` `substring` `position` `overlay` `get_byte` `set_byte` `get_bit` `set_bit` `encode` `decode` `convert_to` `convert_from`）、任意（`md5`・`sha224`〜`sha512`）、B+Tree の opclass、バイナリ | TY-4 |
| uuid | 入力（4 つの形）、出力、比較、`gen_random_uuid()`、B+Tree の opclass、バイナリ | TY-4 |
| 配列 | 1 次元。要素はスカラー型すべて（§3.3）、NULL 要素あり、下限は任意。入出力、`ARRAY[...]`、`ARRAY(SELECT ...)`、`a[i]`（読み取り）、`x op ANY / ALL (array)`、`::T[]`、`=` `<>`、`array_length` `array_upper` `array_lower` `array_dims` `array_ndims` `cardinality` `array_to_string`、`int2vector` / `oidvector` → 配列の読み替え、カタログの `int2[]` / `oid[]` / `text[]` / `"char"[]` 列の移行、バイナリ | TY-5 |
| バイナリ | 全型の表（§6.4）と、`yuzhu-core` 側の `send` / `recv`（`types/binary.rs` の振り分けは 04 章） | TY-6 |
| 試験 | 差分コーパス（§7） | TY-7 |

### 1.2 M5 で作らないもの（実行すると `0A000`、または PG と同じ `42883`）

- ユーザーテーブルの配列列（TY-D4）。`CREATE TABLE t(a int[])`、`ALTER TABLE ... ADD COLUMN a int[]`、`CREATE TABLE AS` で配列列ができる場合。
- 多次元配列（TY-D2）、配列のスライス `a[1:2]`（`0A000 array slices are not supported yet`）、添字での代入 `UPDATE t SET a[1] = 0`（`0A000 array element assignment is not supported yet`）、`ARRAY[[1,2],[3,4]]`。
- `unnest`、`generate_subscripts`、SELECT 句の集合返却関数、`array_agg`、`array_position`、`array_remove` `array_replace` `array_fill` `string_to_array`、`@>` `<@` `&&`、配列の `<` `<=` `>` `>=`、`ORDER BY` / `GROUP BY` / `DISTINCT` / B+Tree の配列キー（00 D25）。**ただし §6.5 のとおり、psql の `\d tbl` に要る `string_agg` と FROM 句の `unnest` などは確認事項**。
- `bytea` の `LIKE`（`~~` 2016、`!~~` 2017）、`bit_count(bytea)`、`btrim` / `ltrim` / `rtrim(bytea, bytea)`、`string_agg(bytea, bytea)`。`bit` / `varbit`、`money`、`inet` 系、json / jsonb、enum、range。
- `uuid_extract_timestamp` `uuid_extract_version`（PG17 で追加）、`uuidv7`（PG18）。
- numeric の `sqrt` 系の本体（TY-D15）、`to_char`、`to_number`、`random(numeric, numeric)`（PG17）、`generate_series(numeric, ...)`。
- `convert_to` / `convert_from` は `UTF8` のみ。それ以外の符号化名は `0A000 conversion to encoding "LATIN1" is not supported yet`（PG は動く。既知の差）。

### 1.3 保証すること

- 配列のテキストの入出力が PostgreSQL と**一字一句**一致する（引用符の規則、`NULL`、下限つき `[0:2]={...}`、エラーの `malformed array literal` と DETAIL）。
- 配列のディスク形式が PostgreSQL の `ArrayType` と同じバイト列になる（§3.3 の例を単体テストの固定値にする）。
- 全型のバイナリの `send` が PostgreSQL の `*_send` と同じバイト列、`recv` が同じ検査（SQLSTATE と文言）になる（§6.4）。
- `bytea` と `uuid` が `PRIMARY KEY` / `UNIQUE` / `CREATE INDEX` の列に使える（opclass）。
- `\d tbl` に要る配列の機能（§6.5）が、テーブルが空でも解析を通る。

---

## 2. 構成

`★` は M5 で新規、`△` は変更、括弧内は持ち主。00 §2 の木に、この章の分を足す。M4 のファイル名は M4 の確定した名前を正とする。

```
impl/rust/crates/yuzhu-core/src/
├── types/
│   ├── mod.rs            △ `oid` に BYTEA / UUID / 配列型の定数、`TypeEnv.bytea_output`（§4.2）、`SqlType::is_array`
│   ├── datum.rs          △ `cmp_datum` / `rank` の新しい変種（Bytea、Uuid、Array）。変種そのものは F0 が足す（00 §4.9）
│   ├── hash.rs           △ `hash_datum` の新しい変種（M4 の X2 が作る。TY が足す）
│   ├── io.rs             △ `input_text` / `output_text` / `input_text_typed` / `input_is_eager` の新しい型の分岐
│   ├── typmod.rs         △ M4 が作る。bpchar / numeric の `typmod_in` / `typmod_out`、配列の要素ごとの `apply_typmod`
│   ├── typeinfo.rs       ★ 配列の要素に要る型の性質（typlen・byval・align・要素の符号化）。`catalog::builtin::TYPES` から引く
│   ├── bytea.rs          ★ 入出力、演算、関数、send / recv、`encode_bytea` / `decode_bytea`
│   ├── uuid.rs           ★ 入出力、`gen_random_uuid`、send / recv
│   ├── array.rs          ★ `ArrayValue`、入出力、ディスク形式、send / recv、`array_view`、要素ごとのキャスト
│   ├── array_ops.rs      ★ `array_length` ほかの関数、`=` `<>`、`ANY` / `ALL`、添字
│   ├── md5.rs            ★ 任意（`md5(text)` / `md5(bytea)`。手書き。RFC 1321）
│   ├── wire.rs           ★ 任意（既存型の `send` / `recv` を 1 か所に集める場合。§8 の TY-6）
│   ├── numeric.rs        △ M4 が作る。TY-2 の関数・バイナリの橋渡し
│   └── bpchar.rs         △ M4 が作る。TY-3
├── catalog/builtin/      F0 が分割した後の構成（00 §2）
│   ├── type_io.rs        ★ `TYPE_PROCS`（recv / send / typmodin / typmodout / typsubscript / typanalyze）、配列型の行の生成、型の 8 点チェックリストの表（§5.1）
│   ├── numeric.rs        △ numeric の行の追加（TY-2）
│   ├── text_misc.rs      △ bpchar・bytea・uuid の行
│   └── array.rs          △ 配列の関数・演算子・`PROCS` の行
├── catalog/opclass.rs    △ `bytea_ops`・`uuid_ops` の行（§5.1）
├── analyzer/
│   ├── polymorphic.rs    ★ ポリモーフィック型の解決（TY-D12）
│   └── array_expr.rs     ★ `ARRAY[...]`、`ARRAY(SELECT)`、`a[i]`、`ANY` / `ALL`、配列のキャストの解析
├── executor/
│   └── eval_array.rs     ★ `ArrayCtor` などの評価（`executor/eval.rs` から 1 行で呼ぶ）
└── storage/heap/tuple.rs △ `Kind::{Bytea, Uuid, Array}` の追加（§3、§11 の依頼 3）
```

- **新しい型の追加の決まり（8 点チェックリスト）**を §5.1 に書いた。TY-1 がこの表を `catalog/builtin/type_io.rs` のコメントと単体テスト（各項目に漏れがないことを `TYPES` の全行について検査）にする。TD の `interval` / `time` も同じ表に従う。
- `types` は `catalog` より下の層（00 §2 の依存図）。配列は要素型の性質（typlen など）を要るので、`types::typeinfo` が `catalog::builtin::type_by_oid` を呼ぶ（M1 の `type_display_name` が `catalog::builtin::format_type_name` を呼ぶのと同じ既存の逆向きの依存。静的表だけを引くので循環しない）。
- 章の規約（M2・M3・M5 の規約に加えて）:
  1. **エラーは PostgreSQL と同じ SQLSTATE と文言**。この章の表に載せたものを定数にして単体テストで固定する。
  2. `unsafe` を使わない。配列・bytea のバイト操作は `u32::from_le_bytes` などの安全な API だけ。
  3. 長さの上限は配列が `MaxArraySize = 134_217_727` 要素（`MaxAllocSize / 8`）、bytea が 1GB − 1（`MaxAllocSize`）。**1 行 8160 バイトの上限（M2、`54000 row is too big`）が先に効く**ので、巨大な値は問い合わせの途中（`repeat` などで作る）でだけ起きる。
  4. 乱数は `yuzhu_auth::random`（00 規約 6）。`SystemTime::now()` は使わない（この章に時刻は出ない）。

---

## 3. ディスク上の形式

ヒープタプルとインデックスタプルの列の符号化（M2 §3.6、M4-09 §3.5）に、`bytea`・`uuid`・配列を足す。varlena のヘッダ（1 バイト / 4 バイト）の規則は M2 §3.6 のまま。以下の「ペイロード」は varlena ヘッダを除いた部分。

### 3.1 bytea

| 項目 | 値 |
|---|---|
| OID / typlen / typbyval / typalign / typstorage | 17 / -1 / f / `i` / `x`（【確認】`pg_type`） |
| ペイロード | 生のバイト列（そのまま）。長さ 0 も可（1 バイトヘッダ `0x03` だけ） |

例（varlena の 1 バイトヘッダ `((len + 1) << 1) | 1`）:

| 値 | 保存されるバイト列 |
|---|---|
| `'\x0001ff'::bytea` | `09 00 01 FF` |
| `''::bytea` | `03` |

### 3.2 uuid

| 項目 | 値 |
|---|---|
| OID / typlen / typbyval / typalign / typstorage | 2950 / 16 / f / `c` / `p`（【確認】） |
| ペイロード | 16 バイト。UUID の文字列を 16 進 2 桁ずつ左から並べた順（ネットワークバイトオーダー）。ヘッダなし・整列なし |

例: `'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'` → `A0 EE BC 99 9C 0B 4E F8 BB 6D 6B B9 BD 38 0A 11`。比較は 16 バイトの辞書順（`memcmp`）。

### 3.3 配列

PostgreSQL の `ArrayType`（PG:src/include/utils/array.h）と同じ。実機（`pageinspect` の `heap_page_items`）で確かめたバイト列を例にする。

**イメージ**（PG が計算する基準の形。ヘッダ 4 バイトの `vl_len_` を含む）:

| イメージの位置 | 大きさ | 名前 | 内容 |
|---|---|---|---|
| 0 | 4 | `vl_len_` | varlena ヘッダ（ペイロードには含めない） |
| 4 | 4 | `ndim` | 次元数（0〜6。空の配列は 0） |
| 8 | 4 | `dataoffset` | NULL 要素がなければ 0。あれば**データ部の先頭の位置（イメージの先頭から）**= `MAXALIGN(16 + 8 * ndim + ⌈nitems / 8⌉)` |
| 12 | 4 | `elemtype` | 要素型の OID（`pg_type.typelem`。例: `int4` = 23 = `0x17`） |
| 16 | 4 × ndim | `dims[]` | 各次元の要素数（i32） |
| 16 + 4 × ndim | 4 × ndim | `lbound[]` | 各次元の下限（i32。SQL の既定は 1） |
| 16 + 8 × ndim | ⌈nitems / 8⌉ | NULL ビットマップ | `dataoffset != 0` のときだけ。要素 i（0 始まり、行優先）が非 NULL ならバイト `i / 8` のビット `i % 8` が 1 |
| `dataoffset`（または `MAXALIGN(16 + 8 * ndim)`） | | データ部 | 非 NULL の要素を順に（NULL は何も置かない）。**各要素のあとを `typalign` に切り上げる** |

- 全整数はリトルエンディアン。**タプルに保存するペイロードは、イメージの 4 〜 末尾**（`vl_len_` を除く）。データ部の先頭は、ペイロードの位置では `dataoffset − 4`（または `MAXALIGN(16 + 8 * ndim) − 4`）。
- データ部の先頭はイメージで 8 の倍数なので、各要素の整列は**データ部の先頭からの相対位置で数えてよい**（`typalign` は最大 8）。
- 1 バイトヘッダ（ペイロード + 1 バイトが 127 以下）か 4 バイトヘッダかは、配列そのものには関係ない（タプルの書き込みが決める。M2 §3.6）。**`dataoffset` と位置の計算は、どちらの形でも 4 バイトヘッダのイメージを基準にする**（PG は読み出すとき常に 4 バイトヘッダの像へ復元する）。
- 空の配列は `ndim = 0`、`dataoffset = 0`、`elemtype` だけ（`dims` なし。PG の `construct_empty_array`）。`'[1:0]={}'` のような長さ 0 の次元は入力でエラー（`2202E`）なので、`ndim > 0` で要素数 0 の配列は作れない。
- 要素の符号化（`types::typeinfo::ElemInfo`。**配列の中だけの規則で、列の符号化とは違う点がある**）:

| 要素型 | typlen | byval | align | 要素のペイロード |
|---|---|---|---|---|
| `bool` / `"char"` | 1 | t | c | 1 バイト |
| `int2` | 2 | t | s | i16 |
| `int4` / `float4` / `date` | 4 | t | i | i32 / f32 のビット / i32 |
| `int8` / `float8` / `timestamp` / `timestamptz` / `time` | 8 | t | d | i64 / f64 のビット / i64 / i64 / i64 |
| `oid` / `regproc` / `regclass` / `regtype` / `xid` / `cid` | 4 | t | i | u32 |
| `tid` | 6 | f | s | block u32 + offset u16 |
| `name` | 64 | f | c | 64 バイト固定（UTF-8、NUL 埋め） |
| `uuid` | 16 | f | c | 16 バイト |
| `interval` | 16 | f | d | `time i64`、`day i32`、`month i32`（TD-1 の `Interval` と同じ） |
| `text` / `varchar` / `bpchar` | -1 | f | i | **4 バイトヘッダ** `(len + 4) << 2`（LE）+ UTF-8 のバイト列（bpchar は埋めた後の文字列） |
| `bytea` | -1 | f | i | 4 バイトヘッダ + 生のバイト列 |
| `numeric` | -1 | f | i | 4 バイトヘッダ + M4 の numeric のペイロード（8 バイトヘッダ + 桁。M4-09 §3.5） |

  - **varlena の要素は常に 4 バイトヘッダで書く**（実機: `text[]` の要素 `a` は `14 00 00 00 61` + 整列の 0 が 3 バイト）。読み出しは 1 バイトヘッダ（`VARSIZE_ANY`）も受けるが、整列は**名目どおり**（列の読み出しのような「先頭が 0 なら詰め物」の判定をしない。PG の `att_align_nominal`）。
  - 配列の配列は作れない（`elemtype` が配列型の値は `XX001`）。`int2vector` / `oidvector` / `aclitem` / `record` / `cstring` の配列（`_int2vector` など）は型としては `pg_type` に行があるが、**値は NULL 専用**（M2 の `is_null_only_type` を引き継ぐ）。

**書き込み（`encode_array(&ArrayValue) -> Result<Vec<u8>>`、ペイロードだけを返す）**:

```text
n = items.len();  ndim = dims.len()
has_null = items に Null がある
overhead = MAXALIGN(16 + 8*ndim + (has_null ? ceil(n/8) : 0))
buf = [0u8; overhead - 4]                               // イメージの 4 以降
buf[0..4]  = ndim (i32 LE)
buf[4..8]  = has_null ? overhead : 0
buf[8..12] = elemtype
buf[12 + 4*i ..]       = dims[i]   (i = 0..ndim)
buf[12 + 4*ndim + 4*i] = lbound[i]
if has_null: ビットマップを buf[12 + 8*ndim ..] に書く
for item in items (NULL は飛ばす):
    要素のペイロードを書き、末尾を typalign に 0 で切り上げる（varlena は 4 バイトヘッダを前置）
return buf                                              // 長さ上限は呼び出し側（54000）
```

**読み出し（`decode_array(payload: &[u8], expected_elem: Oid) -> Result<ArrayValue>`）**の検査（失敗はすべて `XX001`、`corrupted array value in a tuple`。壊れたデータを黙って返さない。M2 の規約）: `payload.len() >= 12`、`0 <= ndim <= 6`、`elemtype == expected_elem`（列の型から）、`dims[i] >= 0`、`Π dims[i] <= MaxArraySize`、`dataoffset == 0` または `overhead_with_nulls <= dataoffset <= payload.len() + 4`（かつ 8 の倍数）、ビットマップが範囲内、データ部が各要素の長さ（varlena は読んだヘッダの長さ）を足してちょうど（または詰め物の分だけ短く）終わる。

**例**（すべて実機の `heap_page_items` / `array_send` から。ペイロードのバイト列を 16 進で。先頭の 1 バイトは 1 バイトヘッダ）:

| 値（型） | 保存されるバイト列 | 読み方 |
|---|---|---|
| `'{1,NULL,3}'::int4[]` | `4B` `01000000` `20000000` `17000000` `03000000` `01000000` `05` `00000000000000` `01000000` `03000000` | ヘッダ `0x4B` = 長さ 37。`ndim 1`、`dataoffset 32`（イメージ。ペイロードでは 28）、`elemtype 23`、`dim 3`、`lbound 1`、ビットマップ `0b101`（要素 0 と 2 が非 NULL）+ 詰め物 7 バイト（`16 + 8 + 1` を 32 に切り上げ）、データ `1`、`3`（NULL は何も置かない） |
| `'{a,bb}'::text[]` | `4B` `01000000` `00000000` `19000000` `02000000` `01000000` `14000000` `61` `000000` `18000000` `6262` `0000` | `dataoffset 0`（NULL なし）、`elemtype 25`、`dim 2`、`lbound 1`。要素 `a` = 4 バイトヘッダ `0x14`（長さ 5）+ `61` + 詰め物 3、要素 `bb` = ヘッダ `0x18`（長さ 6）+ `6262` + 詰め物 2。データ部の先頭はペイロードの位置 20（イメージ 24 = `MAXALIGN(16 + 8)`） |
| `'{}'::int4[]` | `1B` `00000000` `00000000` `17000000` | `ndim 0`、`dataoffset 0`、`elemtype 23`（長さ 13 → ヘッダ `0x1B`） |
| `('1 2'::int2vector)::int2[]`（`[0:1]={1,2}`） | `33` `01000000` `00000000` `15000000` `02000000` `00000000` `0100` `0200` | `elemtype 21`、`dim 2`、**`lbound 0`**、要素は 2 バイトずつ（長さ 25 → `0x33`）。【PG の `array_send` 出力から組み立てた。ヘッダ以外の位置は上の規則どおり】 |

### 3.4 numeric・bpchar・int2vector・oidvector（変更なし）

M4-09 §3.5 のとおり（numeric は 8 バイトの固定ヘッダ + 10000 進の桁、bpchar は UTF-8 のバイト列、`int2vector` は `i16` の並び、`oidvector` は M2 の `u32` の並び）。**`int2vector` と `oidvector` は yuzhu 独自のまま**（PG の配列ヘッダを持たない。TY-D1。値が `Datum::Int2Vector` / `OidVector` なので、配列として読むときだけ §5.3 の `array_view` で下限 0 の `ArrayValue` に変える）。配列の要素としての numeric は §3.3 の表（4 バイトヘッダ + 同じペイロード）。

### 3.5 カタログの配列列の移行（M5-Q15）

| 列の型 | M4 までの値 | M5 の値 |
|---|---|---|
| `int2[]`（`pg_constraint.conkey` `confkey` など） | `Datum::Int2Vector`（`i16` の並びの varlena） | `Datum::Array`（`elemtype 21`、下限 1、PG の `ArrayType`） |
| `oid[]`（`conpfeqop` など） | NULL 専用 | `Datum::Array`（`elemtype 26`）。M5 で値を書くのは FK の `pg_constraint`（07 章）だけ |
| `text[]` `"char"[]` | NULL 専用 | 同様に `Array`（空の表の列は NULL のまま） |
| `aclitem[]` `anyarray` | NULL 専用 | **NULL 専用のまま**（`aclitem` に `Datum` がない） |
| `int2vector`（`pg_index.indkey`、`indoption`、`pg_trigger.tgattr` など）、`oidvector`（`indclass`、`proargtypes`） | `Int2Vector` / `OidVector` | **変更なし** |

`catalog_version` を上げる（00 D35、F0 が番号を決める）。移行の手順は §5.6。

### 3.6 ディスク形式の変更点（00 §3.7 への追記）

| 対象 | 変更 |
|---|---|
| 配列 | PG の `ArrayType`（§3.3）。`int2[]` の符号化が変わる |
| bytea / uuid | §3.1、§3.2（新規） |
| `AttrDesc` | 配列型・bytea・uuid の `typlen` / `typbyval` / `typalign` を `TYPES` の行から引く（`storage/mod.rs` の `fallback_len_byval` / `type_align` の既定は、行のない型だけ。§11 の依頼 3） |

---

## 4. 共通の型（契約）

00 §4.9 のシグネチャ（`Datum` の変種、`ArrayDim`、`ArrayValue`、`binary.rs` の 3 関数）は**変えない**。ここはその具体化と、足すものを書く。足すもののうち他の章のファイルに触れるものは §11 に依頼として挙げた。

### 4.1 OID の定数と型の判定（`types/mod.rs`、`types/typeinfo.rs`）

```rust
// types/mod.rs の pub mod oid に足す（値は pg_type.dat と一致。実機で確認済み）
pub const BYTEA: Oid = 17;
pub const UUID: Oid = 2950;
pub const INT2VECTOR_ARRAY: Oid = 1006;      // _int2vector（値は NULL 専用）
pub const CSTRING_ARRAY: Oid = 1263;         // _cstring（typmodin の引数型。値は NULL 専用）
pub const RECORD_ARRAY: Oid = 2287;          // _record（値は NULL 専用）
pub const ANYELEMENT: Oid = 2283;
pub const ANYCOMPATIBLE: Oid = 5077;
pub const ANYCOMPATIBLEARRAY: Oid = 5078;
// ANYNONARRAY(2776)、ANYARRAY(2277) は既存。配列型の定数（INT4_ARRAY など）は、使う箇所が要るものだけ足す。
// 配列型の OID は array_type_of(elem) で引く（定数の表を作らない）

// types/typeinfo.rs ★（catalog::builtin::TYPES から引く。静的表だけを見るので DB に問い合わせない）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArrayKind {
    /// `_int4` など。値は `Datum::Array`
    Regular,
    /// `int2vector`（elem = 21）と `oidvector`（elem = 26）。値は `Int2Vector` / `OidVector`。添字の下限は 0
    Vector,
}
/// `typcategory = 'A'` かつ `typelem != 0` の型の種別（それ以外は None）
pub fn array_kind(type_oid: Oid) -> Option<ArrayKind>;
/// `get_element_type`: Regular と Vector の要素型
pub fn element_type(type_oid: Oid) -> Option<Oid>;
/// `get_array_type`: `typarray`（0 なら None。`pg_node_tree` や `unknown` は None）
pub fn array_type_of(elem: Oid) -> Option<Oid>;
/// 配列の要素にできる型か（§3.3 の表の型。配列型・疑似型・NULL 専用型・`int2vector` / `oidvector` は false）
pub fn is_array_element_type(elem: Oid) -> bool;
/// ユーザーテーブルの列に使えない配列型か（TY-D4。Regular の配列型）
pub fn is_array_type(type_oid: Oid) -> bool;

#[derive(Clone, Copy, Debug)]
pub struct ElemInfo { pub oid: Oid, pub typlen: i16, pub byval: bool, pub align: u8 /* 1, 2, 4, 8 */ }
pub fn elem_info(elem: Oid) -> Result<ElemInfo>;     // is_array_element_type でなければ 42704（could not find array type...）ではなく Error::internal
```

`SqlType`（`oid` + `typmod`）の**配列型の typmod は要素型の typmod**（PG と同じ。`varchar(3)[]` は `SqlType { oid: 1015, typmod: 7 }`、`numeric(10,2)[]` は `{ 1231, 655366 }`）。`format_type` は要素の表示名 + `[]`（`integer[]`、`character varying(3)[]`、`numeric(10,2)[]`、`text[]`、`"char"[]`、`name[]`、`int2vector`（そのまま）。【確認】）。`type_display_name` / `format_type_name` の配列の分岐は TY が `catalog/builtin/array.rs` に足す。

### 4.2 `TypeEnv` と設定（`types/mod.rs`）

M4-09 §3.4 の `TypeEnv`（`extra_float_digits`、`datetime`、`names`）に 1 項目足す。

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ByteaOutput { #[default] Hex, Escape }
pub struct TypeEnv<'a> {
    /* extra_float_digits, datetime, names（M4） */
    pub bytea_output: ByteaOutput,       // ★ TY-D6。Default は Hex
}
```

- `Settings::type_env(..)`（M4-09 P-4）が `bytea_output` を設定の現在値から作る。`settings.rs` に `bytea_output`（列挙、既定 `hex`）を足す（00 §3.6 は TY の持ち分）。値は大文字小文字を区別しない（`SET bytea_output = 'ESCAPE'` も可。実機で確認）。`SHOW bytea_output` は `hex` / `escape`。ParameterStatus は送らない（PG の `GUC_REPORT` ではない）。
- 構造体リテラルで `TypeEnv` を作っている箇所（`session` など）は `..Default::default()` を使う（F0 の作業）。

### 4.3 `ArrayValue`（`types/array.rs`）

00 §4.9 の定義（再掲）に、メソッドと定数を足す。

```rust
pub struct ArrayDim { pub len: i32, pub lbound: i32 }
pub struct ArrayValue { pub elem_type: Oid, pub dims: Vec<ArrayDim>, pub items: Vec<Datum> /* NULL 要素は Datum::Null */ }

pub const MAXDIM: usize = 6;                     // PostgreSQL の MAXDIM
pub const M5_MAX_NDIM: usize = 1;                // TY-D2
pub const MAX_ARRAY_SIZE: usize = 134_217_727;   // MaxAllocSize(0x3FFFFFFF) / sizeof(Datum)

impl ArrayValue {
    /// ndim = 0 の空の配列
    pub fn empty(elem_type: Oid) -> ArrayValue;
    /// 1 次元。items が空なら `empty`。件数が MAX_ARRAY_SIZE を超える、または lbound + len - 1 が i32 を超えたら 54000
    pub fn from_vec(elem_type: Oid, lbound: i32, items: Vec<Datum>) -> Result<ArrayValue>;
    pub fn ndim(&self) -> usize;                 // dims.len()
    pub fn nitems(&self) -> usize;               // items.len()
    /// dim は 1 始まり。範囲外の dim は None（`array_lower(a, 2)` が NULL になる）
    pub fn lower(&self, dim: i32) -> Option<i32>;
    pub fn upper(&self, dim: i32) -> Option<i32>;
    /// 1 次元の添字参照。範囲外は None、NULL 要素は Some(&Datum::Null)
    pub fn get1(&self, subscript: i32) -> Option<&Datum>;
    /// 不変条件の検査: dims.len() <= MAXDIM、各 len >= 1、Π len == items.len() <= MAX_ARRAY_SIZE、
    /// lbound + len が i32 に収まる、ndim == 0 なら items が空、items の各要素が Null か elem_type の変種で、配列でないこと。
    /// 違反は Error::internal（受信・ディスクの読み出しは別のエラー）
    pub fn check(&self) -> Result<()>;
    // カタログ列の読み書き（elem_type は 21 / 26 / 25 / 18）。下限 1。要素に NULL があれば to_* は Err
    pub fn from_int2s(v: &[i16]) -> ArrayValue;     pub fn to_int2s(&self) -> Result<Vec<i16>>;
    pub fn from_oids(v: &[u32]) -> ArrayValue;      pub fn to_oids(&self) -> Result<Vec<u32>>;
    pub fn from_texts<S: AsRef<str>>(v: &[S]) -> ArrayValue;   pub fn to_texts(&self) -> Result<Vec<String>>;
}

/// `anyarray` を取る関数の入口。`Array` はそのまま借り、`Int2Vector`（elem 21）と `OidVector`（elem 26）は
/// **下限 0** の ArrayValue に読み替える（TY-D1）。それ以外の変種は None
pub fn array_view(d: &Datum) -> Option<std::borrow::Cow<'_, ArrayValue>>;
```

入出力・符号化・比較（`types/array.rs`）:

```rust
/// 配列型 array_oid（Regular）のテキスト入力。elem_typmod は要素の typmod（`SqlType.typmod`）。要素の入力は
/// `io::input_text` + typmod の適用（代入の意味。`input_text_typed` と同じ）。区切りは ','（M5 の要素型はすべて）
pub fn array_in(s: &str, array_oid: Oid, elem_typmod: i32, env: &TypeEnv<'_>) -> Result<Datum>;
/// `{...}` / 下限が 1 でなければ `[lb:ub]={...}`。要素の出力は `io::output_text`
pub fn array_out(a: &ArrayValue, env: &TypeEnv<'_>) -> Result<String>;
/// 次の 2 つは §3.3
pub fn encode_array(a: &ArrayValue) -> Result<Vec<u8>>;
pub fn decode_array(payload: &[u8], expected_elem: Oid) -> Result<ArrayValue>;
/// バイナリ（§6.4）。要素の send / recv は `types::binary::{output_binary, input_binary}` に委ねる
/// `binary_send` / `binary_recv`（§4.6）の配列版。`ty` は配列型（`ty.oid` が配列の OID）。要素の型は `ty` の `typelem` から引く
pub fn binary_send(d: &Datum, ty: SqlType) -> Result<Vec<u8>>;          // array_send（2401）。`a: &ArrayValue` は `d` から取り出す
pub fn binary_recv(buf: &mut RecvBuf<'_>, ty: SqlType) -> Result<Datum>;  // array_recv（2400）。要素の typmod は −1。`array_in` と同じ検査（次元数 7 以上は 54000）
/// 要素ごとの typmod の適用（`ArrayCoerce` の評価と `input_text_typed`）
pub fn apply_elem_typmod(a: ArrayValue, elem_ty: SqlType, explicit: bool) -> Result<ArrayValue>;
/// PG の `array_cmp` と同じ全順序（要素を先頭から `cmp_datum`、NULL は非 NULL より後ろ、同じなら ndim・dims・lbound の順）。
/// Equal ⇔ PG の `array_eq`（dims・lbound・要素がすべて同じ。NULL どうしは等しい）
pub fn cmp_array(a: &ArrayValue, b: &ArrayValue) -> std::cmp::Ordering;
pub fn hash_array(a: &ArrayValue, state: &mut dyn std::hash::Hasher);   // dims・lbound・要素ごとの hash_datum（Null はタグ）
/// `=` の本体。elem_type が違えば 42804 `cannot compare arrays of different element types`
pub fn array_eq(a: &ArrayValue, b: &ArrayValue) -> Result<bool>;
```

### 4.4 式の木への追加（`expr/mod.rs`。F0 が 1 回だけ足す。§11 の依頼 1）

M4 の `Expr<C, Q>`（M4 00 §6.2）に次の変種を足す。M4 の規約 1（式の木を複製しない）に従い、Bound・論理・物理のすべての層に現れてよい（§6.4 の表の「それ以外」と同じ扱い）。

```rust
// ExprKind<C, Q> に足す
/// `ARRAY[e1, e2, ...]`。elems は elem_type にそろえ済み（解析が暗黙キャストを挟む）。空は `ARRAY[]::T[]`（elem_type だけが決まる）。
/// Expr.ty = 配列型（typmod は -1）。M5 は 1 次元だけ（入れ子の `ARRAY[[..]]` は解析が 0A000）
ArrayCtor { elem_type: Oid, elems: Vec<Expr<C, Q>> },
/// `a[i]`。indexes は int4 にそろえ済み（代入の暗黙変換。`a[1.5]` は 2）。M5 は len == 1 だけ作る（`a[1][2]` は 0A000）。
/// Expr.ty = 要素型。値の次元数と添字の数が違う、添字が NULL、範囲外はすべて NULL
ArraySubscript { array: Box<Expr<C, Q>>, indexes: Vec<Expr<C, Q>> },
/// `arr::T[]`、代入の暗黙変換。要素ごとに elem_method を適用してから elem_ty の typmod を適用する。
/// 要素型が同じなら elem_method = Binary。Expr.ty = 変換先の配列型（typmod は elem_ty.typmod）
ArrayCoerce { expr: Box<Expr<C, Q>>, elem_method: CastMethod, elem_ty: SqlType, explicit: bool },
/// `left op ANY (array)`（use_or = true）/ `left op ALL (array)`（false）。op は `elem_left op elem_right` の解決結果。
/// array の要素型が op の右の型に暗黙変換できる場合は解析が array を ArrayCoerce で包む
ScalarArrayOp { op: &'static BuiltinOperator, use_or: bool, left: Box<Expr<C, Q>>, array: Box<Expr<C, Q>> },

// SubLinkKind に足す
/// `ARRAY(SELECT ...)`。副問い合わせは 1 列。結果は要素型 = その列の型の配列（0 行は空の配列。NULL にならない）
Array,
```

- **走査・書き換え（`expr/walk.rs`）**: `ArrayCtor`・`ArraySubscript`・`ArrayCoerce`・`ScalarArrayOp` の子は `InList` と同じく `try_map` が再構築する（葉ではない）。`SubLink` の `Array` は `Any` / `All` と違い `test` を持たない（`test = None`）。
- **`ExprKind` を `match` している全箇所**（`InList` を扱う箇所をすべて当たる。見つけ方は `grep -rn "InList"`）: `expr/walk.rs`、`analyzer` の変換、`planner` の論理化・物理化・定数畳み込み（`ArrayCtor` の要素がすべて `Literal` なら畳む。`ScalarArrayOp` は左右が `Literal` なら畳む）、`deparse/`（`ARRAY[1, 2]`、`(a)[1]`、`x = ANY (ARRAY[1, 2])`、`ARRAY(SELECT ...)`。PG の綴り）、`explain/`、`executor/eval.rs`（`executor/eval_array.rs` を 1 行で呼ぶ）。**持ち主は各ファイルの担当で、TY は変換・評価の本体（§5.4、§5.5）だけを書く**（§11 の依頼 1）。
- 論理プランの `SubLink` の最適化（SEMI 結合への書き換えなど）は `Array` を対象にしない（M4 §6.4 のとおり `Any` / `Exists` だけ）。

### 4.5 `FnKind::Env`（`catalog/mod.rs`。§11 の依頼 2）

`array_to_string` や配列の `output_text`（日時・regclass の要素を出力する）は `TypeEnv` が要るが、`FnKind::Pure` は環境を持たない。M4-09 P-3 の `CastMethod::Env` と同じ形で足す。

```rust
pub enum FnKind {
    Pure(BuiltinFn), Context(..), Runtime(..), Set(SetFn),    // M4
    /// TypeEnv（DateStyle・TimeZone・bytea_output・regclass の名前）を使う。stable なので定数畳み込みしない
    Env(fn(&[Datum], &TypeEnv<'_>) -> Result<Datum>),
}
```

### 4.6 型ごとのバイナリ関数（`binary.rs` が呼ぶ実体）

00 §4.9 の `output_binary(d, ty)` / `input_binary(buf, ty)` は `types/binary.rs`（04 章）が型 OID で振り分ける。**各型のモジュールが公開する実体の形は 04 §4.4・00 §4.9 が定める 1 つの形に統一した**（レビュー対応 R-04。以前は 04・05・06 が三通りだった）。`ty.oid` ごとの対応表は §6.4。

```rust
// 例: types/bytea.rs（numeric.rs・bpchar.rs・array.rs・datetime.rs・interval.rs・time.rs・uuid.rs も同じ名前と形）
pub fn binary_send(d: &Datum, ty: SqlType) -> Result<Vec<u8>>;                // *_send。NULL は呼ばない。Datum の変種が違えば Error::internal
pub fn binary_recv(buf: &mut RecvBuf<'_>, ty: SqlType) -> Result<Datum>;      // *_recv。値の分だけ消費する。ty.typmod は −1（XQ-D11）
```

- **長さの過不足は型の `binary_recv` が判定しない**。読み足りなければ `RecvBuf` の `get_*` が `08P01 insufficient data left in message` を返し、`input_binary` が `binary_recv` の後に `RecvBuf::finish()` を呼んで余りがあれば `22P03 incorrect binary data format`（04 が ` in bind parameter N` を付ける）にする。値の中身の不正（numeric の符号、配列の次元など）は型の `binary_recv` が `22P03` と固有の文言で返す（§6.4 の表）。
- **typmod**: `input_binary` が返す `Datum` に typmod は適用しない（00 §4.9）。`binary_recv` に渡す `ty.typmod` は常に −1 なので、numeric・bpchar・varchar の `binary_recv` は丸めも埋めもしない（PG の `*_recv` に typmod = −1 を渡すのと同じ）。長さ・精度の検査と丸めは計画の `CoerceTypmod`（代入の文脈）が行う（XQ-D11、TY-D9）。
- 配列は `binary_recv` の中で要素ごとに `codec(elem_oid)`（04 §4.4 の `BinaryCodec`）を引き、要素の本体を `RecvBuf::get_bytes(len)` で切り出した別の `RecvBuf` で再帰して読み、要素ごとに `finish()` する。

### 4.7 ポリモーフィック型の解決（`analyzer/polymorphic.rs`）

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolyKind { Element /* anyelement */, Array /* anyarray */, NonArray /* anynonarray */, Compatible, CompatibleArray }
pub fn poly_kind(declared: Oid) -> Option<PolyKind>;

/// 候補の絞り込み用（PG の `check_generic_type_consistency`）。actual に UNKNOWN（型の決まらないリテラル・パラメータ）を含んでよい。
/// false なら候補から外れる（最終的に 42883 になる）
pub fn is_consistent(declared: &[Oid], actual: &[Oid]) -> bool;

pub struct PolyResolution {
    /// 宣言型の多相の位置を具体型に置き換えたもの。呼び出し側がこの型へ引数を暗黙キャストする（UNKNOWN のリテラルは入力関数で）
    pub arg_types: Vec<Oid>,
    /// 宣言の戻り型を具体型にしたもの
    pub result: Oid,
}
/// 確定（PG の `enforce_generic_type_consistency`）。規則は §5.4
pub fn resolve(declared: &[Oid], declared_ret: Oid, actual: &[Oid]) -> Result<PolyResolution>;
```

### 4.8 型の枠組みの表（`catalog/builtin/type_io.rs`）

```rust
/// pg_type の recv / send / typmodin / typmodout / typsubscript / typanalyze（TY-D11）。
/// 各 (名前, pg_proc.oid)。0 は列が 0。`rows.rs` が pg_type の行を作るときに type_by_oid(oid) と並べて引く
#[derive(Debug)]
pub struct TypeProcs {
    pub type_oid: Oid,
    pub recv: (&'static str, Oid), pub send: (&'static str, Oid),
    pub modin: (&'static str, Oid), pub modout: (&'static str, Oid),
    pub subscript: (&'static str, Oid), pub analyze: (&'static str, Oid),
}
pub static TYPE_PROCS: &[TypeProcs];
pub fn type_procs(type_oid: Oid) -> Option<&'static TypeProcs>;
/// 配列型の行を要素型の行から作る（§5.1）。`TYPES` に静的に並べる代わりにこの関数が `Vec<BuiltinType>` を返し、
/// `builtin::type_by_oid` / `type_by_name` が引く（`OnceLock`）
pub fn array_type_rows() -> &'static [BuiltinType];
```

### 4.9 typmod の入出力（`types/typmod.rs`。M4 が作った関数に足す）

```rust
/// `typmodout`: `numeric` → "(10,2)"、bpchar / varchar → "(3)"、typmod が無効なら ""（PG の `*typmodout` と同じ。実機で
/// numerictypmodout(655366) = "(10,2)"、bpchartypmodout(7) = "(3)"、bpchartypmodout(4) = ""、numerictypmodout(-1) = ""）
pub fn typmod_out(type_oid: Oid, typmod: i32) -> String;
/// 配列型の typmod は要素型の typmod。takes_typmod は配列型で要素型の takes_typmod を返す
pub fn takes_typmod(type_oid: Oid) -> bool;           // M4 の関数を拡張
pub fn apply_typmod(d: Datum, ty: SqlType, explicit: bool) -> Result<Datum>;   // Array は apply_elem_typmod へ
```

---

## 5. 処理の流れ

### 5.1 型の枠組み（TY-1）

#### 5.1.1 新しい型を足すときの 8 点チェックリスト

`bytea`・`uuid`・配列・`interval`・`time` のすべてがこの表に従う。TY-1 は表を `catalog/builtin/type_io.rs` の単体テスト（`TYPES` の全行について、該当する項目の漏れを検査する）にする。

| # | 項目 | 置き場所 | 規則 |
|---|---|---|---|
| 1 | OID 定数と `SqlType` の定数 | `types/mod.rs` | 値は `pg_type.dat`。実機の `SELECT oid, typname FROM pg_type` で確かめる |
| 2 | `Datum` の変種と入出力 | `types/datum.rs`、`types/io.rs`、型のモジュール | `input_text` / `output_text` / `input_is_eager`（日時・regclass・配列（要素が eager でないもの）は false。M4-09 D-9-3） |
| 3 | `pg_type` の行と `TYPE_PROCS` | `catalog/builtin/*.rs`、`type_io.rs` | `ty(..)` の 16 引数 + `TYPE_PROCS` の recv / send / typmodin / typmodout / typsubscript / typanalyze。`typarray` は PG の値。配列型の行を足す（§5.1.2） |
| 4 | `pg_proc` の行 | `PROCS`（`tools/gen_procs.sh` が生成。M4-09 §3.8） | 入出力・recv・send・typmodin・typmodout・キャスト関数・演算子の実体・`FUNCTIONS` の OID すべて。PG の OID のまま |
| 5 | キャストの行 | `CASTS` | PG の `pg_cast` と同じ行（castfunc の OID、context、method）。長さ強制（typmod）の行は入れない（M4-09 §3.6）。文字列との変換は自動 I/O 変換に任せる（TY-D5） |
| 6 | 演算子・関数の行 | `OPERATORS`・`OPERATOR_META`・`FUNCTIONS` | `oprcode`・`com`・`negate` は PG の値 |
| 7 | 比較・ハッシュ・opclass | `cmp_datum`、`hash_datum`、`catalog/opclass.rs` | §5.1.4 |
| 8 | ディスク形式と `AttrDesc` | `storage/heap/tuple.rs` の `Kind`、`TYPES` の typlen / align | §3。`Kind` の追加は 1 型 1 分岐。`kind_of` の `NullOnly` から外す |

#### 5.1.2 `pg_type` の行

**新しい基本型の行**（`ty(oid, name, typlen, typbyval, typtype, category, preferred, delim, relid, elem, array_oid, input, output, align, storage, collation)`。値は実機の `pg_type`）:

```rust
ty(17,   "bytea",              -1, false, 'b', 'U', false, ',', 0, 0, 1001, ("byteain", 1244),   ("byteaout", 31),      'i', 'x', 0),
ty(2950, "uuid",               16, false, 'b', 'U', false, ',', 0, 0, 2951, ("uuid_in", 2952),   ("uuid_out", 2953),    'c', 'p', 0),
ty(2283, "anyelement",          4, true,  'p', 'P', false, ',', 0, 0, 0,    ("anyelement_in", 2312), ("anyelement_out", 2313), 'i', 'p', 0),
ty(5077, "anycompatible",       4, true,  'p', 'P', false, ',', 0, 0, 0,    ("anycompatible_in", 5086), ("anycompatible_out", 5087), 'i', 'p', 0),
ty(5078, "anycompatiblearray", -1, false, 'p', 'P', false, ',', 0, 0, 0,    ("anycompatiblearray_in", 5088), ("anycompatiblearray_out", 5089), 'd', 'x', 0),
```

`anyarray`（2277）と `anynonarray`（2776）は M2 の行のまま。`bytea` と `uuid` を `is_supported_type` に足し、`unknown` を除く M2 の `KNOWN_UNSUPPORTED_TYPES`（`analyzer/ddl.rs`）から外す。

**配列型の行（`array_type_rows()`）**。要素型 E の行から次の規則で作る（実機の `pg_type` で、下の 31 行すべての OID と、代表 20 行の属性を確認）:

| 列 | 値 |
|---|---|
| `typname` | `_` + E の `typname`（`_int4`、`_bpchar`） |
| `oid` | E の `typarray`（下の表） |
| `typlen` / `typbyval` / `typtype` | -1 / f / `b`（`_record` だけ `p`） |
| `typcategory` / `typispreferred` / `typdelim` | `A`（`_record` は `P`）/ f / `,` |
| `typelem` / `typarray` | E / 0 |
| `typinput` / `typoutput` | `array_in`（750）/ `array_out`（751） |
| `typreceive` / `typsend` | `array_recv`（2400）/ `array_send`（2401） |
| `typmodin` / `typmodout` | E の値（E が numeric なら 2917 / 2918）。E が持たなければ 0 |
| `typsubscript` / `typanalyze` | `array_subscript_handler`（6179）/ `array_typanalyze`（3816） |
| `typalign` | E の `typalign` が `d` なら `d`、それ以外は `i`（`_int2` も `i`、`_name` も `i`。`_record` は E が `d` なので `d`） |
| `typstorage` | `x` |
| `typcollation` | E の `typcollation`（`_text` = 100、`_name` = 950、数値などは 0） |

| 要素 | 配列 OID | 要素 | 配列 OID | 要素 | 配列 OID |
|---|---|---|---|---|---|
| `bool` | 1000 | `int2vector` | 1006 | `float4` | 1021 |
| `bytea` | 1001 | `int4` | 1007 | `float8` | 1022 |
| `"char"` | 1002 | `regproc` | 1008 | `oid` | 1028 |
| `name` | 1003 | `text` | 1009 | `aclitem` | 1034 |
| `int2` | 1005 | `tid` | 1010 | `timestamp` | 1115 |
| `xid` | 1011 | `cid` | 1012 | `date` | 1182 |
| `oidvector` | 1013 | `bpchar` | 1014 | `time` | 1183 |
| `varchar` | 1015 | `int8` | 1016 | `timestamptz` | 1185 |
| `interval` | 1187 | `numeric` | 1231 | `regclass` | 2210 |
| `regtype` | 2211 | `uuid` | 2951 | `cstring` | 1263 |
| `record` | 2287 | | | | |

- 要素型の行が `TYPES` にあるものについてだけ作る（`interval`・`time` は TD が行を足したら自動で増える）。`pg_node_tree` と `unknown` は PG でも `typarray = 0`。
- **値を持てる配列型**は `is_array_element_type` の要素の配列だけ。`_int2vector` `_oidvector` `_aclitem` `_cstring` `_record` は行だけで、値は NULL 専用（`is_null_only_type`）。
- M2 の 5 つ（`_aclitem` `_text` `_int2` `_oid` `_char`）と `_oidvector` は、この関数の結果と同じ値になるので `TYPES` の静的な行は削除して一本化する。M2-Q13 の差（2）と、`pg_type.slt` の「`typarray` は配列型を持つものに限る」という制限が消える（`catalog/pg_type.slt` の該当コメントを TY が直す）。

**`TYPE_PROCS`**: 型ごとの recv / send / typmodin / typmodout / typsubscript。recv と send は §6.4 の表の OID。typmodin / typmodout は `numeric`（2917 / 2918）、`bpchar`（2913 / 2914）、`varchar`（2915 / 2916）、`timestamp`（2905 / 2906）、`timestamptz`（2907 / 2908）、`time`（2909 / 2910）、`interval`（2903 / 2904）、配列型は要素型に従う。`typsubscript` は配列型と `int2vector` / `oidvector` が `array_subscript_handler`（6179）、`name` が `raw_array_subscript_handler`（6180。`name[1]` の添字は M5 では使えない: `42804`）、他は 0。`typanalyze` は配列型だけ `array_typanalyze`（3816）、他は 0。

**`pg_proc` の行**（`PROCS`。`tools/gen_procs.sh` の OID の一覧に足して生成する）: 上の recv / send（§6.4）、`typmodin` / `typmodout`（`bpchartypmodin` 2913 など。引数 `cstring[]` / `integer`）、`array_in` 750、`array_out` 751、`array_recv` 2400、`array_send` 2401、`array_typanalyze` 3816（`internal → bool`）、`array_subscript_handler` 6179（`internal → internal`）、`bytea` / `uuid` / ポリモーフィック型の入出力（`byteain` 1244、`byteaout` 31、`uuid_in` 2952、`uuid_out` 2953、`anyelement_in` 2312、`anyelement_out` 2313、`anycompatible_in` 5086、`anycompatible_out` 5087、`anycompatiblearray_in` 5088、`anycompatiblearray_out` 5089）、§6 の演算子・関数の実体。`internal` / `cstring` の型の行は M2 で入っている。`anycompatiblearray_recv` 5090 / `_send` 5091 と `anyarray_recv` 2502 / `anyarray_send` 2503 は、型の行が参照するので行だけ入れる（本体は `0A000`）。

#### 5.1.3 キャスト

| 変換 | 扱い |
|---|---|
| bytea ⇄ 文字列型、uuid ⇄ 文字列型 | `pg_cast` に行なし。自動 I/O 変換（明示。変換先が文字列型なら代入）（TY-D5）。`bytea → text` は `byteaout`（`bytea_output` に従う）、`text → bytea` は `byteain`。uuid も同様。`'x'::text::uuid` は `22P02` |
| bytea ⇄ 数値・bool・日時 | なし（`42846 cannot cast type integer to bytea`。実機と同じ） |
| 配列 ⇄ 配列 | `pg_cast` に行なし。要素の変換経路があれば `ArrayCoerce`（§5.4.4）。実機: `ARRAY[1,2]::text[]`、`'{1,2}'::int4[]::int8[]` は通り、`'{1,2}'::int[] = '{1,2}'::int2[]` は `42883`（要素型が違う配列の `=` は演算子が見つからない） |
| `int2vector` → `int2[]`、`oidvector` → `oid[]` | `Cast { method: Function(vector_to_array) }`、暗黙。**`pg_cast` に行は出さない**（PG にも行がない。型の解決の経路が配列の要素の同一性から決める）。結果は下限 0 の配列 |
| `anyarray` を取る関数の引数 | `can_coerce` が Regular / Vector の配列型を受ける（M2 の `category == 'A'` の規則を維持） |
| numeric・bpchar の関数形式のキャスト | §5.2 |

#### 5.1.4 `cmp_datum`・`hash_datum`・opclass

`cmp_datum`（`types/datum.rs`）に足す分岐（M4-09 §7.1 に続けて）:

```rust
(Bytea(a), Bytea(b)) => a.cmp(b),                    // memcmp。先頭から比べ、共通部分が同じなら短い方が小さい
(Uuid(a), Uuid(b)) => a.cmp(b),                      // 16 バイトの辞書順
(Array(a), Array(b)) => cmp_array(a, b),             // §4.3
```

- `rank()` は末尾に足す（M4-09 §3.1 の `Int2Vector` = 18 の次: `Bytea` = 19、`Uuid` = 20、`Array` = 21、`Time` / `Interval` は TD）。M4 の `Int4Array` の順位は `Array` に引き継ぐ。
- `hash_datum`（M4-09 §7.2 の表に足す）: `Bytea` = タグ + 長さ + バイト列、`Uuid` = タグ + 16 バイト、`Array` = タグ + `hash_array`（ndim・各 dim・各 lbound・要素ごとの `hash_datum`。`Null` 要素はタグだけ）。**不変条件（`cmp_datum` が Equal なら同じハッシュ）**は、`cmp_array` の Equal が dims と lbound を含む（TY-D1 の `int2vector` 由来の下限 0 の配列と下限 1 の配列は等しくない）ことと整合する。
- `Datum::as_bytes() -> Option<&[u8]>`（`Bytea` のとき）、`Datum::as_uuid()`、`Datum::as_array() -> Option<&ArrayValue>` を足す（`as_str` の仲間）。

B+Tree の opclass（`catalog/opclass.rs` の静的な表。M4 00 §11.3 の `OpFamily` / `OpClass` / `AmOp` / `AmProc`）に足す行。**OID は PG17.11 の `pg_opfamily` / `pg_opclass` / `pg_amop` / `pg_amproc` を実機で確かめた値**（opfamily の OID は `pg_opfamily.dat` に `oid => '428'`（bytea の btree）・`'2968'`（uuid の btree）と明記されている【確認】。opclass の OID は `pg_opclass.dat` に `oid` の指定がなく、genbki が連番で割り当てる 10000 台。PG17.11 の値で、マイナー版をまたいで同じかは未検証。M5-TY-Q6）:

| opfamily（btree） | OID | opclass | OID | 入力型 | default |
|---|---|---|---|---|---|
| `bytea_ops` | 428 | `bytea_ops` | 10006 | bytea (17) | t |
| `uuid_ops` | 2968 | `uuid_ops` | 10065 | uuid (2950) | t |

| family | strategy | 演算子 | 演算子の OID | support 1（比較関数）の `proc_oid` |
|---|---|---|---|---|
| 428（bytea） | 1 `<` / 2 `<=` / 3 `=` / 4 `>=` / 5 `>` | `<` `<=` `=` `>=` `>` | 1957 / 1958 / 1955 / 1960 / 1959 | 1954（`byteacmp`）。`cmp` = `cmp_datum` |
| 2968（uuid） | 同上 | 同上 | 2974 / 2976 / 2972 / 2977 / 2975 | 2960（`uuid_cmp`）。`cmp` = `cmp_datum` |

- PG は両 family に support 2（`bytea_sortsupport` 3331 / `uuid_sortsupport` 3300）と support 4（`btequalimage` 5051）の行も持つ。M4 の `AmProc` は support 1 だけなので**入れない**（`pg_amproc` の行数が PG と違う既知の差）。ハッシュの opfamily（`bytea_ops` 2223、`uuid_ops` 2969）は入れない（M4 にハッシュインデックスがない）。
- `default_opclass(oid::BYTEA)` / `default_opclass(oid::UUID)` が引けること、`CREATE INDEX ... (c bytea_ops)`・PRIMARY KEY が通ることを `catalog/opclass.rs` の単体テストと `tests/slt/m5/types/bytea.slt` / `uuid.slt` で確かめる。
- 配列と、`time`・`interval`（TD）以外の型の opclass は作らない。配列の列への `CREATE INDEX` は列の型が作れない（TY-D4）ので起きない。

#### 5.1.5 関数形式のキャスト（`int4(numeric)`、`text(bpchar)` など）

PG は `CASTS` の `castfunc` が 0 でない行の関数を、型名と同じ名前（変換先の型名）で呼べる（`int4(1.5)`、`float8(1.5::numeric)`、`bpchar('a'::name)`）。M4-09 は `date()` `timestamp()` `timestamptz()` だけを `FUNCTIONS` に入れた。**TY-1 は一般の規則を足す**: `CASTS` の行のうち `func_oid != 0` で、`PROCS` の同じ OID の行の `name` が変換先の型名（`pg_proc.proname`）のものを、`FUNCTIONS` が持っていなければ `FnKind::Pure(CastMethod::Function の関数)`（`Env` なら `FnKind::Env`）、strict、`volatility` は `PROCS` どおりで自動的に足す（`catalog/builtin/mod.rs` の初期化で表を作る。`builtin_hash` に含める）。numeric 関係の行（`numeric(int4)` 1740、`numeric(int8)` 1781、`numeric(int2)` 1782、`numeric(float4)` 1742、`numeric(float8)` 1743、`int2(numeric)` 1783、`int4(numeric)` 1744、`int8(numeric)` 1779、`float4(numeric)` 1745、`float8(numeric)` 1746）と bpchar（`bpchar(name)` 408、`name(bpchar)` 409、`text(bpchar)` 401、`bpchar("char")` 860、`"char"(bpchar)` 944）がこれで入る。

### 5.2 numeric と bpchar の M4 実装との差の洗い出しと穴埋め（TY-2、TY-3）

M4 の 09 章は numeric の関数を `abs sign round trunc ceil ceiling floor mod div scale`、bpchar の関数を `length character_length char_length octet_length` に絞った（M4-09 §1.1）。M4 の実装が確定した時点で、**次の手順で差を機械的に洗い出す**。PG と yuzhu の OID が同じ（00 §3.2）ことを使い、OID の集合の差を取る。

**手順**:

1. **PG 側の一覧を作る**（一度だけ。`tests/tools/typegap/pg17_<type>.tsv` に置く。型 T ごと）。実機で次の問い合わせを流す（`T` は 1700（numeric）または 1042（bpchar））:

```sql
SELECT 'proc' AS kind, p.oid::text AS id, p.proname AS name, p.proargtypes::text AS args, p.prorettype::text AS ret
  FROM pg_proc p
 WHERE p.prorettype = 1700 OR p.proargtypes::text ~ '(^| )1700( |$)'
UNION ALL
SELECT 'oper', o.oid::text, o.oprname, o.oprleft::text || ' ' || o.oprright::text, o.oprresult::text
  FROM pg_operator o WHERE 1700 IN (o.oprleft, o.oprright, o.oprresult)
UNION ALL
SELECT 'cast', c.castsource::text || '>' || c.casttarget::text, c.castcontext::text || c.castmethod::text, c.castfunc::text, ''
  FROM pg_cast c WHERE 1700 IN (c.castsource, c.casttarget)
UNION ALL
SELECT 'amop', a.amopfamily::text || ':' || a.amopstrategy::text, a.amopopr::text, a.amoplefttype::text || ' ' || a.amoprighttype::text, ''
  FROM pg_amop a WHERE a.amoplefttype = 1700 AND a.amoprighttype = 1700
ORDER BY 1, 2;
```

   `pg_cast.oid` は yuzhu で PG と違う（M2-Q13 の差（7））ので、キャストは `(castsource, casttarget)` を鍵にする。`pg_proc.proargtypes::text ~ ...` は yuzhu でも動く（`oidvector` の出力は空白区切り、`~` は M4 の正規表現）。

2. **yuzhu 側で同じ問い合わせを流す**（`tests/tools/typegap/gap.sh --target yuzhu`）。`yuzhu` の `pg_proc` は M2 の方針で「どの表からも参照されない行は載せない」（M2 §6.8.5）ので、**呼べる関数がそのまま見える**。
3. **差を取る**: `kind` と `id` の集合で PG にあって yuzhu にない行を出す（`comm -23`）。
4. **分類する**。各行を次のどれかにして `tests/tools/typegap/expected_missing_<type>.txt` に理由つきで記録する:

| 分類 | 規則 | 扱い |
|---|---|---|
| 必須 | psql・ORM・pg_dump の問い合わせ（pg-compat §2.4 の F1〜F6、`\d`）か M4 / M5 の共有テストが使う | TY-2 / TY-3 で実装 |
| 安い | 既存の `yuzhu-numeric` の公開 API で書ける純粋関数・演算子（0.1 日以内） | 実装 |
| M6 | 未実装の基盤（`sqrt` 系の rscale の規則、`to_char` の書式言語、分散系の集約の `AggKind`、ハッシュインデックス、`pattern_ops`）が要る | 行だけ入れて `0A000`（TY-D15）、または行を作らず `42883`。**既知の差**に記録 |
| 不要 | 内部の補助。`proname` が `*_support`、`*_sortsupport`、`*_equalimage`、`*_accum*`、`*_aggstate*`、`*_poly_*`、`hash_*`、`*_extended`、`in_range`、`int8_avg`、`int8_sum`、`numeric_inc`、`numeric_larger` / `smaller`、`bpchar_larger` / `smaller`、`*_cmp` の内部名など | 入れない（呼べなくても困らない） |

5. **合格条件**: 手順 3 の差の残りがすべて「M6」と「不要」に分類されており、`expected_missing_<type>.txt` と一致すること（`gap.sh --check` が差を出したら CI を落とす。新しい行が M4 / M5 で増えたら一覧を更新する）。

**設計時点の結果**（PG17 の `pg_proc` / `pg_operator` / `pg_cast` と M4-09 の表の突き合わせ。M4 の実装が仕様どおりという前提で、実装後に手順をもう一度流して確かめる）:

*numeric（TY-2。合計約 1.5 日）*

| 行 | OID | 分類 | 内容 |
|---|---|---|---|
| `gcd(numeric, numeric)` | 5048 | 安い | 非負にして `checked_rem` のユークリッド互除法。結果の scale は max。NaN と ±Infinity は NaN（実機: `gcd('NaN',1)` = `NaN`、`gcd('Infinity',1)` = `NaN`、`gcd(1.5, 3)` = `1.5`、`gcd(0,0)` = `0`、`gcd(-12,18)` = `6`） |
| `lcm(numeric, numeric)` | 5049 | 安い | どちらかが 0 なら 0。それ以外は `abs(a / gcd(a,b) * b)`（実機: `lcm(0,5)` = `0`、`lcm(1e100, 3)` = `3` の後に 0 が 100 個）。NaN / Infinity は NaN |
| `min_scale(numeric)` | 5042 | 安い | 末尾のゼロを除いた scale。NaN / Infinity は NULL、0 は 0（実機: `min_scale(1.500)` = `1`、`min_scale(0.00)` = `0`） |
| `trim_scale(numeric)` | 5043 | 安い | `min_scale` まで `trunc`。NaN / Infinity はそのまま（実機: `trim_scale(0.0010)` = `0.001`、`trim_scale(100.00)` = `100`） |
| `width_bucket(numeric, numeric, numeric, int4)` | 2170 | 安い | 下の式。`2201G`（`count must be greater than zero`、`operand, lower bound, and upper bound cannot be NaN`、`lower bound cannot equal upper bound`、`lower and upper bounds must be finite`）、結果が int4 を超えたら `22003 integer out of range` |
| `factorial(int8)` | 1376 | 安い | `numeric` を返す。負は `22003 factorial of a negative number is undefined`、n >= 32178 は `22003 value overflows numeric format`（実機: `factorial(32177)` は 131068 桁で成功、32178 でエラー） |
| `@`（前置、numeric） | 1763（oprcode 1704 `numeric_abs`） | 安い | `abs` と同じ |
| `int2(numeric)` ほか関数形式のキャスト 10 行 | 1783 1744 1779 1745 1746 1782 1740 1781 1742 1743 | 安い | §5.1.5 の規則で自動 |
| `sqrt` 1730、`exp` 1732、`ln` 1734、`log(numeric)` 1741、`log(numeric, numeric)` 1736、`log10` 1481、`power` 2169、`pow` 1738 | 左のとおり | M6（行だけ） | 本体は `0A000 numeric <name> is not supported yet`（TY-D15） |
| `to_char(numeric, text)` 1772、`to_number(text, text)` 1696、`random(numeric, numeric)` 6341、`generate_series(numeric, ...)`、`variance` / `stddev` 系の集約 | | M6 | 行を作らない（`42883`）。既知の差 |
| `numeric_inc` 1764、`hash_numeric*`、`numeric(numeric, int4)` 1703、`int8_avg`、`int8_sum`、`in_range`、`numeric_support` | | 不要 | |

`width_bucket(op, b1, b2, count)`（PG:numeric.c の `width_bucket_numeric`。`b1 < b2` のとき）: `op < b1` → 0、`op >= b2` → `count + 1`（`count == i32::MAX` なら 22003）、それ以外は `floor(count * (op - b1) / (b2 - b1)) + 1`。`b1 > b2` は対称（`op > b1` → 0、`op <= b2` → `count + 1`、それ以外は `floor(count * (b1 - op) / (b1 - b2)) + 1`）。除算の scale は `select_div_scale` ではなく、PG は内部で十分な桁（`NUMERIC_MIN_SIG_DIGITS` 以上）で計算して floor するので、**`checked_mul` と `checked_div` で計算して `floor` すれば同じ整数になる**（未検証: 境界値は差分コーパスで確かめる）。

*bpchar（TY-3。約 0.5 日）*

| 行 | OID | 分類 | 内容 |
|---|---|---|---|
| 関数形式のキャスト 5 行（`bpchar(name)` 408、`name(bpchar)` 409、`text(bpchar)` 401、`bpchar("char")` 860、`"char"(bpchar)` 944） | | 安い | §5.1.5 の規則で自動 |
| `bpcharcmp(bpchar, bpchar)` | 1078 | 安い | `cmp_datum` を -1 / 0 / 1 に |
| `bpchar` の比較演算子の実体（`bpchareq` 1048 ほか） | 1048 1053 1049 1050 1051 1052 | 安い | `PROCS` には M4 が入れる。`FUNCTIONS`（呼べる）にも入れる（`bpchareq('a','a ')`）。優先度は低い |
| `bpchar_pattern_lt` 2174 `_le` 2175 `_ge` 2177 `_gt` 2178、`btbpchar_pattern_cmp` 2180、`~<~` 2326 `~<=~` 2327 `~>=~` 2329 `~>~` 2330 の演算子、`bpchar_pattern_ops` | 左のとおり | M6 | 行を作らない |
| `bpcharsend` 2431 / `bpcharrecv` 2430 | | TY-6 | §6.4 |
| `typmodin` 2913 / `typmodout` 2914 | | TY-1 | `typmod_out` |

さらに、**M4 の実装そのものの差**を、行の有無ではなく動作として洗い出す。TY-7 の差分コーパス（§7.2 の `numeric_gap` と `bpchar_gap`）が担う。M4-09 の仕様のうち特に確かめる点: `char(n)` 列への代入（22001 と空白だけの超過の切り捨て）、`'é'::char(3)` の `octet_length` = 4、`char = text` が `text = text`（rtrim）に解決されること、`LIKE` / `~` がパディング込みであること（`'ab '::char(3) ~ 'b$'` は偽）、`||` で空白が消えること（`'a'::char(3) || 'b'::char(3)` = `ab`）、`concat('a'::char(3), 'b')` は `a  b`（出力関数を通すので空白が残る）、`min` / `max` が空白を保持して返すこと、`format_type(1042, 5)` = `character(1)`、`'a'::bpchar(0)` の `22023`。numeric は小数リテラルの型、`numeric(p,s)` の丸めと `22003` の DETAIL、`NaN` / `Infinity` と typmod、`float → numeric` の 15 桁、`numeric → int` の 0 から遠い方への丸め。

### 5.3 配列のテキスト入出力（TY-5）

#### 5.3.1 入力（`array_in`）

PG:src/backend/utils/adt/arrayfuncs.c の `array_in` / `ReadArrayDimensions` / `ReadArrayStr` / `ReadArrayToken`（17.11）の移植。**要素の入力は読み進めながら行う**（PG と同じ。`{1,a,` は構文エラーより先に `22P02 invalid input syntax for type integer: "a"` になる）。

```text
array_in(s):
  1. 先頭の空白を飛ばし、"[" が続く間、次元の指定を読む:  [n]（下限 1）または [m:n]
       数値は ReadDimensionInt（符号 +/- と数字だけ。先頭の空白は不可。i32 の範囲外は 54000 "array bound is out of integer range"）
       数字がなければ 22P02 malformed array literal DETAIL "[" must introduce explicitly-specified array dimensions.
       ':' の後に数字がなければ DETAIL Missing array dimension value. / "]" がなければ DETAIL Missing "]" after array dimensions.
       ub < lb は 2202E "upper bound cannot be less than lower bound"、ub = i32::MAX は 54000 "array upper bound is too large: 2147483647"、
       ub - lb + 1 が i32 を超えたら 54000 "array size exceeds the maximum allowed (134217727)"。次元が 7 個目は 54000 "number of array dimensions exceeds the maximum allowed (6)"（**テキスト入力 `array_in` は次元数を含まない**。【実機】PG17.11 の `'{{{{{{{1}}}}}}}'::int[]`。`ARRAY[[[[[[[1]]]]]]]` の構築と `array_recv` は `number of array dimensions (7) exceeds the maximum allowed (6)` と次元数が入る。経路ごとに違うのが PG の実際で、章内の食い違いではない。レビュー対応 R-30）
  2. 次元の指定があれば "=" が必須（DETAIL Missing "=" after array dimensions.）、空白を飛ばして "{" が必須（DETAIL Array contents must start with "{".）。
     指定がなければ "{" で始まること（DETAIL Array value must start with "{" or dimension information.）
  3. ReadArrayStr: トークン列を読む（下の状態機械）。要素ごとに input_text + apply_typmod
  4. 閉じ括弧の後は空白だけ（DETAIL Junk after closing right brace.）
  5. 要素 0 件 → ArrayValue::empty（'{}'、'{{}}' も空の配列。次元の指定があっても）
  6. ndim > M5_MAX_NDIM → 0A000 "multidimensional arrays are not supported yet"（構文エラーの検出がすべて済んだ後）
  （次元の指定があるのに数えた次元・要素数と合わなければ、状態機械が 22P02 DETAIL Specified array dimensions do not match array contents. を返す）
```

トークン（`ReadArrayToken`。空白は `scanner_isspace` = スペース、`\t`、`\n`、`\r`、`\v`、`\f`）:

| 先頭 | トークン | 規則 |
|---|---|---|
| `{` / `}` | LEVEL_START / LEVEL_END | |
| 区切り（`,`） | DELIM | |
| `"` | 引用符つき要素 | `\` は次の 1 文字をそのまま取り込む。閉じ `"` の後は、空白を飛ばして区切り・`}`・`{` のどれかが来ること（それ以外は DETAIL `Incorrectly quoted array element.`）。**引用符つきは決して NULL にならない** |
| それ以外 | 引用符なしの要素 | 区切りか `}` まで。**末尾の空白は除く**（途中の空白は残す: `{ a  b }` は `a  b`）。`\` は次の 1 文字をそのまま取り込み、その文字は空白でも末尾の除去の対象にしない。`"` を含むと `Incorrectly quoted array element.`、`{` を含むと `Unexpected "{" character.`。**エスケープなしで `NULL`（大文字小文字を問わない）なら NULL 要素** |
| 文字列の終わり | エラー | DETAIL `Unexpected end of input.` |

状態機械（`nest_level`、`ndim`（読みながら増える）、`nelems[]`、`expect_delim`、`ndim_frozen`）:

- `LEVEL_START`: `expect_delim` なら DETAIL `Unexpected "{" character.`。7 段目は 54000。`nest_level++`、`nest_level > ndim` なら（`ndim_frozen` なら次元不一致、でなければ）`ndim = nest_level`。
- `LEVEL_END`: 直前の要素のあとに区切りがなければ（`nelems > 0 && !expect_delim`）DETAIL `Unexpected "}" character.`（空の `{}` は可）。`nest_level--`。外側の `nelems` を 1 増やす。最初に閉じたサブ配列の長さを `dim[nest_level]` に記録し、以降は一致を要求（不一致は DETAIL `Multidimensional arrays must have sub-arrays with matching dimensions.`、次元の指定があれば `Specified array dimensions do not match array contents.`）。`expect_delim = true`。
- `DELIM`: `!expect_delim` なら DETAIL `Unexpected "," character.`（`{1,,2}`）。`expect_delim = false`。
- `ELEM` / `ELEM_NULL`: `expect_delim` なら DETAIL `Unexpected array element.`。要素を入力する。`ndim_frozen = true`。`nest_level != ndim` なら次元不一致。`nelems[nest_level-1]++`。`expect_delim = true`。

**入力の例**（実機で確認。【確認】）:

| 入力 → 型 | 結果 |
|---|---|
| `'{1,2,NULL}'::int4[]` | `{1,2,NULL}` |
| `'{ 1 , 2 }'::int4[]`、`'{"1",2}'::int4[]` | `{1,2}` |
| `'{a b}'::text[]`、`'{ a  b }'::text[]` | `{"a b"}`、`{"a  b"}`（出力は空白を含むので引用符つき） |
| `'{NULL}'::text[]`、`'{null}'::text[]`、`'{"null"}'::text[]` | `{NULL}`、`{NULL}`、`{"null"}` |
| `'[0:2]={1,2,3}'::int4[]` | `[0:2]={1,2,3}`（下限が 1 でないので次元つきで出力） |
| `'{}'::int4[]` | `{}`（`array_length` は NULL、`cardinality` は 0） |
| `'{a}'::int4[]` | `22P02 invalid input syntax for type integer: "a"` |
| `'{1,2'::int4[]` | `22P02 malformed array literal: "{1,2"` DETAIL `Unexpected end of input.` |
| `'1,2'::int4[]` | `22P02 ... DETAIL Array value must start with "{" or dimension information.` |
| `'{1,,2}'::int4[]` | `22P02 ... DETAIL Unexpected "," character.` |
| `'{{1,2},{3}}'::int4[]` | `22P02 ... DETAIL Multidimensional arrays must have sub-arrays with matching dimensions.`（M5 は構文検査が先なのでこのエラーが先に出る） |
| `'{{1,2},{3,4}}'::int4[]` | **`0A000 multidimensional arrays are not supported yet`**（PG は `{{1,2},{3,4}}`） |
| `'[1:0]={}'::int4[]`、`'[2:1]={1}'::int4[]` | `2202E upper bound cannot be less than lower bound` |
| `'[1:2]={1}'::int4[]` | `22P02 ... DETAIL Specified array dimensions do not match array contents.` |
| `'{{{{{{{1}}}}}}}'::int4[]` | `54000 number of array dimensions exceeds the maximum allowed (6)` |

`SQLSTATE` の定数: `ARRAY_SUBSCRIPT_ERROR = "2202E"` を `error.rs` に足す（00 §3.5 の「ほかに必要になった章は自分で足してよい」）。

#### 5.3.2 出力（`array_out`）

PG の `array_out`（17.11）と同じ。

- 要素 0 件は `{}`。
- いずれかの次元の下限が 1 でなければ、先頭に `[lb:ub]`（次元ごと）+ `=`。
- 要素は `output_text(elem, SqlType::of(elem_type), env)`。NULL は `NULL`（引用符なし）。
- **要素に引用符をつける条件**: 空文字列、`NULL`（大文字小文字を問わず）と等しい文字列、`"` `\` `{` `}` 区切り（`,`）または空白（`scanner_isspace`）を含むもの。引用符つきでは `"` と `\` の前に `\` を置く。
- 例: `{"a b",c,"",NULL,"NULL","a\"b","a\\b"}`、`bytea` の要素 `\x0102` → `{"\\x0102"}`（`\` を含むので引用符つき、`\` は 2 つ）、日時・interval の要素は空白を含むので引用符つき。
- 次元ごとの `{ }` と区切りは PG の入れ子の出力と同じ（M5 は 1 次元だけだが、多次元の出力ループも PG の形で書く）。

### 5.4 配列の解析（`analyzer/polymorphic.rs`、`analyzer/array_expr.rs`）

#### 5.4.1 ポリモーフィック型の解決

候補の絞り込み（`is_consistent`）と確定（`resolve`）。PG の `check_generic_type_consistency` / `enforce_generic_type_consistency` の部分集合。

```text
入力: declared[i]（宣言の引数型）、actual[i]（実際の型。UNKNOWN = 型の決まらないリテラル / パラメータ）
前処理: 多相の位置だけを見る。

[Element / Array / NonArray の組]
  elem := None
  for i where declared[i] ∈ {anyelement, anynonarray}:
      if actual[i] == UNKNOWN: continue
      candidate := actual[i]
      if declared[i] == anynonarray and array_kind(candidate).is_some(): NG       // 実際の型が配列
      unify(elem, candidate)                                                      // 違えば NG
  for i where declared[i] == anyarray:
      if actual[i] == UNKNOWN: continue
      candidate := element_type(actual[i]) or NG                                  // 配列でなければ NG（int2vector / oidvector は elem 21 / 26）
      unify(elem, candidate)
  if declared に Element 系があり elem == None:                                   // 全部 UNKNOWN
      anyelement / anynonarray のみ → elem := text
      anyarray を含む → NG（resolve では 42P18 "could not determine polymorphic type because input has type unknown"）
  戻り型: anyelement → elem、anyarray → array_type_of(elem)（None なら 42704 "could not find array type for data type X"）、anynonarray → elem

[Compatible / CompatibleArray の組]
  types := [ actual[i] (declared[i] == anycompatible かつ UNKNOWN でない)
           , element_type(actual[i]) (declared[i] == anycompatiblearray かつ UNKNOWN でない。配列でなければ NG) ]
  common := select_common_type(types)        // 暗黙キャストで全員が common に変換できること。できなければ NG。空なら text
  引数の具体型: anycompatible → common、anycompatiblearray → array_type_of(common)
  戻り型: anycompatible → common、anycompatiblearray → array_type_of(common)
```

- `unify(elem, c)`: `elem == None` なら `elem := c`、同じ型ならそのまま、違えば NG。NG の理由（`resolve` のエラーに使う。42804）: `arguments declared "anyelement" are not all alike` DETAIL `integer versus text`、`argument declared anyarray is not an array but type integer`、`argument declared anyarray is not consistent with argument declared anyelement` DETAIL `integer[] versus text`（PG と同じ文言。未検証: DETAIL の細部）。
- **候補の絞り込みで NG は「その候補に合わない」**（最終的に一致する候補がなければ `42883 function f(...) does not exist`）。`resolve` のエラーは、絞り込みを通ったのに確定できない稀な場合だけ（全引数が UNKNOWN の `anyarray`: `array_length('{1}', 1)` は `42P18`）。
- 確定後、呼び出し側（`analyzer/resolve.rs` の関数・演算子の解決）が `arg_types` へ引数を暗黙キャストする。UNKNOWN のリテラルは、配列型の位置なら `array_in`（`input_text`）で、要素型の位置なら要素の入力関数で評価する（M4-09 §4.1 の eager / 日時の規則に従う）。
- 組み込みの関数・演算子の宣言（§6.3）が使うのは `anyarray`・`anyelement`・`anycompatible`・`anycompatiblearray` だけ。M2 の `anynonarray`（`text || anynonarray`）はそのまま動く。

**解析の入口（§11 の依頼 4）**: `analyzer/resolve.rs` の候補の評価（M2 の `is_polymorphic(declared)` と `declared == ANYARRAY` の分岐）を、`polymorphic::is_consistent` の呼び出しに置き換える。`Expr.ty`（結果の型）は `PolyResolution.result`。

#### 5.4.2 `ARRAY[...]`（`ArrayCtor`）

PG の `transformArrayExpr` の 1 次元の場合:

1. **要素が 0 個**: 型の指定（`ARRAY[]::int[]` のキャストの中）があればその要素型で `ArrayCtor { elem_type, elems: [] }`。なければ `42P18 cannot determine type of empty array` + HINT `Explicitly cast to the desired type, for example ARRAY[]::integer[].`。
2. **要素の式が配列型**（`ARRAY[ARRAY[1,2], ARRAY[3,4]]`、配列型の列・パラメータを並べる）: `0A000 multidimensional arrays are not supported yet`。
3. **型の指定がない場合**: 要素の型の共通型を `select_common_type`（UNION / CASE と同じ。UNKNOWN だけなら `text`）で決め、各要素を暗黙変換する（UNKNOWN のリテラルは共通型の入力関数で評価。`ARRAY[1,'a']` は `22P02 invalid input syntax for type integer: "a"`）。共通型が決まらなければ `42804 ARRAY types integer and boolean cannot be matched`。配列型がなければ `42704 could not find array type for data type X`。実機: `ARRAY[1,2.5]` は `numeric[]`、`ARRAY['a','b']` は `text[]`、`ARRAY[NULL,NULL]` は `text[]`。
4. **型の指定がある場合**（`ARRAY[...]::T[]`。キャストの対象が `ARRAY[...]` の構文のとき）: 各要素を**明示キャスト**で要素型 T に変換して `ArrayCtor` にする（`ArrayCoerce` を挟まない。実機: `ARRAY['1','2']::int[]` = `{1,2}`、`ARRAY[1,2]::text[]` = `{1,2}`）。`T[]` の typmod は各要素の `CoerceTypmod` に使う。

#### 5.4.3 `a[i]`（`ArraySubscript`）と `ANY` / `ALL`（`ScalarArrayOp`）

**`a[i]`**: 基底の型の `element_type`（Regular / Vector）がなければ `42804 cannot subscript type integer because it does not support subscripting`。添字は代入の文脈で `int4` にする（`a[1.5]` は 2、`a['2']` は 2。変換できなければ `42804 array subscript must have type integer`）。スライス（`a[1:2]`）は `0A000 array slices are not supported yet`。2 つ以上の添字は `0A000`。**`int2vector` / `oidvector` の添字は下限 0**（`('1 2'::int2vector)[1]` = `2`。`array_view` が下限 0 に読み替える）。NULL の添字・範囲外・次元数の不一致は NULL（実機: `(ARRAY[10,20,30])[0]`、`[4]`、`[NULL]` はすべて NULL）。構文: 括弧で囲んだ式の後ろ、または列参照の後ろ（`a[1]`）。リテラルの直後の `ARRAY[1,2,3][1]` は PG でも構文エラー（`42601`）。

**`x op ANY (array)` / `x op ALL (array)`**（PG の `make_scalar_array_op`）:

1. 右辺が配列式（副問い合わせでなければ。副問い合わせは M4 の `SubLink::Any / All`）。右辺の型が UNKNOWN でなく配列（Regular / Vector）でもなければ `42809 op ANY/ALL (array) requires array on right side`。
2. 演算子 `op` を `(左辺の型, 右辺の要素の型（右辺が UNKNOWN なら UNKNOWN）)` で通常の二項演算子の解決にかける（見つからなければ `42883`）。
3. 左辺を演算子の左の型に、右辺を**演算子の右の型の配列**に暗黙変換する（右辺が UNKNOWN のリテラルなら `array_in` で `array_type_of(右の型)` として評価。**パラメータ `$1` なら 04 章が `array_type_of(演算子の右の型)` に推論する**。`1 = ANY($1)` は `$1` が `int4[]`）。要素型が違う配列は `ArrayCoerce`（暗黙）で包む。
4. 演算子の戻り型が `bool` でなければ `42809 op ANY/ALL (array) requires operator to yield boolean`。
5. `ScalarArrayOp { op, use_or: ANY なら true, left, array }`。型は `bool`。
6. `SOME` は `ANY` の別名。`x IN (...)` は M4 の `InList` のまま（配列ではない）。

#### 5.4.4 配列のキャスト（`ArrayCoerce`）と `unknown` リテラル

`find_coercion_pathway(target, source, ctx)` に配列の分岐を足す（PG の `COERCION_PATH_ARRAYCOERCE`）:

- source と target が両方 Regular の配列型で、要素の経路 `find_coercion_pathway(target_elem, source_elem, ctx)` が `None` でなければ `Pathway::ArrayCoerce(要素の経路)`。要素型が同じなら `Relabel`（`typmod` だけ違うとき）。要素の経路が `CoerceViaIo` でもよい（`'{1,2}'::int4[]::text[]`）。
- source が Vector（`int2vector` / `oidvector`）で target が `int2[]` / `oid[]`: `Pathway::Cast(CastMethod::Function(vector_to_array))`。逆（配列 → vector）は `None`（`42846 cannot cast type smallint[] to int2vector`。実機と同じ）。
- 変換結果は `ExprKind::ArrayCoerce { expr, elem_method, elem_ty, explicit }`。`CoerceTypmod` は配列型の `takes_typmod` が真なら要素ごとに適用（`ArrayCoerce` が持つ）。
- UNKNOWN リテラル → 配列型: `coerce_type` の `Literal(Text)` 分岐が `io::input_text(s, SqlType::new(array_oid, typmod))` を呼ぶ（要素が eager な型のとき。日時の配列は `Cast { InOut }` を残して計画時に畳む）。`input_is_eager(array_oid)` は `input_is_eager(element)`。

#### 5.4.5 `ARRAY(SELECT ...)`（`SubLinkKind::Array`）

`ARRAY (` の後に `SELECT` / `WITH` / `VALUES` / `(` が続く構文。副問い合わせを解析し、出力列が 1 つでなければ `42601 subquery must return only one column`。列の型の `array_type_of` がなければ `42704`。`SubLink { kind: Array, test: None, query }`、型は配列型（typmod -1。実機: `pg_typeof(array(select 'a'::name))` = `name[]`）。相関副問い合わせも可（M4 の SubPlan の仕組み）。論理プランでは SEMI 化の対象にしない（§4.4）。

### 5.5 配列の評価（`executor/eval_array.rs`）

| ノード | 評価 |
|---|---|
| `ArrayCtor` | 要素を左から順に評価し、`ArrayValue::from_vec(elem_type, 1, items)`（空は `empty`）。`Datum::Array(Box::new(..))` |
| `ArraySubscript` | 配列が NULL → NULL。添字のどれかが NULL → NULL。`array_view` で読み、`ndim != indexes.len()` → NULL、`get1(i)` が None → NULL。Some なら要素（NULL 要素は NULL） |
| `ArrayCoerce` | NULL → NULL。要素ごとに: NULL はそのまま、`Function(f)` は `f(&[item])`、`Binary` はそのまま、`InOut` は `output_text(item, SqlType::of(src_elem), env)` → `input_text(s, elem_ty, env)`、`Env(f)` は `f(&[item], env)`。続けて `apply_typmod(d, elem_ty, explicit)`。`dims` はそのまま、`elem_type = elem_ty.oid` |
| `ScalarArrayOp` | 下の擬似コード（PG の `ExecEvalScalarArrayOp` と同じ三値論理） |
| `SubLink::Array` | 副問い合わせの全行の第 1 列を順に集め `ArrayValue::from_vec`。0 行は `empty`（NULL にならない）。`ORDER BY` が効く |

```text
eval_scalar_array_op(left, array, op, use_or):
    a := eval(array);  if a is NULL: return NULL
    view := array_view(a);  if view.nitems == 0: return !use_or      // ANY は false、ALL は true。左辺が NULL でも
    l := eval(left)
    saw_null := false
    for item in view.items:
        if l is NULL or item is NULL: saw_null := true; continue       // strict の演算子は NULL を返す
        r := op.func(&[l, item])  (strict なので NULL でない引数だけ)
        match r: NULL → saw_null := true
                 true  → if use_or: return true
                 false → if !use_or: return false
    return saw_null ? NULL : !use_or
```

実機: `1 = any(array[1,2])` = t、`1 = any(null::int[])` = NULL、`1 = any('{}'::int[])` = f、`1 <> all('{}'::int[])` = t、`null = any('{1}')` = NULL、`1 = any('{2,NULL}')` = NULL、`1 = all('{1,NULL}')` = NULL、`1 <> all('{2,NULL}')` = NULL（【確認】）。**多次元でも全要素を行優先で走査する**（M5 は 1 次元だけなので影響しない）。

### 5.6 カタログの `int2[]` / `oid[]` 列の移行（M5-Q15。TY-5 の最後）

1. `grep -rn "Int2Vector\|Int4Array\|INT2_ARRAY\|INT4_ARRAY\|OID_ARRAY\|TEXT_ARRAY\|CHAR_ARRAY" impl/rust/crates/yuzhu-core/src` で全箇所を出し、次の 3 つに分類する。
   - (a) `int2vector` / `oidvector` 型の列・値（`pg_index.indkey` `indoption`、`pg_trigger.tgattr`、`proargtypes`、`indclass`）: **変えない**。
   - (b) `int2[]`（1005）・`oid[]`（1028）・`text[]`・`"char"[]` 型の列と値（`pg_constraint.conkey` `confkey` など）: `Datum::Array` に変える。
   - (c) M3 の `Int4Array`（`pg_isolation_test_session_is_blocked(int, int4[])` の引数。`ops.rs`、`catalog/builtin.rs`、`executor`）: `Datum::Array`（elem 23）に変え、`array_view` で読む。`int4_array_in` / `int4_array_out` は `array_in` / `array_out` に吸収して削除する。
2. `catalog/rows.rs` / `catalog/store.rs`（M4 の C1 が書いた `pg_constraint` / `pg_index` の書き込み・読み出し）で、(b) の列を `ArrayValue::from_int2s` / `to_int2s`（など）に置き換える。**読み出しは両方の値を受ける**（移行中の `Int2Vector` を `array_view` で読み替える）が、**書き込みは `Array` だけ**。
3. `is_null_only_type` から `TEXT_ARRAY`・`INT2_ARRAY`・`OID_ARRAY`・`CHAR_ARRAY` を外す（`ACLITEM_ARRAY`・`ANYARRAY` は残す。`_int2vector` `_oidvector` `_cstring` `_record` を足す）。`storage/heap/tuple.rs` の `kind_of` の `NullOnly` からも外し、`Kind::Array` に振る。
4. `catalog_version` を上げる（F0。00 D35）。M4 のデータディレクトリは使えない（M2-Q20 で許容済み）。
5. `tests/slt/m4/catalog/*` のうち `conkey` を `::text` で比べるものは、`{1}` の出力が同じなので変更不要（`int2[]` の出力は `{1,2}` で、`int2vector` の `1 2` とは違う型）。`conkey[1]` を使う問い合わせが新しく書ける。
6. `\d` 系の問い合わせ（§6.5）と `pg_constraint` の FK の行（07 章）が通ることを `tests/slt/m5/types/array_catalog.slt` で確かめる。

---

## 6. モジュールごとの仕様

### 6.1 bytea（`types/bytea.rs`）

**入力（`byteain`）**: 次の 2 形式。

- **hex**（先頭が小文字の `\x`。`\X41` は 22P02）: 16 進数字（大文字小文字可）を 2 桁ずつ。**空白（スペース、`\t`、`\n`、`\r`）は桁の組の前だけ許す**（`'\x 0a 0b'` は `\x0a0b`、`'\x0 a'` は不正）。奇数桁は `22023 invalid hexadecimal data: odd number of digits`、16 進以外の文字は `22023 invalid hexadecimal digit: "z"`（`"` で囲んだ 1 文字。空白も同じ）。`'\x'` は空の bytea。
- **escape**（それ以外）: `\\` → `\`、`\` + 8 進 3 桁（`[0-3][0-7][0-7]`）→ 1 バイト、それ以外の文字はそのまま（UTF-8 のバイト列。`'é'::bytea` = `\xc3a9`）。`\` の後が 8 進 3 桁でも `\\` でもなければ（`\1b`、`\400`、末尾の `\`）`22P02 invalid input syntax for type bytea`。空文字列は空の bytea。
- リテラルの `\` は `standard_conforming_strings = on` なので、SQL の `'a\\b'` は 4 文字、`E'\\x41'` は `\x41` の 3 文字（M1 のまま）。

**出力（`byteaout`、`bytea_output`）**: `hex` は `\x` + 小文字の 16 進（空は `\x`）。`escape` は、`\` → `\\`、`0x20..=0x7E` の印字可能な ASCII はそのまま、それ以外（`0x00..=0x1F`、`0x7F..=0xFF`）は `\` + 8 進 3 桁。実機: `'\x00ff5c27412022'` → `\000\377\\'A "`、`'\x0d0a09'` → `\015\012\011`、`'\x7f80'` → `\177\200`。

**演算子**（OID は実機）: `=` 1955、`<>` 1956、`<` 1957、`<=` 1958、`>` 1959、`>=` 1960（oprcode は `byteaeq` 1948、`byteane` 1953、`bytealt` 1949、`byteale` 1950、`byteagt` 1951、`byteage` 1952。`com` と `negate` は PG の値）、`||` 2018（oprcode `byteacat` 2011。`bytea || bytea` だけ。`bytea || unknown` は unknown が bytea の入力になる: `'x'::bytea || 'y'::bytea`、実機の `pg_typeof('\x41'::bytea||'x')` = `bytea`）。

**関数**（すべて strict・immutable。`FnKind::Pure`）:

| OID | 関数 | 規則 |
|---|---|---|
| 2010 / 720 / 1810 | `length(bytea)` / `octet_length(bytea)` / `bit_length(bytea)` | バイト数 / バイト数 / バイト数 × 8 |
| 2012 / 2013 / 2085 / 2086 | `substring(bytea, int4, int4)` / `substring(bytea, int4)` / `substr(..)` 2 つ | 開始位置は 1 始まり。開始が 1 より小さければ長さをその分だけ縮める（実機: `substring('\x010203' from 0 for 2)` = `\x01`、`from -1 for 3` = `\x01`、`from 5` = `\x`）。長さが負なら `22011 negative substring length not allowed`。`2147483647` のような大きい長さで桁あふれしない（`checked_add`） |
| 2014 | `position(bytea, bytea)`（SQL の `POSITION(sub IN str)` は `position(str, sub)` に書き換わる） | 見つかった位置（1 始まり）、なければ 0、空の部分列は 1（実機: `position(''::bytea in '\x01')` = 1） |
| 749 / 752 | `overlay(bytea placing bytea from int4 [for int4])` | `substring(s, 1, from - 1) \|\| r \|\| substring(s, from + for)`。`for` の既定は `length(r)`。`from` が 1 未満は `22011 negative substring length not allowed`（実機: `from 0`）。`for` が負でも通る（実機: `overlay('\x010203' placing '\xff' from 2 for -1)` = `\x01ff010203`） |
| 721 / 722 | `get_byte(bytea, int4)` / `set_byte(bytea, int4, int4)` | 0 始まり。範囲外は `2202E index 5 out of valid range, 0..0`（負も同じ文言: `index -1 ...`。上限は `length - 1`、空の bytea は `0..-1`）。`set_byte` の新しい値は下位 8 ビット（`set_byte('\x01', 0, 256)` = `\x00`） |
| 723 / 724 | `get_bit(bytea, int8)` / `set_bit(bytea, int8, int4)` | ビット 0 は先頭バイトの**最下位ビット**（`get_bit('\x01', 0)` = 1、`get_bit('\x80', 7)` = 1）。範囲外は `2202E index 8 out of valid range, 0..7`。`set_bit` の新しい値が 0 / 1 以外は `22023 new bit must be 0 or 1` |
| 1946 | `encode(bytea, text)` | 形式名は大文字小文字を区別しない。`hex`（小文字）、`base64`（**76 文字ごとに `\n`**）、`escape`（`\` → `\\`、`0x00` と `0x80..=0xFF` → `\` + 8 進 3 桁、**他はそのまま**（`\n`・`\t`・`0x7F` も素通し。`bytea_output = escape` の出力とは違う））。未知は `22023 unrecognized encoding: "foo"` |
| 1947 | `decode(text, text)` | `hex`（入力と同じ検査と空白）、`base64`（空白（スペース・`\t`・`\n`・`\r`）は無視。不正な文字は `22023 invalid symbol "?" found while decoding base64 sequence`、パディング不足・打ち切りは `22023 invalid base64 end sequence` + HINT `Input data is missing padding, is truncated, or is otherwise corrupted.`。`decode('YQ', 'base64')` もエラー）、`escape`（`\\` → `\`、`\ooo` → バイト、それ以外の `\x` は `22P02 invalid input syntax for type bytea`） |
| 1717 / 1714 | `convert_to(text, name)` / `convert_from(bytea, name)` | **`UTF8` だけ**（名前は大文字小文字と `-` `_` を無視: `utf8`・`UTF-8`・`UTF_8`）。`convert_to` は UTF-8 のバイト列、`convert_from` は検査して `text`（不正は `22021 invalid byte sequence for encoding "UTF8": 0xff`）。他の既知の符号化名（`LATIN1` など）は `0A000 conversion to encoding "LATIN1" is not supported yet`（`from` は `conversion from encoding ...`）、未知の名前は `22023 invalid destination encoding name "nosuch"`（`convert_from` は `invalid source encoding name`） |

**任意（TY-4 の末尾。不要なら落とせる）**: `md5(text)` 2311 / `md5(bytea)` 2321（32 桁の小文字 16 進。`types/md5.rs` の手書き。RFC 1321 のテストベクタで検証）、`sha224` 3419 / `sha256` 3420 / `sha384` 3421 / `sha512` 3422（`bytea → bytea`。`yuzhu-auth` に `pub mod digest { pub fn sha224/sha256/sha384/sha512(data: &[u8]) -> Vec<u8> }` を足してもらう（D29。§11 の依頼 5。`sha2` クレートの薄い包み））。実機: `md5('')` = `d41d8cd98f00b204e9800998ecf8427e`、`encode(sha256('abc'), 'hex')` = `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`。

**キャスト**: §5.1.3。`bytea` の `typmodin` はなし。`pg_type` の行は §5.1.2。

### 6.2 uuid（`types/uuid.rs`）

**入力（`string_to_uuid`。PG:src/backend/utils/adt/uuid.c）**: 先頭が `{` なら末尾に `}` が必須。16 進数字（大文字小文字可）2 桁を 16 回読む。**4 桁（2 バイト）ごと、かつ最後を除く位置の直後にだけ `-` を 1 つ許す**（`i % 2 == 1 && i < 15` のバイトの後）。前後の空白は不可。不正はすべて `22P02 invalid input syntax for type uuid: "<入力そのまま>"`。実機で通る形: `A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11`、`{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}`、`a0eebc999c0b4ef8bb6d6bb9bd380a11`、`a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11`、`a0eebc99-9c0b-4ef8-bb6d6bb9bd380a11`（ハイフンの位置は 4 桁の区切りならどこでもよい）。通らない形: `{...`（閉じなし）、先頭の空白、桁の不足・超過、`a-0eebc99-...`、`-a0ee...`、末尾の `-`、`--`、空文字列。

**出力**: 小文字の `8-4-4-4-12`。**比較**: 16 バイトの辞書順。**演算子**: `=` 2972、`<>` 2973、`<` 2974、`<=` 2976、`>` 2975、`>=` 2977（oprcode は `uuid_eq` 2956、`uuid_ne` 2959、`uuid_lt` 2954、`uuid_le` 2955、`uuid_gt` 2958、`uuid_ge` 2957）。**関数**: `gen_random_uuid()` 3432（volatile、引数なし。`FnKind::Pure` の `volatility = 'v'` を `PROCS` に書く。**定数畳み込みしない**: 引数なしの純粋関数の畳み込み規則（M4-09 §4.2）が volatile を除外することを `const_fold` の単体テストで確かめる）。version 4: 乱数 16 バイトのうち `b[6] = (b[6] & 0x0f) | 0x40`、`b[8] = (b[8] & 0x3f) | 0x80`（TY-D7）。`uuid_hash` 2963 などの内部関数は不要。

**M6**: `uuid_extract_timestamp` 6342、`uuid_extract_version` 6343（PG17）。`min` / `max` はない（TY-D8）。

### 6.3 配列の関数・演算子（`types/array_ops.rs`、`catalog/builtin/array.rs`）

宣言の型は `anyarray`（2277）など（TY-D12）。`anyarray` を取る関数は引数を `array_view` で読む（`int2vector` / `oidvector` も受ける）。

| OID | 関数 / 演算子 | 引数 → 結果 | vol / strict | 規則（実機で確認） |
|---|---|---|---|---|
| 2176 | `array_length` | anyarray, int4 → int4 | i / strict | 第 2 引数は次元（1 始まり）。`dim < 1`、`dim > ndim`、空の配列は NULL（`array_length('{}', 1)` = NULL）。それ以外はその次元の要素数 |
| 2092 / 2091 | `array_upper` / `array_lower` | anyarray, int4 → int4 | i / strict | 上限 / 下限。範囲外の次元と空の配列は NULL（`array_upper(array[1,2], 2)` = NULL）。`[0:1]={1,2}` は `lower` = 0、`upper` = 1 |
| 747 | `array_dims` | anyarray → text | i / strict | `[1:3]`（次元ごとに `[lb:ub]`）。空の配列は NULL |
| 748 | `array_ndims` | anyarray → int4 | i / strict | 次元数。空の配列は NULL |
| 3179 | `cardinality` | anyarray → int4 | i / strict | 要素の総数。空の配列は 0、NULL 配列は NULL |
| 395 | `array_to_string` | anyarray, text → text | **s** / strict | 要素を `output_text` で文字列にして区切りで連結。**NULL 要素は飛ばす**。空の配列は空文字列。区切りが NULL なら NULL（strict） |
| 384 | `array_to_string` | anyarray, text, text → text | **s** / **非 strict** | 第 3 引数が NULL でなければ NULL 要素をその文字列にする。配列または区切りが NULL なら NULL。第 3 引数が NULL なら 2 引数版と同じ（実機: `array_to_string(array[1,null], ',', null)` = `1`） |
| 1070 | `=`（anyarray, anyarray → bool） | | i / strict | `array_eq` 744。dims・lbound・要素がすべて同じ（NULL どうしは等しい。`'[0:1]={1,2}' = '{1,2}'` は false、`'{1,NULL}' = '{1,NULL}'` は true）。要素型が違えば 42804 |
| 1071 | `<>` | | i / strict | `array_ne` 390 |
| 349 / 374 / 375 | `\|\|`（任意 O1） | anycompatiblearray, anycompatible / anycompatible, anycompatiblearray / anycompatiblearray, anycompatiblearray | i / **非 strict** | `array_append` 378 / `array_prepend` 379 / `array_cat` 383。NULL の配列は空として扱う（`array_append(NULL::int[], 1)` = `{1}`、`array_cat(NULL, '{1}')` = `{1}`、両方 NULL は NULL）。1 次元: append は下限を保って末尾に足し（`[5:6]={7,8}` + 9 → `[5:7]={7,8,9}`）、**prepend も下限を保つ**（`[5:7]={1,7,8}`）、cat は最初の配列の下限（`[5:8]={7,8,1,2}`）。**`\d+` のためだけ**（§6.5） |

- 演算子の `oprcode`・`com`・`negate` は PG の値（`=` の com は 1070、negate は 1071）。**`<` `<=` `>` `>=`（1072〜1075）、`@>` `<@` `&&`、`array_position` などは行を作らない**（M6）。配列の `ORDER BY` / `GROUP BY` / `DISTINCT` は演算子がないので `42883`（既知の差）。
- 関数の戻り値の配列は、要素型が `anyarray` の引数と同じ。`array_to_string` に `timestamp[]` を渡すと要素の出力が DateStyle に依存するので **`FnKind::Env`**（§4.5）で書く。`Pure` で書けるのは、環境を使わない `array_length` `array_upper` `array_lower` `array_dims` `array_ndims` `cardinality`、`=` `<>`、`||`。
- `array_to_string` の要素の出力: `bytea` は `\x01`（`bytea_output` に従う）、numeric は `1.50`（dscale のまま）、timestamp は `2000-01-01 00:00:00`（実機）。多次元は平坦に連結する（M5 では起きない）。

### 6.4 全型のバイナリ形式（TY-D10。`send` と `recv`）

**この表が正**。値は実機の `*_send`（`SELECT xxx_send(...)`）と、拡張プロトコルでバイナリのパラメータ・結果を送る実験（Perl の試験用クライアント）で取った。整数はすべてネットワークバイトオーダー（BE。ディスクの LE と違う）。`recv` の「長さの検査」は PG の `ReceiveFunctionCall` が `recv` の後で行う: **読み足りなければ `08P01 insufficient data left in message`、余りがあれば `22P03 incorrect binary data format`**（04 章が ` in bind parameter N` を付けて返す。実機: `$1::int4` に 3 バイトは `08P01`、5 バイトは `22P03 incorrect binary data format in bind parameter 1`）。型ごとの `recv` の検査はこの表の「recv」の列。

| 型 | OID | `send` | `recv` | 例（値 → バイト列） | 実体 |
|---|---|---|---|---|---|
| `bool` | 16 | 1 バイト（`00` / `01`） | 非 0 は真（`02` → `t`） | `true` → `01` | 既存 |
| `bytea` | 17 | 生のバイト列 | 任意の長さ（空も可） | `\x0001ff` → `0001ff` | `bytea.rs` |
| `"char"` | 18 | 1 バイト | 1 バイトちょうど | `'a'` → `61` | 既存 |
| `name` | 19 | UTF-8 のバイト列（`textsend` と同じ） | UTF-8 検査。**64 バイト以上は `42622 identifier too long` DETAIL `Identifier must be less than 64 characters.`**（63 バイトまで） | `'ab'` → `6162` | 既存 |
| `int8` | 20 | i64 BE | 8 バイト | `-1` → `ffffffffffffffff` | 既存 |
| `int2` | 21 | i16 BE | 2 バイト | `-2` → `fffe` | 既存 |
| `int2vector` | 22 | **配列形式**（下の「配列」。elemtype 21、**下限 0**、NULL なし） | 配列の `recv` + 1 次元・NULL なし・elemtype 21・**下限 0** でなければ `22P03 invalid int2vector data` | `'1 2'` → `00000001 00000000 00000015 00000002 00000000 00000002 0001 00000002 0002` | `array.rs`（vector） |
| `int4` | 23 | i32 BE | 4 バイト | `258` → `00000102` | 既存 |
| `regproc` | 24 | u32 BE | 4 バイト（存在検査なし） | `'now'` → `00000513`（1299） | 既存 |
| `text` | 25 | UTF-8 のバイト列 | UTF-8 検査。不正は `22021 invalid byte sequence for encoding "UTF8": 0xff`、**NUL バイトも `22021 ...: 0x00`** | `'héllo'` → `68c3a96c6c6f` | 既存 |
| `oid` | 26 | u32 BE | 4 バイト | `5` → `00000005` | 既存 |
| `tid` | 27 | block u32 BE + offset u16 BE（6 バイト） | 6 バイト | `(1,2)` → `000000010002` | 既存 |
| `xid` / `cid` | 28 / 29 | u32 BE | 4 バイト | `5` → `00000005` / `7` → `00000007` | 既存 |
| `oidvector` | 30 | 配列形式（elemtype 26、下限 0） | `22P03 invalid oidvector data`（上と同じ条件） | `'1 2'` → `00000001 00000000 0000001a 00000002 00000000 00000004 00000001 00000004 00000002` | `array.rs`（vector） |
| `pg_node_tree` | 194 | UTF-8 のバイト列（`textsend`） | UTF-8 検査 | | 既存 |
| `float4` | 700 | f32 のビット BE | 4 バイト。**ビットをそのまま保つ**（NaN のペイロードも） | `1.5` → `3fc00000`、`NaN` → `7fc00000` | 既存 |
| `float8` | 701 | f64 のビット BE | 8 バイト。同上 | `-0` → `8000000000000000`、`NaN` → `7ff8000000000000`（受信した `7ff8000000000001` はそのまま返る） | 既存 |
| `unknown` | 705 | UTF-8 のバイト列 | UTF-8 検査 | | 既存 |
| `bpchar` | 1042 | **埋めた後の**文字列の UTF-8 | UTF-8 検査。**typmod は呼び出し側**（パラメータに typmod はない。列への代入のときに `apply_typmod`: 短ければ空白で埋める、空白以外が超過すれば `22001`） | `'ab'::char(4)` → `61622020` | `bpchar.rs` |
| `varchar` | 1043 | UTF-8 | UTF-8 検査（typmod は呼び出し側。明示キャストは切り詰め） | `'ab'` → `6162` | 既存 |
| `date` | 1082 | i32 BE（2000-01-01 からの日数） | 4 バイト。範囲検査は 06 章（【実機】PG17.11: `date_recv` は `22008 date out of range`。`7ffffff0` で確認。R-32） | `2000-01-02` → `00000001`、`infinity` → `7fffffff`、`-infinity` → `80000000` | 06 章 |
| `time` | 1083 | i64 BE（0 時からのマイクロ秒） | 8 バイト。**`0 .. 86400000000` の外は `22008 time out of range`**（実機: `7fffffffffffffff`） | `01:02:03.5` → `00000000ddf019e0` | 06 章 |
| `timestamp` | 1114 | i64 BE（2000-01-01 00:00:00 からのマイクロ秒） | 8 バイト | `2000-01-01 00:00:01.5` → `000000000016e360`、`infinity` → `7fffffffffffffff` | 06 章 |
| `timestamptz` | 1184 | i64 BE（UTC） | 8 バイト | `2000-01-01 00:00:00+00` → `0000000000000000` | 06 章 |
| `interval` | 1186 | `time` i64 BE（マイクロ秒）+ `day` i32 BE + `month` i32 BE（16 バイト） | 16 バイト | `1 year 2 mons 3 days 04:05:06` → `000000036c8bc080 00000003 0000000e`、`infinity` → `7fffffffffffffff 7fffffff 7fffffff` | 06 章 |
| `numeric` | 1700 | `ndigits` u16、`weight` i16、`sign` u16、`dscale` u16、`digits` の u16 × ndigits（10000 進）。**`yuzhu_numeric::Numeric::to_binary`**。`sign` は `0000` 正 / `4000` 負 / `c000` NaN / `d000` +Inf / `f000` -Inf。**±Infinity の dscale は 32（`0020`）、NaN は 0** | **`Numeric::from_binary(buf, typmod)`**: `invalid sign in external "numeric" value`（sign が 5 値以外）、`invalid scale in external "numeric" value`（dscale が 14 ビットを超える）、`invalid digit in external "numeric" value`（桁が 0..9999 の外）、いずれも `22P03`。**特殊値の桁は読み捨てる**（NaN に ndigits = 1 が付いても NaN）。dscale が隠す桁は**切り捨て**（四捨五入しない）。末尾のゼロ・先頭のゼロの桁は正規化。typmod は丸め + `22003`（Infinity は `cannot hold an infinite value`） | `0` → `0000 0000 0000 0000`、`0.00` → `0000 0000 0000 0002`、`1` → `0001 0000 0000 0000 0001`、`-1.5` → `0002 0000 4000 0001 0001 1388`、`1234.5678` → `0002 0000 0000 0004 04d2 162e`、`12345678.9` → `0003 0001 0000 0001 04d2 162e 2328`、`0.00001` → `0001 fffe 0000 0005 03e8`、`100000000` → `0001 0002 0000 0000 0001`、`NaN` → `0000 0000 c000 0000`、`Infinity` → `0000 0000 d000 0020`、`1.10` → `0002 0000 0000 0002 0001 03e8` | `numeric.rs`（クレートの橋渡し） |
| `regclass` / `regtype` | 2205 / 2206 | u32 BE | 4 バイト（**存在検査なし**: `00000403` は通る） | `'pg_class'` → `000004eb`（1259）、`int4` → `00000017` | 既存（M4） |
| `void` | 2278 | 0 バイト | 0 バイト | | 既存 |
| `uuid` | 2950 | 16 バイト | 16 バイトちょうど（15 は `08P01`、17 は `22P03`） | `a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11` → `a0eebc999c0b4ef8bb6d6bb9bd380a11` | `uuid.rs` |
| 配列（`_int4` など） | 各 | 下の「配列」 | 下の「配列」 | 下の例 | `array.rs` |
| `aclitem`、`anyarray`、`record`、`cstring`、`internal`、`_aclitem`、`_cstring`、`_record`、`_int2vector`、`_oidvector` | | `supports_binary` は false | | | 04 章がエラー（`0A000`）にする |

**numeric のバイナリの読み方（NBASE = 10000 の表現）**: 値 = `Σ digits[i] × 10000^(weight − i)`（`digits[0]` が最上位の組、`weight` はその組の 10000 のべき）。小数点の位置は `weight` が決め、`dscale` は**表示する小数の桁数**（値には影響しない）。先頭と末尾の 0 の組は落とす（0 は `ndigits = 0`）。

| 値 | `digits`（組） | `weight` | `dscale` | バイト列 |
|---|---|---|---|---|
| `1234.5678` | `[1234, 5678]` = `04d2 162e` | 0（先頭の組が 10000⁰） | 4 | `0002 0000 0000 0004 04d2 162e` |
| `12345678.9` | `[1234, 5678, 9000]` = `04d2 162e 2328` | 1（先頭の組が 10000¹） | 1 | `0003 0001 0000 0001 04d2 162e 2328` |
| `-1.5` | `[1, 5000]` = `0001 1388` | 0 | 1 | `0002 0000 4000 0001 0001 1388`（`sign` = `4000`） |
| `100000000` | `[1]`（末尾の 0 の組は落とす） | 2（10000²） | 0 | `0001 0002 0000 0000 0001` |
| `0.00001` | `[1000]` = `03e8`（1000 × 10000⁻² = 10⁻⁵） | -2（`fffe`） | 5 | `0001 fffe 0000 0005 03e8` |
| `1.10` | `[1, 1000]`（0.10 = 1000 / 10000） | 0 | 2 | `0002 0000 0000 0002 0001 03e8`（`1.1` との違いは `dscale` だけ） |

**配列のバイナリ（`array_send` / `array_recv`）**:

| 位置 | 大きさ | 内容 |
|---|---|---|
| 0 | 4 | `ndim` |
| 4 | 4 | `flags`（NULL 要素が 1 つでもあれば 1、なければ 0） |
| 8 | 4 | 要素型の OID |
| 12 + 8i | 4 + 4 | 次元 i の要素数、下限（`ndim` 組） |
| その後 | | 要素ごとに: 長さ i32（NULL は -1）+ 要素の `send` のバイト列 |

- 空の配列は `ndim = 0`、`flags = 0`、要素型だけの 12 バイト。
- **`recv` の検査**（PG:arrayfuncs.c の `array_recv`、17.11。メッセージは実機で確認）: `ndim < 0` は `22P03 invalid number of dimensions: -1`、`ndim > 6` は `54000 number of array dimensions (7) exceeds the maximum allowed (6)`、**M5 は `ndim > 1` を `0A000 multidimensional arrays are not supported yet`**（上の 54000 の検査の後）、`flags` が 0 / 1 以外は `22P03 invalid array flags`（NULL の有無は要素の長さから決め直す。flags の値は信用しない）、要素型の OID が期待と違えば（両方が 16384 未満のとき）`42804 binary data has array element type 25 (text) instead of expected 23 (integer)`（OID の後ろの名前は `format_type`）、要素数 0 の次元（`ndim >= 1` で `dim = 0`）は空の配列として返す、次元の積・下限の検査は `54000 array size exceeds the maximum allowed (134217727)` / `array lower bound is too large: N`、要素の長さが `-1` 未満または残りより大きければ `22P03 insufficient data left in message`、要素の `recv` が消費したバイト数が長さと違えば `22P03 improper binary format in array element 3`（1 始まりの要素番号）、要素の `recv` が足りなければその型のエラー（`int4` なら `08P01`）。
- **例**（実機の `array_send`）:

| 値 | バイト列 |
|---|---|
| `{1,NULL,3}`（int4） | `00000001 00000001 00000017 00000003 00000001 00000004 00000001 ffffffff 00000004 00000003` |
| `{a,"b c"}`（text） | `00000001 00000000 00000019 00000002 00000001 00000001 61 00000003 622063` |
| `{}`（int4） | `00000000 00000000 00000017` |
| `[0:1]={5,6}`（int4） | `00000001 00000000 00000017 00000002 00000000 00000004 00000005 00000004 00000006` |
| `{1,2}`（int2） | `00000001 00000000 00000015 00000002 00000001 00000002 0001 00000002 0002` |
| `{1.5}`（numeric） | `00000001 00000000 000006a4 00000001 00000001 0000000c 0002 0000 0000 0001 0001 1388` |

- `ANY($1)` の `$1` を配列のバイナリで送る（tokio-postgres・pgx の通常の使い方）: 実機で `3 = any($1::int4[])` に `{3,4}` のバイナリを送って真を確認。型の推論は 04 章（§5.4.3 の 3.）。

### 6.5 psql の `\d` が使う配列の機能（確定）

実機の psql 17（`psql -E`）で `\d c`（PK と FK を持つ表）、`\d p`、`\d+ c`、`\dt+`、`\di+`、`\l`、`\du`、`\df`、`\dn`、`\dT`、`\dp` を流して取った問い合わせから、配列に関係する構文を拾った。**`\d tbl` は表の種類に関係なく次の 3 本（pg_policy・pg_statistic_ext・pg_publication）を送る**ので、表が空でも**解析を通る**必要がある（`pg-compat-tools.md` §3.1 の要点。実機で確認）。

| 問い合わせ（psql 17） | 配列に関係する式 | 要る機能 | 区分 |
|---|---|---|---|
| ポリシー | `pol.polroles = '{0}'`（`oid[]` と unknown リテラル） | `=`（anyarray）。**unknown のリテラルを相手の型 `oid[]` として `array_in`**（演算子解決の「unknown は相手の型」の規則） | 必須 |
| 同上 | `array(select rolname from pg_catalog.pg_roles where oid = any (pol.polroles) order by 1)` | **`ARRAY(SELECT)`**（要素型 `name` → `name[]`）、`oid = ANY(oid[])`、`array_to_string(name[], text)`（`anyarray` に `name[]`） | 必須 |
| 拡張統計 | `'d' = any(stxkind)`（`stxkind` は `"char"[]`、unknown の `'d'` が `"char"` として読まれる） | ANY で左辺の unknown を要素型に | 必須 |
| 出版物 | `pr.prattrs::pg_catalog.int2[]`（`prattrs` は `int2vector`）、`array_upper(.., 1)`、`attnum = prattrs[s]`（**`int2vector` の添字**）、`generate_series(0, array_upper(...))`（FROM 句、M4） | `int2vector → int2[]` のキャスト（下限 0）、`array_upper`、`int2vector` の添字 | 必須 |
| `\l`・`\dp` | `array_length(d.datacl, 1) = 0`、`array_to_string(d.datacl, E'\n')`（`aclitem[]` は NULL 専用） | `anyarray` に `aclitem[]` を受ける（実行時は NULL） | 必須（`\l` は M2 の完了条件。M2 の `ops::array_unsupported` を本体に置き換える） |
| `\dp` | `ARRAY(SELECT ...)` の入れ子、`oid = ANY(polroles)` | 上と同じ | 任意（`\dp` は GRANT がない） |
| `\d+` | `c.reloptions \|\| array(select 'toast.' \|\| x from pg_catalog.unnest(tc.reloptions) x)`（`text[] \|\| text[]`）、FROM 句の `unnest(text[])` | `\|\|`（`array_cat`）、**FROM 句の `unnest`** | 任意（`\d+` は M5 の完了条件でない） |
| `\dT` | `NOT EXISTS(SELECT 1 FROM pg_type el WHERE el.oid = t.typelem AND el.typarray = t.oid)` | `pg_type.typarray` が本物であること | 必須（§5.1.2。`\dT` は任意） |

**配列以外で `\d tbl` に要るもの（この章の範囲外。持ち主へ渡す。確認事項 M5-TY-Q2）**:

- `string_agg(attname, ', ')`（出版物の問い合わせ。`name` → `text` の暗黙キャストで `string_agg(text, text)` 3538）: 集約（M4 の `AggKind`）。**解析を通すために必須**。推奨: TY-5 が `AGGREGATES` に 1 行と `AggKind::StringAgg` の実体を足す（0.5 日。M4 の executor の変更を伴う）。
- FK を持つ表・参照される表（`relhastriggers` が真）では、さらに `pg_catalog.pg_partition_ancestors('18547')` を **SELECT 句の集合返却関数**として使う副問い合わせ（`confrelid IN (SELECT pg_partition_ancestors(..) UNION ALL VALUES (..))`）と、トリガーの問い合わせの `pg_partition_ancestors(t.tgrelid) WITH ORDINALITY AS a(relid, depth)`（FROM 句）。00 D25 は「SELECT 句の SRF は M6」だったが、そのままでは **FK のある表の `\d tbl` が M5 で動かない**ので、**「SELECT 句に SRF が 1 つだけで、他の出力列も FROM 句もない `SELECT srf(args)` を `SELECT * FROM srf(args)` に書き換える」最小の対応を M5 に足す（TY-D17。TY-5c。解析段階の書き換え。0.5 日、M4 の `FnKind::Set` を使う）**。00 D25 に例外を足した（レビュー対応 R-28）。
- `pg_get_statisticsobjdef_columns`、`pg_relation_is_publishable`、`regnamespace` へのキャスト、`pg_get_partkeydef` などの関数と、空のカタログ（`pg_policy` `pg_statistic_ext` `pg_publication*` `pg_inherits` `pg_trigger`）は M4 07 章（C2）の範囲。

**この表から確定した配列の機能（TY-5 の必須）**: `ARRAY(SELECT)`、`a[i]`（`int2vector` を含む）、`= ANY`、`anyarray` の `=`（unknown リテラルの解決）、`::int2[]`（`int2vector` から）、`array_upper`、`array_to_string`、`array_length`（`aclitem[]` を含む）、`pg_type.typarray`。`||` と FROM 句の `unnest` は `\d+` のための任意項目（M5-TY-Q2）。

### 6.6 エラーの一覧（この章が出すもの）

| 状況 | SQLSTATE | 文言 |
|---|---|---|
| bytea の hex 入力が奇数桁 / 不正な文字 | 22023 | `invalid hexadecimal data: odd number of digits` / `invalid hexadecimal digit: "z"` |
| bytea の escape 入力が不正 | 22P02 | `invalid input syntax for type bytea` |
| bytea_output が不正 | 22023 | `invalid value for parameter "bytea_output": "foo"`（HINT `Available values: escape, hex.`） |
| `encode` / `decode` の形式名 | 22023 | `unrecognized encoding: "foo"` |
| base64 の不正 | 22023 | `invalid symbol "?" found while decoding base64 sequence` / `invalid base64 end sequence`（HINT あり） |
| `get_byte` などの範囲外 | 2202E | `index 5 out of valid range, 0..0` |
| `set_bit` の値 | 22023 | `new bit must be 0 or 1` |
| 負の長さ | 22011 | `negative substring length not allowed` |
| 変換できない型 | 42846 | `cannot cast type integer to bytea`（uuid も同様） |
| uuid の入力 | 22P02 | `invalid input syntax for type uuid: "x"` |
| 配列リテラルの構文 | 22P02 | `malformed array literal: "..."` + DETAIL（§5.3.1） |
| 配列の次元・下限 | 2202E / 54000 | `upper bound cannot be less than lower bound` / `array bound is out of integer range` / `array upper bound is too large: N` / `array size exceeds the maximum allowed (134217727)` / `number of array dimensions exceeds the maximum allowed (6)` |
| 多次元・スライス・要素の代入 | 0A000 | `multidimensional arrays are not supported yet` / `array slices are not supported yet` / `array element assignment is not supported yet` |
| ユーザーテーブルの配列列 | 0A000 | `array types are not supported yet` |
| 空の `ARRAY[]` | 42P18 | `cannot determine type of empty array`（HINT `Explicitly cast to the desired type, for example ARRAY[]::integer[].`） |
| `ARRAY[1, true]` | 42804 | `ARRAY types integer and boolean cannot be matched` |
| 配列型がない | 42704 | `could not find array type for data type X` |
| 添字できない型 / 添字の型 | 42804 | `cannot subscript type integer because it does not support subscripting` / `array subscript must have type integer` |
| `ANY` の右辺 | 42809 | `op ANY/ALL (array) requires array on right side` / `op ANY/ALL (array) requires operator to yield boolean` |
| `ARRAY(SELECT a, b)` | 42601 | `subquery must return only one column` |
| 要素型の違う配列の `=` | 42804 | `cannot compare arrays of different element types`（解析で `42883` になることが多い） |
| ポリモーフィック型 | 42804 / 42P18 | `arguments declared "anyelement" are not all alike` / `could not determine polymorphic type because input has type unknown` |
| バイナリ（配列） | 22P03 / 54000 / 42804 | §6.4 |
| ディスクの配列が壊れている | XX001 | `corrupted array value in a tuple` |
| `convert_to` ほか | 0A000 / 22023 / 22021 | §6.1 |
| numeric の関数 | 2201G / 22003 / 0A000 | `count must be greater than zero` ほか（§5.2）、`factorial of a negative number is undefined`、`numeric sqrt is not supported yet` |

---

## 7. テスト

共有テスト（`tests/slt/m5/types/`）は**本物の PostgreSQL 17 でも通る**ものだけを書く（`tests/run.sh --target pg`）。PG と違う挙動（多次元、`ORDER BY` の配列キーなど）は `onlyif yuzhu` / `skipif yuzhu` で分け、**既知の差**として `10-tests-plan.md` に集めてもらう（本章の 10 節の確認事項と §1.2 が出典）。先頭で `SET TIME ZONE 'UTC'`、`SET DateStyle = 'ISO, MDY'`、`SET bytea_output = 'hex'` を明示する。numeric は `::text` で比べる（sqllogictest の `R` は scale の違いを見落とす）。`gen_random_uuid()` は値を比べず、`IS NOT NULL`・`pg_typeof`・version / variant の桁（`substr(u::text, 15, 1) = '4'`、`substr(u::text, 20, 1) IN ('8','9','a','b')`）で確かめる。

### 7.1 共有テスト（`tests/slt/m5/types/*.slt`）

| ファイル | 内容 |
|---|---|
| `bytea.slt` | 入力の全形式（hex の大文字・空白・空、escape の `\\` `\ooo` と不正な並び 4 種、`'é'`）と、不正入力の `22023` / `22P02`。出力（`bytea_output` の両方。全 256 バイトを `generate_series` + `set_byte` で作って `escape` の出力を確かめる）。演算子（比較・`\|\|`・NULL）、関数（§6.1 の全行と、境界: `substring` の開始 0 / 負 / 大きい長さ、`overlay` の `from 0`、`get_byte` の範囲外、`position` の空）、`encode` / `decode` の 3 形式（base64 の 76 文字の折り返し、不正な入力 4 種）、`convert_to` / `convert_from`（UTF8）、`::text` / `::bytea` のキャスト（`1::bytea` の `42846`）、列への INSERT・`ORDER BY`・`DISTINCT`・`GROUP BY`・`PRIMARY KEY`・`UNIQUE`（`\x00` を含む値、前方一致する値）、`bytea[]` の入出力（要素に `\` を含むときの引用符） |
| `uuid.slt` | 入力の形 5 種と不正な形 8 種、出力の小文字化、比較、`PRIMARY KEY`・`UNIQUE`・`ORDER BY`（B+Tree と Sort が同じ順序）、`gen_random_uuid()`（上のとおり。1000 行生成して `count(DISTINCT)` = 1000）、`uuid[]`、`::text`（`'x'::text::uuid` の `22P02`、`1::uuid` の `42846`）、`DEFAULT gen_random_uuid()` 列 |
| `array_io.slt` | §5.3.1 の入力表の全行、引用符の規則（空文字列・`NULL`・空白・`{` `}` `,` `"` `\` を含む要素の出力）、`NULL` 要素、下限つき（`[0:2]=...`）、`'{}'`、各要素型の入出力（int2 / int4 / int8 / float4 / float8 / numeric / text / varchar / bpchar（空白の保持）/ bool / date / timestamp / uuid / bytea / name / oid / `"char"`）、`::int[]` の要素の入力エラー、構文エラー 12 種と DETAIL（`\set VERBOSITY` 相当は sqllogictest にないので、メッセージの `malformed array literal: ...` までを `statement error` で。DETAIL は Rust のテストで） |
| `array_basic.slt` | `ARRAY[...]`（型の決まり方: `pg_typeof(ARRAY[1,2.5])` = `numeric[]` ほか）、空（`ARRAY[]::int[]`、`ARRAY[]` の `42P18`）、`::T[]` のキャスト（要素型の変換、`typmod` つき: `ARRAY[1.234]::numeric(3,1)[]`、`'{abc}'::varchar(2)[]`）、`=` / `<>`（NULL どうし、下限の違い、長さの違い）、`array_length` ほか §6.3 の全関数と境界（空、NULL、範囲外の次元）、`array_to_string` の NULL の扱い、`\|\|`（任意） |
| `array_any.slt` | `ANY` / `ALL` の三値論理の全組み合わせ（左辺 NULL / 非 NULL × 配列 NULL / 空 / 一致あり / 一致なし / NULL 要素あり × `=` `<>` `<` `>`）、`= ANY('{1,2}')`（unknown の配列リテラル）、`1.5 = ANY(ARRAY[1,2])`（numeric と int の混合）、`x = ANY(ARRAY(SELECT ...))`、`IN` との同値、`WHERE id = ANY(ARRAY[1,2,3])` の結果（実行計画は問わない）、右辺が配列でないときの `42809` |
| `array_subscript.slt` | `a[i]`（範囲内・外・0・負・NULL・`a[1.5]`・`a['2']`）、`int2vector` / `oidvector` の添字（0 始まり）、`('1 2'::int2vector)::int2[]` の出力（`[0:1]={1,2}`）と `array_lower` / `array_upper`、`(1)[1]` の `42804`、スライスと 2 つの添字の `0A000`（`onlyif yuzhu`）、`array_upper(prattrs::int2[], 1)` の形（psql の問い合わせの縮小版） |
| `array_subquery.slt` | `ARRAY(SELECT ...)`（0 行 = `{}`、`ORDER BY`、複数行、NULL を含む、相関、要素型 `name` → `name[]`、`UNION` を含む、2 列の `42601`）、psql の `\d tbl` の 3 本（ポリシー・拡張統計・出版物）を**そのまま**流す（PG 17 の `psql -E` の出力。空の結果でよい） |
| `array_catalog.slt` | `pg_constraint.conkey` / `confkey` を `::text`・`[1]`・`array_length` で読む（PK・UNIQUE・FK）、`pg_type.typarray` と `typelem` の整合（`typarray` を持つ全型で `typelem` が元の型を指す）、`pg_type` の配列型の行（`typname`、`typlen`、`typcategory`、`typalign`、`typinput`、`typreceive`）、`pg_index.indkey`（`int2vector`）が配列として読めること（`indkey[0]`）、ユーザーテーブルの配列列の `0A000`（`onlyif yuzhu`） |
| `numeric_gap.slt` | §5.2 の numeric の穴（`gcd` `lcm` `min_scale` `trim_scale` `width_bucket` `factorial` `@`、関数形式のキャスト、`sqrt` 系の `0A000`（`onlyif yuzhu`）） |
| `bpchar_gap.slt` | §5.2 の bpchar の確認項目（実機の値を期待値にする。M4 の `bpchar.slt` と重ねない: M4 が見ない境界だけ） |
| `psql_d_arrays.slt` | §6.5 の表の配列の式だけを抜き出した縮小版（`oid[] = '{0}'`、`'d' = any("char"[])`、`array(select ...)`、`int2vector` の添字、`aclitem[]` への `array_length`） |

### 7.2 差分コーパス試験（TY-7）

数値（`yuzhu-numeric/tests/pg_corpus.rs`、約 3.5 万行）と日時（`yuzhu-datetime`、約 7 万行）の既存の TSV コーパスは**そのまま維持**する（M4-09 §12.1。numeric の `send` の種別を含む）。TY-7 が足すのは、**bytea・uuid・配列・numeric と bpchar の穴**の分で、**PG が出した値を期待値にした sqllogictest を機械的に作る**（TY-D16）。

**生成器（`tests/tools/typecorpus/`。TY-7 が書く。外部言語に依存しない: psql と bash だけ）**:

```text
tests/tools/typecorpus/
├── gen.sh                 exprs/<name>.txt を読み、PG に流して tests/slt/m5/types/corpus_<name>.slt を書き出す
├── exprs/
│   ├── bytea.txt  uuid.txt  array.txt  numeric_gap.txt  bpchar_gap.txt
├── gen_binary.sh          binary_cases.txt から tests/data/ty_binary.tsv（send のバイト列）を作る
├── binary_cases.txt       型 OID、typmod、テキスト値
├── pgx.pl                 拡張プロトコルでバイナリのパラメータを送る試験用クライアント（Perl の標準モジュールだけ）。recv のエラーの期待値の生成用
└── README.md
```

- **`exprs/*.txt` の書式**: 1 行 1 式（SQL のスカラー式）。`#` で始まる行はコメント。`@set <文>` は以降の式の前に流す文（`SET bytea_output = 'escape'`）、`@reset` で戻す。式の末尾の `-- メモ` は無視。**先頭と末尾に空白が付く値は式の中で `|` を連結して見えるようにする**（`('a'::char(3))::text || '|'`）。改行を含む値は `replace(.., E'\n', '<LF>')` で書く。
- **評価**: `gen.sh` は一時的な PL/pgSQL 関数 `pg_temp.ev(q text) RETURNS text` を作る。`EXECUTE 'SELECT (' || q || ')::text' INTO r` を `EXCEPTION WHEN OTHERS` で包み、成功は `OK:<値>`（NULL は `OK:` + 特別な印）、失敗は `ERR:<SQLSTATE>:<MESSAGE_TEXT>` を返す。1 式ずつ別のトランザクション（`SAVEPOINT` は PL/pgSQL の例外ブロックが内部で使う）。
- **出力（slt）**: 成功は `query T` + `SELECT (<式>)::text` + `----` + 値。NULL は `NULL`、空文字列は `(empty)`。失敗は `statement error <メッセージの正規表現エスケープ>`（sqllogictest-rs は `statement error` にメッセージの一致を使える）。SQLSTATE は slt で照合できないので、**`.tsv` の副産物 `corpus_<name>.sqlstate`（式ごとの期待 SQLSTATE）を Rust の統合テスト `yuzhu-server/tests/ty_corpus_sqlstate.rs` が読み、yuzhu に同じ式を流して SQLSTATE も照合する**。
- **ファイルの扱い**: 生成物（`corpus_*.slt`、`*.sqlstate`、`ty_binary.tsv`）はリポジトリに**コミットする**（PG なしでも yuzhu 側の試験が回る。数値のコーパスと同じ運用）。式の一覧を直したら生成し直してコミットする。差分が出たら（PG のマイナー版による変化）レビューする。

**コーパスの内容と規模**（式の数の目安）:

| ファイル | 式 | 内容 |
|---|---|---|
| `bytea.txt` | 約 400 | 入力の全パターン（hex 0〜20 バイト × 大文字小文字 × 空白、escape の `\ooo` 全部の桁の組み合わせ、不正 40 種）、出力の両モード（0〜255 の全バイト）、関数の境界（`substring` の開始・長さの 7 × 7、`overlay` の 5 × 5 × 5、`get_byte` / `set_byte` / `get_bit` / `set_bit` の端、`encode` / `decode` の各形式 × 長さ 0〜10）、比較（長さの違い・先頭が同じ・`\x00` を含む）、`bytea[]` |
| `uuid.txt` | 約 120 | 入力の形（ハイフンの位置を 0〜8 か所の全組み合わせ、波括弧、大文字）、不正 30 種、比較、`uuid[]` |
| `array.txt` | 約 600 | 入力（引用符・エスケープ・空白・NULL の組み合わせ、要素型 20 種、不正 50 種）、出力、下限、`=` / `<>`、`ANY` / `ALL`（三値論理の全 64 通り）、添字、キャスト、`array_*` 関数の境界、`ARRAY[...]` の型決定、`ARRAY(SELECT)`、`int2vector` の読み替え |
| `numeric_gap.txt` | 約 300 | `gcd` / `lcm`（符号・scale・NaN・Infinity・巨大値・0）、`min_scale` / `trim_scale`（0、末尾のゼロ、負の scale 相当）、`width_bucket`（昇順・降順・境界・エラー 5 種）、`factorial`（0、1、20、170、32177、32178、負）、`@`、関数形式のキャスト |
| `bpchar_gap.txt` | 約 200 | §5.2 の確認項目と、`char(n)` の入力 × typmod × 代入 / 明示 / 比較 / `length` / `octet_length` / LIKE / `~` / `\|\|` / `concat` / `min` / `max` / 多バイト文字 / 空白のみ |

**バイナリのコーパス（`ty_binary.tsv`）**: 行 = `型の OID \t typmod \t テキスト値 \t send の 16 進（PG の `encode(<type>send(v), 'hex')`）`。`yuzhu-core/tests/binary_corpus.rs` が、(1) `input_text` → `output_binary` が期待の 16 進と一致、(2) `input_binary(期待の 16 進)` → `output_text` が元のテキストと一致（正規形）、を全行で確かめる。値は型ごとに 20〜60 個（整数の最小・最大、float の ±0・NaN・±Inf・subnormal、numeric は §6.4 の例と `pg17_corpus.tsv` の `send` の抜粋、text は多バイト、bytea は 0 / 1 / 255 バイト、uuid、日時 5 型は 06 章の境界値、配列は全要素型 × {通常、NULL を含む、空、下限 0、1 要素}）。**`recv` の不正入力**（§6.4 の検査）は `ty_recv_errors.tsv`（型 OID、16 進、期待 `ERR:<SQLSTATE>:<メッセージ>`）。`pgx.pl` で PG に実際に送って期待値を作る。50〜80 行。

### 7.3 Rust の単体テスト

- `types/typeinfo.rs`: `ElemInfo`（typlen / byval / align）が `TYPES` の全要素型の行と一致。`array_type_of(element_type(x)) == x`（Regular 配列の全行）、`array_type_rows()` の全行が PG の `pg_type`（`tools/gen_procs.sh` が生成した定数表）と一致。
- `types/array.rs`: §3.3 のバイト例 4 つを `encode_array` / `decode_array` と `form_tuple` / `deform_tuple`（`storage/heap/tuple.rs`）で往復し、バイト列が一致。1 バイトヘッダ（127 バイト以下）と 4 バイトヘッダ（それ以上。`dataoffset` と整列の確認）の境界。破損したペイロードの `XX001`（検査項目ごとに 1 つ）。`array_in` / `array_out` の表（§5.3.1 の全行 + DETAIL）、**ランダムな要素の文字列（空白・引用符・`\`・`{`・NULL に似た文字列を含む）で `out → in` が恒等**になること（10 万回）。`cmp_array` と `array_eq` と `hash_array` の一致（Equal ⇒ 同じハッシュ）。
- `types/bytea.rs`: 全 256 バイトの hex / escape の往復、入力エラーの全分岐、`encode` / `decode`、RFC 1321 の md5 のテストベクタ（任意）。
- `types/uuid.rs`: 入力の形のハイフン位置の全組み合わせ、`gen_random_uuid` の version / variant ビット（1 万回）。
- `analyzer/polymorphic.rs`: §5.4.1 の規則の表形式のテスト（anyelement の不一致、anyarray に非配列、全 UNKNOWN の `42P18`、anycompatible の共通型、`int2vector` の要素型）。
- `analyzer/array_expr.rs`: §5.4.2〜§5.4.5 の各エラーと型決定（`ARRAY[1,2.5]`、`ARRAY[NULL,NULL]`、`ANY` の右辺が unknown、パラメータの型は 04 章のテスト）。
- `executor/eval_array.rs`: `ScalarArrayOp` の三値論理の全組み合わせ（左辺 NULL / 非 NULL × 配列 NULL / 空 / 要素 {一致, 不一致, NULL} の部分集合 × ANY / ALL）、`ArrayCoerce`（`InOut` と typmod と NULL 要素）、`ArraySubscript`（`int2vector` の下限 0）、`SubLink::Array`（0 行 = 空）。
- `catalog/opclass.rs`: `bytea_ops` / `uuid_ops` の行（OID、strategy、`cmp`）が実機の値と一致、`comparator(family, 17, 17)`・`operator_strategy(1955, 428)` が引ける。
- `catalog/builtin/type_io.rs`: §5.1.1 のチェックリストの機械検査（`TYPES` の全行について recv / send の `PROCS` の行がある、`typarray` が実在する配列型の行を指す、など）。
- `types/typmod.rs`: `typmod_out` の全分岐（実機の値: `numerictypmodout(655366)` = `(10,2)`、`bpchartypmodout(4)` = 空）、配列の要素ごとの `apply_typmod`。

### 7.4 結合・永続・並行

- **再起動**（`tests/restart/` の仕組み）: `bytea` / `uuid` 列を持つ表に行を入れ、停止・起動後に読み返す（`\x00` を含む値、1 バイトヘッダと 4 バイトヘッダの境界の長さ 126 / 127 / 128 バイト、`uuid PRIMARY KEY` の索引で検索）。FK の `conkey`（配列）が再起動後に読める（07 章のテストと共用）。
- **B+Tree**: `uuid PRIMARY KEY` に 1 万行（乱数）を挿入し、`ORDER BY id`（索引順）と `ORDER BY id + 0` 相当（Sort）が同じ順序。`bytea UNIQUE` で `\x00` と `\x0000`、`\x01` と `\x0100` を区別して一意性を検査。キーが `BT_MAX_ITEM_SIZE` を超える bytea は `54000`（M4 の規則）。
- **複数セッション**: `gen_random_uuid()` を 8 接続から同時に 1 万回呼び、重複がないこと。
- **psql**: 実物の psql 17 で `\d plain`（PK のある表）の 9 本が通ること、`\l` が通ること（`tests/compat/psql/`。TS の仕組み。**§6.5 の確認事項の結論しだいで FK のある表の `\d` を足す**）。
- **ドライバ**（`tests/compat/`、04 章の XQ-7 と共用）: tokio-postgres で `bytea` / `uuid` / `int4[]` / `text[]` / `numeric` のバイナリのパラメータと結果の往復、`WHERE id = ANY($1)`（`$1: Vec<i32>`）。

---

## 8. 実装の分担と工数

担当は 00 §7 の WP（TY-1〜TY-7）。工数は AI の実装エージェント 1 本の日数（粗い見積もり）。**TY-5 は 00 の 5 日から 6 日に増やし、さらに SELECT 句の SRF の最小対応（TY-D17。TY-5c）で +0.5 日の 6.5 日とする**（§11 の依頼 8。ポリモーフィック型の解決・式の木の 4 変種・`\d tbl` に要る範囲を含めたため）。合計 17.5 日（00 は 16 日）。

| WP | 内容 | 主なファイル | 依存 | 日数 |
|---|---|---|---|---|
| TY-1 | 型の枠組み: `typeinfo.rs`、`TYPE_PROCS`、`array_type_rows()`、新しい基本型の行（bytea・uuid・ポリモーフィック 3 型）、`PROCS` の行（recv / send / typmod / array 関連）と `tools/gen_procs.sh` の OID の追加、`typmod_out`、関数形式のキャスト（§5.1.5）、`cmp_datum` / `hash_datum` / `rank` の新しい変種（`Bytea` `Uuid` `Array` の分岐。実体は空でよい）、`oid` 定数、`TypeEnv.bytea_output`、`bytea_output` 設定、チェックリストの検査テスト、`pg_type.slt` の更新 | `types/{mod,typeinfo,typmod,datum,hash,io}.rs`、`catalog/builtin/{mod,type_io}.rs`、`catalog/opclass.rs`（行の枠だけ）、`settings.rs`（`bytea_output`）、`tools/gen_procs.sh` | F0 | 3 |
| TY-2 | numeric の穴埋め（§5.2 の表）: 差の洗い出し（`typegap`）、7 関数 + `@` + `sqrt` 系の行、numeric のバイナリの橋渡し（`send` / `recv`）、`numeric_gap` の式の一覧と slt | `types/numeric.rs`、`catalog/builtin/numeric.rs`、`tests/tools/typegap/`、`exprs/numeric_gap.txt` | TY-1、M4（T1 の完了） | 1.5 |
| TY-3 | bpchar の穴埋め: 差の洗い出し、`bpcharcmp` と関数形式のキャスト、`bpchar` のバイナリ、`bpchar_gap` の式の一覧 | `types/bpchar.rs`、`catalog/builtin/text_misc.rs` | TY-1、M4（T2 の完了） | 0.5 |
| TY-4 | bytea・uuid: 入出力、演算子・関数（§6.1、§6.2）、`gen_random_uuid`、opclass の行（`bytea_ops` `uuid_ops`）、ディスク形式（`Kind::Bytea` `Kind::Uuid`。§11 の依頼 3）、`bytea` / `uuid` の slt と式の一覧、バイナリ。任意: `md5` / `sha*`（+0.3 日） | `types/{bytea,uuid,md5}.rs`、`catalog/builtin/text_misc.rs`、`catalog/opclass.rs`、`storage/heap/tuple.rs` の 2 分岐 | TY-1 | 2 |
| TY-5 | 配列: `ArrayValue` とディスク形式（`encode_array` / `decode_array`、`Kind::Array`）、入出力（§5.3）、`cmp_array` / `hash_array`、ポリモーフィック型の解決（§5.4.1）、`ARRAY[...]`・`a[i]`・`ANY` / `ALL`・`::T[]`・`ARRAY(SELECT)` の解析と評価（§5.4、§5.5。構文は `sql/parser/array.rs`）、関数・演算子の行（§6.3）、`int2vector` の読み替え、カタログの `int2[]` 列の移行（§5.6）、`array_*` の slt、`\d tbl` の 3 本の確認。**TY-5c（+0.5 日）: SELECT 句の SRF の最小の書き換え（TY-D17。`pg_partition_ancestors`。レビュー対応 R-28）**。任意: `\|\|`・FROM 句の `unnest`・`string_agg`（M5-TY-Q2） | `types/{array,array_ops}.rs`、`analyzer/{polymorphic,array_expr}.rs`、`executor/eval_array.rs`、`sql/parser/array.rs`、`catalog/builtin/array.rs`、`catalog/rows.rs`・`store.rs` の `conkey` など（§5.6）、`storage/heap/tuple.rs` の 1 分岐、`analyzer/srf_rewrite.rs` ★（TY-5c） | TY-1 | 6.5 |
| TY-6 | バイナリ: §6.4 の全型の `send` / `recv`（既存型の分を含む。日時は 06 章の関数を表に載せるだけ）、配列の `array_send` / `array_recv`、`int2vector` / `oidvector` の配列形式、`ty_binary.tsv` と `binary_corpus.rs`、`ty_recv_errors.tsv`、`pgx.pl` | `types/{bytea,uuid,array,numeric,bpchar}.rs` の `send` / `recv`、既存型のモジュールの `send` / `recv`（`types/io.rs` の隣の `types/wire.rs` ★ に集約してもよい）、`tests/tools/typecorpus/` | TY-1、XQ-4（`binary.rs` の振り分け） | 2 |
| TY-7 | 差分コーパス: `gen.sh`・`exprs/*.txt`・生成物のコミット・SQLSTATE の照合テスト、`gen_binary.sh`、README、`difftest` への配列・bytea・uuid の式の提案（TS へ） | `tests/tools/typecorpus/`、`tests/slt/m5/types/corpus_*.slt`、`yuzhu-server/tests/ty_corpus_sqlstate.rs` | TY-2〜TY-5 | 2 |

**並列化**: TY-1 の最初の 1 日（`oid` 定数・`Datum` の分岐の枠・`TYPE_PROCS` の枠・チェックリストの検査）が済めば、TY-2 / TY-3（numeric・bpchar。M4 の完了が前提）、TY-4（bytea・uuid）、TY-5（配列）、TY-6 は互いにほぼ独立。共有するのは `catalog/builtin/*.rs` の表への行の追加（行を足す場所は TY-1 が空のコメント区画として用意する）と `types/io.rs` の分岐の追加だけ。クリティカルパスは TY-1（3）→ TY-5（6）→ TY-7（2）の約 11 日。FK（07 章）の FK-1 が `Array`（`conkey` など）に依存するので、**TY-5 の `ArrayValue` と `encode_array` / `decode_array`、`from_int2s` / `to_int2s` を最初の 2 日で先に出す**（FK-1 は TY-5 の完了ではなくこの部分に依存する）。

**先に始めてよいもの（00 D49）**: `yuzhu-auth` の `digest`（AU-1 と共同）、`tests/tools/typecorpus/` の生成器と式の一覧（実機の PG だけで作れる）、`typegap` の PG 側の一覧、`bytea.rs` / `uuid.rs` / `array.rs` の**純粋な部分**（`Datum` の変種が F0 で入る前はスタブの型で。`types/` の新規ファイルだけで済む）。

---

## 9. 未検証の点

実装前に確かめること。

| 項目 | 内容 |
|---|---|
| M4 の章との突き合わせ | `06-btree.md`（`OpClass` / `AmProc` の構造体が 00 §11.3 のまま、キーの最大長 `BT_MAX_ITEM_SIZE` と bytea）、`07-catalog-ddl.md`（`pg_constraint.conkey` の書き込みを 1 つの関数に集めているか。§11 の依頼 9）、`04-planner-optimizer.md`（`const_fold` が新しい `ExprKind` の変種を知らないと畳み込まないだけで正しさは保たれるか、物理化の `match` が網羅か）、`03-parser-analyzer.md`（`Expr` の AST に `ARRAY` / 添字 / `ANY (expr)` が既にあるか。M4 は `QuantifiedSubquery` だけを足す）、`05-executor.md`（`SubLinkKind::Array` を `SubPlanDef` が表せるか。メモリ会計の `Datum::Int4Array` の行を `Array` に変える） |
| `SessionInfo` / `ExecCtx` から `TypeEnv` を取る手段 | `FnKind::Env` の評価に `TypeEnv` が要る。M4 05-executor の `EvalCtx.type_env`（D5-4）が `FnKind::Env` の呼び出しに渡せる形か |
| opclass の OID | `pg_opclass.dat` に `oid` の指定がなく、genbki が割り当てる（`bytea_ops` btree = 10006、`uuid_ops` btree = 10065 は PG17.11）。**マイナー版・メジャー版で変わりうる**か（M5-TY-Q6） |
| `\d tbl` の版による違い | psql 17.x の問い合わせは `server_version` で変わる（M2 D11 で 17.0 を名乗る）。実機の psql 17 で取った 12 本が、17.0 と 17.11 で同じか（未検証。差があれば `psql_d_arrays.slt` を直す） |
| `bytea_output` の設定の細部 | 大文字小文字は区別しない（`SET bytea_output = 'ESCAPE'` は通る。確認済み）。`RESET` と `SET LOCAL`、`ALTER ROLE ... SET` の保存は 08 章の範囲 |
| `date_recv` の範囲検査 | `date_recv` が `22008 date out of range` を返すか（**確認済み**: 【実機】PG17.11。06 章 §6.7 と同じ） |
| ポリモーフィック型のエラーの DETAIL | `arguments declared "anyelement" are not all alike` の DETAIL の文言（`integer versus text`）と SQLSTATE（42804 か 42P18 か）。PG:src/backend/parser/parse_coerce.c |
| `width_bucket` の境界 | `floor(count * (op - b1) / (b2 - b1)) + 1` を `checked_mul` / `checked_div` で計算して PG と一致するか（丸めの規則の違いで境界の値が 1 ずれないか）。差分コーパスで確かめる |
| `gcd` / `lcm` の scale と大きな値 | `yuzhu-numeric` の公開 API（`checked_rem`、`digits()`）で PG の結果の dscale が再現できるか。再現できなければ 00 §2 の「numeric クレートの変更なし」を破って小さな関数を足す（M5-TY-Q11） |
| `name` 型の配列の要素 | `name` は `typlen = 64` の固定長（要素は 64 バイト）。`Datum::Text` との往復で 63 バイトを超える値が入らないこと |
| 配列の `typmod` | `varchar(3)[]` の列は M5 では作れないが、`::varchar(3)[]` のキャストの `ArrayCoerce` の `explicit` と typmod の適用順序が PG（`ArrayCoerceExpr` の `resulttypmod`）と一致するか |
| `ARRAY[...]` に NULL リテラルだけ・先頭が NULL | `ARRAY[NULL, 1]` = `int4[]`、`ARRAY[NULL::text, 1]` のエラー、`select_common_type` の UNKNOWN の扱い（M4 の共通型の関数を再利用できるか） |
| `unnest` の FROM 句 | M4 の `FnKind::Set` に引数が配列のポリモーフィックな関数（結果の型が引数から決まる）を載せられるか。`SetFn.begin` が型を受け取らない（`fn(&[Datum])`）ので、`anyelement` を返す `unnest` は結果の列の型を持てるか（M5-TY-Q2 の任意項目の可否） |
| 1 バイトヘッダ付きの配列の読み出し | `deform` が配列の列で `buf[off] == 0` の詰め物判定をしたとき、4 バイトヘッダの配列の先頭バイトが 0（長さが 64 の倍数）になりうる（`(len << 2) & 0xFF == 0`）。M2 §3.6 の規則は varlena 一般に同じなので配列に固有の問題ではないが、**長さ 64・128・192 バイト付近の配列で往復する**テストを足す |

---

## 10. 確認事項

仮決めのまま進める。ID は `M5-TY-Q<n>`（`10-tests-plan.md` が集める）。ディスク形式に関わるものに ★。

### M5-TY-Q1: `ARRAY(SELECT)` と添字をカットラインから外す（TY-D14）

- **仮決め**: TY-5 の必須にする（00 §1.3 のカットラインの最後の項目を外し、代わりに `\|\|`・FROM 句の `unnest` などの任意項目を先に落とす）。
- **理由**: psql 17 の `\d tbl` が**どんな表でも**ポリシーの問い合わせ（`array(select rolname ...)`）と出版物の問い合わせ（`prattrs[s]`）を送り、解析が通らないと `\d tbl` が何も表示しない（実機で確認）。要件の「psql が繋がる」に直結する。
- **変えたい場合の影響**: 落とすと `\d tbl` を M5 の完了条件から外す必要がある（M4 は `\d tbl` を任意としているので契約には反しない）。節約できるのは `ARRAY(SELECT)` と添字で約 1.2 日。`= ANY` と `::int2[]` だけでは `\d tbl` は通らない。

### M5-TY-Q2: `\d tbl` の配列以外の要素（`string_agg`、SELECT 句の SRF、`\d+`）

- **仮決め**: (1) `string_agg(text, text)`（OID 3538）を TY-5 が足す（+0.5 日。M4 の `AggKind::StringAgg` と executor の実体）。(2) **SELECT 句に SRF が 1 つだけの `SELECT srf(args)` を FROM 句の形に書き換える**最小の対応を **M5 に足す（TY-D17、TY-5c。+0.5 日。持ち主は TY。`pg_partition_ancestors`・`unnest`・`generate_subscripts` が使える）**。足さなければ、**FK を持つ表・FK から参照される表の `\d tbl` は M5 で動かない**（その場合は 07 の完了条件 9 と 10 §7.3 の 9 を外す）。(3) `\|\|`（配列）と FROM 句の `unnest` は `\d+` 用の任意項目（合計 +1 日）。
- **理由**: §6.5。FK は M5 の目玉で、`\d` で制約を確かめるのは最初にやることだが、そのとき psql が SELECT 句の SRF を送る。00 D25 が「SELECT 句の SRF は M6」としたのは `\d` のこの用途を見落としていた。
- **変えたい場合の影響**: (1) を入れないと `\d tbl` が全滅（出版物の問い合わせが解析で落ちる）。(2) を入れないと FK のある表だけが落ちる。(3) は `\d+` が使えないだけ。

### M5-TY-Q3: 多次元配列は `0A000`（TY-D2）

- **仮決め**: 値・ディスク形式・入出力・バイナリは多次元に対応した書き方にして、入口で `M5_MAX_NDIM = 1` を超えるものを `0A000` にする。
- **理由**: 00 D25。多次元が要るのは `ARRAY[[..]]`、2 つ以上の添字、スライス、`unnest` の多次元の平坦化など、M5 の範囲外の機能の組み合わせ。
- **変えたい場合の影響**: `M5_MAX_NDIM` を 6 にし、`ARRAY[[..]]` のコンストラクタ（各部分配列の次元の一致検査）と添字の複数指定を足す（約 +1 日）。入出力・`array_eq`・ディスク・バイナリは変更なし。

### M5-TY-Q4: ユーザーテーブルの配列列は `0A000`（TY-D4）

- **仮決め**: `0A000 array types are not supported yet`（M4 の `int2[]` と同じ文言）。ディスク形式は PG と同じで、`ALTER`・`CREATE TABLE` の 1 か所の検査を外せば列を作れる。
- **理由**: 00 D25（M5-Q8）。列にするなら比較・ハッシュ・`ORDER BY` / `GROUP BY` / `DISTINCT`・`array_ops` の opclass（`<` 系の演算子 4 つ、`btarraycmp`）が要る。
- **変えたい場合の影響**: 配列の列を許す（検査を外す）+ 演算子 `<` `<=` `>` `>=`（OID 1072〜1075、本体は `cmp_array`）と opclass `array_ops`（btree 397）、`unnest`・`array_agg` などの関数群が欲しくなる。約 +3 日（比較系だけなら +1.5 日）。Rails・Django の `ARRAY` 列は M6 まで使えない。

### M5-TY-Q5: 配列の `ORDER BY` / `GROUP BY` / `DISTINCT` は `42883`

- **仮決め**: `<` 系の演算子と opclass がないので、PG と違い `42883 could not identify an ordering operator for type integer[]` を返す（`cmp_datum` は全順序で書くので、`DISTINCT` / `GROUP BY` は演算子さえ足せば動く）。
- **理由**: M5-TY-Q4 と同じ。M5 で配列を並べ替えるのはカタログの列くらい。
- **変えたい場合の影響**: M5-TY-Q4 の費用に含まれる。

### M5-TY-Q6: opclass の OID と `pg_amproc` の行

- **仮決め**: `bytea_ops` btree の opclass OID は 10006、`uuid_ops` は 10065（PG17.11 の実機の値）。`pg_amproc` の support 2（`bytea_sortsupport`）と support 4（`btequalimage`）は入れない（M4 の `AmProc` は support 1 だけ）。ハッシュの opfamily は入れない。
- **理由**: `pg_opclass.dat` が `oid` を指定せず、genbki が連番で決める。共有テストが opclass の OID を比べることはまずない。
- **変えたい場合の影響**: OID を実装時に `pg_opclass` から読み直す（`tools/gen_procs.sh` に opclass の行を足す。0.2 日）。support 2 / 4 を入れるには `AmProc.support` の値の拡張（M4 の 06 章）。`pg_amproc` の行数が PG と違うのは既知の差。

### M5-TY-Q7: `md5` と `sha*` は任意項目

- **仮決め**: TY-4 の末尾の任意項目（+0.3 日）。`md5` は手書き（`types/md5.rs`）、`sha224`〜`sha512` は `yuzhu-auth` に `digest` を足してもらう（§11 の依頼 5）。
- **理由**: ORM・アプリが `md5(...)` / `sha256(...)` を直接 SQL で呼ぶことはあるが、psql・pg_dump・ドライバは使わない。
- **変えたい場合の影響**: 落とすと `42883 function md5(unknown) does not exist`（PG は動く）。

### M5-TY-Q8: `convert_to` / `convert_from` は UTF8 だけ

- **仮決め**: `UTF8`（`utf8`・`UTF-8`・`UTF_8`）だけ。他の既知の符号化名は `0A000`、未知の名前は `22023`。
- **理由**: yuzhu のデータベースの符号化は UTF8 だけで、他の符号化への変換表を持たない。
- **変えたい場合の影響**: `LATIN1`（ISO-8859-1 は 1 対 1 で安い）と `SQL_ASCII` を足すのは約 +0.2 日。他は変換表が要る（M6 以降）。

### M5-TY-Q9: `FnKind::Env` を足す

- **仮決め**: `FnKind` に `Env(fn(&[Datum], &TypeEnv<'_>) -> Result<Datum>)` を足す（M4-09 の `CastMethod::Env`（P-3）と同じ形）。`array_to_string` が使う。
- **理由**: 要素の出力が DateStyle・`bytea_output`・regclass の名前に依存する。`Context(.., &SessionInfo)` に `TypeEnv` を含めさせる案もあるが、`TypeEnv` を `SessionInfo` から作る経路が M4 になく、`Cast(Env)` と対にした方が揃う。
- **変えたい場合の影響**: 足さないと `array_to_string` は日時・bytea の要素で `DateStyle` / `bytea_output` を無視する（`Pure` で既定の環境を使う）。別案（`Context` に寄せる）は `SessionInfo` に `type_env()` を足す変更になる。

### M5-TY-Q10: `int2vector` / `oidvector` は yuzhu 独自の符号化のまま

- **仮決め**: 値は `Datum::Int2Vector` / `OidVector`（PG の配列ヘッダを持たない）。`anyarray` を取る関数・添字・キャストでだけ、下限 0 の `ArrayValue` に読み替える（TY-D1）。`pg_index.indkey` などの列のディスク形式は M4 のまま。
- **理由**: M4 が済ませたディスク形式を変えない。PG のディスク形式との互換は元々ない（M2-Q2）。
- **変えたい場合の影響**: `int2vector` も `ArrayType` にするとカタログの列（`indkey` `indclass` `indoption` `proargtypes` ほか）の符号化とすべての読み出し箇所（インデックス・制約の読み込み）が変わる（約 +1.5 日と `catalog_version` の影響）。得るものは「配列が 1 系統になる」ことだけ。

### M5-TY-Q11: numeric の関数を `yuzhu-numeric` の公開 API だけで書く

- **仮決め**: `gcd` `lcm` `min_scale` `trim_scale` `width_bucket` `factorial` は `types/numeric.rs` に書き、`yuzhu-numeric` を変えない（00 §2「M5 での変更なし」）。
- **理由**: 差分コーパスを持つクレートに手を入れず、M4 の統合の責任範囲を保つ。
- **変えたい場合の影響**: 再現できない境界（`gcd` の dscale、`width_bucket` の丸め）が出たら、クレートに小さな関数を足して差分コーパスにも種別を足す（+0.5 日）。

### M5-TY-Q12: 差分コーパスは式の一覧から生成した sqllogictest（TY-D16）

- **仮決め**: §7.2。生成物をコミットする。
- **理由**: サンドボックスに Python がない。式をそのまま PG と yuzhu の両方で流せ、解析・演算子解決・出力まで確かめられる。
- **変えたい場合の影響**: 既存の TSV 形式（`gen_corpus.py`）に揃えると、Rust の橋渡し層だけを確かめる試験になり、解析の差を見逃す。Python を使える環境（ホスト）で TSV を作る案は、生成器が 2 系統になる。

### M5-TY-Q13: `numeric` の `sqrt` 系の行だけを入れて `0A000` にする（TY-D15）

- **仮決め**: 行（OID は PG17）を入れ、本体は `0A000`。
- **理由**: 行がないと numeric の引数が float8 に暗黙変換され、**結果の型が黙って float8 になる**（PG は numeric）。
- **変えたい場合の影響**: 行を入れないと `sqrt(2.0)` が float8 `1.4142135623730951` を返す（PG は numeric `1.414213562373095`）。本体を実装すると D23 の +M（結果の scale の規則の移植）。

---

## 11. 契約への変更依頼

00（と M4 の契約）に従えない点、足したい点。**F0 が反映するか、持ち主の章が受けるか**を決めてほしい。

| # | 依頼 | 持ち主 | 理由・影響 |
|---|---|---|---|
| 1 | `expr/mod.rs` に `ExprKind::{ArrayCtor, ArraySubscript, ArrayCoerce, ScalarArrayOp}` と `SubLinkKind::Array` を足す（§4.4）。00 §4.9 は `ExternParam` 以外の変種を足していない | F0（変種）、`expr/walk.rs` / `deparse/` / `explain/` / `planner` の `match` の各担当、`executor/eval.rs`（1 行で `eval_array` を呼ぶ） | TY-D13。`InList` と同じ扱い。TY は解析と評価の本体を書く。**変種の追加を F0 の最初の作業（Datum・Statement の変種と一緒）にする**と、TY-5 が他の章を待たない |
| 2 | `catalog/mod.rs` の `FnKind` に `Env(fn(&[Datum], &TypeEnv<'_>) -> Result<Datum>)` を足す（§4.5。M5-TY-Q9）。評価（`executor/eval.rs`）と定数畳み込み（しない）が対応 | F0（変種）、M4 の eval の持ち主 | `CastMethod::Env`（M4-09 P-3）と対 |
| 3 | `storage/heap/tuple.rs` の `Kind` に `Bytea` / `Uuid` / `Array` を足す（`kind_of` の `NullOnly` から `TEXT_ARRAY` `INT2_ARRAY` `OID_ARRAY` `CHAR_ARRAY` を外す）。`storage/mod.rs` の `AttrDesc::from_type` が `typlen` / `typbyval` / `typalign` を `TYPES`（配列型の行を含む）から引く | `storage/heap/tuple.rs` の持ち主（M4 の H4。M5 では RW か VC のどちらか。00 §8 に記載がない）→ **TY に分岐の追加（各 1 型 1 分岐）を許す** | 3 つの型で 1 分岐ずつ。ディスク形式の本体は `types/*.rs` の `encode_*` / `decode_*`（M4-09 §3.5 の方式） |
| 4 | 解析の新しいファイル `analyzer/polymorphic.rs`、`analyzer/array_expr.rs`、構文の新しいファイル `sql/parser/array.rs`（`ARRAY[...]`、`ARRAY(SELECT)`、`expr[...]`、`ANY (expr)` / `ALL (expr)`、`T[]` の型名の `ARRAY` キーワード形）を TY の持ち分にする。`analyzer/resolve.rs`（M2 の `is_polymorphic` と `ANYARRAY` の分岐を `polymorphic::is_consistent` に置き換える）、`analyzer/coerce.rs`（`find_coercion_pathway` の配列の分岐、`coerce_type` の配列リテラル）、`analyzer/expr.rs`（式の振り分けの 4 行）、`sql/ast.rs`（`Expr` に `ArrayConstructor` / `ArraySubquery` / `Subscript` / 配列の `Quantified` の変種）の**小さな編集を TY に許す** | F0（AST の変種）、M4 の N 担当（上記 3 ファイル） | 00 §8 の表に解析の配列の持ち主がない |
| 5 | `yuzhu-auth` に `pub mod digest { pub fn sha224 / sha256 / sha384 / sha512(data: &[u8]) -> Vec<u8> }`（任意項目。M5-TY-Q7） | AU-1 | D29（`yuzhu-core` は暗号クレートに直接依存しない） |
| 6 | 00 §8 の持ち主の表に追加: `types/{bytea,uuid,array,array_ops,typeinfo,md5,wire}.rs`、`types/typmod.rs`（M4 の T2 から TY へ）、`types/hash.rs` の新しい変種の分岐、`catalog/builtin/type_io.rs`、`tests/tools/typecorpus/`・`tests/tools/typegap/`、`tests/slt/m5/types/` | 00 | TY の範囲を明確にする |
| 7 | `error.rs` の SQLSTATE（00 §3.5 に未掲載のもの）: `ARRAY_SUBSCRIPT_ERROR` 2202E、`SUBSTRING_ERROR` 22011、`CHARACTER_NOT_IN_REPERTOIRE`（`22021`）、`NAME_TOO_LONG` 42622、`INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION` 2201G、`CANNOT_COERCE` 42846、`WRONG_OBJECT_TYPE` 42809（RW が足すなら重複させない） | TY が足す（00 §8 の例外） | §6.6 |
| 8 | 00 §7 の TY-5 を 5 日から **6.5 日**（6 日 + SELECT 句の SRF の最小対応 TY-5c の 0.5 日。TY-D17）に、合計を 16 日から **17.5 日**に | 00 | §8 |
| 9 | **M4 の章（07-catalog-ddl の C1）への提案**: M4 が `pg_constraint.conkey` / `confkey` を書く箇所を、`Datum::Int2Vector(..)` を直接作らず 1 つの関数（例: `catalog::rows::int2_array(&[i16]) -> Datum`）に集める。M5 の移行（§5.6）が 1 か所の置き換えで済む | M4 の C1 | M5 の作業を小さくする |
| 10 | 00 §3.6 の設定の表: `bytea_output` の持ち主は TY（`settings.rs` に行を足す）。値は大文字小文字を区別しない列挙（実機で確認） | 00 | TY-D6 |
| 11 | 00 §4.9: `types/binary.rs` の `supports_binary` を「§6.4 の表の型」とする（`aclitem` `anyarray` `record` `cstring` `internal` と NULL 専用の配列は false）。`input_binary` の「長さの過不足」は `08P01`（不足）/ `22P03`（余り）を、型ごとの `binary_recv` が返すのではなく、`RecvBuf`（不足は `get_*` が 08P01）と `input_binary` の `finish()`（余りは 22P03）が決める（§4.6、04 §4.4。00 §4.9 に反映済み。レビュー対応 R-04） | 04 章（XQ-4） | 実機の挙動（`ReceiveFunctionCall`）と一致させる |
| 12 | 04 章へ: `x = ANY($1)` のパラメータは `array_type_of(演算子の右の型)` に推論する（§5.4.3 の 3.）。`input_binary` の配列は要素の `recv` を `binary.rs` に戻る形（相互再帰）で呼ぶ | 04 章（XQ-3、XQ-4） | 境界 |
| 13 | 00 §1.3 のカットラインの最後（「TY-5 の `ARRAY(SELECT)` と添字」）を外す（TY-D14、M5-TY-Q1）。落とす順は、TY-5 の任意項目（`\|\|`・FROM 句の `unnest`・`string_agg`）→ `md5` / `sha*`（TY-4 の任意項目）→ `convert_to` / `convert_from` の順にする | 00 | `\d tbl` の完了条件。00 §1.3 の記述を直す |
| 14 | 06 章へ: 日時 5 型の `send` / `recv` の固定値は §6.4 の表を使う。`ElemInfo`（§4.1）に `time`（8 / t / d）と `interval`（16 / f / d）を足す（`typeinfo.rs` は `TYPES` の行から引くので、行が入れば自動） | 06 章（TD） | 境界 |
| 15 | 00 D23: numeric の `sqrt` / `exp` / `ln` / `log` / `log10` / `power` / `pow` は、numeric 版の行だけ入れて本体を `0A000` にする（TY-D15）。**numeric の引数（`sqrt(2.0)`）は float8 版に暗黙変換されず `0A000` になる**。00 D23 の元の文面（「numeric 以外の引数の `sqrt(2)` は float8 で動く」）は、整数・float8 の引数についてだけ正しい。**00 D23 は R-34 でこの形に直した** | 00（反映済み） | PG は numeric を返すので、float8 を黙って返すより安全。M4 の `^` と同じ扱い。ドライバや ORM が numeric に `sqrt` を使う場合は M6 まで動かない（M5-TY-Q13） |
| 16 | 00 §7 / D25: SELECT 句の SRF の最小対応（TY-5c。TY-D17）。00 D25 に例外を足し、TY-5 を 6.5 日にした（依頼 8 の更新） | 00（反映済み） | psql 17 の `\d tbl`（FK のある表）。R-28 |
