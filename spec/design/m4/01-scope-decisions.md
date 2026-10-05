# yuzhu M4 設計 01: 範囲・決定・完了条件・保証（01-scope-decisions）

M4（インデックスとクエリ）の**範囲、横断する決定 D-1〜D-30 の選択肢と理由、調査どうしの食い違いの決定、完了条件、保証**を定める章です。`00-contracts.md`（以下「00」）の §2 は結論だけを並べ、各章の「決定」は章の中の決定だけを持つので、**横断する決定の「選択肢と理由」の正本はこの章**です。書式は `00 §1.2` に従います。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>（M4: B+Tree、PRIMARY KEY / UNIQUE / SERIAL、JOIN、集約、サブクエリ、ルールベース最適化、EXPLAIN。M5 以降は 00 §19 が予約）
- 前提（必読）: `spec/design/m1.md`、`m2.md`、`m3.md`（M2 と M3 は**実装が進行中**。この章は M3 の設計どおりに完成した状態を前提にする）、`QUESTIONS.md`（Q-010〜Q-014）、`PROGRESS.md`
- 調査（根拠）: `spec/research/m4-btree.md`、`m4-query.md`、`research-pg-types.md`、`pg-compat-tools.md`、`m5-types-fk.md`（numeric / 日時 / char(n)）、`m3-tx-semantics.md` §7（シーケンス）
- 正解の基準は **PostgreSQL 17**（実機は 17.11）。実機で確かめたものは「（実測）」、確かめていないものは「（未検証）」と書く。
- **優先順位**: M4 の範囲と完了条件は**この章が正**（`11-tests-plan.md` §1.3 はこの章の §3 を引く）。章をまたぐ名前・署名の食い違いは 11 §2 の D11-1 と §7.1 の C-n が決める。
- この章は 11 章（`11-tests-plan.md`）と並行して書かれ、最初は未完だった。11 §1.3 の完了条件（暫定）をこの章で確定し、11 の側を直した（レビュー対応 R-01。`98-review-response.md`）。

---

## 1. 範囲

### 1.1 M4 のゴール

M4 は yuzhu を「繋がって動く・永続化される・耐久性がある」から**「実用的な問い合わせとインデックスが使える」**に進める。具体的には次の 3 つ。

1. **インデックス**: PostgreSQL の nbtree に準拠した B+Tree を作り、PRIMARY KEY / UNIQUE / 通常のインデックス、シーケンスと SERIAL / IDENTITY を使えるようにする。一意違反のエラーメッセージとフィールドは PostgreSQL と一致させる。
2. **問い合わせ**: JOIN 全種、集約、サブクエリ、集合演算、非再帰の WITH、`UPDATE ... FROM` / `DELETE ... USING` を実行できるようにし、ルールベースの最適化と EXPLAIN（テキスト形式）を持つ。**そのために M1〜M3 の内部構造（列参照の表現、論理プランと物理プランの分離）を M4 の冒頭で作り直す**（Q-013。章 02）。
3. **周辺ツール**: psql 17 の `\dt` `\dn` `\di` `\ds` `\l` と `\d シーケンス`、pgbench の `-i` と組み込みスクリプト（tpcb-like）が動く。そのために numeric・`char(n)`・`timestamp` / `timestamptz` / `date`・COPY FROM STDIN を前倒しで入れる（Q-011、Q-012）。

M4 の終わりの状態は「`pgbench -i` で初期化し、`pgbench -c 4 -T 30 -M simple` を `kill -9` 試験つきで流せる」（完了条件 5、7）。性能は目標にしない（計測して `PROGRESS.md` に記録するだけ）。

### 1.2 M4 で入れるもの

範囲の一覧は 00 §3 が持つ（二重管理しない）。この章は**分類**だけを決める。

| 分類 | 内容 | 扱い |
|---|---|---|
| **必須** | 00 §3 の「M4 で入れる」の表のうち、下の「任意」以外のすべて | 完了条件（§3）で検査する |
| **後半・任意**（カットライン） | (a) `RETURNING`（INSERT / UPDATE / DELETE。00 D-26）。Bound と物理プランには欄を用意し、未実装の間は `0A000`。(b) インデックス順によるソートの省略（04 §7.4）。(c) 内側 Index Scan の Nested Loop（04 §7.5.3）。(d) `IS DISTINCT FROM`（M4-Q22）。(e) 行値の `IN`（M4-Q18）。(f) `pg_get_serial_sequence`。(g) `\dt+`・`\df`・`\du`。(h) `\d tbl`（約 7 日。10 §6.3） | 完了条件に入れない。余力があれば入れる。遅れたら落とす順は (h) → (g) → (d)(e)(f) → (c) → (b) → (a)。ただし (a) は ORM が使うので M5 の最初に入れる |
| **前倒し** | numeric の基本部分（Q-011。D-3）、`char(n)`・`timestamp` 系・COPY FROM STDIN（Q-012。D-4〜D-6）、`regclass` / `regtype` / `int2vector` / 1 次元 `int2[]`（カタログ用の最小限） | M5 の計画から外す（M5 の `05-types-core.md` は「穴埋め」だけ） |
| **作り直し** | 列参照の表現と論理 / 物理プランの分離（Q-013。D-1、D-2。章 02） | M4 の最初の約 7 日。以後のすべての担当の前提 |

