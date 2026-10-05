# yuzhu M5 基本設計 06: 日時（interval・time・タイムゾーンと DateStyle・日時の関数・バイナリ形式）

M5 の日時の仕事は、**M4 が統合した date・timestamp・timestamptz の上に、interval と time を載せ、セッション設定（`TimeZone`・`DateStyle`・`IntervalStyle`）を SET で実際に効かせ、日時の関数と演算子をそろえ、日時 5 型のバイナリ形式（Extended Query 用）を作る**ことです。`yuzhu-datetime` クレートは interval・全 DateStyle / IntervalStyle・TZif の読み取り・`extract` / `date_trunc` などを PostgreSQL 17 の C コードから移植済みで、約 7 万件の差分コーパス（`tests/fixtures/pg17_corpus.tsv`）を通している。したがってこの章の実体は「クレートの足りない部分（`time`、`age`、`make_*`、interval ゾーンの `AT TIME ZONE`、型修飾子の入出力）を足す」ことと「`yuzhu-core` への統合（`Datum`・カタログの行・演算子・設定・パーサ・バイナリ）」である。

- 契約: `spec/design/m5/00-contracts.md`（以下「00」）。**この章は 00 に従う**（食い違いは §11 に書いた）。型の枠組みは 05 章（TY）、バイナリの振り分けは 04 章（XQ）が持つ。
- 前提の設計書: `spec/design/m4/00-contracts.md`（以下「M4 契約」。§12 型、§14.2・§14.3 `Settings::type_env` と `RuntimeInfo`、§15.4 設定）。M4 の章 09（`09-types-functions.md`）と章 07（`07-catalog-ddl.md`）は並行して作成中で、まだ無い。**確定したら、この章の担当は最初に突き合わせ直す**（§9 の最初の項目）。
- 調査資料: `spec/research/m5-types-fk.md` §3（日付・時刻型）、`pg-compat-tools.md`（Rails が `intervalstyle = iso_8601` を、pg_dump と pgAdmin が `DateStyle` を接続時に SET する）。
- 既存のコード（読み取りだけ）: `impl/rust/crates/yuzhu-datetime/`（`lib.rs` の公開 API、`tests/pg_corpus.rs`、`tests/fixtures/gen_corpus.py`）、`yuzhu-core/src/settings.rs`、`sql/parser/{expr,misc}.rs`。
- 実機: 設計時に PostgreSQL 17.11（`sandbox/pg.sh`、TimeZone `Etc/UTC`）で SQL を投げて確かめた事実には【確認】を付けた。OID は同じ実機の `pg_proc` / `pg_operator` / `pg_cast` / `pg_type` から取った（【確認】。実装時に再度 `.dat` で確かめる。00 §3.2）。【記憶】は未照合、【提案】は yuzhu への推奨。
- 略号: TD。決定は `TD-D<n>`、確認事項は `M5-TD-Q<n>`。

---

## 0. 決定（この章で扱う論点。調査間・既存設計との食い違いも）

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| TD-D1 | 日時の実装方式 | 新規に書く／`yuzhu-datetime` を使う（00 D22） | **使う。クレートに足すのは `Time`、`age`、`make_*`、`to_timestamp(float8)`、`timezone(interval, ts)`、型修飾子の入出力、`Time` の `extract` だけ。それ以外の日時の意味論は、クレートの関数をそのまま呼ぶ** | クレートは PostgreSQL 17 の C コードの移植で、差分コーパスが通っている。`yuzhu-core` に意味論を二重に持たない |
| TD-D2 | DateStyle の範囲 | ISO と MDY / DMY / YMD だけ（m5-types-fk C-10・§3.5）／クレートが実装する全形式（00 D24） | **全形式（ISO / SQL / Postgres / German、`postgres` / `postgres_verbose` / `sql_standard` / `iso_8601`）**。調査の推奨（0A000）は採らない | 00 D24 の決定。クレートが出力側を実装済みで、追加の工数がない。Rails の `iso_8601`、pg_dump の `DateStyle` が実際に効く |
| TD-D3 | interval の型修飾子 | 精度 `interval(p)` だけ対応し、フィールド指定は 0A000（m5-types-fk C-6）／PostgreSQL と同じ全形式 | **全形式（`YEAR`・`MONTH`・`DAY`・`HOUR`・`MINUTE`・`SECOND`、`... TO ...`、`SECOND(p)`、`(p)`）。符号化は PostgreSQL と同じ（§3.2）** | クレートの `Interval::with_typmod` と `interval_typmod` が対応済み。パーサに足すのは数十行 |
| TD-D4 | `Datum::Time` の中身 | `Time(yuzhu_datetime::Time)`（M4 の `Date` などと同じ新型）／`Time(i64)`（00 §4.9） | **`Time(i64)`（00 のとおり）。クレートには `Time(pub i64)` の新型を置き、境界で `Time(v)` ⇄ `v` に変える** | 00 の契約に従う。`Datum` の他の整数系と同じ扱いで、比較・ハッシュが整数 |
| TD-D5 | `timetz` | 対応する／しない（m5-types-fk §3.1。docs も非推奨） | **対応しない（0A000）。型名 `timetz` / `time with time zone`、`current_time`、`timetz` を返す関数・キャストはすべて `0A000 type timetz is not supported yet`** | 00 §1.1 の「M5 でも対応しないもの」。`localtime` は `time` を返すので対応する |
| TD-D6 | 現在時刻の取得 | `SystemTime::now()` を散在させる／`Clock` トレイト（00 §2 規約 6） | **`util/clock.rs` に `Clock` トレイトを置く。`Cluster` が `Arc<dyn Clock>` を 1 つ持ち、`Session` がすべての時刻をそこから取る。トランザクションの開始時刻は「そのトランザクションの最初の文の `statement_timestamp`」と同じ値（PostgreSQL と同じ。§5.2）** | テストで時計を固定できる。`now() = statement_timestamp()`（暗黙のトランザクション）が PostgreSQL と一致する【確認】 |
| TD-D7 | `TimeZone` の検証と保存形式 | 文字列のまま保存（M1 の `Kind::Str`）／SET 時に `ZoneDb` で検証し、正規名で保存 | **SET 時に `parse_timezone_setting` で検証し、`TimeZone::name()`（正規の綴り。`asia/tokyo` → `Asia/Tokyo`、`9` → `<+09>-09`）を保存する。不正は 22023** | PostgreSQL の `check_timezone` と同じ。ParameterStatus と SHOW が正規名になる【確認】 |
| TD-D8 | 設定から `DateTimeEnv` を作る場所 | 文ごとに文字列から解析（M4 契約 §14.2 の読みどおり）／`Settings` が解析結果を持つ | **`Settings` が解析済みの `DateTimeSettings`（`TimeZone`、`DateStyle`、`DateOrder`、`IntervalStyle`）を持ち、値を変える操作（`set`・`reset`・`reset_all`・`commit`・`rollback`・`new`）の最後に作り直す。`type_env()` はそれを借りるだけ** | `DateTimeEnv` は `&TimeZone` を借りる。文ごとに `ZoneDb` を引くと遅く、借用の持ち主も要る |
| TD-D9 | `timezone_abbreviations` | 設定ファイルを読む／`Default` 固定（m5-types-fk §3.6.2、00 §3.6） | **`Default` 固定。SET は `Default`（大文字小文字を区別する）だけ受け付け、それ以外は 22023** | クレートが `Default` の略称表を持つ。実機は `'default'` も 22023 だった【確認】 |
| TD-D10 | `DEFAULT 'now'` | 毎回評価する（M2・M4 D-21 は DEFAULT を SQL テキストのまま保存し、使うたびに解析する）／PostgreSQL と同じく CREATE TABLE の時点で畳み込む（m5-types-fk C-7） | **畳み込む。`'now'`・`'today'`・`'tomorrow'`・`'yesterday'` を含む日時リテラルの型変換は、CREATE TABLE の解析時に値にして、保存テキストに畳み込んだ値を書く（§5.3）。M4 の `analyzer/ddl.rs` に 1 か所の呼び出しが要る（§11 の依頼）** | PostgreSQL は `DEFAULT 'now'` を作成時刻の定数にする【確認】。毎回評価する実装は、`DEFAULT 'now'` を使う既存のスキーマが静かに別の意味になる |
| TD-D11 | `to_char`・`to_date`・`to_timestamp(text, text)` | M5／M6（m5-types-fk §3.7） | **M6。`pg_proc` の行は作り、実行すると `0A000`（`to_char is not supported yet`）** | 書式言語が大きい。行を作っておけば `42883` ではなく `0A000` になる（00 §1.1 の「実行すると 0A000」） |
| TD-D12 | `OVERLAPS`・`timeofday()`・`pg_timezone_names` / `pg_timezone_abbrevs`・`generate_series` の `text` ゾーン版 | M5／M6 | **M6（`OVERLAPS` はパーサが `0A000`、残りは存在しない）** | 使用頻度が低い |
| TD-D13 | `generate_series(timestamp, timestamp, interval)` | M5／M6 | **M5。ただし M4 の `FunctionScan` の表（`generate_series` の登録方式）に乗る。M4 の章 03 / 09 が決める方式に合わせる（§6.5.6）。乗れなければ M6** | M4 が `generate_series(int4, int8)` を FROM 句で実装する。登録の形を共有できれば半日 |
| TD-D14 | バイナリ形式の受信（`*_recv`）の検査 | 検査しない／PostgreSQL と同じ範囲検査 | **PostgreSQL と同じ検査（§6.7）。長さの過不足は型の `binary_recv` ではなく `RecvBuf` と 04 章の `input_binary` が決める: 不足は `08P01 insufficient data left in message`、余りは `22P03 incorrect binary data format`**（【実機】`::time` に 1 バイトは 08P01。05 §6.4、04 XQ-D10。レビュー対応 R-04） | 00 §4.9 の契約 |
| TD-D15 | 日付時刻の型修飾子の適用 | 各型の関数がそれぞれ持つ／`types::datetime::apply_typmod` に集める | **`apply_typmod(d, ty) -> Result<Datum>` に集める（text 入力・binary 入力・キャスト・代入のすべてが通る）** | 経路ごとの丸め忘れを防ぐ |
| TD-D16 | 既定の `TimeZone` | `UTC`（M1）／`Etc/UTC`（initdb が検出した PostgreSQL の既定） | **`UTC`（M1 のまま）。`ClusterOptions.timezone` で変えられる** | 共有テストは先頭で `SET TIME ZONE 'UTC'` を明示する（m5-types-fk §3.6.3）。`SET` 後の `SHOW` は両者で `UTC` になる。既知の差（§10） |

---

## 1. 範囲

### 1.1 対応するもの

| 分類 | 内容 | WP |
|---|---|---|
| `interval` | `Datum::Interval`、ディスク形式（16 バイト）、入出力（`IntervalStyle` 4 形式）、比較・ハッシュ（正規化）、`+ - * /`・単項 `-`、日時との演算（`timestamp - timestamp`、`timestamp ± interval`、`date ± interval`、`time ± interval`、`time - time`）、キャスト（`interval(p)`・フィールド指定、`time` ⇄ `interval`）、型修飾子、`sum` / `avg` / `min` / `max`、`justify_*`、`isfinite`、`make_interval`、`extract` / `date_part` / `date_trunc` | TD-1、TD-3 |
| `time`（without time zone） | `yuzhu-datetime` の `Time`、`Datum::Time`、入出力、比較、`time ± interval`、`time - time`、`date + time`、キャスト（`timestamp` / `timestamptz` / `interval` → `time`）、`time(p)`、`make_time`、`extract` / `date_part`、`localtime`、`min` / `max`、バイナリ | TD-4 |
| 設定 | `TimeZone`（IANA 名、POSIX 文字列、数値オフセット、`INTERVAL`）、`DateStyle`、`IntervalStyle`、`timezone_abbreviations`（`Default` 固定）。SET・SET LOCAL・RESET・ROLLBACK・ParameterStatus・`SET TIME ZONE` の全構文 | TD-2 |
| `AT TIME ZONE` | `timestamp` / `timestamptz` と、ゾーンが `text` または `interval`。`AT LOCAL`（PostgreSQL 17）。`timezone(zone, ts)` / `timezone(ts)` 関数 | TD-2 |
| 関数 | `now()` 系と `Clock`、`extract` / `date_part`、`date_trunc`、`date_bin`、`age`、`make_date` / `make_time` / `make_timestamp` / `make_timestamptz` / `make_interval`、`to_timestamp(float8)`、`justify_days` / `justify_hours` / `justify_interval`、`isfinite`、`date_add` / `date_subtract`（任意）、`pg_sleep_for`（任意） | TD-3 |
| バイナリ形式 | `date`・`time`・`timestamp`・`timestamptz`・`interval` の `send` / `recv`（§6.7。例つき） | TD-5 |
| opclass | `time_ops`、`interval_ops`（B+Tree。M4 の `datetime_ops` に並べて行を足す） | TD-1、TD-4 |
| テスト | `yuzhu-datetime` の差分コーパスの拡張（PostgreSQL 17 との比較）、`tests/slt/m5/datetime/`、Rust のバイナリ試験 | TD-5 |

### 1.2 対応しないもの（実行すると `0A000`）

- **`timetz`**（`time with time zone`、`current_time`、`timetz` を引数・結果に持つ関数と演算子。TD-D5）。メッセージは `type timetz is not supported yet`（型名の解決で出す。M4 が `interval` / `time` / `timetz` に対して使っている形と同じ）。
- **`to_char`・`to_date`・`to_timestamp(text, text)`**（TD-D11。`pg_proc` の行は作る。OID は §6.5.3）。
- **`OVERLAPS`**（パーサが `0A000`）。**`timeofday()`**・`pg_timezone_names`・`pg_timezone_abbrevs`・`pg_postmaster_start_time` 以外のシステム情報（存在しない。それぞれ `42883` / `42P01`）。
- **`timezone_abbreviations` の変更**（`Default` 以外）、**`lc_time`** の効果（受け付けて保存するだけ）。
- **日時型の配列列**（ユーザーテーブル。00 D25）。`interval[]`・`time[]` の型 OID（1187、1183）は `pg_type` に行がある（M1 から）が、値は 05 章の配列が扱う。この章は要素型としての `send` / `recv` / 比較を提供する。
- **`SET TIME ZONE` の `timetz` 系の副作用**、`AT TIME ZONE` を `time` に使うこと（PostgreSQL は `time → timetz` の暗黙キャストで通る。yuzhu は `42883`。既知の差）。

### 1.3 この章が保証すること

1. 日時 5 型（`date`・`time`・`timestamp`・`timestamptz`・`interval`）の**テキスト入出力は、`DateStyle` / `IntervalStyle` / `TimeZone` の全組み合わせで PostgreSQL 17 と一字一句同じ**（エラーの SQLSTATE と文言を含む）。根拠はクレートの差分コーパスと、この章が足すコーパス（§7.1）。
2. SET が実際に効き、トランザクションのロールバック・`SET LOCAL`・`RESET`・`RESET ALL` で正しく戻る。ParameterStatus は、値が変わったときだけ、PostgreSQL と同じ時機（ReadyForQuery の直前）に送る（M1 の仕組みのまま。§5.1）。
3. 現在時刻（`now()` など）はすべて `Clock` を通り、同じトランザクションの `now()` は変わらない。
4. バイナリ形式は PostgreSQL の `*_send` と同じバイト列で、`*_recv` は同じ範囲を検査する。

