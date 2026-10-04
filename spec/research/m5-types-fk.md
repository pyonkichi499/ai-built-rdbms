# yuzhu M5 調査: 型の追加（numeric・日付時刻・char(n)・bytea・uuid・配列）と FOREIGN KEY

調査日: 2026-10-04。対象は PostgreSQL **REL_17_STABLE**。ソースは `raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/...` から取得して読んだ。挙動は **PostgreSQL 17.11（Docker の `postgres:17`、TimeZone=Etc/UTC、DateStyle=ISO, MDY）** に実際に SQL を投げて確かめた。

記号の意味:

- 【確認】今回ソースを読んだ、または PG17 に SQL を投げて確かめた事実
- 【記憶】過去の知識に基づく記述。今回は細部まで照合していない（**未検証**）
- 【提案】yuzhu への推奨（PostgreSQL の事実ではない）

工数の目安（実装エージェント 1 体あたり）: **S** = 1 日以内、**M** = 2〜5 日、**L** = 1〜2 週間。

参照した主なソース（すべて REL_17_STABLE）:

| ファイル | URL |
|---|---|
| numeric.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/numeric.c> |
| numeric.h | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/utils/numeric.h> |
| datetime.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/datetime.c> |
| timestamp.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/timestamp.c> |
| datatype/timestamp.h | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/datatype/timestamp.h> |
| utils/datetime.h | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/utils/datetime.h> |
| varlena.c（bytea） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/varlena.c> |
| varchar.c（bpchar） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/varchar.c> |
| uuid.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/uuid.c> |
| ri_triggers.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/ri_triggers.c> |
| tablecmds.c（FK の作成） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/commands/tablecmds.c> |
| src/timezone/（IANA tz コードの同梱版） | <https://github.com/postgres/postgres/tree/REL_17_STABLE/src/timezone> |

ドキュメント:

- 8.1 Numeric Types: <https://www.postgresql.org/docs/17/datatype-numeric.html>
- 8.3 Character Types: <https://www.postgresql.org/docs/17/datatype-character.html>
- 8.4 Binary Data Types: <https://www.postgresql.org/docs/17/datatype-binary.html>
- 8.5 Date/Time Types: <https://www.postgresql.org/docs/17/datatype-datetime.html>
- 8.12 UUID Type: <https://www.postgresql.org/docs/17/datatype-uuid.html>
- 8.15 Arrays: <https://www.postgresql.org/docs/17/arrays.html>
- 9.9 Date/Time Functions: <https://www.postgresql.org/docs/17/functions-datetime.html>
- 付録 B Date/Time Support: <https://www.postgresql.org/docs/17/datetime-appendix.html>
- 5.5.5 Foreign Keys: <https://www.postgresql.org/docs/17/ddl-constraints.html#DDL-CONSTRAINTS-FK>
- CREATE TABLE: <https://www.postgresql.org/docs/17/sql-createtable.html>
- 53.13 pg_constraint: <https://www.postgresql.org/docs/17/catalog-pg-constraint.html>

---

## 0. 結論（推奨の要約）

| 項目 | 推奨 | 工数 |
|---|---|---|
| numeric | PG と同じ **base-10000 の可変長表現**を手書きする。scale の規則（加減算 = max、乗算 = 和、除算 = `select_div_scale`）、四捨五入（0 から遠い方へ）、NaN/±Infinity、typmod まで PG と一致させる。ディスク形式も PG の short/long ヘッダ形式に合わせる。sqrt/exp/ln/power は後回し | **L**（基本演算と I/O で M、集約・キャスト・バイナリ形式込みで L） |
| **numeric の前倒し** | 小数リテラル（`1.5`）が numeric 型であること、`avg(int)` や `sum(int8)` の結果が numeric であることから、**M4（集約）までに numeric の核（I/O・四則・比較・キャスト）を入れる**ことを強く推奨 | — |
| date / timestamp / timestamptz / interval | 内部表現は PG と同じ（2000-01-01 起点のマイクロ秒、interval は month/day/µs の 3 つ組）。入力は PG の **トークン分解 + デコード方式**を縮小移植。出力は ISO のみ。`time` も入れる（安い）。`timetz` は入れない | **L** |
| タイムゾーン | **Linux のシステム tzdata（`/usr/share/zoneinfo` の TZif ファイル）を手書きのパーサで読む**。同梱はしない。見つからなければ UTC と固定オフセットだけで動く。略称（PST, JST など）は PG の Default セットの一部を静的表で持つ | **M** |
| DateStyle / IntervalStyle | 出力は `ISO` と `postgres` だけ。入力の日付順（MDY/DMY/YMD）は受け付ける。それ以外の出力形式への SET は 0A000 | S |
| char(n) / bpchar | PG と同じく**空白で埋めて保存**し、比較・長さ・text への変換では末尾空白を無視する | **S〜M** |
| bytea | hex 出力（`\x...`）と hex/escape 両方の入力。`bytea_output = escape` も安いので対応 | **S** |
| uuid | 対応する（ORM が多用する）。16 バイト固定長。`gen_random_uuid()` も入れる | **S** |
| 配列 | **M5 では一般の配列型は入れない**。ただしカタログ列（`conkey int2[]` など）の出力と、ドライバが使う `= ANY(ARRAY[...])` / `= ANY($1)` のための**1 次元配列の最小実装**は入れる（配列の本格対応は M6 以降） | **M**（最小実装） |
| FOREIGN KEY | PG の RI トリガー方式は採らず、**executor に組み込んだ参照整合性チェック**にする。ただしカタログ（`pg_constraint`）、エラー文言、SQLSTATE、チェックのタイミング（文末）、NO ACTION と RESTRICT の違いは PG と一致させる。ON DELETE/UPDATE の CASCADE / SET NULL / SET DEFAULT / RESTRICT / NO ACTION、MATCH SIMPLE/FULL に対応 | **L** |
| DEFERRABLE | **対応しない**。`DEFERRABLE` / `INITIALLY DEFERRED` は 0A000。`NOT DEFERRABLE` / `INITIALLY IMMEDIATE` は受け付ける | S |
| 複数ライター下の FK | PG は親行に `FOR KEY SHARE` の行ロックを取る。yuzhu は **M5 の複数ライター設計で行ロック（lock-only xmax）を入れるならそれに乗る**。入れないなら「キー値単位のロック表」で代用する（§9.5） | M（行ロック本体は別調査の範囲） |

---

## 1. 範囲と、他のマイルストーンとの依存

### 1.1 M1〜M4 の現状と、前倒しが必要なもの

- 現在の実装は、小数リテラル `1.5` を「`type numeric is not supported yet (write the value as '1.5'::float8)`」の 0A000 で拒否している（`impl/rust/crates/yuzhu-core/src/analyzer/expr.rs`）。PG では **小数リテラルは numeric**、int8 に収まらない整数リテラルも numeric【確認: `research-pg-types.md` §4、PG17 で `pg_typeof(1.5)` = numeric】。
- PG の集約の結果型【確認: PG17 で `avg(int4)` が `1.6666666666666667` を返す】:
  - `avg(int2/int4/int8)` → **numeric**、`avg(numeric)` → numeric、`avg(float8)` → float8
  - `sum(int2/int4)` → int8、`sum(int8)` → **numeric**、`sum(numeric)` → numeric
- したがって **M4 の集約で `avg` を PG 互換にするには numeric が要る**。また `tests/` の SLT は PG を正解とするので、小数リテラルを含むテストは numeric がないと書けない。
- 【提案】numeric を 2 段に分ける。
  - **numeric-core（M4 の前半で入れる）**: 型 OID 1700、テキスト I/O、typmod、四則と `%`、比較、ハッシュ、整数・float との相互キャスト、`round/trunc/abs/ceil/floor/sign`、`sum/avg` 用の加算と除算。工数 M。
  - **numeric-full（M5）**: バイナリ送受信（Extended Query 用）、`sqrt/power/exp/ln/log`、`div/mod/scale/min_scale/trim_scale/width_bucket` など残りの関数。工数 M。
- 日付時刻型は M5 のまま。ただし `now()` / `current_timestamp` は psql やアプリが接続直後に使うことがあるので、日付時刻の作業は M5 の早い時期に置く。

### 1.2 M5 の他の作業との関係

- **Extended Query（M5）**: ドライバはバイナリ形式で値を要求することがある（pgx は多くの型で既定がバイナリ、JDBC は timestamp などでバイナリを使う場合がある【記憶】）。この文書で追加する型は、**テキスト形式とバイナリ形式の両方**を実装する前提で工数を見積もる（各型の節にバイナリ形式を書いた）。
- **複数ライター（M5）**: FK のチェックは並行性の扱いが要になる（§9.5）。
- **Repeatable Read（M5）**: FK のチェックは RR でも「最新のスナップショット」で行い、トランザクションのスナップショットと食い違えば 40001 にする（§9.5）。
- **カタログ**: 型を追加するたびに `pg_type`・`pg_cast`・`pg_operator`・`pg_proc` の静的表（`catalog/builtin.rs`）に行を足す。OID は PG と同じにする（§10）。

---

## 2. numeric

### 2.1 PG の内部表現【確認: numeric.c、numeric.h】

- 演算用の形式 `NumericVar`:
  - `ndigits`（桁数）、`weight`（先頭の桁の重み。値 = Σ digits[i] × NBASE^(weight − i)）、`sign`（`NUMERIC_POS 0x0000` / `NUMERIC_NEG 0x4000` / NaN など）、`dscale`（**表示用の小数点以下の 10 進桁数**）、`digits`（`int16` の配列、各要素 0..9999）
  - `NBASE = 10000`、`DEC_DIGITS = 4`（numeric.c にはデバッグ用に NBASE=10/100 の分岐もあるが、既定は 10000）
- 値の大きさの上限（numeric.h）:
  - `NUMERIC_MAX_PRECISION = 1000`（typmod の p の上限）、`NUMERIC_MIN_SCALE = -1000`、`NUMERIC_MAX_SCALE = 1000`
  - `NUMERIC_MAX_DISPLAY_SCALE = 1000`、`NUMERIC_MAX_RESULT_SCALE = 2000`
  - `NUMERIC_MIN_SIG_DIGITS = 16`（除算などの結果に最低限保証する有効桁数）
  - 格納形式での上限: `weight` は int16（`NUMERIC_WEIGHT_MAX = PG_INT16_MAX`）、`dscale` は 14 ビット（`NUMERIC_DSCALE_MAX = 0x3FFF = 16383`）。つまり「小数点の前 131072 桁、後 16383 桁」（docs 8.1.2 の記述と一致）。