### 1.3 M4 で入れないもの（実行すると `0A000`）

00 §3 の「M4 では対応しない」を正とする。要点: `WITH RECURSIVE`・`LATERAL`・ウィンドウ関数・`GROUPING SETS` / `ROLLUP` / `CUBE`・`INSERT ... ON CONFLICT`・`MERGE`・`CREATE VIEW`・`ALTER TABLE` の `ADD PRIMARY KEY / UNIQUE` と `OWNER TO` 以外・`DEFERRABLE`・式 / 部分 / `INCLUDE` インデックス・`interval` / `time` / `timetz`・配列型（`int2vector` と 1 次元 `int2[]` を除く）・`FOREIGN KEY`・`COPY TO` / CSV / バイナリ・`FOR UPDATE` / `FOR SHARE`・`string_agg` / `array_agg`・マージ結合・スピル・コストベース最適化・`CREATE DATABASE` / `CREATE SCHEMA`。**これらは `0A000` と PostgreSQL にしかない機能の文言で拒否し、黙って別の動作にしない**。差は `tests/slt/m4/KNOWN-DIFFS.md`（11 §3.2.7）の ID で管理する。

### 1.4 QUESTIONS.md の Q-010〜Q-014 と M2-Q8 の扱い

| 項目 | M4 での扱い |
|---|---|
| Q-010（64 ビット XID） | M2 で実現済み。M4 に作業なし |
| Q-011（numeric の前倒し） | D-3。`yuzhu-numeric` を統合（章 09）。`sum(int8)` / `avg(整数)` の結果型が PostgreSQL と一致する |
| Q-012（COPY・char(n)・timestamp の前倒し） | D-4〜D-6 と D-25（章 09、10）。pgbench の初期化と tpcb-like が動く |
| Q-013（列参照の表現と論理 / 物理プランの作り直し） | D-1、D-2（章 02）。移行は段階的（P0-a〜e）で、各段階で `tests/slt/m1`〜`m3` が通る |
| Q-014（MultiXact の簡易版） | M5。M4 は D-8 の `WaitFor` の口だけ用意する |
| M2-Q8（psql の `\dt` は M4） | D-24。`\dt` `\dn` `\di` `\l` に加えて `\ds` と `\d シーケンス`（§3 の完了条件 5）。`\d tbl` は任意（10 §6.3。M4 では動かない） |
| M2-Q9（`pg_get_expr` の正規形） | D-21 で解決 |
| M2-Q22（slt の後始末） | `slttools lint` と `z_final/no_leftovers.slt`（11）で解決 |

### 1.5 この章が決めないもの

各機能の仕様（章 02〜10）、ディスク形式（06 が B+Tree、07 がカタログ、08 がシーケンス、09 が numeric 等）、型・署名の契約（00）、テストの形式（11）。この章は**何を作り、何をもって終わりとし、何を保証するか**だけを決める。

---

## 2. 決定

### 2.1 横断する決定 D-1〜D-30（選択肢と理由）

00 §2 の結論に対応する。`出典` は根拠の調査・章。変えたい場合の影響は各章の確認事項（`99-questions.md` の M4-Q）に書いた。