---

## 2. 構成

`★` 新規、`△` 変更（M4 が作ったものを含む）。M5 全体の木は 00 §2。持ち主は 00 §8 のとおり（TD は `types/{datetime,interval,time}.rs`、`catalog/builtin/datetime.rs`、`catalog/opclass.rs` の time・interval の行、`yuzhu-datetime/*`、`settings.rs` の日時の設定）。この章が新しく作るファイルのうち 00 の表に無いもの（`util/clock.rs`、`sql/parser/datetime.rs`、`yuzhu-datetime/src/{time,typmod,age,make}.rs`）は §11 で持ち主の追記を依頼する。

```
impl/rust/crates/
├── yuzhu-datetime/src/
│   ├── time.rs          ★ Time、DecodeTimeOnly の移植、time の演算・extract（TD-4。早期着手可。D49）
│   ├── typmod.rs        ★ interval / time / timestamp の型修飾子の入出力（interval_typmod_in / out ほか。TD-1）
│   ├── age.rs           ★ age（timestamp_age / timestamptz_age の移植。TD-3）
│   ├── make.rs          ★ make_date / make_time / make_timestamp / make_timestamptz / make_interval / to_timestamp(float8)（TD-3）
│   ├── interval.rs      △ `at_time_zone_interval` を timestamp.rs 側に足す。interval 自体は変えない
│   ├── timestamp.rs     △ `timestamptz_at_time_zone_interval`、`timestamp_to_time` など time との変換（TD-2、TD-4）
│   ├── fields.rs        △ `extract_time` / `date_part_time`（TD-4）
│   ├── lib.rs           △ 上の公開
│   └── tests/ pg_corpus.rs △  fixtures/gen_corpus.py △  fixtures/pg17_corpus.tsv △（種別の追加。§7.1）
├── yuzhu-core/src/
│   ├── util/clock.rs                  ★ Clock、SystemClock、ManualClock（TD-3）
│   ├── types/
│   │   ├── datetime.rs                △ M4 が作成。日時 5 型の send / recv、apply_typmod、typmod_in / out、DateTimeError → Error、DEFAULT の畳み込み（TD-1、TD-5）
│   │   ├── interval.rs                ★ interval の関数本体（演算子・キャスト・集約の状態）（TD-1）
│   │   └── time.rs                    ★ time の関数本体（TD-4）
│   ├── catalog/builtin/datetime.rs    △ F0 が M4 の行を移した先。interval / time の pg_type・pg_proc・pg_operator・pg_cast・集約の行（TD-1、TD-3、TD-4）
│   ├── catalog/opclass.rs             △ time_ops・interval_ops の行（§6.8）
│   ├── settings.rs                    △ TimeZone の検証、DateStyle / IntervalStyle のクレート呼び出し、timezone_abbreviations、DateTimeSettings（TD-2）
│   ├── sql/parser/datetime.rs         ★ INTERVAL のフィールド指定・型付きリテラル、AT TIME ZONE / AT LOCAL、SET TIME ZONE INTERVAL（TD-1、TD-2）
│   ├── session/（mod.rs・txn_ctl.rs）  △ started_at / statement 時刻を Clock から取る（F0 と LK の持ち物。§11 で依頼）
│   └── engine.rs                      △ Cluster に clock と zones、ClusterOptions.{clock, timezone, timezone_dir}（DB・LK の持ち物。§11 で依頼）
```

依存の方向: `util::clock` は `types` より下（他に依存しない）。`types::{datetime, interval, time}` は `yuzhu-datetime` と `error` だけに依存する。`settings` は `yuzhu-datetime` を使う（`ZoneDb` は `Arc` で受け取る）。`catalog/builtin/datetime.rs` は `types::{datetime, interval, time}` の関数を指す。

**この章の規約**:

1. 日時の意味論（算術・丸め・範囲検査・エラーの文言）は **`yuzhu-datetime` に置き、`yuzhu-core` は呼ぶだけ**にする。`yuzhu-core` の側で月・日・時刻の計算をしない。
2. `DateTimeError`（SQLSTATE と文言と DETAIL / HINT を持つ）から `Error` への変換は `types::datetime::dt_err` の 1 か所（§4.3）。
3. セッションの `TimeZone` に依存する関数・演算子・キャストは、`FnKind::Runtime`（`RuntimeInfo::datetime_env()`）で実装する。依存しないものは `FnKind::Pure`。§6.5 の表の「種別」列が正。
4. `SystemTime::now()` は `SystemClock` の実装だけが呼ぶ（00 規約 6）。既存の呼び出し（`control.rs`・`datadir.rs`）は §11 で `Clock` に置き換えを依頼する。
5. 入力関数に日時の特殊文字列（`now` など）を渡すときの「今」は `DateTimeEnv.now`（= トランザクション開始時刻）。DDL の解析でも同じ（§5.3）。

---

## 3. ディスク上の形式

M2 §3.6 の「列の値の符号化」の表に次を足す。`date`・`timestamp`・`timestamptz` は M4 §12.3 のまま（再掲しない）。**変えない**。

### 3.1 time

| 項目 | 内容 |
|---|---|
| OID・性質 | 1083、typlen 8、`typbyval` t、typalign `d`、typstorage `p`、typcategory `D`、`typispreferred` f |
| 符号化 | `i64` リトルエンディアン。**0 時からのマイクロ秒**。範囲は `0 ..= 86_400_000_000`（`24:00:00` を含む） |
| 型修飾子 | 小数秒の桁数 p（0〜6）。+4 しない。無指定は -1 |
| `Datum` | `Time(i64)` |

例: `'04:05:06.789'::time` = 14 706 789 000 µs = `0x0000_0003_6C97_CA88`。ディスクは `88 ca 97 6c 03 00 00 00`。`'24:00:00'::time` = 86 400 000 000 = `0x0000_0014_1DD7_6000`。ディスクは `00 60 d7 1d 14 00 00 00`。

### 3.2 interval

| 項目 | 内容 |
|---|---|
| OID・性質 | 1186、typlen **16**、`typbyval` **f**、typalign `d`、typstorage `p`、typcategory `T`、**`typispreferred` t** |
| 符号化 | **16 バイト固定長（varlena ではない）**。リトルエンディアンで `time: i64`（マイクロ秒）→ `day: i32` → `month: i32` の順。M2 §3.6 の固定長の規則（`typalign` で整列して `typlen` バイトを書く）に乗る |
| 無限 | `infinity` = `time = i64::MAX, day = i32::MAX, month = i32::MAX`。`-infinity` はすべて最小値。クレートの `Interval::INFINITY` / `NEG_INFINITY` と同じ（PostgreSQL 17 の `INTERVAL_NOBEGIN` / `INTERVAL_NOEND` と同じ） |
| 型修飾子 | `typmod = (range << 16) \| precision`。`range` は 15 ビットのフィールドのマスク（`MONTH` = 0x2、`YEAR` = 0x4、`DAY` = 0x8、`HOUR` = 0x400、`MINUTE` = 0x800、`SECOND` = 0x1000。全部 = `0x7FFF` = `FULL`）、`precision` は下位 16 ビット（`0xFFFF` = 指定なし）。**`typmod = -1` は無指定**。`interval(p)`（フィールド指定なし）は `(0x7FFF << 16) \| p` |
| `Datum` | `Interval(yuzhu_datetime::Interval)`（`months: i32, days: i32, micros: i64`。**メモリ上のフィールド順とディスクの順は違う**。`types::interval::encode` / `decode` が変換する） |

実機の `atttypmod`【確認】: `interval` = -1、`interval(3)` = 2147418115（`0x7FFF0003`）、`interval(0)` = 2147418112、`interval second` = 268500991（`0x1000FFFF`）、`interval second(3)` = 268435459（`0x10000003`）、`interval year to month` = 458751（`0x0006FFFF`）、`interval hour to minute` = 201392127（`0x0C00FFFF`）。クレートの `interval_typmod(precision, range)` はこの値を作る。

例: `'1 year 2 mons 3 days 04:05:06.789'::interval` = `{ months: 14, days: 3, micros: 14_706_789_000 }`。ディスクの 16 バイト（リトルエンディアン）:

```text
88 ca 97 6c 03 00 00 00   time  = 14_706_789_000
03 00 00 00               day   = 3
0e 00 00 00               month = 14
```

`'-1 days -01:00:00'::interval` = `{ months: 0, days: -1, micros: -3_600_000_000 }` → `00 5c 6c 29 ff ff ff ff | ff ff ff ff | 00 00 00 00`。

**比較**（B+Tree・ソート・`=`）は、月 = 30 日、日 = 24 時間に換算した 128 ビット整数（`Interval::cmp_value`）で行う。`'1 mon' = '30 days'`、`'1 day' = '24:00:00'` は真【確認】。**格納する 3 つ組は正規化しない**（`'30 days'` と `'1 mon'` は別のバイト列で、値としては等しい。PostgreSQL と同じ）。ハッシュも換算値から作る（`cmp_datum` が Equal なら同じハッシュ。M4 §12.2）。

### 3.3 date・timestamp・timestamptz（再掲なし。この章が扱う点だけ）

- 値の符号化は M4 §12.3 のまま（`date` は i32 LE、`timestamp` / `timestamptz` は i64 LE。2000-01-01 起点。`∓infinity` は最小値 / 最大値）。
- バイナリ形式のバイト列は §6.7（ネットワークバイトオーダー。**ディスクと値は同じでバイト順が逆**）。

---

## 4. 共通の型（契約）

00 §4.9 の `Datum` の変種（`Time(i64)`、`Interval(yuzhu_datetime::Interval)`）に従う。ここに書いたシグネチャは 00 と食い違わない（食い違うものは §11）。

### 4.1 yuzhu-datetime への追加（TD-1〜TD-4）

```rust
// time.rs（TD-4。D49: 新規ファイルだけの作業なので M4 の実装中に始めてよい）
/// time without time zone。0 時からのマイクロ秒（0..=USECS_PER_DAY。24:00:00 を含む）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(pub i64);
impl Time {
    pub const MIN: Time = Time(0);
    pub const MAX: Time = Time(MICROS_PER_DAY);                           // 24:00:00
    /// time_in（DecodeTimeOnly の移植）。typmod は time(p)。-1 = 指定なし
    pub fn parse(input: &str, typmod: i32, env: &DateTimeEnv<'_>) -> Result<Time>;
    /// time_out（EncodeTimeOnly。DateStyle によらず HH:MM:SS[.ffffff]）
    pub fn format(self) -> String;
    pub fn with_typmod(self, typmod: i32) -> Result<Time>;                // AdjustTimeForTypmod（四捨五入）
    /// time + interval / time - interval。months と days は無視し、micros だけを足して 24 時間で巻き戻す
    pub fn add_interval(self, span: &Interval) -> Result<Time>;
    pub fn sub_interval(self, span: &Interval) -> Result<Time>;
    /// time - time → interval { months: 0, days: 0, micros: self - other }（巻き戻さない。24:00:00 - 00:00:00 = 24:00:00）
    pub fn sub_time(self, other: Time) -> Interval;
    pub fn to_interval(self) -> Interval;                                 // time → interval（暗黙）
    /// interval → time（代入）。micros を 24 時間で正に巻き戻す。無限は 22008
    pub fn from_interval(iv: &Interval) -> Result<Time>;
    /// make_time(hour, min, sec)。範囲外は 22008 `time field value out of range: H:MM:SS`
    pub fn make(hour: i32, min: i32, sec: f64) -> Result<Time>;
}
impl Timestamp   { pub fn to_time(self) -> Option<Time>; }                // 無限は None（SQL では NULL）
impl TimestampTz { pub fn to_time(self, tz: &TimeZone) -> Result<Option<Time>>; }
impl Date        { pub fn add_time(self, t: Time) -> Result<Timestamp>; } // date + time（datetime_pl。無限の date は無限）

// fields.rs
pub fn extract_time(units: &str, t: Time) -> Result<Option<NumericValue>>;
pub fn date_part_time(units: &str, t: Time) -> Result<Option<f64>>;

// typmod.rs（TD-1）。時刻の型修飾子の入出力（pg_type の typmodin / typmodout）
/// time(p) / timestamp(p) / timestamptz(p) の typmodin。p < 0 は 22023 `TIME(-1) precision must not be negative`、
/// p > 6 は WARNING（`TIME(7) precision reduced to maximum allowed, 6`。22023）を返して 6 にする
pub fn precision_typmod_in(type_name: &str, p: i64) -> Result<(i32, Option<DateTimeError>)>;   // Some = WARNING
/// intervaltypmodin。mods は [] / [range] / [range, p]（gram.y が作る形。§6.6.1）
pub fn interval_typmod_in(mods: &[i64]) -> Result<(i32, Option<DateTimeError>)>;
/// typmodout: ""、"(3)"、" year to month"、" second(3)" など（format_type が型名に続ける）
pub fn precision_typmod_out(typmod: i32) -> String;                                              // "(3)"
pub fn interval_typmod_out(typmod: i32) -> String;

// age.rs / make.rs（TD-3）
impl Timestamp   { pub fn age(self, other: Timestamp) -> Result<Interval>; }
impl TimestampTz { pub fn age(self, other: TimestampTz, tz: &TimeZone) -> Result<Interval>; }
pub fn make_date(year: i32, month: i32, day: i32) -> Result<Date>;
pub fn make_timestamp(year: i32, month: i32, day: i32, hour: i32, min: i32, sec: f64) -> Result<Timestamp>;
/// zone が None ならセッションの TimeZone。Some のときは make_timestamptz 用のゾーンの解釈（数値オフセットは ISO の符号。§6.4.4）
pub fn make_timestamptz(year: i32, month: i32, day: i32, hour: i32, min: i32, sec: f64,
                        zone: Option<&str>, env: &DateTimeEnv<'_>) -> Result<TimestampTz>;
pub fn make_interval(years: i32, months: i32, weeks: i32, days: i32, hours: i32, mins: i32, secs: f64) -> Result<Interval>;
pub fn to_timestamp_float(secs: f64) -> Result<TimestampTz>;               // to_timestamp(float8)

// timestamp.rs
impl Timestamp   { pub fn at_time_zone_interval(self, zone: &Interval) -> Result<TimestampTz>; }   // timezone(interval, timestamp)
impl TimestampTz { pub fn at_time_zone_interval(self, zone: &Interval) -> Result<Timestamp>; }     // timezone(interval, timestamptz)
```

- 無限の `interval` と `Time` の組み合わせ（`time + 'infinity'::interval`）は PostgreSQL 17 に合わせてエラー（`cannot add infinite interval to time`。22008。**文言は実装時に実機で確かめる**。§9）。
- `DateTimeEnv` は変えない（`now`・`zones`・`time_zone`・`date_style`・`date_order`・`interval_style`）。

### 4.2 util/clock.rs（TD-3）