- ディスク上の形式 `NumericData`（varlena）:
  - **short 形式**（ヘッダ 2 バイト）: `n_header` の上位 2 ビットが `10`（`NUMERIC_SHORT 0x8000`）。ビット 13 = 符号（`0x2000`）、ビット 7〜12 = dscale（0..63、`NUMERIC_SHORT_DSCALE_MASK 0x1F80`）、ビット 6 = weight の符号（`0x0040`）、ビット 0〜5 = weight（-64..63）。続いて digits。
  - **long 形式**（ヘッダ 4 バイト）: `n_sign_dscale`（uint16: 上位 2 ビットが符号、下位 14 ビットが dscale）+ `n_weight`（int16）+ digits。
  - **特殊値**: `0xC000` = NaN、`0xD000` = +Infinity、`0xF000` = -Infinity（ヘッダだけで digits なし。±Infinity は PG14 以降）。
  - 格納前に**先頭と末尾のゼロの桁を落とす**（正規化）。ゼロは `ndigits = 0`。【記憶: `make_result` → `strip_var` 相当の処理】
- 【提案】yuzhu も **ディスク形式を PG と同じにする**。M2 の方針（タプルのサイズを PG と一致させる。`m2-page-heap.md` §2.5）に合うし、形式自体は単純（ヘッダの分岐が 1 つあるだけ）。typalign は `i`、typlen は -1、typstorage は `m`【記憶】。

### 2.2 typmod【確認: numeric.c `make_numeric_typmod`、PG17 で確認】

- `numeric(p, s)` の typmod = `((p << 16) | (s & 0x7ff)) + 4`。例: `numeric(10,2)` = 655366、`numeric(4,1)` = 262149（PG17 の `pg_attribute.atttypmod` で確認）。
- `numeric(p)` は s = 0。`numeric` だけなら -1（無制限）。
- p は 1..1000。範囲外は **22023** `NUMERIC precision 1001 must be between 1 and 1000`【確認】。s は -1000..1000（PG15 以降は負の scale も可。`1.23::numeric(5,-1)` = `0`、`123::numeric(2,-2)` = `100`【確認】）。
- 適用（`apply_typmod`）: まず s 桁に**四捨五入**し、その後、整数部の桁数が p − s を超えたら **22003** `numeric field overflow`、DETAIL `A field with precision 4, scale 2 must round to an absolute value less than 10^2.`【確認】。
  - NaN は typmod 付きの列にも入る。±Infinity は入らない（DETAIL `A field with precision 4, scale 1 cannot hold an infinite value.`）【確認】。
  - numeric の typmod の適用は、明示キャストでも代入でも同じ（切り詰めではなく丸め + エラー）。この点が varchar（明示キャストは切り詰め）と違う。
- 型名の表示（`format_type`）: `numeric(10,2)`、`numeric`。

### 2.3 テキスト入力（`numeric_in`）【確認: PG17 で挙動確認 / 細部は記憶】

- 前後の空白を許す（`'  1.230 '` → `1.230`）。
- 形式: `[+-]digits[.digits][e[+-]digits]`、`.5`、`5.` も可。
- 特殊値: `NaN`、`Infinity`、`inf`、`+inf`、`-Infinity`、`-inf`（大文字小文字を区別しない）。
- **dscale は入力の小数点以下の桁数**（指数を反映した後）。`'1.230'` → dscale 3（末尾ゼロを保持）、`'1e3'` → `1000`（dscale 0）、`'1.0e-20'` → `0.000000000000000000010`（dscale 21）【確認】。
- PG16 以降: `'0x1F'` → 31、`'1_000.5'` → 1000.5 のように、16/8/2 進の整数表記とアンダースコア区切りも受け付ける【確認】。【提案】M5 では受け付けなくてよい（22P02 を返す）。SQL のリテラルとしての `0x1F` は字句解析で扱う（M1 で対応外にしたまま）。
- 不正な入力は **22P02** `invalid input syntax for type numeric: "..."`。

### 2.4 テキスト出力（`numeric_out` → `get_str_from_var`）【確認: PG17】

- **常に指数を使わない固定小数点**で、ちょうど dscale 桁の小数部を出す。`1e100::numeric` は 1 の後に 0 が 100 個並ぶ【確認】。`1.50 * 2.25` は `3.3750`【確認】。
- 負のゼロはない（`-0.0::numeric` は `0.0`）【確認】。
- 特殊値は `NaN`、`Infinity`、`-Infinity`。
- 指数表記は `to_char` や `numeric_out_sci`（内部用）でだけ使う。

### 2.5 演算と結果の scale【確認: numeric.c / PG17 の出力で確認】

| 演算 | 結果の dscale | 例（PG17 で確認） |
|---|---|---|
| `+` `-` | max(s1, s2) | `0.1 + 0.2` = `0.3` |
| `*` | s1 + s2（上限 `NUMERIC_DSCALE_MAX`） | `1.50 * 2.25` = `3.3750` |
| `/` | `select_div_scale`（下記） | `1.0/3` = `0.33333333333333333333`、`10::numeric/4` = `2.5000000000000000`、`100::numeric/3` = `33.3333333333333333` |
| `%`、`mod` | max(s1, s2)【記憶】 | `7.0 % 2.5` = `2.0`、`mod(-7.5, 2)` = `-1.5`（被除数の符号） |
| 単項 `-`、`abs` | そのまま | |
| `round(x, n)` / `trunc(x, n)` | n | `trunc(1.999, 2)` = `1.99` |
| `round(x)` | 0 | `round(2.5)` = `3`、`round(-2.5)` = `-3` |

- **`select_div_scale` のアルゴリズム**【確認: numeric.c 9831 行付近】:

```
weight1, firstdigit1 = 被除数の最初の非ゼロ桁の重みと値（ゼロなら 0, 0）
weight2, firstdigit2 = 除数の同じもの
qweight = weight1 - weight2
if firstdigit1 <= firstdigit2: qweight -= 1      // 商の重みの推定
rscale = NUMERIC_MIN_SIG_DIGITS(16) - qweight * DEC_DIGITS(4)
rscale = max(rscale, s1, s2, 0)
rscale = min(rscale, NUMERIC_MAX_DISPLAY_SCALE(1000))
```

  例: `1/3` は weight1 = weight2 = 0、firstdigit 1 ≤ 3 なので qweight = -1、rscale = 16 + 4 = 20 → `0.33333333333333333333`（20 桁）。`100/3` は qweight = 0 − 0 で firstdigit 100 > 3 なので qweight = 0、rscale = 16 → 16 桁。

- 除算は rscale 桁まで計算して**四捨五入**する（PG の `div_var` は余分な桁まで正確に求めてから丸める【記憶】）。
- **丸めは 0 から遠い方へ（round half away from zero）**（`round_var`）。`round(2.5) = 3`、`round(-2.5) = -3`、`2.5::numeric(3,0) = 3`、`(-2.5)::numeric(3,0) = -3`【確認】。float8 の `round(2.5::float8)` は `2`（偶数丸め。libc の rint）なので混同しない【確認】。
- ゼロ除算（`/`、`%`、`div`）は **22012** `division by zero`。`'Infinity'::numeric / 0` もエラー【確認】。
- 特殊値の算術【確認】: `Inf − Inf = NaN`、`Inf × 0 = NaN`、`1 / Inf = 0`。NaN を含む演算は NaN。
- 比較【確認】:
  - 値で比較し、scale は無視する: `1.10 = 1.1` は真。**ハッシュも同じ値なら同じ**にする（`hash_numeric(1.10) = hash_numeric(1.1)`）。末尾ゼロを落としてからハッシュする。
  - **NaN は NaN と等しく、すべての値（+Infinity を含む）より大きい**（ソート・インデックス用の全順序）。順序は `-Infinity < 有限値 < +Infinity < NaN`。
- 型の解決: numeric と整数の混合演算子はなく、整数側が numeric に暗黙変換される（`1.5 + 1` は numeric）。float と numeric の混合では numeric → float8 が暗黙なので **float8 に落ちる**（`research-pg-types.md` §3）。

### 2.6 キャスト【確認: PG17 / 一部記憶】

| 変換 | 規則 |
|---|---|
| int2/int4/int8 → numeric | 暗黙。正確 |
| numeric → int2/int4/int8 | 代入。**0 から遠い方へ丸める**（`1.5::int = 2`、`2.5::int = 3`、`(-2.5)::int = -3`）。範囲外は 22003 `integer out of range` / `bigint out of range` / `smallint out of range`。NaN・Infinity は 0A000 `cannot convert NaN to integer` など【記憶】 |
| float4/float8 → numeric | 代入。**float8 は有効 15 桁（DBL_DIG）、float4 は 6 桁（FLT_DIG）に丸めた文字列を経由する**（`0.30000000000000004::float8::numeric = 0.3`、`(1/3::float8)::numeric = 0.333333333333333`）【確認】。NaN と ±Infinity はそのまま |
| numeric → float4/float8 | 暗黙。文字列にしてから `strtod` 相当で変換【記憶】。yuzhu は Rust の `str::parse::<f64>()` で同じ結果になる（正しく丸める実装なので） |
| numeric → numeric(p,s) | sizing cast（§2.2） |
| text ↔ numeric | I/O 変換 |

- float8 → numeric の 15 桁丸めは、M1 で作った float の最短表現の出力（`float_fmt.rs`）とは別物。`format!("{:.*e}", 14, x)` で 15 有効桁に丸めた 10 進表現を作り、それを numeric の入力として読めばよい【提案】。

### 2.7 関数と集約

- numeric-core（M4）: `abs`、`sign`、`round(x)`、`round(x, int)`、`trunc(x)`、`trunc(x, int)`、`ceil`/`ceiling`、`floor`、`mod`、`div`、`scale`、比較演算子 6 つ、`+ - * / %`、単項 `- +`。集約 `sum(numeric)`、`avg(numeric)`、`sum(int8)`、`avg(int2/int4/int8)`、`min/max(numeric)`。
  - `avg` は「numeric の和 ÷ 件数（numeric）」を `select_div_scale` で割った値【記憶: `numeric_avg` は `div_var` + `select_div_scale` 相当】。`avg(values 1,2,2) = 1.6666666666666667`、`avg(1.5, 2.25) = 1.8750000000000000`【確認】。
  - PG は int の集約を内部で int128 で累積して最後に numeric にする（高速化）が、結果は同じ。yuzhu は最初から numeric で累積してよい。