| # | 論点 | 選択肢 | 決定 | 理由 | 出典 |
|---|---|---|---|---|---|
| D-1 | 式・プランの表現 | (A) M1 のまま Bound から直接 PhysicalPlan（`ColumnRef { index }`）／(B) Bound → 論理 → 物理の 3 層を別々の式型で／(C) 式の木を `Expr<C, Q>` 1 つに集約し、列参照の型だけ層ごとに変える | **(C)**。AST → Bound（`Var`）→ 論理プラン（`ColId`）→ ルール → 物理プラン（`PhysCol`）→ Executor | (A) は JOIN の並べ替えと述語の押し下げに耐えない（列位置が結合の形で変わる）。(B) は式の木を 3 回定義して変種の追加漏れが起きる。(C) は変種を 1 か所に足し、層ごとの許可は検証関数で守る | m4-query §0-1、Q-013、章 02 |
| D-2 | サブクエリの表現 | 式の外の「サブプラン表」／式の中の `SubLink` | **式の中に `SubLink { kind, test, query }`**。ANY / ALL の比較は `test` に `SubLinkOutput(i)` を含む式。`IN` は `ANY`、`NOT IN` は `NOT ANY` | 相関の `Var` / `ColId` の扱いが式の走査に統一され、ルールが 1 つの木を書き換えるだけで済む | 章 02、03 |
| D-3 | numeric | M5 に回す／最小限を前倒し／`yuzhu-numeric` を統合 | **既存の `yuzhu-numeric` クレートを統合**（`Datum::Numeric`）。ディスク形式は 00 §12.3 の固定ヘッダ（PG の short / long ヘッダは採らない） | `sum(int8)`・`avg(整数)`・`1.5` の型が numeric で、ないと集約の互換が崩れる（Q-011）。独立したクレートがあり新規実装が要らない | Q-011、m4-query §5.2、09-Q1 |
| D-4 | 日時 | 全部 M4／`date` `timestamp` `timestamptz` だけ | **後者**。`interval`・`time`・`timetz` は M5（キーワードは `0A000`）。`timestamp - timestamp` は `42883` | pgbench の初期化に要るのは `timestamp`。`interval` は連鎖（比較・ハッシュ・出力形式・演算子・集約）が大きい | Q-012、09-Q2 |
| D-5 | `char(n)` | `text` の別名にする／別の変種 | **`Datum::BpChar(String)`**（OID 1042）。比較・ハッシュは末尾の空白を無視する | PostgreSQL の挙動と `pgbench_accounts.filler` の互換 | Q-012 |
| D-6 | COPY | M5／FROM STDIN（テキスト）だけ M4 | **COPY FROM STDIN（テキスト形式、Simple Query）**。TO・CSV・バイナリは M5 | pgbench の既定の初期化（`-I g`。クライアント側でデータを生成して `COPY` で流す）と、pg_dump のリストアのデータ部が使う | Q-012、章 10 |
| D-7 | B+Tree の形 | PG の nbtree に準拠／独自の簡素な木 | **nbtree に準拠**（8KB、メタページ、high key、right-link、ヒープ TID をタイブレークに使う）。suffix truncation・重複排除・項目の削除・ページ削除は作らない。ルート葉は CREATE INDEX の時点で作る。構造変更は全画像の 1 レコード `BTREE_PAGES`、通常の挿入は差分の `BTREE_INSERT_LEAF` | M5 の複数ライターと VACUUM が right-link とフラグの予約の上に載る。全画像 1 レコードは「未完了分割」という状態を作らずクラッシュ試験を単純にする | m4-btree §0、章 06 |
| D-8 | PK / UNIQUE | 制約専用の検査／一意インデックス + `pg_constraint` | **一意インデックス + `pg_constraint`**。検査は行ごとに即時。一意性検査の API は `WaitFor(xid)` を返せる形だが M4 では起きない | PG と同じ（名前 `t_pkey`、`23505` の文言）。M5 の待ちを API 変更なしで足せる | m4-btree §0-6、§6 |
| D-9 | シーケンス | 通常の表（MVCC）／1 ページのリレーションをその場で上書き | **relkind `S` の 1 ページ**。MVCC を使わない。WAL は `SEQ` rmgr の `SEQ_LOG`（`SEQ_LOG_VALS = 32`）。コミット時の flush は `Transaction.wal_flush_upto` | ロールバックしても値が戻らない PG の意味論。1 ページなので WAL が小さい | m3-tx-semantics §7、章 08 |
| D-10 | SERIAL / IDENTITY | 実行時に特別扱い／解析時に書き換え | **解析時に「シーケンス + DEFAULT + NOT NULL + 所有関係」に書き換える** | 実行器に特別な経路が要らない。`pg_depend` の `a` / `i` で DROP の挙動が PG と同じになる | 章 08 |
| D-11 | TRUNCATE / VACUUM / ANALYZE | `DELETE` で代替／新しい relfilenode | **TRUNCATE は新しい relfilenode（ロールバック可能）**。`VACUUM` / `ANALYZE` は何もせず成功。**ブロック内で `25001` になるのは `VACUUM` だけ**（`ANALYZE` は成功。実測。C-16） | PG と同じ。VACUUM の本体は M5 | 章 07 |
| D-12 | ALTER TABLE | 全部 `0A000`／最小限 | **`ADD [CONSTRAINT n] PRIMARY KEY / UNIQUE` と `OWNER TO`（ロールを検証して `relowner` を更新）だけ**。それ以外は `0A000` | pgbench の `-I p`、ORM のマイグレーションが使う最小限。`OWNER TO` を何もしないと `\dt` の Owner が誤る（C-16） | 章 07 |
| D-13 | 結合アルゴリズム | ハッシュ + マージ + NLJ／ハッシュ + NLJ | **HashJoin（INNER / LEFT / RIGHT / FULL / SEMI / ANTI）、NestedLoopJoin（内側を Materialize）、NestedLoopParam（内側のインデックス検索）。マージ結合は作らない**。RIGHT は論理プラン構築で LEFT に直す | ハッシュ結合で等値結合の全種を賄える。マージ結合は M6 のコストベースで意味を持つ | m4-query §0-4 |
| D-14 | 集約 | ソート集約のみ／ハッシュのみ／両方 | **HashAggregate、Aggregate（GROUP BY なし）、GroupAggregate（`enable_hashagg = off` のとき Sort + 逐次）**。集約関数は `BuiltinAggregate` の静的な表 | PG の `enable_hashagg` と対応する。表引きで `pg_aggregate` と型が一致 | m4-query §5 |
| D-15 | サブクエリの最適化 | 素朴な再実行のみ／PG と同じ範囲の書き換え | **WHERE と内部結合の ON の最上位 AND にある `EXISTS` / `IN` / `NOT EXISTS` を SEMI / ANTI 結合に**、残りは SubPlan（`Rescan`）/ InitPlan / ハッシュ化 SubPlan。`NOT IN` は結合にしない（NULL の意味論） | PG と同じ範囲で正しさが保てる。一般的な非相関化は M6 | m4-query §0-7 |
| D-16 | CTE | 全部インライン／全部実体化／PG と同じ規則 | **PG と同じ規則**（非再帰・副作用なし・参照 1 回・`MATERIALIZED` なし → インライン。それ以外は CteScan）。外側の列を参照する共有 CTE は、`MATERIALIZED` の明示と揮発性なら `0A000`、それ以外はインライン（R-05） | `m4-query.md` の M4Q-4（全部インライン）は `MATERIALIZED` と揮発性の意味を壊す | m4-query §3.7、章 04 |
| D-17 | 最適化の枠 | コストベース／規則のみ | **決まった順に 1 回ずつ適用するルール列。統計は持たず、サイズの手がかりは計画時点の `nblocks` だけ** | PG も `estimate_rel_size` で実ページ数を使う。予測可能で slt が安定 | m4-query §0-8 |
| D-18 | インデックス選択 | コスト比較／ヒューリスティクス | **ヒューリスティクス**（一意で全列等値 > 等値の列数 > 先頭列の範囲）。`IN (...)`・`OR`・Index Only Scan・Bitmap Scan は行わない。**ソートの省略（前向き・後ろ向き）は後半・任意**。後ろ向きスキャンは B+Tree に実装し単体テストで検証 | PG もコストで選ぶが、M4 の表は統計がない。後ろ向きスキャンは M5 の ORDER BY ... DESC LIMIT のため | m4-query §7.3、m4-query §2.1 の「M5」を前倒し |
| D-19 | メモリ | スピルを作る／上限を超えたらエラー | **スピルしない**。`yuzhu.query_mem_limit`（既定 256MB）を超えたら `53200`。CREATE INDEX のソートは予算の対象外 | 外部ソート・Grace ハッシュは M6。`work_mem` は SET / SHOW だけできる | m4-query §6.6 |
| D-20 | EXPLAIN | JSON も／テキストのみ | **テキスト形式のみ**。`COSTS OFF` の出力は PG と同じ書式（プランの選び方は一致しない）。コスト欄は `cost=0.00..0.00 rows=0 width=N` | 統計がないのでコストは偽。書式の一致は `onlyif yuzhu` の slt で守る | m4-query §8、章 10 |
| D-21 | 式の逆変換 | 保存形式を変える／deparse を共通部品に | **`deparse/` を EXPLAIN と `pg_get_expr` が共有**。`pg_get_expr` は保存テキストを parse → analyze → deparse して返す（保存形式は変えない） | M2-Q9 の移行を保存形式の変更なしで行える | M2-Q9、章 10 |
| D-22 | 正規表現 | `regex` クレート／手書き | **手書きのエンジン（Pike VM。`types/regex.rs`）** | CLAUDE.md の「外部クレートは周辺用途のみ」。psql の `\dt` が `~` を使う。後方参照・先読みは `0A000` | M2-Q8、章 09 |
| D-23 | カタログの追加 | 全部／必要な 9 個 | **9 カタログ**（`pg_index`・`pg_depend`・`pg_sequence`・`pg_language`・`pg_opfamily`・`pg_opclass`・`pg_amop`・`pg_amproc`・`pg_description`（空））。カタログ自体のインデックスは作らない。**`\d tbl` 用の空のカタログ表は含まない**（任意の C1b。R-19） | 範囲を絞る。カタログのスキャンは小さい | 章 07 |
| D-24 | psql | `\dt` のみ／`\d tbl` まで | **`\dt` `\dn` `\di` `\ds` `\l` と `\d シーケンス` を完了条件にする**（`\ds` と `\d シーケンス` の追加は R-25 による）。`\dt+` `\df` `\du` `\d tbl` は任意（`\d tbl` は M4 では動かない。R-18） | `\d tbl` は配列・拡張統計・出版物の 3 本が M4 の型で解析できず、空のカタログ表の担当もない | M2-Q8、pg-compat-tools §5、章 10 |
| D-25 | pgbench | 初期化のみ／完走まで | **`pgbench -i` と tpcb-like（`-M simple`）の完走（`-c 4 -T 30`）を完了条件にする**。パーティション確認の `CROSS JOIN LATERAL` が `0A000` で失敗しても pgbench は続行する（ログは出ない。R-24） | 性能目標（要件）の測定に使う | Q-012、章 10 |
| D-26 | UPDATE ... FROM / RETURNING | 両方／片方 | **`UPDATE ... FROM` / `DELETE ... USING` を入れる。`RETURNING` は後半・任意**（未実装の間は `0A000`） | 結合の上に既存の Update / Delete を載せるだけ。RETURNING は 02・04・05 の欄が要る | 章 03、05 |
| D-27 | エラーのフィールド | M3 のまま／`s` `t` `c` `n` `W` を足す | **`Error` に `schema` / `table` / `column` / `constraint` / `context` を足し、ErrorResponse の `s` `t` `c` `n` `W` で送る**。23502・23514・23505 でも付ける | ドライバ・ORM が制約名で分岐する。psql の `\set VERBOSITY verbose` | 章 00 §14.4 |
| D-28 | カタログの互換 | 移行する／initdb のやり直し | **`CATALOG_VERSION_NO` を上げる。M3 のデータディレクトリは使えない** | M2-Q20 の方針。新しいカタログと `reltype` などの変更が多い | M2-Q20 |
| D-29 | DDL の実行場所 | `session.rs` に足す／`ddl/` に移す | **`ddl/` モジュールに移す** | `session.rs` が DDL で肥大する。07・08 が `ddl/` に書く | 章 07 |
| D-30 | reloptions / COPY FREEZE | 保存する／検証して捨てる | **`WITH (fillfactor = N)` は検証して捨てる**。`COPY ... WITH (FREEZE)` は受け付けて無視（FREEZE の前提検査は行う） | 保存しても M4 では使わない。pg_dump のリストアが通ればよい | 章 07、10 |