```rust
/// 壁時計。SystemTime::now() を呼ぶのは SystemClock だけ（00 規約 6）
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// UNIX エポック（1970-01-01 00:00:00 UTC）からのマイクロ秒
    fn unix_micros(&self) -> i64;
    /// PostgreSQL のエポック（2000-01-01 00:00:00 UTC）からのマイクロ秒。timestamptz の値そのもの
    fn pg_micros(&self) -> i64 { self.unix_micros() - PG_EPOCH_UNIX_MICROS }
}
pub const PG_EPOCH_UNIX_MICROS: i64 = 946_684_800_000_000;

#[derive(Debug, Default)] pub struct SystemClock;                          // SystemTime::now()。時計が 1970 年より前なら 0
#[derive(Debug)] pub struct ManualClock { /* AtomicI64 */ }                // テスト用。進めなければ止まっている
impl ManualClock {
    pub fn new(unix_micros: i64) -> Arc<Self>;
    pub fn set(&self, unix_micros: i64);
    pub fn advance(&self, micros: i64);
}

// engine.rs（00 §4.8 の Cluster に足す。§11）
//   ClusterOptions { clock: Arc<dyn Clock>（既定 SystemClock）, timezone: String（既定 "UTC"）, timezone_dir: PathBuf（既定 /usr/share/zoneinfo） }
//   impl Cluster { pub fn clock(&self) -> &Arc<dyn Clock>; pub fn zones(&self) -> &Arc<yuzhu_datetime::ZoneDb>; }
```

### 4.3 types/datetime.rs・interval.rs・time.rs（TD-1、TD-4、TD-5）

```rust
// types/datetime.rs
/// DateTimeError → Error（SQLSTATE・メッセージ・DETAIL・HINT をそのまま写す。M5 規約 7）
pub fn dt_err(e: yuzhu_datetime::DateTimeError) -> Error;
/// 型修飾子の適用。text 入力・binary 入力・キャスト・代入のすべてが通る（TD-D15）。
/// date は typmod なし（そのまま返す）。timestamp / timestamptz / time / interval は丸める。無限はそのまま
pub fn apply_typmod(d: Datum, ty: SqlType) -> Result<Datum>;
/// 日時 5 型の入出力（io.rs の振り分けから呼ばれる）
pub fn input_text(s: &str, ty: SqlType, env: &TypeEnv<'_>) -> Result<Datum>;     // typmod は ty.typmod で適用済み
pub fn output_text(d: &Datum, ty: SqlType, env: &TypeEnv<'_>) -> Result<String>;
/// バイナリ（§6.7）。output は PostgreSQL の *_send、input は *_recv（範囲検査つき。typmod は適用しない）。
/// **形は 04 §4.4・00 §4.9 の `binary_send` / `binary_recv` に統一した**（レビュー対応 R-04）。各型のモジュール
/// （`types/{datetime,interval,time}.rs`）は同じ名前で公開する。`RecvBuf` は `types/binary.rs`（04）の型
pub fn binary_send(d: &Datum, ty: SqlType) -> Result<Vec<u8>>;
pub fn binary_recv(buf: &mut RecvBuf<'_>, ty: SqlType) -> Result<Datum>;     // ty.typmod は −1。長さの過不足は RecvBuf と input_binary が判定する
/// pg_type.typmodin / typmodout の実体（型 OID ごと）。警告は notices に積む
pub fn typmod_in(type_oid: Oid, mods: &[i64], notices: &mut Vec<Notice>) -> Result<i32>;
pub fn typmod_out(type_oid: Oid, typmod: i32) -> String;
/// DEFAULT の畳み込み（§5.3）
pub fn fold_time_dependent_literals(expr_text: &str, target: Option<SqlType>, env: &TypeEnv<'_>) -> Result<Option<String>>;

// types/interval.rs
pub fn encode(iv: &Interval) -> [u8; 16];            // ディスク（LE）: time, day, month
pub fn decode(b: &[u8; 16]) -> Interval;
// 演算子・関数の本体は §6.5 の表の fn 名。シグネチャは FnKind の型（Pure: fn(&[Datum]) -> Result<Datum>、Runtime: fn(&[Datum], &dyn RuntimeInfo) -> Result<Datum>）
```

`types::io` の振り分け（05 章が持つ）は、OID が 1082 / 1083 / 1114 / 1184 / 1186 のとき `types::datetime::{input_text, output_text}` を呼ぶ。`cmp_datum`・`hash_datum` は `Datum::Time(i64)`（整数）、`Datum::Interval`（`Ord` / `Hash`。クレートの実装は換算値）を扱う（M4 §12.2 の規約に従う。変種が型を表す）。

### 4.4 settings.rs（TD-2）

```rust
/// 解析済みの日時の設定。Settings が持つ（TD-D8）
#[derive(Debug, Clone)]
pub struct DateTimeSettings {
    pub time_zone: yuzhu_datetime::TimeZone,
    pub date_style: yuzhu_datetime::DateStyle,
    pub date_order: yuzhu_datetime::DateOrder,
    pub interval_style: yuzhu_datetime::IntervalStyle,
}

impl Settings {
    /// startup のパラメータ（DateStyle・TimeZone・IntervalStyle を含みうる）の検証にも使うので、ZoneDb を受け取る
    pub fn new(user: &str, startup: &[(String, String)], zones: Arc<yuzhu_datetime::ZoneDb>, default_timezone: &str) -> Settings;
    pub fn datetime(&self) -> &DateTimeSettings;
    /// M4 契約 §14.2 の type_env。datetime は Some(DateTimeEnv { .., now: TimestampTz(0) }) で、now は Session が文ごとに上書きする
    pub fn type_env<'a>(&'a self, zones: &'a yuzhu_datetime::ZoneDb) -> TypeEnv<'a>;
}
// Kind に足す: TimeZone（SET 時に parse_timezone_setting）、DateStyle（parse_datestyle の結果を format_datestyle で保存）、
// IntervalStyle（parse_intervalstyle）、TzAbbrev（"Default" だけ）。既存の Kind::DateStyle の normalize_datestyle は廃止する
```

M4 契約 §14.2 の `Settings::type_env(&self, zones)` と `Settings::new` の引数が変わる（`zones` を持たせる）。**§11 に依頼として書いた**（`Settings::new` の呼び出し元は `session/mod.rs`）。

---

## 5. 処理の流れ

### 5.1 SET・SET LOCAL・RESET・ParameterStatus

M1 の `Settings`（`session`・`current`・`txn_start`・`reset_values`）の枠はそのまま使う。TD-2 が変えるのは「値の検証と正規化」と「解析結果のキャッシュの作り直し」だけ。

```
Settings::set(name, args, local)
  1. 名前の検索（M1 のまま）。TimeZone / DateStyle / IntervalStyle / timezone_abbreviations は新しい Kind で検証・正規化する:
       TimeZone:   args は 1 つ（parse_zone_value が作る。§6.6.3）。
                   yuzhu_datetime::parse_timezone_setting(&zones, raw) → TimeZone。保存する値は tz.name()
                   失敗は 22023 `invalid value for parameter "TimeZone": "<raw>"`（DETAIL があればそれも）
       DateStyle:  args を "," でつないで yuzhu_datetime::parse_datestyle(raw, 現在の(style, order))
                   → format_datestyle(style, order) を保存（"ISO, MDY" "German, DMY" など）
                   失敗は 22023 `invalid value for parameter "DateStyle": "<raw>"` + DETAIL（Unrecognized key word / Conflicting "datestyle" specifications.）
       IntervalStyle: parse_intervalstyle(raw)。失敗は 22023 `invalid value for parameter "<ユーザーが書いた綴り>": "<raw>"`
                   + HINT `Available values: postgres, postgres_verbose, sql_standard, iso_8601.`
       timezone_abbreviations: raw == "Default" だけ許す。それ以外は 22023 `invalid value for parameter "timezone_abbreviations": "<raw>"`
  2. M1 のまま session / current に入れる（local なら current だけ）
  3. refresh_datetime(): current の 3 つの値から DateTimeSettings を作り直す
Settings::{reset, reset_all, commit, rollback, new, set_server_value} も最後に refresh_datetime() を呼ぶ
```

- **名前のつづり**: PostgreSQL 17 の `IntervalStyle`（enum）のエラーは**ユーザーが書いたつづり**で名前を出す（`set intervalstyle='foo'` → `"intervalstyle"`、`set "IntervalStyle"='foo'` → `"IntervalStyle"`）【確認】。`DateStyle` と `TimeZone` は常に正規のつづり【確認】。M1 の `Kind::Enum` は正規のつづりを出すので、IntervalStyle だけ `name` 引数（ユーザーのつづり）を使う。
- **`SET TIME ZONE` の全構文**（`SET TIME ZONE 9`、`'Asia/Tokyo'`、`INTERVAL '+09:00' HOUR TO MINUTE`、`LOCAL`、`DEFAULT`）は §6.6.3。実機の結果【確認】（TimeZone の既定は `Etc/UTC`）:

| 入力 | `SHOW TimeZone` |
|---|---|
| `'asia/tokyo'` | `Asia/Tokyo`（正規の綴り） |
| `9` / `'+9'` | `<+09>-09`（POSIX 形式の表示。ISO の符号） |
| `-5.5` | `<-05:30>+05:30` |
| `'UTC+9'` | `UTC+9`（POSIX の符号で 9 時間西。POSIX の名前は大文字化） |
| `'utc'` / `'UTC'` | `UTC` |
| `'Etc/GMT+9'` | `Etc/GMT+9` |
| `INTERVAL '+09:00' HOUR TO MINUTE` | `<+09>-09` |
| `'PST8PDT'` / `'EST'` | そのまま（tzdata にある名前） |
| `LOCAL` / `DEFAULT` / `RESET TIME ZONE` | 起動時の既定（yuzhu は `UTC`。実機は `Etc/UTC`。TD-D16） |
| `'JST'`・`'PST'`・`'Z'`・`''`・`'foo/bar'`・`'Asia/Tokyo '` | 22023（略称は SET できない） |
| `INTERVAL '1 day'` | 22023 + DETAIL `Cannot specify days in time zone interval.` |

- **ParameterStatus**: `DateStyle`・`IntervalStyle`・`TimeZone` は M1 から `reported = true`（`settings.rs` の `SETTINGS`）で、`Session::flush_parameter_status` が ReadyForQuery の直前に「最後に送った値と違うものだけ」を送る。**SET LOCAL の巻き戻し・ROLLBACK で値が戻ったときも、この仕組みで戻した値が送られる**（M1 の実装と試験がある）。TD が足すのは、送る値が**正規化後の値**になること（`TimeZone` なら `Asia/Tokyo`、`DateStyle` なら `ISO, MDY`）だけ。起動直後の ParameterStatus も同じ値。
- startup パケットの `DateStyle` / `TimeZone` / `IntervalStyle`（libpq は `PGDATESTYLE` → `datestyle`、`PGTZ` → `timezone` を送る）は `Settings::new` が同じ検証を通す。不正な値は無視する（M1 のまま。PostgreSQL は接続を拒否するが、M1 の決定を変えない）。
- `RESET` と `SET ... TO DEFAULT` は `reset_values` に戻す。`reset_values` は組み込みの既定を startup の値が上書きしたもの（M1 のまま）。

### 5.2 現在時刻と Clock

```
接続:        Session が cluster.clock() の参照を持つ
メッセージ受信（Simple Query の Q、Parse・Bind・Execute の各メッセージ）:
             session.stmt_ts = clock.pg_micros()                   ← statement_timestamp()。PostgreSQL は「最後のコマンドメッセージの受信時刻」【記憶】
トランザクション開始（BEGIN、または暗黙のトランザクションの最初の文）:
             txn.started_at = session.stmt_ts                      ← now() / transaction_timestamp() / current_timestamp / current_date / localtimestamp
文ごとの TypeEnv:
             env.datetime.now = TimestampTz(txn.started_at)         ← 'now' / 'today' / 'tomorrow' / 'yesterday' の「今」
clock_timestamp():           clock.pg_micros()                      ← 呼ぶたびに読む
statement_timestamp():       session.stmt_ts
```

- 実機【確認】: 暗黙のトランザクションで `now() = statement_timestamp()` は真。`BEGIN` の中の 2 文目以降では `now() < statement_timestamp()`。1 つの Simple Query メッセージに複数の文を入れると、`statement_timestamp()` は文ごとには変わらない（`select 1; select now() = statement_timestamp()` を 1 メッセージで送ると真）。
- `RuntimeInfo`（M4 §14.3）の `transaction_timestamp()` は `txn.started_at`、`statement_timestamp()` は `session.stmt_ts`、`clock_timestamp()` は `clock.pg_micros()`、`datetime_env()` は上の env。
- `current_date` は `now()` の日付をセッションの TimeZone で取る（`timestamptz::date`）、`localtimestamp[(p)]` は `now()::timestamp[(p)]`、`localtime[(p)]` は `now()::time[(p)]`、`current_timestamp[(p)]` は `now()` に精度を適用（`timestamptz(p)`）。**パーサは M4 が `FuncCall` に直す**（M4 契約 §3 の関数表）。`current_time` は `0A000`（TD-D5）。型は `now()`・`current_timestamp` = `timestamptz`、`current_date` = `date`、`localtime` = `time`、`localtimestamp` = `timestamp`【確認】。
- `now()`・`transaction_timestamp()`・`statement_timestamp()` は stable、`clock_timestamp()` は volatile（定数畳み込みと索引条件の扱いが変わる）。

### 5.3 DEFAULT の畳み込み（TD-D10）

PostgreSQL は `DEFAULT 'now'`（`timestamp` 列）、`DEFAULT 'today'`（`date` 列）、`DEFAULT 'now'::timestamptz` を、CREATE TABLE の解析時に評価して定数にする。実機【確認】:

| 書いた DEFAULT | `pg_get_expr` |
|---|---|
| `timestamp DEFAULT 'now'` | `'2026-10-05 05:13:28.759716'::timestamp without time zone` |
| `date DEFAULT 'today'` | `'2026-10-05'::date` |
| `timestamptz DEFAULT now()` | `now()`（関数呼び出し。挿入のたびに評価） |
| `timestamp DEFAULT 'now'::text::timestamp` | `('now'::text)::timestamp without time zone`（`text` を経由すると畳み込まれない。挿入のたびに評価） |
| `timestamptz DEFAULT current_timestamp` | `CURRENT_TIMESTAMP` |
| `time DEFAULT 'allballs'` | `'00:00:00'::time without time zone`（`allballs` は定数だが畳み込まれて出力形になる） |

M4 D-21 のとおり、yuzhu は DEFAULT を**利用者が書いたテキストのまま**保存して、挿入のたびに解析・評価する。このままだと `DEFAULT 'now'` が挿入のたびの時刻になる。**畳み込みの規則**:

1. CREATE TABLE（と M6 の ALTER TABLE ADD COLUMN）の DEFAULT を解析するとき、式の木の中の「**文字列リテラル（unknown）を `date` / `time` / `timestamp` / `timestamptz` に変換している箇所**」（暗黙の変換、`::type`、`type 'literal'` のどれも）を見つける。
2. その文字列が**時刻に依存する特殊値**（`now`、`today`、`tomorrow`、`yesterday`、または `today 12:00` のようにそれを含むもの）を含むなら、`env.datetime.now`（= CREATE TABLE のトランザクションの開始時刻。§5.2）で評価した値の**出力形の文字列**に置き換えたテキストを、保存する DEFAULT のテキストに書く（その箇所だけ。リテラルの位置はパーサの `Span` で分かる）。`'epoch'`・`'infinity'`・`'allballs'` は時刻に依存しないので置き換えない。
3. 置き換えは `types::datetime::fold_time_dependent_literals(expr_text, target, env) -> Result<Option<String>>`（`Some` = 書き換えたテキスト、`None` = 変更なし）が行う。内部では、クレートの `parse` に特殊文字列の判定（`has_now_token(&str) -> bool`。クレートの `tokens.rs` の `now`・`today`・`tomorrow`・`yesterday` の分類を公開する）を足して使う。

```text
CREATE TABLE t (a timestamp DEFAULT 'now', b date DEFAULT 'tomorrow', c timestamptz DEFAULT now());
  保存される DEFAULT: a → '2026-10-05 05:13:28.759716'      b → '2026-10-06'      c → now()
  （a の保存テキストは、リテラルを出力形の文字列に置き換えたもの。型は列の型で決まる。pg_get_expr は M4 の deparse が
   '2026-10-05 05:13:28.759716'::timestamp without time zone と出す）
```

- **呼び出しは M4 の `analyzer/ddl.rs`（M4 の持ち物）の CREATE TABLE にある DEFAULT の保存の 1 か所**。M5 の章は直接編集できないので、§11 に依頼として書いた。**M4 が受け入れない場合の退避**: 既知の差として扱い（`DEFAULT 'now'` が挿入のたびの時刻になる）、共有テストにこのケースを入れない。
- DEFAULT の評価（挿入のたび）は、保存テキストを解析して `env.datetime.now` で評価する（M2 のまま）。畳み込み後は定数なので何も起きない。

---

## 6. モジュールごとの仕様

### 6.1 yuzhu-datetime への追加（TD-1〜TD-4）

#### 6.1.1 `Time`（TD-4）

- **入力 `Time::parse`**: PostgreSQL の `time_in`（`ParseDateTime` + `DecodeTimeOnly`）。クレートには `ParseDateTime` の移植（`parse.rs::parse_datetime`）がある。`DecodeTimeOnly` を `parse.rs` に移植して `decode_time_only(fields, env) -> DtResult<Decoded>` を足す（`time.rs` から呼ぶ）。実機で確かめた規則【確認】:
  - 受理: `04:05:06.789`、`4:05`、`04:05 PM`（16:05:00）、`12:00 AM`（00:00:00）、`12:00 PM`（12:00:00）、`24:00:00`、**`23:59:60` → `24:00:00`**、`040506`、`0405`、`T04:05`、`allballs`（00:00:00）、`now`（`env.now` を現在のゾーンで時刻にしたもの）、`0:00 AM`、`04:05:06 BC`（BC は無視）。
  - **日付つき・ゾーンつきでもよい**: `'2024-01-02 03:04:05'::time` = `03:04:05`、`'2024-01-02 03:04:05+09'` = `03:04:05`、`'03:04:05+09'` = `03:04:05`、`'03:04:05 PST'` = `03:04:05`（日付とゾーンは**黙って捨てる**。`timestamp` の入力と同じ）。ただし**日付だけ**（`'2024-01-02'`）は 22007。`Asia/Tokyo` のようなゾーン**名**を後ろに付けると 22007（略称は通る）。
  - 小数秒は 7 桁以上を四捨五入（`04:05:06.1234567` → `.123457`、`04:05:59.9999999(3)` → `04:06:00`、`23:59:59.9999995` → `24:00:00`）。
  - エラー（`ereport` の `DateTimeParseError` と同じ）: `'24:00:01'`・`'25:00'`・`'04:60'`・`'04:05:61'`・`'24:00:00.5'`・`'13:00 PM'`・`'2024-13-01 03:04'` は **22008 `date/time field value out of range: "<入力>"`**。`'garbage'`・`'infinity'`・`'today'`・`'epoch'`・`'-04:05'`・`'12:00 PM X'` は **22007 `invalid input syntax for type time: "<入力>"`**。
- **出力 `Time::format`**: `HH:MM:SS[.ffffff]`。小数部は末尾のゼロを落とす（`15:04:05.25`、`00:00:00`、`24:00:00`）。`DateStyle` によらない【確認】。
- **丸め `with_typmod`**: p 桁に四捨五入。`'23:59:59.9999995'::time(6)` は `24:00:00`、`time(0)` も `24:00:00`【確認】。p が 0〜6 の外は typmodin が扱う（§6.1.3）。
- **演算**: `add_interval` は `time + span.micros` を `USECS_PER_DAY` で巻き戻す（`result -= result / USECS_PER_DAY * USECS_PER_DAY; if result < 0 { result += USECS_PER_DAY }`）。**`months` と `days` は無視する**（`'01:00'::time + interval '1 month 2 days 3 hours'` = `04:00:00`）。`'23:00'::time + '2 hours'` = `01:00:00`、`'01:00'::time - '2 hours 30 min'` = `22:30:00`、`time '24:00:00' + interval '1 sec'` = `00:00:01`【確認】。`sub_time` は巻き戻さない（`'01:00' - '23:00'` = `-22:00:00`、`'24:00:00' - '00:00:00'` = `24:00:00`）。
- **キャスト**: `time → interval`（暗黙。`'04:05:06'::time::interval` = `04:05:06`）、`interval → time`（代入。`'1 day 2 hours'` → `02:00:00`、`'-1 hour'` → `23:00:00`、`'25:30:00'` → `01:30:00`）、`timestamp → time` / `timestamptz → time`（代入。`timestamptz` はセッションのゾーンの壁時計。**無限は NULL**）。`time → time(p)`（型修飾子の適用。暗黙）。
- **`extract` / `date_part`**【確認】: 対応する単位は `hour`・`minute`・`second`・`milliseconds`・`microseconds`・`epoch`（`extract(epoch from time '04:05:06.789')` = `14706.789000`、`extract(second ...)` = `6.789000`、`extract(microseconds ...)` = `6789000`、`extract(milliseconds ...)` = `6789.000`）。`day`・`timezone` などは **0A000 `unit "day" not supported for type time without time zone`**、未知の単位は 22023 `unit "foo" not recognized for type time without time zone`。結果は `extract` が `numeric`、`date_part` が `float8`。
- **`date_trunc('hour', time '04:05')`** は `time` 版が無い。**`time → interval` の暗黙キャストで `date_trunc(text, interval)` に解決され、`interval`（`04:00:00`）を返す**【確認】。yuzhu も解決の規則どおりに同じになる（特別な処理は要らない）。
- **`date + time`**（`datetime_pl`。1272 / 1296 の `date + time`、`time + date`）: `Date::add_time`。`date '2024-01-02' + time '03:04'` = `2024-01-02 03:04:00`。`timestamp(date, time)`（2025）、`timestamptz(date, time)`（1176）も同じ。

#### 6.1.2 `age`（TD-3）

`timestamp_age` / `timestamptz_age`（`PG:src/backend/utils/adt/timestamp.c`）の移植。有限どうしの手順:

```
tm1 = dt1 の壁時計（timestamp は UTC 換算なし。timestamptz はセッションのゾーン）、tm2 = dt2 の壁時計
(year, mon, mday, hour, min, sec, fsec) = tm1 - tm2（項目ごとの差）
if dt1 < dt2: すべての項目の符号を反転する
借り上げ: fsec < 0 → sec−−、sec < 0 → min−−、min < 0 → hour−−、hour < 0 → mday−−、
          mday < 0 → （dt1 < dt2 なら tm1 の、そうでなければ tm2 の）月の日数を足して mon−−、mon < 0 → mon += 12; year−−
if dt1 < dt2: すべての項目の符号を戻す
結果 = Interval { months: year * 12 + mon, days: mday, micros: hour・min・sec・fsec の合計 }
```

- 無限: `timestamp - timestamp` と同じ規則（`age(timestamp 'infinity', t)` は `infinity`。同じ符号の無限どうしは 22008 `interval out of range`）【確認: infinity の例】。
- **1 引数の `age(ts)`** は SQL 関数で、`age(current_date::timestamp, ts)`（`timestamptz` 版は `current_date::timestamptz`）【記憶: 1386・2059 は `language sql`】。yuzhu は Rust で同じ合成をする（`FnKind::Runtime`。今日の日付はセッションのゾーンで取る）。
- 例【確認】: `age('2024-03-15 10:00', '2000-02-29 12:30')` = `24 years 14 days 21:30:00`、`age('2024-01-31', '2024-03-01')` = `-1 mons -1 days`、`age('2024-03-10 12:00+00', '2024-03-09 12:00+00')`（UTC）= `1 day`。America/New_York の DST をまたぐ `age(timestamptz '2024-03-10 12:00-04', timestamptz '2024-03-09 12:00-05')` は `1 day`（壁時計の差。`-` は `23:00:00`）。

#### 6.1.3 型修飾子（`typmod.rs`。TD-1）

`pg_type` の `typmodin` / `typmodout`（`timetypmodin`、`timestamptypmodin`、`timestamptztypmodin`、`intervaltypmodin` と各 `out`）の実体。

- `precision_typmod_in(type_name, p)`: `p < 0` → 22023 `TIME(-1) precision must not be negative`（`type_name` を大文字にして埋める）。`p > 6` → **WARNING**（SQLSTATE 22023）`TIME(7) precision reduced to maximum allowed, 6` を返して `p = 6`【確認】。typmod = p。
- `interval_typmod_in(mods)`（PostgreSQL の `intervaltypmodin` と同じ）:

| `mods` | 結果 |
|---|---|
| `[]` | -1 |
| `[range]`（`range == 0x7FFF` なら -1） | `interval_typmod(None, range)`（`intervaltypmodin('{4}')` = 327679【確認】） |
| `[range, p]` | `p < 0` → 22023 `INTERVAL(-1) precision must not be negative`。`p > 6` → WARNING `INTERVAL(7) precision reduced to maximum allowed, 6` で 6。`range == 0x7FFF` なら `interval_typmod(Some(p), FULL)`（`'{32767,3}'` = 2147418115【確認】）。そうでなければ `interval_typmod(Some(p), range)` |

  `range` の値が `FULL`・単一のフィールド・`YEAR \| MONTH`・`DAY \| HOUR`・`DAY \| HOUR \| MINUTE`・`DAY \| HOUR \| MINUTE \| SECOND`・`HOUR \| MINUTE`・`HOUR \| MINUTE \| SECOND`・`MINUTE \| SECOND` のどれかでなければ 22023 `invalid INTERVAL type modifier`。
- `interval_typmod_out(typmod)`: `typmod < 0` → `""`。範囲がフルならば精度があるときだけ `"(p)"`。範囲がフルでなければ `" year"`・`" month"`・`" day"`・`" hour"`・`" minute"`・`" second"`・`" year to month"`・`" day to hour"`・`" day to minute"`・`" day to second"`・`" hour to minute"`・`" hour to second"`・`" minute to second"` のあとに、精度があれば `"(p)"`（`" second(3)"`、`" day to second(2)"`）。`format_type(1186, typmod)` は `interval` + この文字列（`interval(3)`、`interval second(3)`、`interval year to month`）。`time` は `time(3) without time zone`（精度があるとき。`precision_typmod_out`）。

#### 6.1.4 `timezone(interval, ts)`（TD-2）

`Timestamp::at_time_zone_interval(zone)` / `TimestampTz::at_time_zone_interval(zone)`（`timestamp_izone` / `timestamptz_izone`）。`zone.months != 0 || zone.days != 0` は **22023 `interval time zone "1 day" must not include months or days`**（引用符の中は `zone` を `IntervalStyle` に関係なく postgres 形式で出した文字列）【確認】。無限は 22008。それ以外は `micros` を秒に切り捨てた符号付きオフセットとして `AT TIME ZONE` と同じに扱う。`timestamp AT TIME ZONE interval '3 hours'` = `2024-01-01 15:00:00+00`（UTC 表示。`'12:00'` を UTC+3 とみなす）。

#### 6.1.5 `make_*` と `to_timestamp(float8)`（TD-3）

PostgreSQL の `make_date`・`make_time`・`make_timestamp`・`make_timestamptz`・`make_interval`・`float8_timestamptz` の移植。結果と例は【確認】。

| 関数 | 規則 | エラー（22008 など。文言は実機どおり） |
|---|---|---|
| `make_date(y, m, d)` | `y < 0` は紀元前（`-44` → `0044-03-15 BC`）。`y == 0` は不正 | `date field value out of range: 2024-02-30`、`date field value out of range: 0-01-01` |
| `make_time(h, mi, s)` | `h` 0〜24、`mi` 0〜59、`s` 0〜60（`s` は float8。`NaN` は不正）。`24:00:00` は可、`24:00:01` は不可 | `time field value out of range: 25:00:00`、`time field value out of range: 1:02:-1`（書式は `%d:%02d:%02g`） |
| `make_timestamp(y, m, d, h, mi, s)` | 日付と時刻をそれぞれ検査して結合。`s = 60` は繰り上げ（`2024-01-01 00:01:00`）、`24:00:00` は翌日 | `date field value out of range: 2024-13-01`、結果が範囲外は `timestamp out of range: 2024-01-01 0:00:00` 系（**書式は実装時に実機で確かめる**。§9） |
| `make_timestamptz(..., [zone])` | zone なし = セッションのゾーン。zone あり = §6.4.4 の解釈 | `time zone "Foo/Bar" not recognized`（22023）、`time zone "" not recognized` |
| `make_interval(years => 0, months => 0, weeks => 0, days => 0, hours => 0, mins => 0, secs => 0)` | 引数は名前つき（既定 0）。`years * 12 + months`、`weeks * 7 + days`、`hours`・`mins`・`secs` を µs に。`make_interval(1, 2)` = `1 year 2 mons`、`make_interval()` = `00:00:00`。`secs` は float8（`NaN`・無限は 22008）。オーバーフローは 22008 `interval out of range` | `interval out of range` |
| `to_timestamp(float8)` | UNIX エポックからの秒（小数可）。`to_timestamp(1700000000.123456)` = `2023-11-14 22:13:20.123456+00`。`'infinity'::float8` は `infinity` | `timestamp cannot be NaN`（22008）、`timestamp out of range: "1e+20"`（22008） |

- **名前つき引数**（`make_interval(years => 1)`）は M4 のアナライザが関数呼び出しの `name => value` を対応していなければ、`make_interval` は位置引数だけ（`make_interval(1, 2)`）にして、名前つきは `0A000` にする（M4 の章 09 で確かめる。§9）。`pg_proc` の `proargnames` は PostgreSQL と同じに入れる。

### 6.2 interval の統合（TD-1）