- numeric-full（M5）: `sqrt`、`exp`、`ln`、`log(x)`、`log(b, x)`、`power`/`^`、`min_scale`、`trim_scale`、`gcd`、`lcm`、`factorial`、`width_bucket`。
  - **sqrt/exp/ln/power の結果 scale の規則は複雑**（`sqrt(2::numeric)` = `1.414213562373095`（15 桁）、`exp(1::numeric)` = `2.7182818284590452`（16 桁）、`2::numeric ^ 0.5` = `1.4142135623730950`）【確認: 出力値】。numeric.c の `numeric_sqrt`、`numeric_exp`、`numeric_ln`、`numeric_power` がそれぞれ独自に rscale を決めている。移植するなら、各関数の rscale 決定部を逐語的に移す。【提案】使用頻度が低いので M6 へ回してよい（確認事項 C-3）。
- `to_char(numeric, text)` は書式言語が大きい（L）。M5 では対応しない。

### 2.8 バイナリ形式（Extended Query 用）【記憶: `numeric_send`】

- `int16 ndigits`、`int16 weight`、`uint16 sign`（0x0000 / 0x4000 / 0xC000 NaN / 0xD000 +Inf / 0xF000 -Inf）、`int16 dscale`、続いて `int16 digits[ndigits]`（base 10000）。すべてネットワークバイトオーダー（BE）。
- 受信時（`numeric_recv`）は各フィールドを検証し、不正なら 22P03 `invalid_binary_representation`。

### 2.9 yuzhu の実装案【提案】

```rust
// types/numeric.rs（外部クレートは使わない）
#[derive(Clone, Debug)]
pub enum Numeric {
    NaN, PosInf, NegInf,
    Finite(NumVar),
}
#[derive(Clone, Debug)]
pub struct NumVar {
    pub neg: bool,
    pub weight: i32,          // 演算中は i16 を超えてよい。格納時に検査する
    pub dscale: i32,          // 0..=16383（格納時）
    pub digits: Vec<i16>,     // 0..=9999、先頭と末尾のゼロは正規化で落とす
}
```

- `Datum::Numeric(Box<Numeric>)` を追加する（`Datum` のサイズを増やさないため Box）。
- 加減算・乗算は base-10000 の筆算。乗算は桁数が大きいときも O(n·m) で十分（PG も基本は筆算）。
- 除算は **Knuth のアルゴリズム D（base 10000）**で rscale + 余分 1 桁まで求め、`round_var` 相当で四捨五入する。
- 比較は (1) 特殊値、(2) 符号、(3) weight、(4) digits の順。正規化済みなら単純。
- 「`rust_decimal` などのクレートを使わない」理由: (1) CLAUDE.md の手書き方針（型の中核）、(2) 既存クレートは scale の上限（28 桁など）や除算の scale 規則が PG と異なり、互換性の中心である表示が一致しない（`research-pg-types.md` §6 の注意と同じ）。
- テスト: PG17 に対して乱数で生成した式（`a op b`、`round(a, n)`、キャスト）の結果を大量に突き合わせる差分テストを作る（M1 の float 出力でやった方法と同じ）。工数 S。

---

## 3. 日付・時刻型

### 3.1 型と内部表現【確認: pg_type（`research-pg-types.md` §1）、datatype/timestamp.h】

| 型 | OID | typlen | 内部表現 | 範囲 |
|---|---|---|---|---|
| `date` | 1082 | 4 | i32: 2000-01-01 からの日数 | 4713-01-01 BC 〜 5874897-12-31【確認】 |
| `time` | 1083 | 8 | i64: 00:00 からのマイクロ秒（0..=86400000000、`24:00:00` を含む） | |
| `timestamp` | 1114 | 8 | i64: 2000-01-01 00:00:00 からのマイクロ秒（**現地時刻の壁時計値**） | 4713 BC 〜 294276 AD（`294277-01-01` は 22008）【確認】 |
| `timestamptz` | 1184 | 8 | i64: 2000-01-01 00:00:00 **UTC** からのマイクロ秒 | 同上 |
| `interval` | 1186 | 16 | `{ time: i64 µs, day: i32, month: i32 }` | |
| `timetz` | 1266 | 12 | 対応しない（docs も使用を勧めていない） | |

- 定数【確認: timestamp.h】: `POSTGRES_EPOCH_JDATE = 2451545`（2000-01-01 のユリウス日）、`UNIX_EPOCH_JDATE = 2440588`、`USECS_PER_DAY = 86400000000`、`MAX_TIMESTAMP_PRECISION = 6`、`MIN_TIMESTAMP = -211813488000000000`、`END_TIMESTAMP = 9223371331200000000`、`DATE_END_JULIAN = 2147483494`、`TIMESTAMP_END_JULIAN = 109203528`。
- **infinity**: timestamp/timestamptz の `-infinity` = `i64::MIN`（`DT_NOBEGIN`）、`infinity` = `i64::MAX`（`DT_NOEND`）。date は `i32::MIN` / `i32::MAX`。PG17 から interval にも `infinity` / `-infinity` がある（month = day = time がすべて最小値/最大値）【確認: `'infinity'::interval` が通る / 表現は記憶】。
- 暦: **先発グレゴリオ暦**（1582 年以前もグレゴリオ暦で数える）。年 0 はなく、1 BC の次が 1 AD（`'0001-12-31 BC'::date + 1 = 0001-01-01`）【確認】。ユリウス日との相互変換は datetime.c の `date2j` / `j2date` を移植する（20 行程度）【記憶: アルゴリズムは整数演算のみ】。
- ディスク形式: date は i32 LE（typalign i）、time/timestamp/timestamptz は i64 LE（typalign d）、interval は time(i64) → day(i32) → month(i32) の順の 16 バイト（typalign d）。M2 の固定長列の規則に乗る。
- 比較:
  - timestamp/timestamptz/date/time は整数比較。
  - **interval は「1 か月 = 30 日、1 日 = 24 時間」に換算した 128 ビット整数で比較**する（`'36 hours' = '1 day 12 hours'` は真）【確認: 結果 / 換算式は記憶: `interval_cmp_value`】。ハッシュも同じ換算値で取る。
- 型の相互変換: `date → timestamp`（暗黙）、`date → timestamptz`（暗黙。セッションの TimeZone で 00:00 を解釈）、`timestamp ↔ timestamptz`（暗黙。TimeZone 依存）、`timestamptz → date`（TimeZone での日付）。`date = timestamp` の比較は timestamp に揃う。

### 3.2 typmod（秒の小数部の精度）【確認: PG17】

- `timestamp(p)`、`timestamptz(p)`、`time(p)`、`interval(p)` の typmod は **p そのもの**（+4 しない）。0..6。省略時 -1。
- 適用時に小数部を p 桁に**四捨五入**する: `timestamp(0)` に `'2024-01-01 00:00:00.6'` を入れると `00:00:01`、`timestamptz(2)` に `.555` を入れると `.56`【確認】。
- interval の typmod には精度のほかにフィールド制限（`interval year to month`、`interval second` など）が入る（上位ビット）【記憶: `INTERVAL_TYPMOD(precision, range)`】。【提案】M5 ではフィールド制限の構文は受け付けず 0A000、精度 `interval(p)` だけ対応する（確認事項 C-6）。
- 型名表示: `timestamp(0) without time zone`、`timestamp(2) with time zone`、`timestamp without time zone`【確認】。

### 3.3 テキスト入力【確認: datetime.c の関数構成 / 挙動は PG17 で確認】

PG の入力処理は 2 段構えになっている。

1. **`ParseDateTime`**（datetime.c 754 行付近）: 文字列をフィールドに分解し、各フィールドに種別（`DTK_NUMBER`、`DTK_STRING`、`DTK_DATE`、`DTK_TIME`、`DTK_TZ`、`DTK_SPECIAL` など）を付ける。
2. **`DecodeDateTime`**（978 行付近）/ `DecodeTimeOnly` / `DecodeInterval`（3364 行付近）/ `DecodeISO8601Interval`（3829 行付近）: フィールド列を解釈して `pg_tm` 構造体（年月日時分秒）+ 小数秒 + タイムゾーンにする。日付の順序は DateStyle の MDY/DMY/YMD に従う。曖昧さの規則は付録 B.1 に書かれている。

【提案】yuzhu は**同じ 2 段構えを縮小移植**する。正規表現で書くと、PG が受け付ける表記の組み合わせ（`'January 8, 1999'`、`'1999-Jan-08'`、`'19990108'`、`'1/8/1999'`、`'2024-01-02T03:04:05+09'` など）に追いつけない。

M5 で受け付ける入力（すべて PG17 で通ることを確認済み）:

| 分類 | 例 |
|---|---|
| ISO 8601 | `2024-01-02`、`2024-01-02 03:04:05`、`2024-01-02T03:04:05.123456`、`20240102` |
| 秒の小数部 | 7 桁以上は µs に**四捨五入**（`03:04:05.123456789` → `.123457`） |
| MDY の日付 | `1/8/1999`（DateStyle が MDY のとき 1 月 8 日。DMY なら 8 月 1 日。`'01/02/2024'` は DMY で `2024-02-01`） |
| 月名 | `January 8, 1999`、`1999-Jan-08`、`8 Jan 1999` |
| BC | `4713-01-01 BC` |
| タイムゾーン | `Z`、`UTC`、`GMT`、`+09`、`+05:30`、`-03:00:15`、`+0900`、`Asia/Tokyo`、略称 `PST`、`JST` |
| 特殊な文字列 | `epoch`、`infinity`、`-infinity`、`now`、`today`、`tomorrow`、`yesterday`、`allballs`（time の 00:00:00） |
| 時刻の境界 | `24:00:00` は可（timestamp では翌日 00:00）、秒 60（うるう秒）は次の分に繰り上げ（`23:59:60` → 翌日 `00:00:00`） |

規則の要点:

- **`timestamp`（タイムゾーンなし）への入力に含まれるタイムゾーンは黙って無視される**（`'2024-01-01 00:00+09'::timestamp` = `2024-01-01 00:00:00`）【確認】。
- `timestamptz` への入力にタイムゾーンがなければ、セッションの TimeZone で解釈する。
- 2 桁の年は不可ではないが（`'99-01-08'` は YMD として解釈しようとして 22008）、扱いが入り組んでいる。【提案】M5 では 4 桁の年だけを正しく扱い、2 桁の年の規則（70 以上は 19xx、未満は 20xx【記憶】）は PG と食い違ったら直す。
- `now`/`today` などは**評価した時点の値に置き換わる**。DEFAULT 式に `'now'::timestamp` を書くと CREATE TABLE 時点の値で固定される（PG と同じ罠）。DEFAULT はテキストで保存し使うたびにパースし直す yuzhu の方式（Q-006）だと、**PG と違って毎回その時点の値になってしまう**。【提案】DEFAULT 式を保存するときに、`'now'` などの文字列定数のキャストだけは定数畳み込みしてから保存する（または PG と違うことを既知の差分として記録する。確認事項 C-7）。