### 2.2 調査どうし・調査と設計の食い違いの決定

| # | 論点 | 食い違い | 決定 | 根拠 |
|---|---|---|---|---|
| X-1 | CTE のインライン | `m4-query.md` の M4Q-4 は全部インライン／`research-pg-types` と PG は規則つき | **PG の規則**（D-16） | `MATERIALIZED` と揮発性の意味 |
| X-2 | ルートの作成 | `m4-btree.md` は遅延（`root = 0`）／00 D-7 は作成時 | **作成時**（06-Q2 ★） | 挿入途中のルート作成の分岐と REDO が要らない |
| X-3 | 構造変更のブロック数 | 調査は `3h + 2`、00 初版は `2h + 3` | **`3h + 1`**。静的な高さの上限なし（C-13、06-Q3 ★） | 06 §5.5 の数え直し |
| X-4 | numeric のディスク形式 | 調査は PG の short / long ヘッダ／00 は固定ヘッダ | **固定ヘッダ**（09-Q1 ★） | M2-Q2 でタプルヘッダが PG と違いサイズ互換の意味がない |
| X-5 | `VACUUM` / `ANALYZE` のブロック内 | `pg-compat-tools` と 00 は両方 `25001`／実測は `VACUUM` だけ | **`VACUUM` だけ**（D-11、C-16） | 実機 |
| X-6 | 型をまたぐ日時の比較演算子 | `m4-btree` §3.2 は `datetime_ops` に入れる／09 は作らない | **作らない**（06-Q9 ★、09-Q4） | `CmpFn` が純粋関数で `TimeZone` を渡せない |
| X-7 | `WITH (fillfactor)` | `pg-compat-tools` §5 は保存／00 は検証して捨てる | **検証して捨てる**（D-30） | 保存しても使わない |
| X-8 | 一意索引の構築の検出 | `m4-btree` §7.1 は `btspool2`／00 は隣接比較 | **C1 が生きている版だけを調べ `BuildUnique::No`**（C-11） | 死んだ版の誤検出を避ける |
| X-9 | `CONCURRENTLY` | `m4-btree` §7.1 は `0A000`／00 は通常と同じ | **通常と同じ扱い**（07-Q11） | ORM のマイグレーションが付けても動く |
| X-10 | EXPLAIN の木 | 00 は PhysicalPlan と同形／各章は表示用の木 | **表示用の木。`exec_id` で計測値を借りる**（C-1） | `Hash` の合成・`Filter` の併合 |
| X-11 | `\d tbl` | `pg-compat-tools` §5 は M4 の A 優先（空のカタログ表を作る）／設計は担当を割り当てていない | **M4 では動かない任意項目**。空のカタログ表を作る C1b と配列の最小実装で約 7 日（R-18、R-19） | 実機採取の SQL を突き合わせると 3 本が通らず、`pg_collation` もない |
| X-12 | 内部の `levels_up` | 02 は `BoundQuery` 単位／03 はスコープ単位 | **03（rtable を持つスコープ単位。`Values` の行を含む）**（C-3、R-04） | 生成側が 03 |
| X-13 | 共有 CTE が外側の列を参照 | 04 は常にインライン／02・05 は `0A000` | **`MATERIALIZED` の明示と揮発性は `0A000`、それ以外はインライン**（R-05） | 結果が同じになる場合だけ通す |
| X-14 | 差分ランダムテストの実体 | 00 は新しい `yuzhu-fuzz-sql`／既存の `tests/tools/difftest` がある | **difftest を仕上げて使う**（D11-6、C-5） | 約 4,300 行の既存ツール |