1. **`Datum::Interval`**: F0 が変種を足す（00 §4.9）。`cmp_datum` は `a.cmp(b)`（クレートの `Ord`）、`hash_datum` はクレートの `Hash`（換算値）。`Datum` の等値（`PartialEq`）は M1 から `derive`（フィールドごとの比較）なので、**値の等しさには必ず `cmp_datum` を使う**（M4 §12.2 の規約）。
2. **入出力**: `Interval::parse(s, typmod, env)` / `iv.format(env.interval_style)`。`typmod` は列の型の値（`ty.typmod`）。`interval_style = sql_standard` は**入力の解釈にも効く**（クレートが対応済み）。入力は postgres 形式（`1 day 2 hours`、`1 year 2 mons 3 days 04:05:06.7`、`-1 day`、`@ 1 minute ago`）、ISO 8601 形式（`P1Y2M3DT4H5M6S`）、`sql_standard` の `1-2`・`1 2:03:04` に対応する。小数の単位の繰り下げ（`1.5 years` → `1 year 6 mons`、`0.5 months` → `15 days`）はクレートが `DecodeInterval` の移植で行う【確認】。**出力の実機例（4 形式。同じ値）**【確認】:

| 値 | `postgres` | `postgres_verbose` | `sql_standard` | `iso_8601` |
|---|---|---|---|---|
| `1 year 2 mons 3 days 04:05:06.789` | `1 year 2 mons 3 days 04:05:06.789` | `@ 1 year 2 mons 3 days 4 hours 5 mins 6.789 secs` | `+1-2 +3 +4:05:06.789` | `P1Y2M3DT4H5M6.789S` |
| `-1 year -2 mons +3 days -04:05:06` | `-1 years -2 mons +3 days -04:05:06` | `@ 1 year 2 mons -3 days 4 hours 5 mins 6 secs ago` | `-1-2 +3 -4:05:06` | `P-1Y-2M3DT-4H-5M-6S` |
| `0` | `00:00:00` | `@ 0` | `0` | `PT0S` |
| `1 day -01:00:00` | `1 day -01:00:00` | `@ 1 day -1 hours` | `+0-0 +1 -1:00:00` | `P1DT-1H` |
| `2 days` | `2 days` | `@ 2 days` | `2 0:00:00` | `P2D` |
| `100000 hours` | `100000:00:00` | `@ 100000 hours` | `100000:00:00` | `PT100000H` |
| `-0.5 sec` | `-00:00:00.5` | `@ 0.5 secs ago` | `-0:00:00.5` | `PT-0.5S` |
| `1 mon -1 day` | `1 mon -1 days` | `@ 1 mon -1 days` | `+0-1 -1 +0:00:00` | `P1M-1D` |
| `-1 year 1 mon` | `-11 mons` | `@ 11 mons ago` | `-0-11` | `P-11M` |

   postgres 形式の規則: 年・月・日は `N year(s)`・`N mon(s)`・`N day(s)`（`1` と等しいときだけ単数）、時刻部分は `HH:MM:SS[.ffffff]`（時は 2 桁以上）、すべてゼロなら `00:00:00`、**符号の異なるフィールドが混ざるとき、正のフィールドに `+` を付ける**。これらはクレートの `encode_interval` が行う。
3. **比較・ハッシュ・B+Tree**: §3.2。`interval_ops` の行は §6.8。
4. **演算子とキャスト**: §6.5 の表。`interval * float8`・`interval / float8`（`mul` / `div`）は「月 → 日 → µs」と**端数を下位のフィールドに繰り下げる**（`'1 month' * 1.5` = `1 mon 15 days`、`'1 day' / 2` = `12:00:00`）。オーバーフローと `NaN` は 22008 `interval out of range`、`/ 0` は 22012 `division by zero`【確認】。
5. **型修飾子**: `Interval::with_typmod(typmod)` が丸めとフィールドの切り捨てを行う（`interval '1 year 2 months'::interval year` = `1 year`、`interval '1 day 04:05:06.789' day to second(1)` = `1 day 04:05:06.8`、`'1:2:3'::interval second` = `01:02:03`。`interval '1' day` = `1 day`、`interval '1-2' year to month` = `1 year 2 mons`【確認】）。`interval → interval` の暗黙キャスト（pg_cast 1200）が `apply_typmod` を呼ぶ。
6. **集約**: `sum(interval)`（2113）= `interval_pl` の累積。`avg(interval)`（2106）= 累積した和と件数から `interval_avg`（和を件数で割る。`interval_div(sum, count::float8)` と同じ）。`min` / `max`（2144 / 2128）= `interval_smaller` / `interval_larger`（`cmp` で比べる）。空入力は NULL。例【確認】: `sum(('1 day'),('25 hours'),('1 mon'))` = `1 mon 1 day 25:00:00`、`avg` = `10 days 16:20:00`、`min` = `1 day`、`max` = `1 mon`。M4 の `BuiltinAggregate` の形（状態の型・遷移・最終）に合わせて行を足す（M4 の章 09 で確かめる。§9）。
7. **`isfinite(interval)`**（1390）、`justify_days`（1295）・`justify_hours`（1175）・`justify_interval`（2711）は `Interval` のメソッドをそのまま呼ぶ（例【確認】: `justify_days('35 days')` = `1 mon 5 days`、`justify_hours('50 hours')` = `2 days 02:00:00`、`justify_interval('1 mon -1 hour')` = `29 days 23:00:00`）。

### 6.3 time の統合（TD-4）

§6.1.1 の `Time` を `yuzhu-core` に載せる。

- `Datum::Time(i64)`、ディスクは §3.1。入出力は `types::datetime::{input_text, output_text}` が `Time::parse(s, ty.typmod, &env.datetime)` / `Time::format` を呼ぶ。`ty.typmod` は `time(p)` の p。
- `time_in` は DateStyle の日付順（MDY など）を使う（日付つきの入力のため）。出力は `DateStyle` によらない。
- 比較・ハッシュは整数。`time_ops` の行は §6.8。
- 演算子・キャスト・関数は §6.5 の表。`localtime` = `now()::time`（stable）。
- `min(time)`（2139）・`max(time)`（2123）。`sum` / `avg` は無い（PostgreSQL も無い）。
- 型名 `time` / `time(p)` / `time without time zone` はパーサ（M1 から対応済み）が `time` に直す。`timetz` / `time with time zone` はパーサが `timetz` に直し、**アナライザの型の解決が `0A000`** にする（M4 の `KNOWN_UNSUPPORTED_TYPES` の `time`・`interval` を外し、`timetz` は残す）。

### 6.4 TimeZone・DateStyle・IntervalStyle（TD-2）

#### 6.4.1 `ZoneDb` と tzdata

- `Cluster` が `Arc<ZoneDb>` を 1 つ持つ。`ZoneDb::new(Box::new(FsZoneSource::new(options.timezone_dir)))`。`timezone_dir` の既定は `/usr/share/zoneinfo`（M4 契約 §4 のとおり。CI とコンテナ `sandbox/` に tzdata を入れる）。
- **tzdata が無い環境**: `ZoneDb` は `UTC`・`GMT`・`Etc/UTC` と POSIX 文字列・固定オフセットだけで動く（クレートの `works_without_tzdata` 試験がある）。`Cluster::open` は `timezone_dir` が読めなければ WARNING を 1 度ログに出す（`could not read time zone directory "<path>"; only UTC and fixed offsets are available`）。地域名の `SET` は 22023 になる。
- **I/O の抽象化**: tzdata の読み取りは `ZoneSource` トレイトを通る（`FsZoneSource` が実ファイル、`MemZoneSource` がメモリ。クレートに障害注入の試験がある）。M3 の `Vfs`（データディレクトリ用）には載せない（データディレクトリの外のファイル。CLAUDE.md の「抽象化レイヤ」は `ZoneSource` が満たす）。
- 名前は大文字小文字を区別しない。`..` や絶対パスは拒否する（クレートが対応済み）。読んだゾーンは `ZoneDb` のキャッシュに載る（成功だけ）。

#### 6.4.2 DateStyle

- 出力形式（`ISO`・`SQL`・`Postgres`・`German`）と日付順（`MDY`・`DMY`・`YMD`）の組。`SET DateStyle = 'ISO, DMY'`、`SET DateStyle TO Postgres, MDY`、`SET DateStyle = 'German'`（日付順が `DMY` になる）。指定しなかった側は現在値を保つ。`DEFAULT` は `ISO, MDY`。競合（`'iso, sql'`）は 22023 `invalid value for parameter "DateStyle": "iso, sql"` + DETAIL `Conflicting "datestyle" specifications.`、未知の語は DETAIL `Unrecognized key word: "foo".`【確認】。`parse_datestyle` / `format_datestyle` がこの規則を持つ。
- 出力の実機例（`Asia/Tokyo`、`'2024-03-09 15:04:05.25+09'::timestamptz`）【確認】:

| DateStyle | date | timestamp | timestamptz |
|---|---|---|---|
| `ISO, MDY`（`DMY`・`YMD` も同じ） | `2024-03-09` | `2024-03-09 15:04:05.25` | `2024-03-09 15:04:05.25+09` |
| `SQL, MDY`（`YMD` も同じ） | `03/09/2024` | `03/09/2024 15:04:05.25` | `03/09/2024 15:04:05.25 JST` |
| `SQL, DMY` | `09/03/2024` | `09/03/2024 15:04:05.25` | `09/03/2024 15:04:05.25 JST` |
| `Postgres, MDY`（`YMD` も同じ） | `03-09-2024` | `Sat Mar 09 15:04:05.25 2024` | `Sat Mar 09 15:04:05.25 2024 JST` |
| `Postgres, DMY` | `09-03-2024` | `Sat 09 Mar 15:04:05.25 2024` | `Sat 09 Mar 15:04:05.25 2024 JST` |
| `German`（`German, DMY`） | `09.03.2024` | `09.03.2024 15:04:05.25` | `09.03.2024 15:04:05.25 JST` |

  紀元前と地方平均時の例: `'0044-03-15 07:08:09+09 BC'::timestamptz` は ISO で `0044-03-15 07:27:08+09:18:59 BC`、SQL / Postgres / German で `... 07:27:08 LMT BC`（tzdata が要る）。`time` は DateStyle によらず `15:04:05.25`。
- 入力の日付順: `'01/02/2024'` は MDY で 1 月 2 日、DMY で 2 月 1 日、`'2024-01-02'` は ISO 形式として常に年月日（クレートが `DecodeDateTime` の移植で処理）。

#### 6.4.3 IntervalStyle

4 形式（`postgres`・`postgres_verbose`・`sql_standard`・`iso_8601`）。出力は §6.2 の表。`sql_standard` は入力にも効く。`SET LOCAL` / RESET / ROLLBACK は §5.1。Rails が接続時に `SET intervalstyle = iso_8601` を送るので、この形式が実際に使われる。

#### 6.4.4 `AT TIME ZONE`・`timezone()`・数値オフセットの符号

- `timestamp AT TIME ZONE zone` → `timestamptz`（`zone` の壁時計として解釈）、`timestamptz AT TIME ZONE zone` → `timestamp`（`zone` の壁時計）。`zone` は `text`（IANA 名、略称、POSIX 文字列、数値オフセット）または `interval`。**`AT TIME ZONE` の数値オフセットは POSIX の符号**（`AT TIME ZONE '-03'` は UTC+3。`timestamp '2024-01-01 12:00' at time zone '-03'` = `09:00:00+00`）【確認】。クレートの `at_time_zone` が `decode_timezone_name` で処理する。未知の名前は 22023 `time zone "Foo/Bar" not recognized`、略称（`PST`、`JST`）は通る。
- **`make_timestamptz` の数値オフセットは ISO の符号**（`make_timestamptz(2024,1,1,12,0,0,'-03')` = `15:00:00+00`、`'+09:30'` = `02:30:00+00`）。名前・略称・`'UTC+3'`（POSIX 文字列。= UTC−3 で `15:00:00+00`）は `AT TIME ZONE` と同じ【確認】。実装: `make_timestamptz` は zone をまず数値オフセット（`decode_timezone` の ISO の符号）として解釈し、だめなら名前・略称・POSIX として解釈する。
- `AT LOCAL`（PostgreSQL 17。`timezone(timestamptz)` 6334 / `timezone(timestamp)` 6335）= セッションのゾーンに変換（`timestamptz AT LOCAL` → `timestamp`、`timestamp AT LOCAL` → `timestamptz`）【確認】。
- `timezone(text, timestamp)`（2069）・`timezone(text, timestamptz)`（1159）・`timezone(interval, timestamp)`（2070）・`timezone(interval, timestamptz)`（1026）・`timezone(timestamptz)`（6334）・`timezone(timestamp)`（6335）。種別は 6.5 の表。
- DST の境界【確認: America/New_York、2024】: 存在しない壁時計（`'2024-03-10 02:30'`）は遷移の前のオフセットで解釈して `03:30:00-04`、重複する壁時計（`'2024-11-03 01:30'`）は遷移の後（標準時）で `01:30:00-05`。`timestamptz + interval` は month と day をセッションのゾーンの壁時計で加え、time 部分は絶対時間で加える（`'2024-03-09 12:00' + '1 day'` = `2024-03-10 12:00-04`、`+ '24 hours'` = `13:00-04`）。クレートの `TimestampTz::add_interval(span, tz)` が行う。

### 6.5 関数・演算子・キャスト（TD-1、TD-3、TD-4）

OID と性質は PostgreSQL 17.11 の `pg_proc` / `pg_operator` / `pg_cast`（【確認】。実装時に `.dat` で再確認）。「M4」は M4 が済ませる行、無印が M5（TD）が足す行。**種別**: P = `FnKind::Pure`、R = `FnKind::Runtime`（セッションの `TimeZone` または今の時刻が要る）。安定性は `provolatile`（i / s / v）。既存の `builtin.rs` に interval 系の `pg_proc` の行（1160〜1170 など）が M1 から入っているので、**TD-1 の最初に既存の行と下の表を突き合わせて足りない行だけを足す**。

#### 6.5.1 演算子

| OID | 演算子 | 左 | 右 | 結果 | 関数（OID） | 種別 | 備考 |
|---|---|---|---|---|---|---|---|
| 1336 | `-`（前置） | | interval | interval | interval_um（1168） | P | |
| 1337 | `+` | interval | interval | interval | interval_pl（1169） | P | 交換 |
| 1338 | `-` | interval | interval | interval | interval_mi（1170） | P | |
| 1583 | `*` | interval | float8 | interval | interval_mul（1618） | P | |
| 1584 | `*` | float8 | interval | interval | mul_d_interval（1624） | P | |
| 1585 | `/` | interval | float8 | interval | interval_div（1326） | P | |
| 1330 / 1331 / 1332 / 1333 / 1334 / 1335 | `=` `<>` `<` `<=` `>` `>=` | interval | interval | bool | interval_eq / ne / lt / le / gt / ge（1162〜1167） | P | 換算値で比較 |
| 1108 / 1109 / 1110 / 1111 / 1112 / 1113 | `=` `<>` `<` `<=` `>` `>=` | time | time | bool | time_eq / ne / lt / le / gt / ge（1145、1106、1102、1103、1104、1105） | P | |
| 1399 | `-` | time | time | interval | time_mi_time（1690） | P | 巻き戻さない |
| 1800 | `+` | time | interval | time | time_pl_interval（1747） | P | |
| 1801 | `-` | time | interval | time | time_mi_interval（1748） | P | |
| 1849 | `+` | interval | time | time | interval_pl_time（1848） | P | |
| 1360 | `+` | date | time | timestamp | datetime_pl（1272） | P | |
| 1363 | `+` | time | date | timestamp | timedate_pl（1296） | P | |
| 1076 | `+` | date | interval | timestamp | date_pl_interval（2071） | P | |
| 1077 | `-` | date | interval | timestamp | date_mi_interval（2072） | P | |
| 2551 | `+` | interval | date | timestamp | interval_pl_date（2546） | P | |
| 2066 | `+` | timestamp | interval | timestamp | timestamp_pl_interval（2032） | P | |
| 2068 | `-` | timestamp | interval | timestamp | timestamp_mi_interval（2033） | P | |
| 2553 | `+` | interval | timestamp | timestamp | interval_pl_timestamp（2548） | P | |
| 2067 | `-` | timestamp | timestamp | interval | timestamp_mi（2031） | P | 日を月に繰り上げない（`365 days`） |
| 1327 | `+` | timestamptz | interval | timestamptz | timestamptz_pl_interval（1189） | **R** | ゾーンの壁時計で月・日を足す |
| 1329 | `-` | timestamptz | interval | timestamptz | timestamptz_mi_interval（1190） | R | |
| 2554 | `+` | interval | timestamptz | timestamptz | interval_pl_timestamptz（2549） | R | |
| 1328 | `-` | timestamptz | timestamptz | interval | timestamptz_mi（1188） | P | |