エラー【確認】:

| 状況 | SQLSTATE | 文言 |
|---|---|---|
| 書式不正（`'garbage'::timestamp`） | 22007 | `invalid input syntax for type timestamp: "garbage"` |
| フィールドの値が範囲外（`'2024-13-01'`、`'2024-02-30'`） | 22008 | `date/time field value out of range: "2024-13-01"`。日付順を取り違えた可能性があるときは HINT `Perhaps you need a different "datestyle" setting.` |
| 範囲外の timestamp | 22008 | `timestamp out of range: "294277-01-01"` |
| タイムゾーンのオフセットが範囲外（`+16`） | 22009 | `time zone displacement out of range: "2024-01-01 00:00:00+16"`（±15:59:59 まで可） |
| 未知のタイムゾーン名 | 22023 | `time zone "foo/bar" not recognized`（小文字化して表示） |

interval の入力【確認】:

- postgres 形式: `1 day 2 hours`、`1 year 2 mons 3 days 04:05:06.7`、`-1 day`、`1.5 years`（→ `1 year 6 mons`）、`0.5 days`（→ `12:00:00`）、`1 week`（→ `7 days`）、`100000 hours`、`@ 1 minute ago`（→ `-00:01:00`）。
- ISO 8601 形式: `P1Y2M3DT4H5M6S`。
- 小数の単位の繰り下げ規則（`1.5 years` → 18 か月、`1.5 months` → 1 か月 15 日、`0.5 days` → 12 時間）は `DecodeInterval` の `AdjustFractDays` などに従う【記憶】。

### 3.4 テキスト出力（DateStyle = ISO、IntervalStyle = postgres）【確認: PG17】

- date: `YYYY-MM-DD`。年は最低 4 桁（`0999`）、5 桁以上もそのまま（`10000-01-01`）。BC は末尾に ` BC`（`0001-01-01 BC`）。`infinity` / `-infinity`。
- time: `HH:MM:SS[.ffffff]`。
- timestamp: `YYYY-MM-DD HH:MM:SS[.ffffff]`。**小数部は末尾のゼロを落とし、ゼロなら小数点ごと出さない**（`.5`、`.0001`、`.123457`）。
- timestamptz: 上記 + セッションの TimeZone でのオフセット。オフセットは `+HH`、分が 0 でなければ `+HH:MM`、秒もあれば `+HH:MM:SS`（`1000-01-01 00:00:00+09:18:59` は Asia/Tokyo の地方平均時）【確認】。
- interval（postgres 形式）【確認: 出力例】:
  - 年・月・日の部分は `N year(s)`、`N mon(s)`、`N day(s)`。時刻部分は `HH:MM:SS[.ffffff]`（時は 2 桁以上。`100000:00:00`）。
  - すべてゼロなら `00:00:00`。
  - 符号: `-1 days`、`-01:00:00`、`1 mon -1 days`、`1 day -01:00:00`、`1 day -25:00:00`。**符号の異なるフィールドが混ざるとき、正のフィールドに `+` を付ける**（`-1 years -2 mons +3 days -04:05:06`、`-1 mons +1 day`）。
  - 単数・複数: `1 day` / `2 days`、`1 mon` / `2 mons`、`1 year` / `2 years`。`-1 days` は複数形になる（絶対値ではなく「1 と等しいか」で判定）【確認: 出力 / 規則は記憶: `EncodeInterval` の `AddPostgresIntPart`】。
- 実装は datetime.c の `EncodeDateTime`（4342 行付近）と `EncodeInterval`（4585 行付近）の ISO / postgres 分岐だけを移植する。

### 3.5 DateStyle と IntervalStyle【提案】

- ParameterStatus で送っている `DateStyle=ISO, MDY`、`IntervalStyle=postgres` を既定とする（M1 のまま）。
- `SET DateStyle` は「出力形式, 日付順」の組。**出力形式は `ISO` だけ受け付け**、`SQL`/`Postgres`/`German` は 0A000 `DateStyle "SQL" is not supported`。日付順 `MDY`/`DMY`/`YMD` は受け付け、入力の解釈に反映する（安い）。値が変わったら ParameterStatus を送る（M1 で ParameterStatus を送る仕組みはある）。
- `SET IntervalStyle` は `postgres` だけ。`iso_8601`（`P1Y2M`）は出力の追加だけで済むので S で足せる。`sql_standard`、`postgres_verbose` は 0A000。
- 理由: psql やドライバは ISO 以外を要求しない。JDBC と pgx は接続時に `DateStyle=ISO` を前提とし、違うと接続を拒否するものもある【記憶】。

### 3.6 タイムゾーン

#### 3.6.1 PG のやり方【確認: src/timezone/ の存在 / 詳細は記憶】

- PG は IANA tz の参照実装（`localtime.c` など）を `src/timezone/` に同梱し、tzdata を `zic` でコンパイルして `share/timezone/` にインストールする。`--with-system-tzdata` を付けてビルドすると OS の `/usr/share/zoneinfo` を使う（Debian/Ubuntu のパッケージはこちら）。
- 略称（`PST`、`JST` など）の入力は `timezone_abbreviations` パラメータ（既定 `Default`）が指す `share/timezonesets/Default` ファイルで解決する。tzdata ではない。
- `TimeZone` パラメータの値:
  - IANA 名（`Asia/Tokyo`）、`UTC`、`Etc/UTC`
  - **ISO 形式の数値オフセット `+09`** は「UTC より 9 時間進んだ」意味。表示は POSIX 形式の `<+09>-09` になる【確認】
  - **POSIX 形式 `UTC+9` は符号が逆で「UTC より 9 時間遅れ」**（docs 8.5.3 の注意）【記憶】
  - 未知の名前は 22023 `invalid value for parameter "TimeZone": "Foo/Bar"`【確認】
- DST の境界【確認: America/New_York で】:
  - 存在しない現地時刻（春の 02:30）は**遷移前のオフセットで解釈**される: `'2024-03-10 02:30'` → `03:30:00-04`
  - 重複する現地時刻（秋の 01:30）は**遷移後（標準時）**で解釈される: `'2024-11-03 01:30'` → `01:30:00-05`
- 時刻の加算【確認】: timestamptz + interval は、**month と day の部分をセッションの TimeZone の現地時刻で加え、time 部分は絶対時間で加える**。`'2024-03-09 12:00' + '1 day'` = `2024-03-10 12:00-04`、`+ '24 hours'` = `13:00-04`。月の加算は月末で切り詰める（`2024-01-31 + 1 month` = `2024-02-29`）。

#### 3.6.2 yuzhu の選択肢

| 案 | 内容 | 長所 | 短所 | 工数 |
|---|---|---|---|---|
| A. UTC と固定オフセットのみ | TimeZone は `UTC` と `±HH[:MM]` だけ。地域名は 22023 | 最小。テストは UTC で書くので困らない | `SET TIME ZONE 'Asia/Tokyo'` が通らず、アプリ（ORM の接続時設定など）が接続に失敗する可能性がある | S |
| **B. システムの tzdata を読む（推奨）** | `/usr/share/zoneinfo/<名前>` の **TZif（v2/v3）ファイルを手書きのパーサで読む**。遷移表 + 末尾の POSIX TZ 文字列（遷移表の範囲外の未来用）を解釈する | Linux 限定方針に合う。PG の Debian パッケージと同じ情報源なので結果が一致しやすい。依存クレートなし。tzdata の更新は OS に任せられる | TZif と POSIX TZ 文字列の解釈を書く必要がある（合わせて 400〜600 行程度）。tzdata のないコンテナ（distroless など）では地域名が使えない | M |
| C. tzdata を同梱 | `chrono-tz` などのクレート、またはビルド時に tzdata を埋め込む | 環境に依存しない | `chrono-tz` は日付時刻の中核に外部クレートを使うことになり手書き方針に反する。自前で埋め込むなら B のパーサ + ビルドスクリプトが要り、B より大きい | M〜L |

【提案】**案 B**。細部:

- 探索パス: 設定ファイルの `timezone_dir`（既定 `/usr/share/zoneinfo`）。名前は大文字小文字を区別せずに解決する（PG は区別しない【記憶】。Linux のファイル名は区別するので、起動時にディレクトリを走査して小文字化した索引を作る）。
- `..` を含む名前や絶対パスは拒否する（パストラバーサル対策）。
- 読んだゾーンはプロセス全体のキャッシュ（`RwLock<HashMap<String, Arc<TzInfo>>>`）に置く。
- ファイル読み込みも **I/O 抽象化レイヤ経由**にする（CLAUDE.md の規則。テストでは偽の zoneinfo を注入できる）。
- tzdata が見つからない環境では `UTC`、`GMT`、`Etc/UTC`、`Z` と数値オフセットだけ使える（警告ログを出す）。
- 略称: PG の `timezonesets/Default` から、よく使われるもの（`UTC`、`GMT`、`Z`、`PST/PDT`、`MST/MDT`、`CST/CDT`、`EST/EDT`、`JST`、`KST`、`CET/CEST`、`EET/EEST`、`BST`、`IST`（Default では Israel）など 30 個程度）を静的表で持つ【提案。正確な値は Default ファイルから写す】。`timezone_abbreviations` パラメータは `Default` 固定。
- `AT TIME ZONE` 演算子と `timezone(zone, ts)` 関数も B があれば素直に実装できる。

#### 3.6.3 テストへの影響

- `tests/` の SLT は **TimeZone = UTC を前提**にする（PG 側の既定は `Etc/UTC` のことがあるので、タイムゾーンに依存するテストファイルは先頭で `SET TIME ZONE 'UTC'` を明示する）。
- 地域名のテストは、DST のない `Asia/Tokyo` と DST のある `America/New_York` の 2 つに絞る（tzdata の版による差が出にくい年を使う）。

### 3.7 関数と演算子（M5 で入れるもの）【提案】