### 2.3 この章で追加で決めたこと

| # | 論点 | 選択肢 | 決定 | 理由 |
|---|---|---|---|---|
| D01-1 | 完了の判定の方法 | CI のジョブの結果／ローカルで実行できるスクリプト | **ローカル（`claude-sandbox`）で実行できる 1 つのスクリプト `tests/done-check.sh`（K4）の結果**。CI のジョブは同じコマンドを回す参考（push はホストで人が行う。CLAUDE.md） | 実装エージェントは CI も夜間ジョブも起動・確認できない。「連続 7 夜」は 1 回の失敗で途切れ、シードが日付で毎夜変わり再現性のある合否にならない |
| D01-2 | 差分ランダムテストの合否 | 夜間の連続 7 回／固定シード集合の 0 件 + 長時間の一度の完走 | **固定シード集合（1〜32、各 2,000 問い合わせ）で差分 0 件、かつ長時間実行（シード 1001〜1004、各 200,000 問い合わせ）が各 1 回、未解決の差分 0 件** | 固定シードは再現でき、失敗したら同じコマンドで確かめ直せる |
| D01-3 | 任意項目の扱い | 全部必須／カットラインを決める | **§1.2 の「後半・任意」** | 最長経路を守る |
| D01-4 | `\ds` と `\d シーケンス` | 完了条件に含める／任意 | **含める**（D-24） | 08 が必要な機能を作り、実機の SQL は採取済み（10 §6.1） |
| D01-5 | M4 の着手条件 | 常に開始／M3 の統合後 | **M3 が `dev` に統合済みで、`executor/eval.rs` と `session.rs` の変更が止まっていること**（02 §7、11 の U1）。足場（A、S1、K、Z、B1・B2 の新規ファイル）は先に始められる | P0 は M1〜M3 のコードを作り直す |