- `timetz` の演算子（1802・1803・2552 ほか）は作らない。`timestamp - timestamp` と `timestamptz - timestamptz`、`timestamptz ± interval` は M4 が interval なしでは作れないので、**この章（TD-1）が足す**。それ以外の日時どうしの演算子と比較（`date`・`timestamp`・`timestamptz` の混合を含む）は M4。
- 暗黙の型変換で解決される組み合わせ（`date + interval`、`time + date`、`'04:05'::time + '1 day'`（`unknown` → `interval`）、`-'01:00'::time`（`time → interval` で `interval_um`。結果 `-01:00:00`））は、M4 の演算子解決（`pg_cast` の暗黙キャスト）に任せる。

#### 6.5.2 キャスト

| 元 → 先 | 文脈 | 関数（OID） | 種別 | 備考 |
|---|---|---|---|---|
| time → time | i | `time(time, int4)`（1968） | P | 型修飾子の適用 |
| time → interval | i | `interval(time)`（1370） | P | |
| interval → time | a | `time(interval)`（1419） | P | |
| interval → interval | i | `interval(interval, int4)`（1200） | P | 型修飾子の適用 |
| timestamp → time | a | `time(timestamp)`（1316） | P | 無限は NULL |
| timestamptz → time | a | `time(timestamptz)`（2019） | R | |
| time → timetz | — | 作らない | | `0A000` |
| text ↔ time / interval | — | 入出力変換（I/O キャスト。TY-1 の枠組み） | P | `x::text` は出力、`'..'::interval` は入力 |

`timestamp(p)`・`timestamptz(p)` への型修飾子のキャスト（1961・1967）と、`date` / `timestamp` / `timestamptz` どうしのキャストは M4。

#### 6.5.3 関数

| OID | 関数 | 結果 | 種別 | 安定性 | 備考 |
|---|---|---|---|---|---|
| 1299 | `now()` | timestamptz | R | s | M4。`txn.started_at` |
| 2647 / 2648 / 2649 | `transaction_timestamp()` / `statement_timestamp()` / `clock_timestamp()` | timestamptz | R | s / s / v | M4（§5.2 の `Clock` に差し替える。TD-3） |
| 1171 | `date_part(text, timestamptz)` | float8 | R | s | M4 が作った日時の `date_part` / `extract` は、クレートの `date_part_*` / `extract_*` を呼ぶ |
| 1172 / 1385 / 1384 / 2021 | `date_part(text, interval / time / date / timestamp)` | float8 | P | i | interval と time は TD |
| 6199〜6204 | `extract(text, date / time / timetz / timestamp / timestamptz / interval)` | numeric | P（timestamptz は R） | i（timestamptz は s） | **6201（timetz）は作らない**。結果の `NumericValue` は `Numeric::from_str(&nv.to_string())` で `Datum::Numeric` にする（`NumericValue::Finite` の `Display` が scale どおりの文字列。`extract(epoch from timestamptz '2000-01-01 00:00+00')` = `946684800.000000`）。`Infinity` / `-Infinity` は numeric の特殊値 |
| 2020 / 1217 / 1284 / 1218 | `date_trunc(text, timestamp / timestamptz / timestamptz, text / interval)` | 同左 | P / R / R / P | i / s / i / i | 1284 はゾーンつき（`date_trunc('month', ts, 'Asia/Tokyo')`）。`date_trunc(text, time)` は無い（§6.1.1） |
| 6177 / 6178 | `date_bin(interval, timestamp / timestamptz, timestamp / timestamptz)` | 同左 | P | i | 任意（TD-3 の最後。クレートに足す）。`date_bin('15 minutes', '2024-03-09 15:44:05', '2000-01-01')` = `2024-03-09 15:30:00`【確認】 |
| 6221 / 6222 / 6223 / 6273 | `date_add` / `date_subtract(timestamptz, interval[, text])` | timestamptz | R | s / i | 任意。`+` / `-` と同じ（ゾーンつきは zone で計算） |
| 1199 / 1386 / 2058 / 2059 | `age(timestamptz, timestamptz)` / `age(timestamptz)` / `age(timestamp, timestamp)` / `age(timestamp)` | interval | R / R / P / R | i / s / i / s | §6.1.2。1 引数版は `current_date` が要る |
| 1295 / 1175 / 2711 | `justify_days` / `justify_hours` / `justify_interval` | interval | P | i | |
| 1390 / 1373 / 1389 / 2048 | `isfinite(interval / date / timestamptz / timestamp)` | bool | P | i | 1373・1389・2048 は M4 |
| 3846 / 3847 / 3461 / 3462 / 3463 | `make_date` / `make_time` / `make_timestamp` / `make_timestamptz`（6 引数・7 引数）/ `make_interval` は 3464 | date / time / timestamp / timestamptz / interval | P（3462・3463 は R） | i（3462・3463 は s） | §6.1.5 |
| 1158 | `to_timestamp(float8)` | timestamptz | P | i | |
| 1159 / 2069 / 1026 / 2070 / 6334 / 6335 | `timezone(...)` | §6.4.4 | P / P / P / P / R / R | i / i / i / i / s / s | `timezone(text, timestamptz)`（1159）の `text` ゾーンだけで計算でき、セッションのゾーンは要らない |
| 1316 / 1419 / 2019 / 2024〜2029 / 1174 / 1176 / 1178 | `time(...)`・`timestamp(date, time)`（2025）・`timestamptz(date, time)`（1176）など | | | | キャストの実体。§6.5.2。`date(timestamp)` などは M4 |
| 3935 | `pg_sleep_for(interval)` | void | R | v | 任意。`pg_sleep(secs)`（M3）と同じ（秒 = `micros` + 日・月を 24 時間・30 日で換算。PostgreSQL は `interval` を秒に換算する） |
| 1768 / 1770 / 2049 / 1780 / 1778 | `to_char(interval / timestamptz / timestamp, text)`、`to_date(text, text)`、`to_timestamp(text, text)` | | | | **実行すると 0A000**（TD-D11）。`to_char(numeric, text)` などは 05 章 |

- 定数畳み込みで `FnKind::Runtime` を畳み込まない（M4 の `const_fold` の規則。`now()` 系と TimeZone 依存はプランニング時に畳み込まない。**`timestamptz` ⇄ `timestamp` の型変換のようにセッションのゾーンに依存する式を、プランキャッシュ（M5 の準備済み文。00 D19 は Bind ごとに再解析）をまたいで使い回さない**）。
- `extract` の `numeric` の結果は、`extract(hour from interval '100:00:00')` = `100`、`extract(epoch from interval '1 year 2 mons 3 days 04:05:06.5')` = `37015506.500000`、`extract(month from interval '14 months')` = `2`、`extract(year from interval '25 months')` = `2` など（`interval` の年・月・日の換算はクレートの `extract_interval`）【確認】。

#### 6.5.4 集約

| OID | 集約 | 引数 → 結果 | 状態 | 備考 |
|---|---|---|---|---|
| 2113 | `sum(interval)` | interval → interval | `Option<Interval>` | `interval_pl`（オーバーフローは 22008） |
| 2106 | `avg(interval)` | interval → interval | `(Interval, i64)` | 最終: `Interval::div(sum, count as f64)` |
| 2144 / 2128 | `min` / `max(interval)` | interval → interval | `Option<Interval>` | `cmp` |
| 2139 / 2123 | `min` / `max(time)` | time → time | `Option<i64>` | 整数 |

#### 6.5.5 パーサと「現在」の構文

`current_date`・`current_timestamp[(p)]`・`localtimestamp[(p)]`・`localtime[(p)]` を M4 が `FuncCall` に直す（M4 契約 §3。現在のパーサは 0A000 を返す）。TD は `localtime` を足し、`current_time` を 0A000 のまま残す（TD-D5）。型付きリテラル `time '04:05'`・`interval '1 day'`・`timestamp '...'` はパーサが対応済み（`at_typed_literal_keyword`）。

#### 6.5.6 generate_series（TD-D13）

`generate_series(timestamp, timestamp, interval)`（938）と `generate_series(timestamptz, timestamptz, interval)`（939）。M4 の `FunctionScan { func: &'static BuiltinFunction, args }` の登録方式に行を足す。規則【確認】: 刻み 0 は 22023 `step size cannot equal zero`、無限の刻みは 22023 `step size cannot be infinite`、**`cur = cur + step` を繰り返す**（`start + n * step` ではない。`2024-01-31` から `1 month` ずつだと `01-31`、`02-29`、`03-29`、`04-29`）、刻みが正なら `cur <= stop` の間、負なら `cur >= stop` の間、符号が合わなければ 0 行、開始が無限なら 0 行、終了が無限で刻みが正なら `timestamp out of range`（22008）で止まる。`timestamptz` の加算はセッションのゾーン（R）。

### 6.6 パーサ（`sql/parser/datetime.rs`。TD-1、TD-2）

`sql/parser/{expr,misc}.rs`（M1 の持ち物）の関連部分は、**`datetime.rs` に作る関数を 1 行で呼ぶ形に置き換える**（§11 で依頼）。

#### 6.6.1 INTERVAL の型修飾子と型付きリテラル

`gram.y` の `ConstInterval` と `opt_interval` に従う。`TypeName.modifiers` は `[Integer(range), Integer(p)?]`（`interval_typmod_in` の `mods`）に直す。

| 書き方 | `modifiers` |
|---|---|
| `interval` | `[]` |
| `interval(3)` | `[32767, 3]`（**M1 のパーサは `[3]` を作る。`[FULL, 3]` に直す**） |
| `interval year` / `month` / `day` / `hour` / `minute` / `second` | `[mask]`（`4`・`2`・`8`・`1024`・`2048`・`4096`） |
| `interval second(3)` | `[4096, 3]`（精度は最後のフィールドが `SECOND` のときだけ書ける） |
| `interval year to month` | `[6]`。`day to hour`・`day to minute`・`day to second[(p)]`・`hour to minute`・`hour to second[(p)]`・`minute to second[(p)]` も同様 |

- 型付きリテラル `interval '1 day' hour`、`interval(3) '1 day'`、`interval '1' day to second` も同じ。`typed_literal` が `TypeName` と文字列を組にする（M1 のまま）。
- 不正な組み合わせ（`interval year to day`、`interval day to day`）は構文エラー（42601）。`interval second(3) to minute` も構文エラー。
- `KNOWN_UNSUPPORTED_TYPES`（`analyzer/ddl.rs`）から `time` と `interval` を外す（M4 の持ち物。§11）。

#### 6.6.2 AT TIME ZONE / AT LOCAL

`expr AT TIME ZONE expr` → `FuncCall timezone(zone, expr)`、`expr AT LOCAL` → `FuncCall timezone(expr)`。優先順位は PostgreSQL の `%left AT`（`^` より強く、`COLLATE`・単項 `-`・`::` より弱い）で、M1 の `P_AT` が用意してある（`parse_infix` の `"at"` の枝が `0A000` を返しているのを置き換える）。`at` の次が `time zone` または `local` のときだけ中置演算子として扱う（M1 のまま）。

#### 6.6.3 SET TIME ZONE

`parse_zone_value` の `INTERVAL` の枝（`0A000`）を置き換える。`SET TIME ZONE INTERVAL '+09:00' HOUR TO MINUTE` は、パーサが `yuzhu_datetime::Interval::parse(text, typmod, &utc_env)` で値にし、`SetArg::String("INTERVAL '<postgres 形式の出力>'")`（例 `INTERVAL '09:00:00'`）を作る。クレートの `parse_timezone_setting` がこの形（`interval '...'`）を受け付ける。解釈できない文字列は構文エラーにせず、そのまま `SetArg::String` にして SET 時の 22023 に任せる。`utc_env` は `TimeZone::utc()` と `ZoneDb::without_tzdata()` から作る（パーサは環境を持たない）。数値（`SET TIME ZONE 9`、`-5.5`）は M1 の `parse_numeric_only` のまま。

### 6.7 バイナリ形式（TD-5）

すべて**ネットワークバイトオーダー（ビッグエンディアン）**。`integer_datetimes = on`（ParameterStatus で送っている）を前提にする。`send` は PostgreSQL の `*_send` と同じバイト列、`recv` は `*_recv` と同じ範囲検査。typmod の適用は呼び出し側（`apply_typmod`）。バイト列の例は実機の `*_send` の出力【確認】。

| 型 | バイト列 | `recv` の検査 |
|---|---|---|
| `date`（4 バイト） | `i32` BE。2000-01-01 からの日数。`i32::MAX` = infinity、`i32::MIN` = -infinity | 無限は可。それ以外は `date_in` が受け付ける範囲（`-2451545 ..= 2147483493 - 2451545`。**4714-11-24 BC（ユリウス日 0）〜 5874897-12-31**。4713-01-01 BC は別の値で、日数は -2451507）の外なら **22008 `date out of range`**（【実機】PG17.11。`7ffffff0` で確認。`<日数>` は付かない。レビュー対応 R-32） |
| `time`（8 バイト） | `i64` BE。0 時からのマイクロ秒 | `0 ..= 86_400_000_000` の外は **22008 `time out of range`**（【実機】PG17.11。`7fffffffffffffff` で確認。05 §6.4 と同じ。22003 ではない。レビュー対応 R-27） |
| `timestamp`（8 バイト） | `i64` BE。2000-01-01 00:00:00 からのマイクロ秒。`i64::MAX` / `i64::MIN` = ±infinity | 無限は可。それ以外は `IS_VALID_TIMESTAMP`（4714-11-24 BC 〜 294276-12-31）の外なら **22008 `timestamp out of range`** |
| `timestamptz`（8 バイト） | `timestamp` と同じ。値は UTC | 同上 |
| `interval`（16 バイト） | `i64 time`（µs）→ `i32 day` → `i32 month`。無限は 3 つとも最大値 / 最小値 | 検査なし（無限の組はそのまま `infinity`。それ以外のフィールドの組み合わせも受ける。PostgreSQL と同じ【記憶】）。型修飾子は呼び出し側 |