- 現在時刻: `now()`、`current_timestamp[(p)]`、`current_date`、`current_time`（timetz なので**対応しない** → 0A000）、`localtimestamp[(p)]`、`localtime`、`transaction_timestamp()`、`statement_timestamp()`、`clock_timestamp()`。
  - **`now()` = `current_timestamp` = `transaction_timestamp()` はトランザクション開始時刻で固定**【確認: `now() = transaction_timestamp()` が真】。`statement_timestamp()` は文の開始時刻、`clock_timestamp()` は呼んだ時点。
  - 実時間の取得（`SystemTime::now()`）もテストで差し替えられる抽象（`Clock` トレイト）を通す。
- 算術【確認】: `date + int` → date、`date - date` → **integer**（日数）、`date + interval` → timestamp、`date + time` → timestamp、`timestamp - timestamp` → interval（`365 days`。日数は月に繰り上げない）、`timestamp ± interval`、`timestamptz ± interval`、`interval ± interval`、`interval * float8`（`'1 month' * 1.5 = 1 mon 15 days`）、`interval / float8`、単項 `-`。
- 抽出・切り捨て: `extract(field from x)`（PG14 以降 **numeric を返す**。`extract(epoch from timestamptz '2000-01-01 00:00+00')` = `946684800.000000`）、`date_part(text, x)`（float8 を返す）、`date_trunc(text, x)`。
- その他: `age(ts, ts)`、`age(ts)`、`justify_days`、`justify_hours`、`justify_interval`、`make_date`、`make_time`、`make_timestamp`、`make_timestamptz`、`make_interval`、`to_timestamp(float8)`、`isfinite`、`AT TIME ZONE`、`OVERLAPS`（任意）。
- `to_char` / `to_date` / `to_timestamp(text, text)` は書式言語が大きいので M6 以降（L）。ORM が使うことは少ない【記憶】。
- `generate_series(timestamp, timestamp, interval)` は集合を返す関数（SRF）の仕組みが要るので、SRF 全般と合わせて判断する。

### 3.8 バイナリ形式【記憶: `date_send`、`timestamp_send`、`interval_send`】

- date: int32（2000-01-01 からの日数）。time: int64 µs。timestamp/timestamptz: int64 µs（timestamptz は UTC）。interval: int64 time、int32 day、int32 month の順。すべて BE。`integer_datetimes=on` を ParameterStatus で送っているので、クライアントは整数形式を前提にする。

### 3.9 工数

| 作業 | 工数 |
|---|---|
| 内部表現、暦（date2j/j2date）、範囲検査、ディスク形式 | S |
| 入力（ParseDateTime/DecodeDateTime/DecodeTimeOnly の縮小移植）+ エラー文言 | **M〜L** |
| interval の入力（postgres 形式 + ISO 8601）と出力 | M |
| 出力（ISO / postgres）、typmod の丸め | S |
| TZif と POSIX TZ 文字列のパーサ、DST の解決、略称表 | M |
| 算術・比較・キャスト・現在時刻・抽出関数 | M |
| バイナリ形式 | S |
| PG17 との差分テスト（乱数の日付・時刻・interval の入出力と算術） | S |

---

## 4. char(n) / bpchar

### 4.1 PG の意味論【確認: PG17】

- 型 OID 1042（`bpchar`）、typmod = n + 4。DDL の `char` / `character` は `char(1)`。`bpchar` と書くと長さ無制限（typmod -1）【`research-pg-types.md` §1】。型名の表示は `character(3)`。
- **値は空白で埋めて保存する**（`octet_length('ab'::char(5)) = 5`）。
- **末尾の空白は意味を持たない**:
  - 比較: `'ab'::char(5) = 'ab   '::char(5)` は真。`'x'::char(3) < 'x '::char(3)` は偽（どちらも同じ値）。比較は末尾の空白を除いてから行う（`bpchareq`、`bpcharcmp`）。
  - 長さ: `length('ab   '::char(5)) = 2`、`char_length('a  '::char(3)) = 1`。
  - **text / varchar への変換で末尾の空白を落とす**: `'ab '::char(3)::text || '|'` = `ab|`。`'abc'::char(5)::varchar || '|'` = `abc|`。`'a'::char(3)::text = 'a'` は真。
  - `||` は text の演算子なので、bpchar は text に変換されて**末尾空白が消える**: `'abc'::char(5) || 'x'` = `abcx`。ただし `concat('a'::char(3), 'b')` は出力関数を通すので空白が残る（`a  b`）。
  - `LIKE` は bpchar 用の演算子があり、**埋めた空白込み**で照合する: `'a '::char(2) LIKE 'a'` は偽【確認】。
- 長さ超過:
  - 代入・暗黙: 超過部分がすべて空白なら切り捨て、そうでなければ **22001** `value too long for type character(3)`（`'abcd'` は 22001、`'abc   '::char(3)` は `abc`）。
  - 明示キャスト: 切り詰める（`'abcdef'::char(3)` = `abc`）。
  - varchar も同じ規則（`'abcd'::varchar(3)` = `abc`、代入で `'ab  '` は `'ab '` に切り捨て）【確認】。M1 の varchar 実装と同じ規則かを M5 着手時に確認する。
- 長さは**文字数**（バイト数ではない）。

### 4.2 yuzhu の実装案【提案】

- `Datum::Text` をそのまま使い、bpchar の値は**埋めた後の文字列**を持つ（PG と同じ）。ディスク形式も text と同じ varlena。
- 必要な部品: `bpcharin`（typmod で埋める）、sizing cast `bpchar(bpchar, int4, bool)`、`bpchar → text/varchar`（rtrim）、`text/varchar → bpchar`、比較演算子群（rtrim して比較）、`length(bpchar)`、`octet_length(bpchar)`、`bpcharlike`、ハッシュ（rtrim してからハッシュ）。B+Tree のキー比較も rtrim 後。
- 演算子解決: bpchar は S カテゴリで preferred ではない。`bpchar = text` は `text = text` に解決される（bpchar → text が暗黙）【記憶】。M1 の解決アルゴリズムで正しく決まるはず。
- 工数: S〜M（比較・ハッシュ・インデックスの各経路に rtrim を通す確認が要る）。

---

## 5. bytea

### 5.1 PG の意味論【確認: PG17】