---

## 3. 完了条件（M4 を終えたと言える条件）

**すべてローカルで、`tests/done-check.sh`（K4。11 §3.10）が順に実行して判定する**（D01-1）。すべて通ったとき `PROGRESS.md` に結果を書いて M4 完了とする。11 §1.3 は同じ表を持つ（この章が正）。

| # | 条件 | 判定するもの | 実行するコマンド（`impl/rust` または repo ルート） |
|---|---|---|---|
| 1 | `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` が通る。`plan_golden`、proptest（256 ケース）、クラッシュ試験 層 1（固定シード）を含む | CLAUDE.md の確認コマンド | `cd impl/rust && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` |
| 2 | `tests/run.sh --target pg` が `tests/slt`（m1〜m4）で通る（**期待値が PostgreSQL 17 で正しい**）。`tests/run.sh --target yuzhu` も通る | 共有 slt（約 120 ファイルを追加） | `tests/run.sh --target pg`、`tests/run.sh --target yuzhu` |
| 3 | `--restart` と `--crash` が、`tests/restart`・`tests/restart/m3`・`tests/restart/m4` のすべてのシナリオで通る | 再起動・クラッシュをまたぐ永続性（インデックス・シーケンス・DDL） | `tests/run.sh --target {pg,yuzhu} --restart`、`--crash` |
| 4 | isolation の全 spec（M3 の 6 本 + M4 の 2 本）が pg と yuzhu で通る | MVCC と索引・シーケンスの可視性 | `tests/tools/isolation` の `yuzhu-isolation --port <P> tests/isolation/specs`（pg と yuzhu。`tests/README.md` の isolation の節。`done-check.sh` が呼ぶ） |
| 5 | `tests/compat/run.sh --target {pg,yuzhu}` が通る: psql 17 の `\dt` `\dn` `\di` `\ds` `\l` と `\d シーケンス` の出力が PostgreSQL と一致、`pgbench -i`（既定と `-I dtGvp`）と `pgbench -c 4 -T 30 -M simple` が完走し不変条件が成り立つ、COPY の psql スクリプトの出力が一致 | 周辺ツール互換 | `tests/compat/run.sh --target {pg,yuzhu}` |
| 6 | 差分ランダムテスト（11 §3.7）: **固定シード 1〜32（各 2,000 問い合わせ）で yuzhu 対 PostgreSQL の差分 0 件**（差分は `tests/tools/difftest/KNOWN.md` に原因を書いた範囲だけ許す）、**長時間実行（シード 1001〜1004、各 200,000 問い合わせ）が各 1 回完走し未解決の差分 0 件** | 結果の一致 | `tests/tools/difftest` の `run --level m4`（`done-check.sh` が呼ぶ） |
| 7 | クラッシュ試験 層 1 のワークロード 1〜8 が全クラッシュ点で I1〜I16 を満たし、変異テストが**すべて検出される** | 耐久性と構造の原子性 | `cargo test --release -p yuzhu-core --test crash_sim` |
| 8 | `EXPLAIN (COSTS OFF)` の書式の一致を確かめる slt（`explain/format.slt`、`explain/deparse_*.slt`）と `plan_variants`（`enable_*` を変えて同じ結果）が pg と yuzhu で通る | プランの書式と最適化の安全性 | 条件 2 に含まれる（`done-check.sh` が個別にも報告する） |
| 9 | `PROGRESS.md` が M4 完了の状態になっており、`QUESTIONS.md` に `99-questions.md` の確認事項が転記されている | 運用 | 目視（`done-check.sh` は `PROGRESS.md` の更新日を警告するだけ） |