**例**（すべて実機の `*_send` の 16 進。`types::datetime::send` の単体テストの固定値に使う）:

| 値 | バイト列（16 進） |
|---|---|
| `date '2024-01-02'` | `0000223f`（8767 日。`0x223f` = 8767。2000-01-01 からの日数。【実機】`date '2024-01-02' - date '2000-01-01'` = 8767。レビュー対応 R-32） |
| `date '1970-01-01'` | `ffffd533`（-10957 日） |
| `date '4713-01-01 BC'` | `ffda97cd`（-2451507 日。受理範囲の下限 `date '4714-11-24 BC'` は -2451545 日 = `ffda97a7`（【実機】`date_send`）） |
| `date 'infinity'` / `'-infinity'` | `7fffffff` / `80000000` |
| `time '04:05:06.789'` | `000000036c97ca88` |
| `time '24:00:00'` | `000000141dd76000` |
| `timestamp '2000-01-01'` | `0000000000000000` |
| `timestamp '1970-01-01'` | `fffca2fec4c82000` |
| `timestamp '2024-01-02 03:04:05.678901'` | `0002b0ec85204f35` |
| `timestamptz '2024-01-02 03:04:05.678901+00'` | `0002b0ec85204f35` |
| `timestamp 'infinity'` / `'-infinity'` | `7fffffffffffffff` / `8000000000000000` |
| `interval '1 year 2 mons 3 days 04:05:06.789'` | `000000036c97ca88 00000003 0000000e` |
| `interval '-1 days -01:00:00'` | `ffffffff296c5c00 ffffffff 00000000` |
| `interval 'infinity'` | `7fffffffffffffff 7fffffff 7fffffff` |

- `recv` の入力長が型の長さ（4 / 8 / 16）と違えば、`RecvBuf` / `input_binary` が決める（不足は 08P01、余りは 22P03。00 §4.9、04 §4.4。型の `binary_recv` は長さを検査しない）。`timestamptz` の受信値は UTC なので、セッションのゾーンは関係しない。
- パラメータの型が `unknown` で、クライアントがバイナリ形式を指定した場合の型の決まり方は 04 章。
- `pg_type` の `typsend` / `typreceive` / `typmodin` / `typmodout` 列と `pg_proc` の行（`date_send` 2468 など）は TY-1 が埋める（00 §4.9）。日時 5 型の OID は §6.5 と `pg_type` の上の行から取る（`time_recv` 2470 / `time_send` 2471、`interval_recv` 2478 / `interval_send` 2479。`date_*`・`timestamp*` は TY-1 が `.dat` から）。

### 6.8 opclass の行（TD-1、TD-4）

M4 の `catalog/opclass.rs`（`OPFAMILIES` / `OPCLASSES` / `AMOPS` / `AMPROCS`）に足す（M4 契約の「静的な表」。`datetime_ops` の行と同じ形）。OID は PostgreSQL の `pg_opfamily.dat` / `pg_opclass.dat` で確かめる。**`interval_ops` と `time_ops` の opclass の OID（実機では 10022、10038）は `.dat` に固定値が無く、起動ごとの採番（10000 以上）である**ため、yuzhu は独自の値を使ってよい（`pg_opfamily` の OID は実機で `interval_ops` = 1982、`time_ops` = 1996）。

| opfamily（実機 OID） | opclass | 入力型 | 既定 | amop（戦略 1 `<`、2 `<=`、3 `=`、4 `>=`、5 `>`） | amproc |
|---|---|---|---|---|---|
| `time_ops`（1996） | `time_ops` | time | yes | 1110・1111・1108・1113・1112 | 1 = `time_cmp`（1107）。`in_range` と `btequalimage` は作らない |
| `interval_ops`（1982） | `interval_ops` | interval | yes | 1332・1333・1330・1335・1334 | 1 = `interval_cmp`（1315） |

- `CREATE INDEX` の対象型として `time`・`interval` を許す（M4 は `42704`）。`interval` のキーの比較は換算値（`cmp_datum`）。**`interval` と `time` の hash の opclass（1997・1983）は M5 でも作らない**（ハッシュインデックスは範囲外。ハッシュ結合は `hash_datum` を直接使う）。
- `interval` を主キーや UNIQUE に使うと、`'1 day'` と `'24:00:00'` は重複とみなされる（PostgreSQL と同じ）。

---

## 7. テスト

### 7.1 差分コーパス（PostgreSQL 17 との比較。TD-5）

`yuzhu-datetime/tests/fixtures/gen_corpus.py`（実機の PostgreSQL 17 に問い合わせて `pg17_corpus.tsv` を作る。クレートに既にある）の `TEMPLATES` に、次の**種別を追加**し、`tests/pg_corpus.rs` の `eval` に対応を足す。行の形式（`kind \t TimeZone \t DateStyle \t IntervalStyle \t a \t b \t expected`）は既存と同じ。乱数の種は固定（既存の `rng`）。

| 種別 | 件数の目安 | 内容 |
|---|---|---|
| `time_in` / `time_typmod` | 各 600 / 300 | `time` の入力（§6.1.1 の受理・拒否・丸め・ゾーンと日付つき）、`time(p)` |
| `time_pl_iv` / `time_mi_iv` / `time_mi_time` | 各 400 | `time ± interval`（月・日つき）、`time - time` |
| `iv_to_time` / `time_to_iv` / `ts_to_time` / `tstz_to_time` / `date_pl_time` | 各 300 | キャスト（無限は NULL）、`date + time` |
| `extract_time` / `part_time` | 各 200 | `time` の単位（対応外の単位の 0A000 とその文言を含む） |
| `age_ts` / `age_tstz` | 各 800 | `age`（月末・うるう年・DST の境界・負の差・無限） |
| `make_date` / `make_ts` / `make_tstz` / `make_time` / `make_iv` | 各 300 | 範囲外・境界（`24:00:00`、秒 60、BC）、zone の符号 |
| `to_ts_float` | 200 | `to_timestamp(float8)`（`NaN`・無限・範囲外・小数） |
| `tz_iv` | 200 | `AT TIME ZONE interval`（月・日つきの 22023） |
| `set_ds` / `set_is` / `set_tz` | 各 100 / 30 / 60 | SET の検証（`SHOW` の値とエラーの文言。`set_tz` は既存 29 件に足す） |
| `send_*`（`date` / `time` / `ts` / `tstz` / `iv`） | 各 200 | `encode(*_send(x), 'hex')`。`recv` → `send` の往復 |
| `agg_iv` | 100 | `sum` / `avg` / `min` / `max` |
| `date_bin` | 100 | 任意 |

- 地域名は `Asia/Tokyo`（DST なし）と `America/New_York`（DST あり）と、LMT の確認用に `Europe/London`（既存のコーパスが使うゾーン。`tests/fixtures/zoneinfo/` にコピー済み）に絞る（m5-types-fk §3.6.3）。tzdata の版による差が出にくい年（2000〜2030）を主にする。
- **バイナリ**: コーパスの `send_*` は 16 進の文字列で比べる。`recv` の範囲検査は PostgreSQL に直接バイナリを送る手段（`COPY ... FROM STDIN BINARY` または準備済み文のバイナリのパラメータ）が実機側で要るので、**§7.3 の Rust の試験**で固定値を使う。
- `cargo test -p yuzhu-datetime` が `pg_corpus` を通すこと（CI の既存のジョブ）。コーパスの再生成は PostgreSQL 17 のコンテナで `gen_corpus.py` を実行する（CLAUDE.md の「PostgreSQL が正解」）。

### 7.2 共有テスト（`tests/slt/m5/datetime/`。TS-5 に渡す）

PostgreSQL に対しても同じ結果になるもの（00 §9）。**どのファイルも先頭で `SET TIME ZONE 'UTC'`（`DateStyle`・`IntervalStyle` を使うものはその設定も）を明示し、`DEFAULT` の畳み込みを除いて実時間に依存しない**。

| ファイル | 内容 |
|---|---|
| `interval_io.slt` | 入力の形式（postgres・ISO 8601・`@ ... ago`・小数・BC 相当）、4 つの `IntervalStyle` の出力、無限、型修飾子（`interval(3)`・`second(3)`・`year to month`）、エラー（22007 / 22015） |
| `interval_ops.slt` | `+ - * /`、`timestamp - timestamp`、`timestamp ± interval`（月末・うるう年）、`date ± interval`、比較（`'1 mon' = '30 days'`）、`justify_*`、`ORDER BY` / `GROUP BY` / `DISTINCT` / `UNIQUE` / 主キー、`sum` / `avg` / `min` / `max` |
| `time.slt` | 入出力、`24:00:00`、`time(p)`、`time ± interval`、`time - time`、キャスト、`date + time`、`extract`、`ORDER BY`、B+Tree |
| `timezone_set.slt` | `SET TIME ZONE` の全構文、`SHOW TimeZone`、`SET LOCAL` とロールバック、`RESET`、`RESET ALL`、不正値（22023）、`timestamptz` の出力と入力、`AT TIME ZONE`（`Asia/Tokyo`・`America/New_York`・数値・略称・`interval`）、`AT LOCAL`、DST の境界、`timestamptz + interval` |
| `datestyle.slt` | 4 つの出力形式 × 日付順、入力の日付順（`'01/02/2024'`）、`SET LOCAL` / ROLLBACK、不正値（22023 + DETAIL） |
| `datetime_funcs.slt` | `extract` / `date_part` / `date_trunc` の全単位（date・timestamp・timestamptz・interval・time）、`age`、`make_*`、`to_timestamp(float8)`、`isfinite`、`generate_series(timestamp, ...)`（TD-D13） |
| `now.slt` | `now() = transaction_timestamp()`、トランザクション内で変わらない、`BEGIN` の中で `statement_timestamp()` が進む、`pg_typeof(now())` など型、`DEFAULT now()` と `DEFAULT 'now'`（`now()` の結果と同じ型・順序だけを比べる。値は比べない）、`clock_timestamp() >= now()` |
| `datetime_errors.slt` | 22007 / 22008 / 22009 / 22015 / 22023 / 0A000（`timetz`、`to_char`）の SQLSTATE と文言 |

- **差が出るケースは入れない**（既知の差。§10）: `SHOW TimeZone` の既定値、`timetz`、`AT TIME ZONE` を `time` に使う式、`DEFAULT 'now'` の畳み込みが M4 に受け入れられなかった場合（その場合は `now.slt` から外す）。
- `tests/restart/`（再起動をまたぐテスト）: `interval` / `time` の列を持つテーブルを書いて再起動し、同じ値と `ORDER BY` の結果が返ること（ディスク形式と `catalog_version`）。

### 7.3 Rust のテスト

- **`types::datetime` / `interval` / `time` の単体テスト**: §6.7 の例をそのまま固定値にした `send` / `recv` の往復と範囲外（`time` が `-1` と `86_400_000_001`、`timestamp` が範囲外、長さ違い）。`interval::encode` / `decode` の 16 バイト（§3.2 の例）。`Interval` の `cmp_datum`・`hash_datum` が一致すること（`'1 mon'` と `'30 days'`）。`Time` の演算の境界（巻き戻し、`24:00:00`）。
- **`Settings` の単体テスト**（`settings.rs`）: §5.1 の表を 1 行ずつ（`SHOW` の値とエラー）、`SET LOCAL` の後に COMMIT / ROLLBACK したときの `DateTimeSettings` の戻り、`RESET ALL`、startup パラメータ、`DateStyle` の `German`。M1 の `set_show_reset_and_parameter_status` を `TimeZone` に広げる。
- **`Session` の統合テスト**: `ManualClock` で `now()`・`statement_timestamp()`・`clock_timestamp()` と `'today'`・`'now'` のリテラル（暗黙のトランザクションと `BEGIN`）、`ParameterStatus` が `SET TIME ZONE` の後・`ROLLBACK` の後・`SET LOCAL` の後に正しく出ること（M1 の `flush_parameter_status` の試験の拡張）。`DEFAULT 'now'` の畳み込み（`ManualClock` を進めて、2 回目の INSERT が同じ値になる）。
- **tzdata が無い環境**: `ZoneDb::without_tzdata()` で `Cluster` を開き、`SET TIME ZONE 'Asia/Tokyo'` が 22023、`SET TIME ZONE 'UTC'` と `'+9'` は通ること。
- **パーサのテスト**（`sql/parser/tests.rs`）: §6.6.1 の表、`AT TIME ZONE` / `AT LOCAL` の優先順位（`a + b AT TIME ZONE 'x'` は `a + (b AT TIME ZONE 'x')`。`-a AT TIME ZONE 'x'` は `(-a) AT TIME ZONE 'x'`。実機で確かめる）、`SET TIME ZONE INTERVAL '+09:00' HOUR TO MINUTE`。
- **プロパティテスト**: ランダムな `interval` の `encode` / `decode`、`format` → `parse` の往復（4 形式。クレートの `iv_roundtrip` コーパスの補強）、`Time` の `format` → `parse`。

### 7.4 実機での追加確認（実装者が着手時に行うもの）

§9 の未検証の項目を、`sandbox/pg.sh start` の PostgreSQL 17 に SQL を投げて確かめ、結果を `tests/fixtures/` のコーパスまたは slt に固定する。

---

## 8. 実装の分担と工数

00 §7 の WP（TD-1〜TD-5）を詳しくする。ID は変えない。日数は AI の実装エージェント 1 本。**TD の合計は約 13.5 日**（00 の見積もりのとおり）。依存: TD-1 → TD-2・TD-3・TD-4、TD-1・XQ-4 → TD-5。F0（分割）は M4 のマージ後。