- OID 17、typlen -1、category U、typalign i。
- **出力は既定で hex 形式**: `\x` + 小文字の 16 進（`'abc'::bytea` → `\x616263`）。`bytea_output = 'escape'` にすると、印字可能な ASCII はそのまま、それ以外は `\ooo`（8 進 3 桁）、`\` は `\\`（`'\x0001ff41'` → `\000\001\377A`）。
- 入力:
  - `\x` で始まれば hex 形式。16 進数字の間の空白は許される【記憶】。奇数桁は **22023** `invalid hexadecimal data: odd number of digits`【確認】。不正な文字は 22023 `invalid hexadecimal digit: "z"`【記憶】。
  - それ以外は escape 形式: `\\` → `\`、`\ooo` → 1 バイト、それ以外の文字はそのまま。不正な `\` の並びは 22P02 `invalid input syntax for type bytea`。
  - 文字列リテラルとしての `'a\\b'` は standard_conforming_strings=on なので `a\\b` の 4 文字が入力になり、`\\` → `\` で `\x615c62`【確認】。
- 演算と関数: `||`、比較（memcmp 順）、`length` / `octet_length`、`substring`、`position`、`overlay`、`get_byte` / `set_byte`、`encode(bytea, 'hex'|'base64'|'escape')`、`decode(text, ...)`、`md5(bytea)`、`sha256(bytea)`（暗号クレートの利用は許可済み）。
- バイナリ形式: 生のバイト列そのもの。

### 5.2 yuzhu【提案】

- `Datum::Bytea(Vec<u8>)` を追加。ディスク形式は varlena + 生バイト。
- `text → bytea` のキャストはない（`'abc'::bytea` は unknown からの入力）。`convert_to(text, 'UTF8')` で変換する。
- 工数: S。

---

## 6. uuid

### 6.1 PG の意味論【確認: uuid.c、PG17】

- OID 2950、typlen 16、typbyval f、typalign c、category U。
- 入力（`string_to_uuid`）: 先頭に `{` があれば末尾に `}` が必須。16 進 2 桁を 16 回読み、**2 バイトごと（4 桁ごと）の区切りの後にだけ `-` を 1 つ許す**。大文字小文字は問わない。**前後の空白は許さない**。不正なら 22P02 `invalid input syntax for type uuid: "zz"`。
  - 例: `{A0EEBC99-9C0B4EF8-BB6D6BB9BD380A11}` も通る【確認】。
- 出力: 小文字の `8-4-4-4-12` 形式。
- 比較: 16 バイトの memcmp 順。
- 関数: `gen_random_uuid()`（PG13 以降のコア関数。v4）。PG17 には `uuidv7()` はない（PG18 で追加）【記憶】。
- バイナリ形式: 16 バイトそのもの。

### 6.2 yuzhu【提案】

- `Datum::Uuid([u8; 16])`。ディスク形式は 16 バイト固定長（typalign c なので詰めて置ける）。
- `gen_random_uuid()` の乱数は暗号クレート（`getrandom` など。周辺用途として許可済み）か、`/dev/urandom` を I/O 抽象経由で読む。v4 のビット（version = 4、variant = 10）を立てる。
- 工数: S。ORM（Rails、Django、Prisma など）が主キーに使うので費用対効果が高い。

---

## 7. 配列

### 7.1 何が必要になるか

- 一般の配列型（`int[]` 列、多次元、添字、スライス、`array_agg`、`unnest`、`ANY/ALL`、配列の比較、`'{...}'` リテラルの入出力、NULL 要素、下限付き次元 `'[0:2]={...}'`）は大きい。配列の I/O だけでも引用符・エスケープ・NULL・多次元の規則がある（PG17 の出力: `{1,2,NULL}`、`{"a b",c}`【確認】）。
- 一方、yuzhu が M5 までに配列を必要とする場面は限られている:
  1. **カタログの列**: `pg_constraint.conkey/confkey`（int2[]）、`pg_index.indkey`（int2vector）、`pg_proc.proargtypes`（oidvector）、`pg_class.relacl` など。psql の `\d` や ORM のスキーマ取得クエリが参照する。
  2. **ドライバ**: `WHERE id = ANY($1)` に配列パラメータを渡す書き方（pgx、node-postgres、JDBC の `createArrayOf`）。
  3. 各型の配列型 OID（`_int4` = 1007 など）は `pg_type.typarray` に必要（M2 のカタログで既に値は出せる）。

### 7.2 推奨【提案】

- **M5 では「1 次元・NULL 要素あり・組み込みスカラー型の要素だけ」の最小実装**を入れる:
  - `Datum::Array(Box<ArrayValue { elem_type: Oid, items: Vec<Datum> }>)`
  - テキスト I/O（1 次元の `{...}` だけ。多次元は 0A000）、バイナリ I/O（1 次元）
  - `ARRAY[...]` コンストラクタ、`x = ANY(array)`、`x <> ALL(array)`、`array_length(a, 1)`、`cardinality`、添字 `a[i]`（読み取りのみ）、`=` 比較
  - カタログ列（int2[]、oid[]、text[]）の出力
  - **ユーザーテーブルの列型としての配列は 0A000**（ディスク形式を決めずに済む。必要になったら PG の `ArrayType` 形式に合わせる）
- 工数: M。本格対応（多次元、列型、`array_agg`、`unnest`、スライス、更新）は M6 以降で L。
- 配列なしにする案（`= ANY` も未対応）は、ドライバ経由の IN 句が書けなくなるので推奨しない。

---

## 8. その他の型（参考: M5 では入れない）

| 型 | 需要 | 判断【提案】 |
|---|---|---|
| `json` / `jsonb` | ORM・アプリでの需要は大きい | M6 以降の最優先候補。`json`（テキストのまま検証だけ）は S〜M、`jsonb`（バイナリ形式・演算子・インデックス）は L |
| `time` | 小 | **M5 で入れる**（timestamp の部品で作れる。S） |
| `timetz` | ほぼなし | 入れない |
| `inet` / `cidr` / `macaddr` | 中 | M6 以降 |
| enum（`CREATE TYPE ... AS ENUM`） | 中（ORM が使う） | M6 以降（pg_enum と型の動的追加が要る） |
| `money` | 小 | 入れない（docs も非推奨気味） |
| `"char"`、`name`、`oid`、`regclass` など | カタログ用 | M2 で対応済みの前提 |
| ドメイン、複合型、範囲型 | 小〜中 | 入れない |

---

## 9. FOREIGN KEY

### 9.1 PG の実装【確認: ri_triggers.c、tablecmds.c、PG17 での確認】

- FK 制約を作ると、**内部トリガー**（`tgisinternal = t`）が参照側に 2 つ、被参照側に 2 つ作られる。PG17 で確認した組み合わせ:
  - 参照側（子）: `RI_FKey_check_ins`（AFTER INSERT）、`RI_FKey_check_upd`（AFTER UPDATE）
  - 被参照側（親）: 削除時 `RI_FKey_noaction_del` / `RI_FKey_restrict_del` / `RI_FKey_cascade_del` / `RI_FKey_setnull_del` / `RI_FKey_setdefault_del`、更新時はそれぞれの `_upd` 版
  - トリガー名は `RI_ConstraintTrigger_a_<oid>`（親側の action）と `RI_ConstraintTrigger_c_<oid>`（子側の check）
- チェックは SPI で SQL を発行して行う（ri_triggers.c 361 行付近など）:
  - 子の挿入・更新: `SELECT 1 FROM ONLY <pktable> x WHERE pkatt1 = $1 [AND ...] FOR KEY SHARE OF x`
  - 親の削除・更新（NO ACTION/RESTRICT）: `SELECT 1 FROM ONLY <fktable> x WHERE $1 = fkatt1 [AND ...] FOR KEY SHARE OF x`
  - CASCADE: `DELETE FROM ONLY <fktable> WHERE $1 = fkatt1 ...` / `UPDATE ONLY <fktable> SET fkatt1 = $1 ... WHERE $n = fkatt1 ...`
  - SET NULL / SET DEFAULT: `UPDATE ONLY <fktable> SET fkatt1 = NULL|DEFAULT ...`
- **`FOR KEY SHARE` の行ロック**で、親行が並行して削除・キー更新されるのを防ぐ（キー以外の列の UPDATE とは衝突しない弱いロック）。
- **スナップショット**（`ri_PerformCheck`、2370 行付近）: Read Committed では最新のスナップショットで検査する。Repeatable Read / Serializable で新しい行を検出する必要がある検査（`detectNewRows`）では、`CommandCounterIncrement` の後に**最新のスナップショット（test_snapshot）で検査し、トランザクションのスナップショット（crosscheck_snapshot）では見えない行が見つかったらエラー**（シリアライゼーション失敗 40001）にする。
- **検査のタイミング**: 遅延不可（NOT DEFERRABLE）の制約でも、RI トリガーは AFTER トリガーとして**文の終わり**にまとめて発火する【確認】:
  - 自己参照の表で `INSERT INTO s VALUES (2,1),(1,NULL)` は通る（2 行目で親ができる）。
  - `UPDATE s SET id = id + 10, parent = parent + 10` も通る。
  - しかし `DELETE FROM p WHERE id=1; INSERT INTO p VALUES (1);` は、トランザクション内でも**最初の文の終わりで 23503**（遅延不可なので文単位）。
- **NO ACTION と RESTRICT の違い**【確認: ri_triggers.c の `ri_restrict(trigdata, is_no_action)` と `ri_Check_Pk_Match`】: NO ACTION は「削除・更新した親キーと同じキーを持つ別の親行が、文の終わりの時点で存在する」なら違反にしない（同じ文の中でキーを入れ替える UPDATE などが通る）。RESTRICT はこの再確認をしない。DEFERRABLE を扱わない yuzhu では、違いはこの点だけになる。
- **検査の省略**【確認: `RI_FKey_pk_upd_check_required`、`RI_FKey_fk_upd_check_required`】:
  - 親の UPDATE で、古いキーに NULL がある、またはキーが変わっていないなら検査しない。
  - 子の UPDATE で、新しいキーがすべて NULL なら検査しない。一部 NULL なら MATCH SIMPLE では検査しない、MATCH FULL では 23503。キーが変わっていなければ検査しない（同じトランザクションで作った行の場合は例外あり【記憶】）。
- **MATCH の種類**: `MATCH SIMPLE`（既定。キーのどれかが NULL なら検査しない）、`MATCH FULL`（全部 NULL か全部非 NULL。混在は 23503、DETAIL `MATCH FULL does not allow mixing of null and nonnull key values.`）【確認】、`MATCH PARTIAL` は PG でも 0A000 `MATCH PARTIAL not yet implemented`【確認】。
- カタログ `pg_constraint`【確認: PG17 で確認】: `contype = 'f'`、`conrelid`（子）、`confrelid`（親）、`conkey`（子の列番号 int2[]）、`confkey`（親の列番号）、`conindid`（**親側の一意インデックス**。PK なら `p_pkey`）、`confupdtype` / `confdeltype`（`a` = NO ACTION、`r` = RESTRICT、`c` = CASCADE、`n` = SET NULL、`d` = SET DEFAULT）、`confmatchtype`（`s` = SIMPLE、`f` = FULL、`p` = PARTIAL）、`condeferrable`、`condeferred`、`convalidated`、`conpfeqop` / `conppeqop` / `conffeqop`（比較に使う `=` 演算子の OID 配列）、`confdelsetcols`（PG15 の `ON DELETE SET NULL (cols)`）。`pg_get_constraintdef` は `FOREIGN KEY (pid) REFERENCES pc(id) ON DELETE SET DEFAULT` を返す【確認】。
- PG は **子側の列にインデックスを自動では作らない**（親の削除で子を全件走査することになる。docs 5.5.5 の注意）。

### 9.2 yuzhu の方針: executor に組み込んだ RI【提案】

- **トリガーは作らない**。トリガー機構（`CREATE TRIGGER`、AFTER トリガーのキュー、SPI）は M5 の範囲外で、FK のためだけに作るのは過大。代わりに、FK を **ModifyTable（INSERT/UPDATE/DELETE の実行ノード）に組み込んだ検査**として実装する。
- 互換性はカタログとエラーで保つ:
  - `pg_constraint` の行は PG と同じ列・値で作る（`conkey` などの配列は §7 の最小実装で出す）。
  - `pg_trigger` の行は作らない（**既知の差分**。psql の `\d` は FK を `pg_constraint` から表示するので影響しない【記憶: describe.c の FK 表示は `pg_get_constraintdef` を使う】）。
  - 依存関係（`pg_depend`）: 制約 → 親テーブル、制約 → 親の一意インデックス、制約 → 子テーブル（auto）を記録し、DROP の挙動（§9.6）に使う。

#### 9.2.1 文の実行の流れ

```
ModifyTable の実行（1 文）
  各行について:
    heap に挿入 / 削除 / 更新（M3 の MVCC。新しい版を作る）
    この行が関わる FK 制約ごとに RiEvent を文のキューに積む
      子の INSERT/UPDATE:  CheckParentExists { constraint, new_key }（§9.1 の省略条件に当たれば積まない）
      親の DELETE/UPDATE:  ParentKeyGone { constraint, old_key, new_key?, action }
文の本体が終わったら（RETURNING を返す前に）:
  CommandCounterIncrement（この文の変更を自分から見えるようにする）
  キューを順に処理する（下記）。処理中に CASCADE などで新しい DML を実行したら、
  その DML が積んだイベントも同じキューの末尾に追加して、空になるまで繰り返す
  どこかで違反が出たら、文全体を失敗させる（M3 のサブトランザクションなしの文単位ロールバック）