- 参考（合否の条件にしない）: pgbench の TPS・レイテンシ（`PROGRESS.md` に記録。06 §7.15）、PostgreSQL 回帰テストの取り込み結果（11 §3.8。差分の一覧を作るのが目的）、夜間ジョブ（CI。push 後にホストで回す）の結果、`\d tbl` と `\dt+` の出力。
- 条件 6 の実行時間の目安: 固定シード 32 本で約 10〜15 分、長時間 4 本で約 1〜2 時間（11 §5 の U49 で計測する）。

---

## 4. 保証（M4 が保証すること・しないこと）

各行に**持ち主の章**を付ける（保証を破る変更はその章を直す）。

### 4.1 保証すること

| # | 保証 | テスト | 持ち主 |
|---|---|---|---|
| G1 | **結果の一致**: M4 の範囲の SELECT / DML は、PostgreSQL 17 と同じ結果（行の集合と、`ORDER BY` があれば順序）を返す。型・列名・NULL の扱い・エラーの SQLSTATE とメッセージが一致する（既知の差を除く） | 共有 slt、差分ランダムテスト | 03〜05、09 |
| G2 | **インデックスの正しさ**: インデックスの全 TID 集合は、ヒープの「索引されるべき版」に等しい。木の構造は検査器（`check.rs`）を通る。インデックスを使う問い合わせと使わない問い合わせ（`enable_indexscan = off`）の結果は同じ | I13・I14、`plan_variants` | 06 |
| G3 | **一意性**: PRIMARY KEY / UNIQUE は、コミット済みの行と自分のトランザクションの行に対して重複を許さない。エラーは `23505`、メッセージ `duplicate key value violates unique constraint "t_pkey"`、DETAIL `Key (a)=(1) already exists.`、フィールド `s` `t` `n` が PostgreSQL と一致 | 共有 slt、isolation | 05、06 |
| G4 | **耐久性**: `kill -9` の後、COMMIT を返したトランザクションはすべて残り、他は見えない。**インデックスは分割の途中の状態で残らない**（構造変更は全画像の 1 レコード）。DDL（CREATE / DROP / TRUNCATE）も同じ | クラッシュ試験 層 1・2、`tests/restart` | 06、07 |
| G5 | **シーケンス**: `nextval` は待たず、ロールバックしても値は戻らない。クラッシュの後に**払い出した値を二度払い出さない**（PostgreSQL の穴を塞ぐ。08-Q8） | isolation、I15、変異テスト | 08 |
| G6 | **メモリ**: 1 問い合わせのハッシュ表・ソート・集約・溜めた行の合計が `yuzhu.query_mem_limit` を超えたら `53200` で文が失敗し、サーバは落ちない | `mem/` の slt | 05 |
| G7 | **割り込み**: 長い問い合わせ・結合・ソート・ハッシュ構築は `statement_timeout` とキャンセルで止まる（`57014`）。ループを持つ全ノードが入力 1 行ごとに検査する（R-11） | 単体テスト | 02、05 |
| G8 | **プランの安全性**: `enable_*` を変えても結果は同じ。デバッグビルドと CI では `validate` が全プランの不変条件（列の束縛、層ごとの変種）を検査する | `plan_variants`、`yuzhu.validate_plans` | 04 |
| G9 | **互換ツール**: psql 17 の `\dt` `\dn` `\di` `\ds` `\l` と `\d シーケンス`、`pgbench -i` と tpcb-like（`-M simple`）、COPY FROM STDIN（テキスト）が動く | `tests/compat` | 10 |
| G10 | **0A000 の誠実さ**: 対応しない構文・機能は黙って別の動作にせず、`0A000` と PostgreSQL にしかない旨の文言で拒否する | 共有 slt（`skipif yuzhu` + `# KNOWN-DIFF`） | 03、11 |
| G11 | **M3 までの保証を壊さない**: `tests/slt/m1`〜`m3`・`tests/restart`・M3 のクラッシュ試験は M4 の各段階（P0-a〜e）で通り続ける | 条件 2・3・7 | 02 |

### 4.2 保証しないこと

- **プランの形と EXPLAIN の一致**: コストと統計がないので、PostgreSQL と同じプランを選ぶとは限らない（`COSTS OFF` の書式だけを一致させる）。EXPLAIN の slt は `onlyif yuzhu` の期待値を持つ（D-20）。
- **性能**: 目標を持たない。項目を消さないインデックス（同じキーの UPDATE を繰り返すと死んだ版の項目が増え続ける。M4-Q62）、メモリ上のソート・ハッシュ、スピルなし、ページ全画像の WAL のため、PostgreSQL より遅い。pgbench は完走するが TPS は M5 以降に改善する。
- **複数ライター**: M3 の単一ライターロックのまま。インデックス・シーケンスは M5 の並列化で再検討する（00 §19 の予約）。
- **同時 DDL との読み手の競合**: M3 の D16（ストレージバリア）のまま。TRUNCATE は PostgreSQL と違い読み手を待たせない（07 §5.6）。
- **起動時の孤児ファイルの掃除**: クラッシュで中断した DDL のファイルは残る（M3 の D15）。
- **`\d tbl` の動作**（任意。M4 では動かない）、`interval` / `time`、配列、FOREIGN KEY、CSV / バイナリ COPY、Extended Query（M5）。