| WP | 内容 | 主なファイル | 依存 | 日数 |
|---|---|---|---|---|
| **TD-1** | interval の統合: `Datum::Interval`（F0 の変種）、`types/interval.rs`（`encode` / `decode`、演算子・キャスト・集約の本体）、`types/datetime.rs` の `dt_err` / `apply_typmod` / `typmod_in` / `typmod_out` / 入出力の振り分け、クレートの `typmod.rs`、`catalog/builtin/datetime.rs` の interval の行（§6.5 の突き合わせ）、`timestamp - timestamp` と `timestamptz ± interval` の行、`opclass.rs` の `interval_ops`、パーサの INTERVAL のフィールド指定（§6.6.1）、`KNOWN_UNSUPPORTED_TYPES` の修正（M4 に依頼） | `types/{datetime,interval}.rs`、`catalog/builtin/datetime.rs`、`sql/parser/datetime.rs`、`yuzhu-datetime/src/typmod.rs` | F0、M4 | 2.5 |
| **TD-2** | `Settings` の `DateTimeSettings`、`TimeZone` / `DateStyle` / `IntervalStyle` / `timezone_abbreviations` の検証、`Settings::type_env`、ParameterStatus の値、`ZoneDb` の `Cluster` への持たせ方と tzdata が無い環境、`SET TIME ZONE` の全構文（INTERVAL の枝）、`AT TIME ZONE` / `AT LOCAL` のパーサと関数（`timezone` の 6 行）、`at_time_zone_interval` | `settings.rs`、`sql/parser/{datetime,misc}.rs`、`yuzhu-datetime/src/timestamp.rs` | TD-1 | 2.5 |
| **TD-3** | `util/clock.rs`（`Clock`・`SystemClock`・`ManualClock`）と `Session` / `RuntimeInfo` の時刻（§5.2）、`now()` 系・`localtime` の行、`extract` / `date_part` / `date_trunc` の interval 版と `numeric` への変換、`age`・`make_*`・`to_timestamp(float8)`・`justify_*`・`isfinite`・`date_bin`・`date_add` の行と関数、`generate_series(timestamp)`（M4 の登録方式が分かってから）、`to_char` などの 0A000 の行 | `util/clock.rs`、`yuzhu-datetime/src/{age,make}.rs`、`types/datetime.rs`、`catalog/builtin/datetime.rs` | TD-1 | 4 |
| **TD-4** | `time` 型: クレートの `Time`（`time.rs`・`DecodeTimeOnly`・`extract_time`。**D49: M4 の実装中に始めてよい**）、`Datum::Time`、`types/time.rs`、`catalog/builtin/datetime.rs` の time の行、`opclass.rs` の `time_ops`、`min` / `max`、キャスト・演算子、`timetz` の 0A000 | `yuzhu-datetime/src/{time,fields,parse}.rs`、`types/time.rs` | TD-1（クレート側は独立） | 2.5 |
| **TD-5** | 日時 5 型のバイナリ形式（`send` / `recv`。04 章の `binary.rs` の振り分けに登録）、`DEFAULT` の畳み込み（§5.3。M4 への依頼が通れば呼び出しを 1 か所）、差分コーパスの拡張（§7.1）、slt（§7.2）、Rust のテスト（§7.3） | `types/datetime.rs`、`yuzhu-datetime/tests/*`、`tests/slt/m5/datetime/*` | TD-1、XQ-4 | 2 |

- **カットライン**（00 §1.3）: 遅れたら TD-4（time 型）を M6 に回す。ほかの章の契約を変えずに落とせる（`Datum::Time` の変種は F0 が先に足す。`time` を使う箇所は 0A000 に戻す）。TD-4 を落としても TD-1・TD-2・TD-3 は独立（`date_trunc(text, time)` のキャストが消えるだけ）。
- 進め方: (1) TD-4 のクレート側（`Time`・`DecodeTimeOnly`・コーパスの `time_*` 種別）を M4 の実装中に始める（新規ファイル中心。`parse.rs` と `fields.rs` を直すが、これらは M4 が触らない `yuzhu-datetime` のファイル）→ (2) F0 の後に TD-1 → (3) TD-2・TD-3・TD-4 の本体を並列 → (4) TD-5。
- 他の担当に依頼するもの: F0（`Datum` の変種、`catalog/builtin/datetime.rs`）、M4 の `analyzer/ddl.rs`（§5.3 の呼び出し、`KNOWN_UNSUPPORTED_TYPES`）、`session/*`（時刻を `Clock` から取る。LK・F0）、04 章（`binary.rs` の振り分けに日時 5 型を登録）、05 章（`io.rs` の振り分け、`pg_type` の send / recv 列）。

---

## 9. 未検証の点（実装前に確かめること）

1. **M4 の章 09・07 の確定内容**（最初にやる）: (a) `Datum::Date` / `Timestamp` / `TimestampTz` の型と `types/datetime.rs` の関数名（この章の `apply_typmod`・`input_text`・`output_text` と重ならないか）、(b) `FnKind::Runtime` が `datetime_env()` を使えるか、`FnKind::Pure` に `DateTimeEnv` を渡す必要がないか、(c) `BuiltinAggregate` の形（`sum` / `avg(interval)` の状態）、(d) `FunctionScan` の `BuiltinFunction` の形（`generate_series` の日時版）、(e) 名前つき引数（`make_interval(years => 1)`）のアナライザの対応、(f) DEFAULT を保存する箇所（§5.3 の呼び出しの位置）と `deparse` の出力、(g) M4 が日時 3 型に追加した `pg_proc` / `pg_cast` / `pg_operator` の行（この章の §6.5 と重複しないか）。
2. **実機の SQLSTATE と文言の確認**（§6 で「実装時に確かめる」とした項目）: （`time_recv` の範囲外は【実機】で 22008 `time out of range` に確定した。R-27）、`make_timestamp` の範囲外のエラーの書式、`time + 'infinity'::interval` と `interval → time`（無限）のエラー文言、`interval time zone "..." must not include months or days` の SQLSTATE（実機は 22023）、`SET timezone_abbreviations` の DETAIL の有無、`date_recv` の範囲外の文言（`date out of range: <整数>`）。
3. `statement_timestamp()` が「メッセージの受信時刻」であること（§5.2。`exec_simple_query`・`exec_parse_message` などが `SetCurrentStatementStartTimestamp` を呼ぶ位置を `PG:src/backend/tcop/postgres.c` で確認）。
4. `AT TIME ZONE` の優先順位（`a + b AT TIME ZONE 'x'`）を実機で確かめ、パーサの試験にする。
5. `age()` の移植（`timestamp_age` / `timestamptz_age`）の細部（`dt1 < dt2` のときの借り上げ、`timestamptz` の `tm1` / `tm2` のオフセット）を `PG:src/backend/utils/adt/timestamp.c` で逐語的に照合する（コーパスの `age_*` が最終的な検証）。
6. `extract` の単位の一覧（`time`・`interval` の `epoch`・`julian`・`isoyear` など）がクレートの `extract_interval` と PostgreSQL 17 で一致すること（クレートのコーパスで確認済みなのは interval の 335 件）。`time` の `extract` は TD-4 のコーパスで確かめる。
7. `timestamptz` の `AT TIME ZONE` で略称が使われたときの DST の解釈（クレートの `determine_abbrev_offset`）は実機と比べてあるが、略称表（`Default`）の版が PostgreSQL 17.11 と同じか（`tokens.rs` の表と `timezonesets/Default` を突き合わせる）。
8. tzdata の版: CI のコンテナの `/usr/share/zoneinfo` と実機（`postgres:17` の `/usr/share/zoneinfo` またはPostgreSQL 同梱）が違うと、2025 年以降の規則変更（一部の国）で差が出る。コーパスのゾーンファイルは `tests/fixtures/zoneinfo/` に実機から固定してある。
9. `INTERVAL` の型修飾子の `range` の許可リスト（§6.1.3）が PostgreSQL 17 の `intervaltypmodin`（`PG:src/backend/utils/adt/timestamp.c`）と一致すること。
10. `pg_proc` / `pg_operator` の OID（§6.5）を `.dat` で確認すること（00 §3.2）。

---

## 10. 確認事項

ユーザーの不在中に仮決めしたことです。ID は `M5-TD-Q<n>`。10 章（TS）が集める。

| ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|
| M5-TD-Q1 | **`timetz` は対応しない（0A000）**。`current_time` も 0A000（TD-D5） | docs も非推奨。00 の範囲 | `current_time` を使うアプリ（一部の ORM のスキーマ）は動かない。足すなら `Datum::TimeTz` の変種、12 バイトのディスク形式、演算子・関数・キャストの一式で +4〜5 日 |
| M5-TD-Q2 | **既定の `TimeZone` は `UTC`**（実機の initdb は検出した `Etc/UTC` など）（TD-D16） | M1 のまま。共有テストは明示して揃える | `SHOW TimeZone` を使う未設定のテストが実機と食い違う。`Etc/UTC` に変えるなら 1 行 |
| M5-TD-Q3 | **`DEFAULT 'now'` を CREATE TABLE の時点で畳み込む**（M4 の `analyzer/ddl.rs` に呼び出しが 1 か所要る）（TD-D10） | PostgreSQL と同じ。畳み込まないと `DEFAULT 'now'` の既存スキーマが別の意味になる | M4 が受け入れなければ既知の差（挿入のたびの時刻）。共有テストには入れない |
| M5-TD-Q4 | **`to_char`・`to_date`・`to_timestamp(text, text)` は M6**。行は作って実行すると 0A000（TD-D11） | 書式言語が大きい（L） | ORM のうち `to_char` を使うものは動かない（少ない）。入れるなら +5〜8 日 |
| M5-TD-Q5 | **`AT TIME ZONE` を `time` に使うのは 42883**（PostgreSQL は `timetz` 経由で通る）（TD-D5） | `timetz` が無い | まれ。`timetz` を入れれば消える |
| M5-TD-Q6 | **`OVERLAPS` は 0A000（M6）**（TD-D12） | 構文が特殊。使用頻度が低い | 入れるなら +1 日（パーサ 0.5、関数 0.5） |
| M5-TD-Q7 | **`generate_series(timestamp, timestamp, interval)` は M4 の登録方式に乗れれば M5、乗れなければ M6**（TD-D13） | M4 の章 03・09 が確定していない | ない場合は日付の連番をアプリで作ることになる（BI ツールで使う） |
| M5-TD-Q8 | **`date_bin`・`date_add`・`date_subtract`・`pg_sleep_for` は任意（TD-3 の最後）** | 使用頻度が低いが安い（各 0.2 日） | 落とすと `42883` |
| M5-TD-Q9 | **`SET` の startup パラメータの不正値は無視する**（PostgreSQL は接続を拒否）（M1 のまま） | M1 の決定を変えない | `PGTZ=Foo/Bar` で接続した場合、PostgreSQL は FATAL（22023）、yuzhu は無視して既定値で続ける |
| M5-TD-Q10 | **`timezone_abbreviations` は `Default` 固定**で、`SET` は `Default`（大文字小文字を区別）だけ受け付ける（TD-D9） | 00 §3.6 の決定 | 設定ファイルを読むなら +1 日。ほぼ使われない |
| M5-TD-Q11 | **`DateTimeEnv.now` はトランザクションの開始時刻（最初の文の `statement_timestamp`）**。`'now'` 入力も同じ値（§5.2） | PostgreSQL と同じ | — |
| M5-TD-Q12 | **`Datum::Time(i64)` のまま**（クレートの `Time` 新型は境界で変換）（TD-D4） | 00 の契約 | `Datum::Time(yuzhu_datetime::Time)` にしてもよい（M4 の `Date` などと揃う）。変種の中身だけの変更で、ディスク形式は変わらない |

**PostgreSQL との既知の差**（10 章が集める。共有テストに入れない）:

1. `SHOW TimeZone` の既定が `UTC`（実機は `Etc/UTC`）（Q2）。
2. `timetz`・`current_time`・`AT TIME ZONE` の `time` への適用（Q1、Q5）。
3. `to_char`・`to_date`・`to_timestamp(text, text)`・`OVERLAPS`・`timeofday()`・`pg_timezone_names` / `pg_timezone_abbrevs`（Q4、Q6）。
4. `DEFAULT 'now'`（Q3。M4 が受け入れなかった場合）。
5. startup パラメータの不正な `TimeZone` / `DateStyle`（Q9）。
6. tzdata の版が実機と違うときの、新しい規則変更の年の結果。

---

## 11. 契約への変更依頼

00 と食い違う必要はない。ただし 00 の持ち主の表（§8）に無いファイル・項目の追加と、他の章・M4 の持ち物への依頼がある。**00 の担当に反映を依頼する**。

1. **新規ファイルの持ち主の追記**（00 §8 の TD の行）: `util/clock.rs`、`sql/parser/datetime.rs`、`yuzhu-datetime/src/{time,typmod,age,make}.rs`。
2. **`Clock` と `ClusterOptions`**（00 §4.8・§2 の `engine.rs`、持ち主 LK / DB）: `ClusterOptions` に `clock: Arc<dyn Clock>`（既定 `SystemClock`）、`timezone: String`（既定 `"UTC"`）、`timezone_dir: PathBuf`（既定 `/usr/share/zoneinfo`）。`Cluster::clock()`、`Cluster::zones() -> &Arc<ZoneDb>`。`control.rs`・`datadir.rs` の `SystemTime::now()`（制御ファイルの時刻など）は `Clock` に置き換える（00 規約 6。持ち主 B / F0）。
3. **`Session`**（持ち主 F0・LK）: メッセージの受信時に `stmt_ts = clock.pg_micros()` を記録し、`Transaction.started_at` に（暗黙のトランザクションの最初の文、または `BEGIN` の）`stmt_ts` を入れる（§5.2。M4 の `started_at` は `SystemTime::now()` から取る想定なので置き換え）。`RuntimeInfo::{transaction_timestamp, statement_timestamp, clock_timestamp, datetime_env}` の実装。
4. **`Settings::new` の引数**（M4 契約 §14.2 が `Settings::type_env(&self, zones)` を定め、`Settings::new(user, startup)` は M1 のまま）: `zones: Arc<ZoneDb>` と `default_timezone: &str` を足す（§4.4）。呼び出し元は `session/mod.rs`（F0）。
5. **M4 の `analyzer/ddl.rs`**（M4 の持ち物）: (a) CREATE TABLE の DEFAULT を保存するとき `types::datetime::fold_time_dependent_literals` を呼ぶ（§5.3）、(b) `KNOWN_UNSUPPORTED_TYPES` から `time` と `interval` を外し、`timetz` を残す、(c) 型修飾子の解決で `types::datetime::typmod_in` を呼び、`WARNING`（`TIME(7) precision reduced to maximum allowed, 6`）を `notices` に積む、(d) `format_type` の型名に `typmod_out` を使う。
6. **パーサ**（`sql/parser/expr.rs`・`misc.rs`、M1 の持ち物で、00 の表に持ち主が無い）: `parse_infix` の `"at"` の枝、`parse_type_name` の `"interval"` の枝（`INTERVAL fields` の 0A000）、`parse_zone_value` の `INTERVAL` の枝を、`sql/parser/datetime.rs` の関数への 1 行の呼び出しに置き換える（§6.6）。`current_time` の 0A000 は残し、`localtime` は M4 が `FuncCall` に直すのに合わせる。
7. **`ast.rs`**: 変更なし（`SetArg::String` で足りる。`TypeName.modifiers` の中身の約束だけが変わる。§6.6.1）。F0 の `Datum` の変種（`Time(i64)`、`Interval(yuzhu_datetime::Interval)`）と `catalog/builtin/datetime.rs` の分割先は 00 のとおり。
8. **04 章（`binary.rs`）**: 日時 5 型を `types::datetime::{binary_send, binary_recv}`（形は 04 §4.4。R-04）に振り分ける。長さの検査（4 / 8 / 16 バイト）の 08P01 / 22P03 は `RecvBuf` と 04 章の `input_binary`。
9. **05 章（`io.rs`、`pg_type` の列）**: OID 1082 / 1083 / 1114 / 1184 / 1186 を `types::datetime::{input_text, output_text}` に振り分け、`typmodin` / `typmodout` / `typreceive` / `typsend` の列を埋める。**M4 の `OutputOpts` → `TypeEnv` の変更**（M4 契約 §12.4）に従う。
10. **`yuzhu-server` の `config.rs`**（XQ・AU の持ち物）: `--timezone <name>` と `--timezone-dir <path>` の 2 つの CLI オプションを足す（既定は `ClusterOptions` のとおり）。足さなくても動く（既定値のみ）ので任意。