```

- **CheckParentExists**: 親の一意インデックス（M4 の B+Tree。`conindid`）でキーを引き、**最新のスナップショット + 自分の変更**で見える行があれば OK。なければ 23503。
- **ParentKeyGone**:
  - NO ACTION: まず「同じキーの親行がまだ（または新たに）存在するか」を親の一意インデックスで調べ、存在すれば何もしない。なければ子を検索する（子側にインデックスがあれば使い、なければ全件走査）。見える子行があれば 23503。
  - RESTRICT: 親の再確認をせずに子を検索する。
  - CASCADE（DELETE）: 該当する子行を内部の DELETE で消す。CASCADE（UPDATE）: 子のキー列を新しいキーに更新する。
  - SET NULL / SET DEFAULT: 子のキー列（`ON DELETE SET NULL (cols)` なら指定列だけ）を NULL / DEFAULT 式の値に更新する。SET DEFAULT の結果のキーが親に存在しなければ、その更新で積まれた CheckParentExists が 23503 にする（PG17 で確認した挙動と同じ: `Key (pid)=(0) is not present in table "pc".`）。
- 内部 DML は通常の DML と同じ経路（NOT NULL・CHECK の検査、インデックスの更新、さらに先の FK のイベント）を通る。自己参照の CASCADE の連鎖（`s(1) ← s(2) ← s(3)` を 1 を消して全部消える）も、キューを空になるまで回せば自然に処理できる【確認: PG17 で全行消える】。
- 無限ループ対策: 循環する CASCADE UPDATE は、同じ文の中で同じ行の版を 2 度更新しようとした時点で検出できる（M3 の「自分が同じコマンドで作った版は更新しない」規則。PG は `TM_SelfModified` で扱う【記憶】）。安全弁として 1 文あたりのイベント数の上限（例: 1 億）を設け、超えたら 54001 `statement_too_complex` にする。
- RI の検査で読む行は、M3 の可視性判定で「この文の変更まで含めて見る」。単一ライター（M3）なら並行して書く人はいないので、**最新のコミット済み状態 + 自分の変更**を見れば正しい。

#### 9.2.2 エラーの文言【確認: PG17】

| 状況 | SQLSTATE | メッセージ / DETAIL |
|---|---|---|
| 子の挿入・更新で親がない | 23503 | `insert or update on table "c" violates foreign key constraint "c_pid_fkey"` / `Key (pid)=(3) is not present in table "p".` |
| 親の削除・更新で子が残る | 23503 | `update or delete on table "p" violates foreign key constraint "c_pid_fkey" on table "c"` / `Key (id)=(1) is still referenced from table "c".` |
| MATCH FULL で NULL 混在 | 23503 | 上の 1 つ目 + `MATCH FULL does not allow mixing of null and nonnull key values.` |
| 被参照列に一意制約がない | 42830 | `there is no unique constraint matching given keys for referenced table "nouniq"` |
| 型の不整合 | 42804 | `foreign key constraint "bad2_a_fkey" cannot be implemented` / `Key columns "a" and "id" are of incompatible types: text and integer.` |
| 参照されている表の TRUNCATE | 0A000 | `cannot truncate a table referenced in a foreign key constraint` / `Table "c" references "p".` / HINT `Truncate table "c" at the same time, or use TRUNCATE ... CASCADE.` |
| 参照されている表の DROP | 2BP01 | `cannot drop table p because other objects depend on it` / `constraint c_pid_fkey on table c depends on table p`（複数行）/ HINT `Use DROP ... CASCADE to drop the dependent objects too.` |
| MATCH PARTIAL | 0A000 | `MATCH PARTIAL not yet implemented` |

- ErrorResponse には `SCHEMA NAME`（s）、`TABLE NAME`（t）、`CONSTRAINT NAME`（n）のフィールドも付ける（PG17 で確認。ORM が制約名で例外を振り分けるのに使う）。違反のメッセージの表名は、親の削除のときは**親の表名**が主語になる点に注意。
- 複数の FK に同時に違反するとき、どの制約名が報告されるかは PG ではトリガー名（= 作成順の OID）の順【確認: 同じ列に FK を 2 つ付けると古い方の名前が出た】。yuzhu も制約の OID 順に検査する。

### 9.3 DDL【提案 / PG の規則は確認】

- 構文:
  - 列制約: `col type REFERENCES reftable [(refcol)] [MATCH FULL|SIMPLE] [ON DELETE action] [ON UPDATE action] [NOT DEFERRABLE] [INITIALLY IMMEDIATE]`
  - 表制約: `[CONSTRAINT name] FOREIGN KEY (cols) REFERENCES reftable [(refcols)] ...`
  - `action` = `NO ACTION | RESTRICT | CASCADE | SET NULL [(cols)] | SET DEFAULT [(cols)]`（列リストは ON DELETE のみ）
  - `ALTER TABLE ... ADD [CONSTRAINT name] FOREIGN KEY ...`、`ALTER TABLE ... DROP CONSTRAINT name`（ALTER TABLE が M5 に入る場合）
- 被参照列を省略したら親の **PRIMARY KEY** を使う。PK がなければ 42830。
- 被参照列の組は、**親の PRIMARY KEY か UNIQUE 制約（一意インデックス）の列の組と集合として一致**しなければならない（順序は問わない【記憶】）。`(id, k)` のように PK と UNIQUE の列を混ぜた組は 42830【確認】。
- 制約名の自動生成: `<子の表>_<列名を _ でつないだもの>_fkey`（`c_pid_fkey`、`m3_a_b_fkey`）。重複したら `1`, `2` を付ける（CHECK と同じ規則。`ChooseConstraintName`）。名前が 63 バイトを超えるときの切り詰め規則も CHECK と共通にする。
- 型の互換性: PG は「親の一意インデックスの演算子族に、子の型と親の型の `=` があるか」で判定する（int と numeric は通る、text と varchar は通る、text と int は 42804）【確認: 結果 / 判定方法は記憶】。【提案】yuzhu は「子の型から親の型への**暗黙キャスト**があり、親の型の `=` と B+Tree の比較が使える」なら許し、検査時に子の値を親の型にキャストしてからインデックスを引く。
- 一時表と永続表の混在など、PG の細かい制限は yuzhu に一時表がない間は不要。
- `ALTER TABLE ADD FOREIGN KEY` は既存の行を全件検査する（PG の `RI_Initial_Check` は 1 つの LEFT JOIN 問い合わせで違反行を探す）。違反があれば 23503（メッセージは挿入時と同じ）【確認】。yuzhu は子を全件走査して親の一意インデックスを引けば十分。
- `NOT VALID`（検査を省略して後で `VALIDATE CONSTRAINT`）は M5 では 0A000 でよい。

### 9.4 DEFERRABLE【提案】

- `DEFERRABLE`、`INITIALLY DEFERRED`、`SET CONSTRAINTS` は **0A000**（`DEFERRABLE constraints are not supported` のような文言。PG にはない独自メッセージ）。
- `NOT DEFERRABLE`、`INITIALLY IMMEDIATE`（既定と同じ）は受け付けて無視する。`condeferrable = false`、`condeferred = false` で保存する。
- 理由: 遅延制約はコミット時にイベントを処理する仕組み（トランザクション単位のキューとその永続化不要の保持、サブトランザクション・SAVEPOINT との相互作用）が要り、FK 本体と同じくらいの工数（M）がかかる。ORM はまれに使う（Rails の fixtures、Django の一部【記憶】）ので、M6 以降の候補として記録する。

### 9.5 並行性: 単一ライター（M3）と複数ライター（M5）

- **M3（単一ライター）**: 書くトランザクションは常に 1 つなので、RI の検査は「最新のコミット済み状態 + 自分の変更」を見るだけで正しい。行ロックは不要。読むだけのトランザクションは FK に関わらない。
- **M5（複数ライター）**: ロックなしでは次の競合で整合性が壊れる。
  1. T1 が子 `(pid=1)` を挿入して親 1 の存在を確認（未コミット）。
  2. T2 が親 1 を削除し、子を検索するが T1 の行は未コミットで見えない → 削除成功。
  3. 両方コミット → 親のない子が残る。
- 案:

| 案 | 内容 | 長所 | 短所 | 工数 |
|---|---|---|---|---|
| **a. PG と同じ行ロック（推奨、行ロックが M5 に入るなら）** | 子側の検査で親行に `FOR KEY SHARE` 相当のロックを取る（xmax に lock-only ビット、複数の共有ロック保持者は multixact）。親の DELETE / キー UPDATE はそのロックと衝突して待つ | PG と同じ挙動（待ち、デッドロック検出、40001 の出方）。`SELECT ... FOR UPDATE/SHARE` の実装と共通化できる | multixact（共有ロック保持者の集合）の実装が大きい | 行ロック全体で L（FK 固有の部分は S） |
| b. キー値単位のロック表 | プロセス内のロックマネージャに `(親テーブル OID, 制約 OID, キー値のハッシュ)` 単位のロックを持つ。子の検査は共有、親の DELETE / キー UPDATE は排他で取り、トランザクション終了まで保持 | 実装が小さい。ディスク形式に影響しない | ハッシュ衝突による無用な待ち。ロック数がトランザクションの変更行数に比例する。PG と待ちの粒度が違う | M |
| c. テーブル単位のロック | 子の書き込みは親テーブルに共有ロック、親の DELETE / キー UPDATE は排他ロック | 最小 | 親の削除が子への書き込み全部と直列化される | S |

- 【提案】M5 の複数ライター設計で `SELECT ... FOR UPDATE / FOR SHARE`（行ロック）を入れるなら **案 a**。行ロックを M6 以降に回すなら **案 b** で M5 を乗り切る。どちらでも、ロックを取った後の検査は**最新のスナップショット**で行う（ロック待ちの後で相手がコミットした結果を見るため）。
- Repeatable Read（M5）: 検査を最新のスナップショットで行い、**トランザクションのスナップショットでは見えない行（相手が後からコミットした親や子）に依存して結果が変わる場合は 40001** `could not serialize access due to concurrent update` にする（PG の crosscheck_snapshot と同じ考え方）【確認: 方針は ri_triggers.c / 文言は記憶】。

### 9.6 他の DDL との関係【確認: PG17】

- `DROP TABLE 親` は 2BP01。`DROP TABLE 親 CASCADE` は**子の FK 制約だけ**を消す（子テーブルは残る）【記憶: NOTICE `drop cascades to constraint c_pid_fkey on table c`】。
- `DROP TABLE 子` は自由（FK 制約も一緒に消える）。
- `TRUNCATE 親` は 0A000（§9.2.2）。`TRUNCATE 親, 子` と `TRUNCATE ... CASCADE` は通る（TRUNCATE が M5 にあれば）。
- 親の PK / UNIQUE インデックスの削除（`DROP INDEX`、`ALTER TABLE DROP CONSTRAINT p_pkey`）も依存で 2BP01。
- `ALTER TABLE ... DROP COLUMN` で FK 列を消すと制約も消える（CASCADE 不要）【記憶】。

### 9.7 テストの観点

- 共有 SLT（PG17 で検証）:
  - 列制約・表制約・複合キー・自己参照の作成、自動命名、`pg_constraint` の各列、`pg_get_constraintdef`
  - 挿入・更新・削除それぞれの違反と、その SQLSTATE / メッセージ（sqllogictest の `statement error` は文言の正規表現で照合できる）
  - CASCADE / SET NULL / SET NULL (cols) / SET DEFAULT / RESTRICT / NO ACTION、ON UPDATE CASCADE
  - 文末に検査されること（自己参照の複数行 INSERT、キー入れ替えの UPDATE）と、NO ACTION と RESTRICT の違い
  - MATCH SIMPLE の NULL、MATCH FULL の混在
  - 型の互換性（int → numeric の参照、text → varchar の参照）
  - DROP / TRUNCATE の拒否
- yuzhu 固有のテスト（Rust）:
  - 複数ライターの競合（§9.5 の 3 手順）を 2 つのセッションで再現し、片方が待つ / 失敗することを確認
  - RR での 40001
  - クラッシュリカバリ: CASCADE の途中でクラッシュしても、文全体が無かったことになる（WAL は文単位ではなくトランザクション単位で効くので、M3 の仕組みで自然に満たされるはず）

### 9.8 工数

| 作業 | 工数 |
|---|---|
| 構文（列制約・表制約・ALTER TABLE）、名前の自動生成、`pg_constraint` / `pg_depend` の行 | S〜M |
| 被参照側の一意インデックスの特定、型の互換性検査、`ALTER TABLE ADD` の初期検査 | S |
| 文単位のイベントキューと、ModifyTable への組み込み | M |
| 各 action（NO ACTION / RESTRICT / CASCADE / SET NULL / SET DEFAULT）と MATCH、検査の省略条件 | M |
| DROP / TRUNCATE との依存、エラー文言と ErrorResponse のフィールド | S |
| 複数ライター対応（案 b の場合。案 a は行ロック側の工数に含まれる）と RR の 40001 | M |
| SLT と並行性テスト | S〜M |

---

## 10. カタログと SQLSTATE の追加

### 10.1 pg_type に足す行（OID は PG と同じ）【確認: `research-pg-types.md` §1 / uuid と bytea は pg_type.dat の記憶】

| 型 | OID | 配列型 OID | typlen | typbyval | typcategory | typalign | typstorage |
|---|---|---|---|---|---|---|---|
| bytea | 17 | 1001 | -1 | f | U | i | x |
| bpchar | 1042 | 1014 | -1 | f | S | i | x |
| date | 1082 | 1182 | 4 | t | D | i | p |
| time | 1083 | 1183 | 8 | t | D | d | p |
| timestamp | 1114 | 1115 | 8 | t | D | d | p |
| timestamptz | 1184 | 1185 | 8 | t | D（**preferred**） | d | p |
| interval | 1186 | 1187 | 16 | f | T（**preferred**） | d | p |
| numeric | 1700 | 1231 | -1 | f | N | i | m |
| uuid | 2950 | 2951 | 16 | f | U | c | p |

- `pg_cast`（`research-pg-types.md` §2.2 の表）の numeric・日付時刻・bpchar 関係の行、`pg_operator`・`pg_proc` の行も PG の OID で足す。M2 の方針どおり、静的表から initdb で書き出す。
- 既存のデータディレクトリとの互換: 型の追加は組み込み表の追加だけなので、**initdb のやり直しが要るか**はカタログのバージョン番号（M2 の `catalog_version`）の扱いによる。M5 着手時点で yuzhu のディスク形式の互換性を約束していなければ、作り直しでよい。

### 10.2 SQLSTATE の追加（`error.rs` の `sqlstate` モジュール）

| コード | 名前 | 使う場面 |
|---|---|---|
| 22007 | invalid_datetime_format | 日付時刻の書式不正 |
| 22008 | datetime_field_overflow | 日付時刻のフィールド範囲外、timestamp の範囲外 |
| 22009 | invalid_time_zone_displacement_value | タイムゾーンのオフセット範囲外 |
| 22P03 | invalid_binary_representation | バイナリ形式の受信で不正 |
| 22023 | invalid_parameter_value | 既存。typmod の範囲外、未知のタイムゾーン、bytea の hex 不正 |
| 23503 | foreign_key_violation | FK 違反 |
| 42830 | invalid_foreign_key | 被参照側に一意制約がない |
| 2BP01 | dependent_objects_still_exist | 依存のある DROP |
| 40001 | serialization_failure | RR での FK 検査の食い違い（M5 の RR と共通） |
| 54001 | statement_too_complex | CASCADE の暴走の安全弁（yuzhu 独自の使い方） |

---

## 11. sqllogictest の書き方の注意【提案】

- 結果の型指定子（`query ITR` の `R` など）は、numeric を**文字列として比較する**（`T`）。sqllogictest の `R`（実数）は丸めて比べるので、numeric の scale の違い（`2.5` と `2.5000000000000000`）を見落とす。
- `now()`、`clock_timestamp()`、`gen_random_uuid()` は値を比べず、`IS NOT NULL` や `pg_typeof` で確かめる。
- タイムゾーン依存のファイルは先頭で `SET TIME ZONE 'UTC'`、DateStyle 依存のファイルは `SET DateStyle = 'ISO, MDY'` を明示する（PG 側のサーバー設定に左右されないように）。
- エラーの照合は SQLSTATE ではなく文言で行うことになる（sqllogictest の制約。`research-slt.md` 参照）ので、§9.2.2 などの文言を PG と一字一句合わせる。

---

## 12. 工数のまとめ

| 作業 | 工数 | 置き場所（推奨） |
|---|---|---|
| numeric-core（I/O、typmod、四則、比較、キャスト、sum/avg） | M | **M4 の前半に前倒し** |
| numeric-full（バイナリ形式、sqrt/exp/ln/power など） | M | M5（sqrt 以降は M6 でも可） |
| date / time / timestamp / timestamptz / interval（入出力、算術、関数） | L | M5 |
| タイムゾーン（TZif パーサ、DST、略称） | M | M5 |
| char(n) / bpchar | S〜M | M5 |
| bytea | S | M5 |
| uuid | S | M5 |
| 配列の最小実装（1 次元、`= ANY`、カタログ列） | M | M5（カタログ列の出力だけは M4 でも可） |
| FOREIGN KEY（単一ライター前提の本体） | L | M5（M4 の B+Tree と PK/UNIQUE の後） |
| FK の複数ライター対応（案 b）と RR | M | M5（複数ライターと同時期） |
| PG17 との差分テストの仕組み（numeric・日付時刻） | S | 各作業と同時 |

並列化: numeric、日付時刻、タイムゾーン、bytea/uuid/bpchar、FK は互いの依存が小さく、**別々のエージェントに並列で割り当てられる**。共有するのは `Datum` の列挙子の追加、`catalog/builtin.rs` の表、`types/io.rs` のディスパッチだけなので、最初に「列挙子と OID 定数だけを足す小さな PR」を入れてから分岐すると衝突が減る。

---

## 確認事項

ユーザーに判断をお願いしたい点です。いずれも推奨案で仮決めして進められます。

- **C-1 numeric の前倒し**: 小数リテラル（`1.5`）と `avg(int)` の結果が numeric であるため、numeric の基本部分（入出力・四則・比較・キャスト・sum/avg）を **M4 の前半に前倒し**することを推奨します。M5 のままにすると、M4 の集約が PG と違う型を返すか、`avg` を M5 まで保留することになります。（工数 M。前倒ししても総工数は変わりません）
- **C-2 タイムゾーンの情報源**: 推奨は **Linux のシステム tzdata（`/usr/share/zoneinfo`）を手書きのパーサで読む**方式です（工数 M）。tzdata のない環境では UTC と固定オフセットだけで動きます。代案は「UTC と固定オフセットのみ」（S、`SET TIME ZONE 'Asia/Tokyo'` が使えない）と「tzdata を同梱」（M〜L）です。
- **C-3 numeric の数学関数**: `sqrt`、`exp`、`ln`、`power` は結果の桁数の規則が関数ごとに複雑なため、M6 へ回すことを提案します（M5 に入れるなら +M）。
- **C-4 配列**: M5 では一般の配列型は入れず、**1 次元の最小実装**（`ARRAY[...]`、`= ANY(...)`、カタログ列の出力）だけにすることを提案します（工数 M）。ユーザーテーブルの配列列は M6 以降です。
- **C-5 DEFERRABLE**: FK の `DEFERRABLE` / `INITIALLY DEFERRED` は 0A000 で拒否します（依頼どおり）。遅延制約は M6 以降の候補として記録します。
- **C-6 interval のフィールド制限**: `interval year to month` のような書き方は M5 では 0A000 とし、`interval(p)` の精度指定だけ対応します（工数 S で足せます）。
- **C-7 DEFAULT に `'now'` と書いた場合の差分**: DEFAULT 式をテキストで保存して毎回評価する方式（Q-006）のため、`DEFAULT 'now'::timestamp` が PG（作成時の時刻で固定）と違って毎回の時刻になります。推奨は「保存時に文字列定数のキャストを定数畳み込みする」です。既知の差分として残す選択もあります（実害は小さい）。
- **C-8 FK のトリガー**: PG は FK を内部トリガーで実装しますが、yuzhu はトリガーを作らず実行器に組み込みます。`pg_trigger` に行が出ない点が PG との差分になります（psql の `\d` には影響しません）。
- **C-9 複数ライター下の FK**: M5 の複数ライター設計で行ロック（`SELECT ... FOR UPDATE/SHARE`）を入れるならそれを使い（PG と同じ）、入れないなら「キー値単位のロック表」で代用します（工数 M）。行ロックを M5 に入れるかどうかは、複数ライターの調査と合わせて決める必要があります。
- **C-10 DateStyle**: 出力形式は ISO だけに対応し、`SET DateStyle = 'SQL'` などは 0A000 にします。日付の順序（MDY/DMY/YMD）は切り替えられます。
- **C-11 json / jsonb**: 依頼の範囲外ですが需要が大きいため、M6 の最初の型追加の候補として記録します。

---

## 13. 未検証の点（実装前に確かめるとよいもの）

- numeric の `%` / `mod` の結果 scale の正確な規則（max(s1, s2) と記憶しているが、`mod_var` の rscale を読んで確かめる）。
- numeric → 整数のキャストでの NaN / Infinity のエラー文言と SQLSTATE。
- `numeric_send` / `numeric_recv` の検証内容、`date_send` などのバイナリ形式の細部。
- `DecodeInterval` の小数の単位の繰り下げ規則（`AdjustFractDays`、`AdjustFractSeconds`）と、PG17 で入った interval の infinity の内部表現。
- `EncodeInterval`（postgres 形式）で `+` 記号を付ける条件の正確な規則。
- 2 桁の年の解釈規則。
- タイムゾーン名の大文字小文字の扱いと、`timezonesets/Default` に含まれる略称の正確な一覧と値。
- POSIX 形式のタイムゾーン（`UTC+9`、`EST5EDT`）の扱い。
- psql の `\d` が FK の表示に `pg_trigger` を参照しないこと（describe.c の確認）。
- `ON DELETE SET NULL (cols)` の pg_constraint 上の表現（`confdelsetcols`）と `pg_get_constraintdef` の出力。
- `DROP TABLE 親 CASCADE` の NOTICE の文言。
- JDBC / pgx が日付時刻をバイナリで要求する条件と、DateStyle が ISO でないときの挙動。