---

## 5. 担当と工数の要約

詳細は `11-tests-plan.md` §4（確定表）と `README.md`。

- **合計 141.8 日**（任意を除く。任意を含めて 143.3 日。`\d tbl` の任意 WP は含めない）。AI の実装エージェント 1 本の稼働日の粗い見積もり。
- **最長経路**: A（2.0）→ P0（5.1。〜7.1）→ L2（6.9。〜14.0）→ 結合（約 5 日）→ 完了判定（約 2 日）で**約 21〜23 日**。並列度は最大で約 19 担当、平均は約 6.7。
- **不確かさが大きい**のは P0（M1〜M3 のコードの作り直し）、L1・L2（ルール）、K（PostgreSQL での期待値の確認）。

---

## 6. 未検証の点

- M3 の実装が `dev` に統合される時期（P0 の着手条件。D01-5）。M2・M3 は並行で実装中で、M4 の設計は M3 の設計どおりに完成した状態を前提にしている。
- `tests/done-check.sh` の実行時間（条件 6 の長時間実行を含めて約 2〜3 時間と見込む。11 の U49）。sandbox のイメージに `python3` も `uv` もない（11 の U4）ので、スクリプトはシェルと Rust のツールだけで書く。
- 完了条件 5 の psql / pgbench のバージョン（17 系。PGDG の `postgresql-client-17`。11 の U6）。サンドボックスには `/usr/lib/postgresql/17/bin/psql` がある。

---

## 7. 確認事項

ユーザーの不在中に仮決めしたことです。ID は `[01-Q<n>]`（`99-questions.md` が `M4-Q142` 以降に振り直す）。

- **[01-Q1] 完了の判定を「ローカルで実行できるスクリプトの結果」にする**（D01-1、D01-2）。仮決め: `tests/done-check.sh`。CI の夜間ジョブ（連続 7 回）は条件から外し、固定シード 32 本の 0 件と長時間実行の一度の完走に置き換える。理由: 実装側は push も CI の起動もできない（CLAUDE.md）。連続 7 夜は 1 回の失敗で途切れ、シードが日付で変わるので再現できない。変えたい場合の影響: 夜間の連続合格を条件に戻すと完了判定が push 後のホスト作業になり、フェーズ 4（約 5 日）に 7 夜分の余裕が要る。
- **[01-Q2] 後半・任意の項目（RETURNING、ソートの省略、内側 Index Scan の NLJ ほか）を完了条件に入れない**（§1.2、D01-3）。仮決め: 余力があれば入れる。`RETURNING` は M5 の最初に入れる。理由: 最長経路（P0 → L2）を守る。変えたい場合の影響: `RETURNING` を必須にすると X3・L1・L2・N1 に約 2〜3 日、ソートの省略は約 0.4 日、NLJ は約 0.5 日。
- **[01-Q3] `\ds` と `\d シーケンス` を完了条件 5 に含める**（D01-4、D-24 の拡張）。仮決め: 実機（psql 17.11、`psql -E`）で SQL を採取済み（10 §6.1）で、必要な機能は M4 で作るものだけ。理由: シーケンスの互換の検証になる。変えたい場合の影響: 任意に戻しても作業は減らない（slt は残る）。
- **[01-Q4] `\d tbl` は M4 では動かず、動かすなら約 7 日の任意 WP**（R-18、R-19）。仮決め: 実施しない。理由: 実機の SQL 10 本のうち 3 本（行レベルセキュリティ、拡張統計、出版物）が M4 の型・関数で解析できず、`pg_collation` と空のカタログ表を作る担当が設計になかった。変えたい場合の影響: 空のカタログ表（C1b。約 1.0 日）、配列の最小実装（約 5 日。09 の範囲）、`regnamespace` と 2 関数（約 0.5 日）、結合（約 0.5 日）。M5 の `05-types-core.md` の配列と同時に行うのが自然。
- **[01-Q5] M4 の工数は 141.8 日、期間は約 21〜23 日**。仮決め: 並列度約 19 まで使う。理由: 11 §4。担当を絞ると期間が延びる（最長経路は変わらないが、並列度が下がると P0 の後の待ちが増える）。変えたい場合の影響: 担当数を半分にすると期間はおよそ 1.7 倍。
- **[01-Q6] M4 の着手は M3 の `dev` への統合後**（D01-5）。仮決め: A・S1・K・Z と B1・B2・T1〜T3 の新規ファイルだけは先に始める。P0 は待つ。理由: P0 が `eval.rs` と `session.rs` を作り直す。変えたい場合の影響: M3 の統合前に P0 を始めると、M3 の持ち主との衝突を解決する作業が増える（約 +1〜2 日と不確実性）。
