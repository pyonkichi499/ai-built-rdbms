# yuzhu M5 基本設計 07: FOREIGN KEY（FK）

この章は M5 の FOREIGN KEY の設計です。作るのは次の 5 つです。

1. DDL（`REFERENCES`、`FOREIGN KEY`、`ALTER TABLE ... ADD / DROP CONSTRAINT`）
2. カタログ（`pg_constraint` の contype `f`、`pg_depend`）
3. 参照整合性（RI）エンジン（`executor/ri/`。文の終わりにイベントを処理する）
4. 並行性（親の行に `FOR KEY SHARE`、Repeatable Read での `40001`）
5. 他の DDL（`DROP TABLE`、`TRUNCATE`）との連携

トリガー機構は作りません（00 D26）。PostgreSQL が内部トリガーと SPI で行うことを、executor に組み込んだ関数でやります。**SQL 文は生成しません。** B+Tree とヒープを直接引きます。

- 契約: `spec/design/m5/00-contracts.md`（以下「00」）。決定は D6・D26・D27、WAL とロックは 00 §3.4、エラー文言は 00 §3.8。**00 に従う。** 食い違いが必要なものは §11 に書いた。
- 前提の設計書: `m1.md`、`m2.md`、`m3.md`、`spec/design/m4/00-contracts.md`（以下「M4 契約」）。
- 調査資料（根拠）: `spec/research/m5-types-fk.md` §9、`m5-concurrency.md`（行ロック・MultiXact・RR）、`m3-mvcc.md`、`pg-compat-tools.md`（psql の `\d`、pgbench）。
- 略号は FK。決定は FK-D<n>、確認事項は M5-FK-Q<n>。
- PostgreSQL のソースは REL_17_STABLE。`PG:<path>` は `https://github.com/postgres/postgres/blob/REL_17_STABLE/<path>` の略。
- 根拠の記号: 【確認】PG17.11 に SQL を投げて確かめた（この章を書くときに実行した）、【記憶】過去の知識で未照合、【提案】yuzhu への推奨。「（未検証）」はどちらでも確かめていない。
- 担当の境界: **行ロックの実装（`lock_tuple`、MultiXact、`TmResult`）は 02 章**（RW）、**配列の型（`Datum::Array`）は 05 章**（TY）、リレーションロックは 01 章（LK）、TRUNCATE の本体は M4 と 03 章（VC-5）。この章はそれらを呼ぶ側として書く。

---

## 0. 決定（この章で扱う論点。調査間の食い違いも）

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| FK-D1 | イベントを積む時機と、検査の時機 | 行ごとに即時検査／行ごとに積み、文の終わりに検査（`m5-types-fk` §9.2.1。PG と同じ） | **積むのは行の書き込み直後、検査と action の実行は文の終わり**。ただし「検査が要らないと確定しているもの」（キーが NULL、キーが変わらない UPDATE）は積む時点で捨てる。**MATCH FULL の NULL 混在は積んで、文の終わりにエラーにする** | 【確認】自己参照の `INSERT INTO s VALUES (2,1),(1,NULL)` が通る。ほかのエラー（CHECK、一意違反）より FK のエラーが後に報告されるのも PG と同じ |
| FK-D2 | キューの処理順 | PG と同じ（行ごとに積んだ順。1 行につき被参照側の action、参照側の check の順）／制約の OID 順（`m5-types-fk` §9.2.2） | **行ごとに「被参照側（この表を参照する FK）の全制約を OID 昇順 → 参照側（この表が持つ FK）の全制約を OID 昇順」の順に積み、FIFO で処理する** | PG のトリガー名は `RI_ConstraintTrigger_a_<oid>`（被参照側）が `_c_<oid>`（参照側）より前に並ぶ（【確認】`pg_trigger` の名前）。調査の「OID 順」は同じ側の中の順として採る |
| FK-D3 | CASCADE などの連鎖の処理 | 再帰（PG は入れ子の問い合わせの終わりにその問い合わせのイベントを処理するので深さ優先に近い。未検証）／同じキューの末尾に足して空になるまで回す（`m5-types-fk` §9.2.1） | **同じキューの末尾に足して回す（幅優先）。再帰しない** | 【確認】PG17 は 30000 段の自己参照 CASCADE を通す。再帰だと Rust のスタックが尽きる。結果の行は変わらず、**複数の違反が同時にあるときの報告順だけが PG と変わりうる**（既知の差。共有テストに入れない） |
| FK-D4 | 検査の方式 | SQL を作って SPI で実行（PG）／B+Tree とヒープを直接引く（指示・`m5-types-fk` §9.2） | **直接引く**。action の子の書き込みは `executor/dml.rs` の 3 関数（§4.5）を通し、NOT NULL・CHECK・インデックス更新・さらに先の FK イベントを通常の DML と同じ経路にする | トリガーと SPI を作らない（D26）。計画を作る・キャッシュする仕組みが要らない |
| FK-D5 | 検査のスナップショット | 一律に最新のスナップショット + 40001（`m5-types-fk` §9.5）／PG の `ri_PerformCheck`（`detectNewRows`）と同じ | **PG と同じ。Read Committed は検査のたびに取り直した最新のスナップショット。Repeatable Read は、(a) 子の挿入・更新の親の検索だけトランザクションのスナップショット、(b) 親側の検査と action は最新のスナップショット + 書き込みの crosscheck（トランザクションのスナップショットで見えない行に当たったら 40001）** | 【確認】4 つの実機シナリオ（§5.7）。調査の「検査は一律に最新」は、(a) で親が後からコミットされたときに PG が 23503 を返す挙動と合わない |
| FK-D6 | 行ロック | `FOR KEY SHARE`（D27）。D6 の簡易版 MultiXact（Q-014） | **親の行（子の検査）と子の行（親側の検査）に `lock_tuple(KeyShare)`。D6 の簡易版の限界はそのまま受け入れ、§5.8 に差を列挙する** | 00 D6・D27 |
| FK-D7 | `DEFERRABLE` と `INITIALLY` | 依頼文は「`DEFERRABLE` と `INITIALLY` は 0A000（NOT DEFERRABLE のみ）」／調査 §9.4 は `INITIALLY IMMEDIATE` を受け付ける | **`DEFERRABLE` と `INITIALLY DEFERRED` は 0A000。`NOT DEFERRABLE` と `INITIALLY IMMEDIATE` は受け付けて既定と同じに扱う**。`NOT DEFERRABLE INITIALLY DEFERRED` は PG と同じ 42601 | `INITIALLY IMMEDIATE` は既定と同じ意味で、ツールが出力することがある。拒否する理由がない（M5-FK-Q1） |
| FK-D8 | `NOT VALID` / `VALIDATE CONSTRAINT` / `SET CONSTRAINTS` | 対応／0A000 | **0A000**（`ALTER TABLE ... ADD ... NOT VALID` を含む） | `convalidated` は常に `t` |
| FK-D9 | `pg_depend` の行 | M4 契約 §11.6 の「制約 → 表 `a`」／調査 §9.2 の「制約 → 親の表、親の一意インデックス、子の表」／PG17 の実機 | **PG17 の実機と同じ（§3.2）: 制約 → 子の列ごと `a`、制約 → 親の一意インデックス `n`、制約 → 親の列ごと `n`** | 【確認】実機。`2BP01` の DETAIL の主語（`depends on table p` / `depends on index p_pkey`）がこの形から決まる。M4 契約 §11.6 の PK / UNIQUE の行は M4 の持ち物 |
| FK-D10 | 自動命名の衝突範囲 | CHECK と同じ規則（`m5-types-fk` §9.3。M2 の実装は 1 回の CREATE TABLE の中だけ見る）／PG（名前空間全体） | **PG と同じ名前空間全体**。明示名の重複は同じ表の中だけ（42710） | 【確認】`a(b_c)` と `a_b(c)` の自動名が `a_b_c_fkey` と `a_b_c_fkey1` になる。明示名 `shared_name` は 2 つの表に付けられる |
| FK-D11 | `SET DEFAULT` の後の再検査 | しない／PG の `ri_set` と同じく NO ACTION の検査をもう一度 | **する** | 【確認】既定値が消した親のキーと同じだと、更新後の子が同じキーのままになる。子の検査は「キーが変わらない」ので走らない。PG はこのとき親側のメッセージで失敗する |
| FK-D12 | 子の行の自動インデックス | 作る／作らない | **作らない（PG と同じ）**。子のインデックスは、先頭の列の集合が FK の列と一致するものがあれば使い、なければ全件走査 | 【確認】docs 5.5.5。使えるインデックスの条件は §6.4 |
| FK-D13 | `pg_trigger` と `relhastriggers` | 行を出す／出さない（D26） | **行を出さない。`pg_class.relhastriggers` も `f` のまま** | 【確認】psql の `\d` は `relhastriggers` が `t` のときだけ `pg_trigger` を引く。FK の表示は `pg_constraint` から作る。差は既知の差に載せる |
| FK-D14 | エラーの `CONTEXT` | SQL 文を載せる（PG）／載せない | **載せない** | SQL を作らないので載せる文がない。共有テストは文言を正規表現で見るので影響しない |
| FK-D15 | 子と親の列の型の互換 | PG の規則（演算子族のクロスタイプ演算子、なければ子から親への暗黙キャスト）／yuzhu の簡約 | **簡約（§6.2）。同じ型、整数どうし、子から親への暗黙キャストのどれか。それ以外は 42804** | 【確認】`bigint` → `int`、`int` → `numeric`、`text` → `varchar` は通り、`numeric` → `int` は 42804。`timestamp` の子が `date` の親を参照するような演算子族のクロスタイプだけの組は yuzhu では 42804（既知の差） |
| FK-D16 | `ALTER TABLE ... DROP CONSTRAINT` の範囲 | FK だけ／CHECK・PK・UNIQUE も | **FK だけを消す。PK / UNIQUE / CHECK は、FK が依存していれば PG と同じ 2BP01、依存がなければ 0A000** | 00 §1.1 は ALTER の大半を M6 とした。M4 は ADD PRIMARY KEY / UNIQUE だけ |
| FK-D17 | RI が使う関係の開き方 | `ExecCtx.catalog`（文の最初のカタログ用スナップショット）／ロックの後に取り直す | **`RiEnv::open_relation`（§4.4）。リレーションロックを取った後に、カタログ用スナップショットを取り直して読む** | ロックを待っている間にコミットされた DDL（`TRUNCATE` の新しい relfilenode、`CREATE INDEX` の新しいインデックス）を見逃すと、更新したタプルがインデックスから漏れる |
| FK-D18 | TRUNCATE | D42「参照されている表は 0A000（CASCADE も 0A000）」／PG（同じ TRUNCATE に子を並べれば通る。CASCADE は子も切り詰める） | **PG と同じく、参照している表がすべて同じ TRUNCATE の対象に入っていれば通す。それ以外と `CASCADE` は 0A000** | 【確認】`TRUNCATE p, c` は通る。D42 の「拒否」は、入っていない場合の拒否として読む（M5-FK-Q2） |

---

## 1. 範囲

### 1.1 対応するもの

| 分類 | 内容 |
|---|---|
| 構文 | 列制約 `REFERENCES t [(col)]`、表制約 `[CONSTRAINT n] FOREIGN KEY (cols) REFERENCES t [(cols)]`、`MATCH SIMPLE \| FULL`、`ON DELETE` / `ON UPDATE` の 5 つの action（`NO ACTION`、`RESTRICT`、`CASCADE`、`SET NULL`、`SET DEFAULT`）、`ON DELETE SET NULL (cols)` / `SET DEFAULT (cols)`、`NOT DEFERRABLE`、`INITIALLY IMMEDIATE` |
| DDL | `CREATE TABLE` の中の FK（自己参照を含む）、`ALTER TABLE t ADD [CONSTRAINT n] FOREIGN KEY ...`（既存の行を全件検査）、`ALTER TABLE t DROP CONSTRAINT [IF EXISTS] n [CASCADE \| RESTRICT]`（FK だけ。FK-D16） |
| カタログ | `pg_constraint`（contype `f`）、`pg_depend`、`pg_get_constraintdef` の FK 形式、`psql \d` の「Foreign-key constraints」「Referenced by」 |
| 検査 | 子の INSERT / UPDATE（親があるか）、親の DELETE / UPDATE（子が残っていないか、または action）、MATCH FULL と NULL、自己参照、複数列、複数の FK |
| 並行性 | 親・子の行の `FOR KEY SHARE`、ロック待ち、Read Committed と Repeatable Read（40001） |
| 他の DDL | `DROP TABLE` と `DROP INDEX` と `DROP CONSTRAINT`（PK / UNIQUE）の 2BP01、`CASCADE` での FK 制約の削除、`TRUNCATE` の拒否 |

### 1.2 対応しない（実行すると PostgreSQL にない 0A000 を返す。黙って無視しない）

| 構文 | SQLSTATE | メッセージ |
|---|---|---|
| `DEFERRABLE` | 0A000 | `DEFERRABLE constraints are not supported` |
| `INITIALLY DEFERRED` | 0A000 | `INITIALLY DEFERRED constraints are not supported` |
| `ALTER TABLE ... ADD ... FOREIGN KEY ... NOT VALID` | 0A000 | `NOT VALID constraints are not supported` |
| `ALTER TABLE ... VALIDATE CONSTRAINT`、`SET CONSTRAINTS` | 0A000 | `VALIDATE CONSTRAINT is not supported`、`SET CONSTRAINTS is not supported` |
| `TRUNCATE ... CASCADE`（と参照している表を含まない TRUNCATE） | 0A000 | §6.7 |
| `ALTER TABLE ... DROP CONSTRAINT`（FK 以外で、FK の依存がないもの） | 0A000 | `DROP CONSTRAINT is supported only for foreign key constraints` |

`MATCH PARTIAL` は PostgreSQL も 0A000（`MATCH PARTIAL not yet implemented`。【確認】）。そのまま同じ文言で返す。`DEFERRABLE` 系の文言は M4 の PK / UNIQUE の `DEFERRABLE` と**同じ文言**にする（M4 の 07 章と突き合わせる。M5-FK-Q1）。

### 1.3 保証すること

- **カタログとエラーが PostgreSQL 17 と一致する**: `pg_constraint` の全列、`pg_get_constraintdef` の出力、23503 / 42830 / 42804 などの SQLSTATE・メッセージ・DETAIL、ErrorResponse の `s`（schema）`t`（table）`n`（constraint）フィールド。
- **検査の時機が PG と同じ**: 文の終わり。文の中で一時的に壊れていても、終わりで整っていれば通る。
- **文の原子性**: CASCADE などの途中で失敗したら、文全体が無かったことになる（M3 の文単位の中断。FK 固有の WAL は無い。§5.10）。
- **クラッシュ安全**: FK の書き込みはすべて通常のヒープ / B+Tree の WAL とコミットレコードで守られる。FK 専用の WAL レコードは作らない。

### 1.4 保証しないこと（既知の差。10 章が集める）

1. `pg_trigger` に行が出ない。`relhastriggers` が `f`（FK-D13）。
2. エラーの `CONTEXT: SQL statement "..."` 行が無い（FK-D14）。
3. D6 の簡易版により、**子の INSERT を済ませた（親の行に `KeyShare` を持つ）トランザクションがあると、別のトランザクションによる親の非キー列の UPDATE が待たされる**。PG は待たない（【確認】§5.8）。共有テストと分離性テストの期待ファイルには、この差が出るケースを入れない。
4. 同じ親の行に `KeyShare` を持つ複数のトランザクションが、それぞれその親の非キー列を UPDATE するとデッドロック（40P01）になりうる。PG は通る。
5. 複数の違反が同時にあるときに、どの違反が先に報告されるか（FK-D3）。
6. 子と親の型の組のうち、PG が演算子族のクロスタイプ演算子でだけ許す組（FK-D15）。
7. `TRUNCATE ... CASCADE` が 0A000（FK-D18）。

---

## 2. 構成

`★` は M5 で新規、`△` は M5 での変更。持ち主は 00 §8 のとおり FK。他の章のファイルを直す必要があるものは §11 に依頼として書いた。

```
impl/rust/crates/yuzhu-core/src/
├── sql/
│   ├── ast.rs                        △ ForeignKeySpec、FkActionSpec、列制約・表制約・ALTER の変種（§4.1）
│   └── parser/fk.rs                  ★ REFERENCES 句、FOREIGN KEY 表制約、ALTER の ADD / DROP CONSTRAINT（FK の部分）
├── catalog/
│   ├── fk.rs                         ★ ForeignKeyDef、FkAction、FkMatch、FkEqOps、pg_get_constraintdef の FK 形式（§4.2）
│   └── store_fk.rs                   ★ impl CatalogStore: FK の行の読み書き（pg_constraint と pg_depend）
├── analyzer/ddl_ext/foreign_key.rs   ★ bind_foreign_key、BoundForeignKey、BoundAddForeignKey、BoundDropConstraint（§4.3）
├── ddl/constraint.rs                 △ FK の作成・ALTER ADD / DROP・初期検査・DROP / TRUNCATE 連携（§6.6〜§6.8）
├── executor/ri/
│   ├── mod.rs                        ★ フック（after_insert / after_update / after_delete / finish_statement）、定数、drain のループ
│   ├── queue.rs                      ★ RiQueue、RiEvent、積むときの絞り込み
│   ├── keys.rs                       ★ キーの取り出し、NULL の分類、画像の等価、RiKeyCmp、プローブの作成
│   ├── rel.rs                        ★ RiEnv、RiRel、FkPlan（制約ごとの索引・比較方法の決定）
│   ├── check.rs                      ★ 子の検査（CheckParent）、親側の検査（NO ACTION / RESTRICT）、ロック付きの検索、違反のエラー
│   └── action.rs                     ★ CASCADE / SET NULL / SET DEFAULT
└── （依頼先）storage/mod.rs の RelHandle に `ri`、catalog の TableDef に FK の 2 つの一覧、
    executor/dml.rs の 3 関数、session の RiQueue の組み立て（§11）
```

**依存の方向**（00 §2 の図のとおり）: `executor::ri` は `TableStore`・`IndexStore`・`LockManager`・`catalog::fk` を使う。ヒープのページには触れない。`ddl::constraint` は `catalog::store_fk` と `executor::ri::check` の検索関数（初期検査）を使う。`executor::ri` は `session` を知らない。session が実装する `RiEnv`（§4.4）を通して、スナップショットとカタログを受け取る。

**この章の規約**:

1. RI の関数は、リレーションロックの待ち・行ロックの待ちに入る前に、ページのラッチもバッファのピンも持たない（00 §2 規約 1）。検索は「候補の TID を集めてから、1 件ずつロックする」順にする。`HeapScan` と `IndexScan` はピンを持ち越さないので、走査の途中でロックしてもよいが、デバッグビルドでは `assert_no_pins` を待つ前に呼ぶ。
2. RI の待ちは必ず `ctx.wait`（`WaitCtx`）を渡す（規約 2）。
3. xmax と infomask を直接触らない。行ロックは `TableStore::lock_tuple` だけで行う（規約 3）。
4. FK のコードは `Datum` を比較するときに `cmp_datum` と §6.3 の `images_equal` だけを使う。型ごとの特別扱いを書かない。

---

## 3. カタログ上の形式

FK 専用のディスク形式は無い。FK を表すのは **`pg_constraint` と `pg_depend` の行**だけで、通常のカタログのヒープの形式（M2 §3）と、配列の形式（05 章。00 §3.7）に従う。

### 3.1 `pg_constraint` の行（contype `f`）

列は M2 が作った `PG_CONSTRAINT_COLUMNS` のとおり（`catalog/schema.rs`。PG17 と同じ 26 列）。FK の行の値は次のとおり。

| 列 | 値 |
|---|---|
| `oid` | カウンタから採る（16384 以上） |
| `conname` | 制約名（§6.5 の規則） |
| `connamespace` | **子の表**の名前空間の OID |
| `contype` | `'f'` |
| `condeferrable` / `condeferred` | 常に `false` / `false` |
| `convalidated` | 常に `true` |
| `conrelid` | 子の表の OID |
| `contypid` | 0 |
| `conindid` | 参照先の**一意インデックス**の OID（PK なら PK のインデックス） |
| `conparentid` | 0 |
| `confrelid` | 親の表の OID |
| `confupdtype` / `confdeltype` | `'a'` NO ACTION、`'r'` RESTRICT、`'c'` CASCADE、`'n'` SET NULL、`'d'` SET DEFAULT |
| `confmatchtype` | `'s'` SIMPLE、`'f'` FULL（`'p'` は作れない） |
| `conislocal` | `true` |
| `coninhcount` | 0 |
| `connoinherit` | `true`（【確認】PG17 の FK の行） |
| `conkey` | 子の列の attnum（`int2[]`。FK に書いた順） |
| `confkey` | 親の列の attnum（`int2[]`。FK に書いた順。**インデックスの列順とは限らない**） |
| `conpfeqop` / `conppeqop` / `conffeqop` | `oid[]`。列ごとの `=` 演算子（§3.3） |
| `confdelsetcols` | `ON DELETE SET NULL / SET DEFAULT (cols)` の列の attnum（`int2[]`）。指定がなければ NULL |
| `conexclop` / `conbin` | NULL |

【確認】PG17 で `create table c(id int primary key, pid int references p(id))` の行は次のとおり。

```text
conname=c_pid_fkey contype=f condeferrable=f condeferred=f convalidated=t conrelid=c conindid=p_pkey confrelid=p
confupdtype=a confdeltype=a confmatchtype=s conislocal=t coninhcount=0 connoinherit=t
conkey={2} confkey={1} conpfeqop={96} conppeqop={96} conffeqop={96} confdelsetcols=NULL conparentid=0
```

配列の列は 05 章の `Datum::Array(Box<ArrayValue>)`（00 §4.9）で書く。`conkey` は `ArrayValue { elem_type: 21 /* int2 */, dims: [ArrayDim { len: n, lbound: 1 }], items: [Datum::Int2(..), ..] }`、`conpfeqop` は `elem_type: 26 /* oid */` で `Datum::Oid(..)`。`store_fk.rs` が 2 つの小さな構築関数（`int2_array(&[i16])`、`oid_array(&[Oid])`）を持つ。

### 3.2 `pg_depend` の行

【確認】PG17 の実機（FK-D9）。制約 1 つにつき次の行を書く。`classid` = `pg_constraint`（2606）、`refclassid` = `pg_class`（1259）、`objsubid` = 0。

| `refobjid` | `refobjsubid` | `deptype` | 行数 |
|---|---|---|---|
| 子の表 | `conkey` の各 attnum | `a`（AUTO） | 列の数 |
| 参照先の一意インデックス（`conindid`） | 0 | `n`（NORMAL） | 1 |
| 親の表 | `confkey` の各 attnum | `n` | 列の数 |

例: `create table p(id int primary key); create table c(id int primary key, pid int references p(id));` の `c_pid_fkey`（OID は例）。

```text
p 16384、p_pkey（インデックス）16385、c 16387、c_pkey 16388、c_pid_fkey（制約）16390

classid objid  objsubid refclassid refobjid refobjsubid deptype
2606    16390  0        1259       16387    2            a      -- 子の列 pid
2606    16390  0        1259       16385    0            n      -- 親の一意インデックス p_pkey
2606    16390  0        1259       16384    1            n      -- 親の列 id
```

- 自己参照（`create table s(id int primary key, parent int references s(id))`）も同じ 3 種類の行を書く（子と親が同じ表）。
- 意味: 子の表（またはその列）を消すと制約は一緒に消える。親の表・親の列・親の一意インデックスを消そうとすると、制約が依存しているので 2BP01（§6.7）。

### 3.3 `conpfeqop` / `conppeqop` / `conffeqop` の決め方

列の組ごとに、親の列の型を P、子の列の型を F、親の一意インデックスの演算子族を O とする。

| 配列 | 意味 | 決め方 |
|---|---|---|
| `conpfeqop` | 親の値 = 子の値 | O に（P, F）の `=` があればそれ。なければ（P, P）の `=` |
| `conppeqop` | 親の値 = 親の値 | O の（P, P）の `=` |
| `conffeqop` | 子の値 = 子の値 | O に（P, F）があるとき（F, F）の `=`。なければ（P, P）の `=`（子を親の型にキャストして比べるから） |

【確認】PG17: `bigint` の子が `int` の親を参照: `pf = 15（int48eq）`、`pp = 96（int4eq）`、`ff = 410（int8eq）`。`int` の子が `bigint` の親を参照: `pf = 416（int84eq）`、`pp = 410`、`ff = 96`。`text` の子が `varchar` の親を参照: 3 つとも `98（texteq）`。`int` の子が `numeric` の親を参照: 3 つとも `1752（numeric_eq）`。同じ型どうしは 3 つとも同じ（`int4` なら 96）。

`catalog/opclass.rs` の `AMOPS`（M4 契約 §11.3）から引く。M4 の `integer_ops` にクロスタイプの `=`（`int24eq`、`int42eq`、`int28eq`、`int82eq`、`int48eq`、`int84eq`）が入っているか、OID が PG17 の `pg_operator.dat` と一致するかは（未検証）。FK-1 の最初の作業で確かめ、足りなければ 07 章（M4）の持ち主に追加を依頼する。

### 3.4 `pg_get_constraintdef` の FK 形式

【確認】PG17。`pg_get_constraintdef(oid[, pretty])` の contype `f` の出力（pretty の有無で変わらない）。

```text
FOREIGN KEY (<子の列>, ...) REFERENCES <親>(<親の列>, ...)[ MATCH FULL][ ON UPDATE <action>][ ON DELETE <action>[ (<列>, ...)]]
```

- `<親>` は検索パスで見える名前空間なら修飾しない。見えなければ `schema.table`（M4 の `CatalogReader::relation_name` と同じ規則。【確認】`REFERENCES s9.q(id)` と、`search_path = s9` のときの `REFERENCES q(id)`）。識別子は必要なときだけ二重引用符で囲む。
- **`ON UPDATE` が先、`ON DELETE` が後**。`NO ACTION` は出さない。`MATCH SIMPLE` は出さない。
- action の綴りは `RESTRICT`、`CASCADE`、`SET NULL`、`SET DEFAULT`。`confdelsetcols` があれば `ON DELETE` の action の後に ` (b)` のように付ける（【確認】`ON DELETE SET NULL (b)`）。
- 例（【確認】）:

```text
FOREIGN KEY (pid) REFERENCES p(id)
FOREIGN KEY (a, b) REFERENCES p(id, k) MATCH FULL ON UPDATE SET NULL ON DELETE CASCADE
FOREIGN KEY (a, b) REFERENCES p(id, k) ON UPDATE SET DEFAULT ON DELETE SET NULL (b)
FOREIGN KEY (v) REFERENCES p(v) ON DELETE RESTRICT
FOREIGN KEY (b, a) REFERENCES p(k, id)
```

- `DEFERRABLE` / `INITIALLY DEFERRED` / `NOT VALID` の接尾辞は、M5 では作れないので出力しない（出力する分岐は書くが到達しない）。
- この関数は `catalog/fk.rs` の `fk_constraint_def` として FK が持ち、M4 の `pg_get_constraintdef`（`deparse/`）が contype `f` のときに呼ぶ（§11）。

---

## 4. 共通の型（契約）

00 §4.6 は `ExecCtx.ri: &'a mut RiQueue` と、`RiQueue` とイベントの型を FK-2 が決めることだけを固定した。この章はそれを具体化する。00 のほかの署名は変えない。

### 4.1 AST（`sql/ast.rs`、`sql/parser/fk.rs`）

```rust
/// REFERENCES 句と FOREIGN KEY 制約の構文。列制約では columns は 1 つ（その列）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeySpec {
    pub name: Option<Ident>,             // CONSTRAINT n
    pub columns: Vec<Ident>,             // 子の列。列制約では暗黙に 1 列
    pub ref_table: ObjectName,           // 親（スキーマ修飾可）
    pub ref_columns: Vec<Ident>,         // 空 = 親の PRIMARY KEY
    pub match_type: FkMatchSpec,
    pub on_update: FkActionSpec,
    pub on_delete: FkActionSpec,
    pub deferrable: Option<bool>,        // 書かれていれば Some（Some(true) と initially_deferred は解析後に 0A000）
    pub initially_deferred: Option<bool>,
    pub not_valid: bool,                 // ALTER の ADD だけ
    pub span: Span,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FkMatchSpec { Simple, Full, Partial }
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FkActionSpec { NoAction, Restrict, Cascade, SetNull(Vec<Ident>), SetDefault(Vec<Ident>) }
```

- 列制約: `ColumnConstraintKind::References(ForeignKeySpec)`。表制約: `TableConstraintKind::ForeignKey(ForeignKeySpec)`。ALTER: `AlterTableAction::AddConstraint(TableConstraint)`（M4 が PK / UNIQUE 用に持つ変種に FK を載せる）、`AlterTableAction::DropConstraint { name: Ident, if_exists: bool, cascade: bool }`（FK-4 が足す）。`SET CONSTRAINTS`・`VALIDATE CONSTRAINT` の構文は、ALTER のパーサが 0A000 にする。
- **文法の順序は PG と同じ固定**（【確認】）: `REFERENCES t [(cols)]` の後に、`MATCH ...`、次に `ON UPDATE` / `ON DELETE`（それぞれ高々 1 回、順序は自由）、最後に `[NOT] DEFERRABLE` / `INITIALLY ...`。順序違い（`ON DELETE CASCADE MATCH FULL`）や重複（`ON DELETE ... ON DELETE ...`）は 42601 `syntax error at or near "match"` / `"delete"`。
- 列制約では、`NOT DEFERRABLE` の `NOT` と次の列制約 `NOT NULL` を区別するために 2 トークン先読みする。
- `ON UPDATE SET NULL (cols)` と `ON UPDATE SET DEFAULT (cols)` は解析時に 0A000 `a column list with SET NULL is only supported for ON DELETE actions`（`SET DEFAULT` のときは `SET DEFAULT`。【確認】SET NULL の文言と SQLSTATE。DEFAULT の文言は（未検証））。

### 4.2 カタログの型（`catalog/fk.rs`）

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FkAction { NoAction, Restrict, Cascade, SetNull, SetDefault }
impl FkAction {
    pub fn code(self) -> char;                         // 'a' 'r' 'c' 'n' 'd'
    pub fn from_code(c: char) -> Option<FkAction>;
    pub fn sql(self) -> &'static str;                  // "NO ACTION" "RESTRICT" "CASCADE" "SET NULL" "SET DEFAULT"
}
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FkMatch { Simple, Full }                      // 's' 'f'

/// 列ごとの比較演算子（pg_operator.oid）。§3.3
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FkEqOps { pub pf: Oid, pub pp: Oid, pub ff: Oid }

/// pg_constraint の 1 行（contype 'f'）の実行用の写し
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ForeignKeyDef {
    pub oid: Oid,
    pub name: String,
    pub child: Oid,                    // conrelid
    pub parent: Oid,                   // confrelid
    pub parent_index: Oid,             // conindid
    pub child_cols: Vec<i16>,          // conkey（attnum）
    pub parent_cols: Vec<i16>,         // confkey（attnum）
    pub match_type: FkMatch,
    pub on_update: FkAction,
    pub on_delete: FkAction,
    /// ON DELETE SET NULL / SET DEFAULT (cols)。空 = FK の全列。attnum は子の表のもの
    pub set_cols: Vec<i16>,
    pub eq_ops: Vec<FkEqOps>,
}
impl ForeignKeyDef {
    pub fn is_self_referencing(&self) -> bool { self.child == self.parent }
}

/// DROP / TRUNCATE の説明と pg_get_constraintdef 用
pub fn fk_constraint_def(fk: &ForeignKeyDef, names: &dyn FkNames) -> String;
pub trait FkNames {
    fn column_name(&self, table: Oid, attnum: i16) -> Option<String>;
    fn relation_name(&self, table: Oid) -> Option<String>;       // 検索パスに応じた修飾（relation_name と同じ規則）
}
```

`TableDef`（M4 §11.1）に 2 つの一覧を足し、`RelHandle` に実行用の写しを持たせる（§11 の依頼）。

```rust
// catalog/mod.rs（依頼。M4 の TableDef に足す。kind = Table のときだけ入る。どちらも OID 昇順）
pub struct TableDef { /* … */
    pub foreign_keys: Vec<Arc<ForeignKeyDef>>,     // child = この表
    pub referenced_by: Vec<Arc<ForeignKeyDef>>,    // parent = この表（自己参照はどちらにも入る）
}
// storage/mod.rs（依頼。RelHandle::from_table が作る）
pub struct RiRelInfo {
    pub outgoing: Arc<[Arc<ForeignKeyDef>]>,       // = TableDef.foreign_keys（この表が子）
    pub incoming: Arc<[Arc<ForeignKeyDef>]>,       // = TableDef.referenced_by（この表が親）
}
impl RiRelInfo { pub fn is_empty(&self) -> bool; }
pub struct RelHandle { /* … */ pub ri: Arc<RiRelInfo> }
```

`CatalogReader` に 3 つのメソッドを足す（既定実装つき。実装本体は `catalog/store_fk.rs`。M4 の `reader.rs` には委譲の 3 行だけを F0 が足す。§11）。

```rust
pub trait CatalogReader {
    /// 子（conrelid）が table の FK。OID 昇順
    fn foreign_keys_of(&self, table: Oid) -> Result<Vec<Arc<ForeignKeyDef>>> { Ok(Vec::new()) }
    /// 親（confrelid）が table の FK。OID 昇順
    fn foreign_keys_referencing(&self, table: Oid) -> Result<Vec<Arc<ForeignKeyDef>>> { Ok(Vec::new()) }
    /// 名前空間の中に、この名前の制約があるか（自動命名の衝突回避。FK-D10）
    fn constraint_name_exists(&self, namespace: Oid, name: &str) -> Result<bool> { Ok(false) }
}
```

`CatalogStore` の書き込み（`catalog/store_fk.rs`。M2 の `create_table` などの引数の形に倣う）。

```rust
impl CatalogStore {
    /// pg_constraint の 1 行と pg_depend の行（§3.2）を書く。呼び出し側が WriteCtx とスナップショットを渡す
    pub fn insert_foreign_key(&self, w: &WriteCtx, snap: &Snapshot, def: &ForeignKeyDef) -> Result<()>;
    /// pg_constraint の行と、objid = def.oid の pg_depend の行を消す
    pub fn delete_foreign_key(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid) -> Result<()>;
    /// DROP の第 2 段（01 LK-D16・§4.6 `RelationResolver::drop_peers`）が使う。`rel` と FK でつながる相手の表の OID（`rel` が参照する表と `rel` を参照する表。
    /// `constraint` が Some ならその制約の相手だけ。重複なし・昇順）。01 §11-13 の依頼を受け入れた（レビュー対応 R-37）
    pub fn fk_peer_tables(&self, snap: &Snapshot, rel: Oid, constraint: Option<&str>) -> Result<Vec<Oid>>;
}
```

### 4.3 Bound（`analyzer/ddl_ext/foreign_key.rs`。`BoundCreateTable` と `BoundDdl` は 00 §4.7 のとおり）

```rust
/// 解析済みの FK（CREATE TABLE と ALTER の両方）。名前は解析で決める（M4 の BoundIndexConstraint と同じ）
#[derive(Clone, Debug)]
pub struct BoundForeignKey {
    pub name: String,
    pub child_cols: Vec<i16>,                // 子の attnum（CREATE TABLE では列の並び順 + 1）
    pub parent: BoundFkParent,
    pub parent_cols: Vec<i16>,               // 親の attnum。空にならない（PK の省略は解析で解決）
    pub match_type: FkMatch,
    pub on_update: FkAction,
    pub on_delete: FkAction,
    pub set_cols: Vec<i16>,                  // 子の attnum。ON DELETE SET ... (cols)
    pub eq_ops: Vec<FkEqOps>,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub enum BoundFkParent {
    /// 既存の表。index は FK が使う一意インデックス（FK-1 の規則で解析時に決める）
    Existing { oid: Oid, schema: String, name: String, index: Oid },
    /// 作成中の表自身。index_constraint は BoundCreateTable.constraints の添字（その PK / UNIQUE のインデックスを使う）
    SelfTable { index_constraint: usize },
}

// BoundCreateTable に foreign_keys: Vec<BoundForeignKey> を足す（00 §4.7）
pub struct BoundAddForeignKey {
    pub child_oid: Oid, pub child_schema: String, pub child_name: String,
    pub fk: BoundForeignKey,
}
pub struct BoundDropConstraint {
    pub table_oid: Oid, pub schema: String, pub table: String,
    pub name: String, pub if_exists: bool, pub cascade: bool,
}

/// 子の対象。CREATE TABLE では、作成中の列と PK / UNIQUE の一覧から解決する
pub enum FkChild<'a> {
    Existing(&'a TableDef),
    New { schema: &'a str, name: &'a str, columns: &'a [ColumnDef], index_constraints: &'a [BoundIndexConstraint] },
}
/// taken: この文の中ですでに決まった制約名（CHECK・PK / UNIQUE・ほかの FK）。自動命名の衝突回避に使う
pub fn bind_foreign_key(catalog: &dyn CatalogReader, child: FkChild<'_>, spec: &ForeignKeySpec, taken: &mut Vec<String>) -> Result<BoundForeignKey>;
```

### 4.4 RI エンジンの型（`executor/ri/`）

```rust
// executor/ri/queue.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RiOp { Delete, Update }

#[derive(Clone, Debug)]
pub enum RiEvent {
    /// 子の行（INSERT、またはキーが変わった UPDATE）に親があるか。key は子の列の値（child_cols の順）。
    /// child_tid は積んだ時点の新しい版。処理するとき、その版がまだ生きているか確かめる（PG の SnapshotSelf の確認）
    CheckParent { fk: Arc<ForeignKeyDef>, child_tid: Tid, key: Vec<Datum> },
    /// 親のキーが無くなった（DELETE、またはキーが変わった UPDATE）。old_key は親の列の値（parent_cols の順）、
    /// new_key は UPDATE のときだけ（CASCADE と SET NULL / SET DEFAULT の更新に使う）
    ParentKeyGone { fk: Arc<ForeignKeyDef>, op: RiOp, old_key: Vec<Datum>, new_key: Option<Vec<Datum>> },
}

/// 1 文の間のイベントキューと、RI が開いた関係の覚え。session が文ごとに作る（00 §4.6 の ExecCtx.ri）
#[derive(Debug)]
pub struct RiQueue { /* events: VecDeque<RiEvent>、env: Arc<dyn RiEnv>、rels: HashMap<Oid, Arc<RiRel>>、
                       plans: HashMap<Oid, Arc<FkPlan>>、locked: HashSet<(Oid, LockMode)>、
                       verified: HashSet<(Oid, HashKey)>、draining: bool、crosscheck: Option<Snapshot>、
                       mem_charged: usize、processed: u64 */ }
impl RiQueue {
    pub fn new(env: Arc<dyn RiEnv>) -> RiQueue;
    pub fn is_empty(&self) -> bool;
    pub fn pending(&self) -> usize;
    /// action の書き込みが storage.update / delete に渡す crosscheck。Repeatable Read の action の間だけ Some（§5.7）。
    /// executor/dml.rs の delete_row / update_with_indexes がこれを読む（§4.5）
    pub fn crosscheck(&self) -> Option<&Snapshot>;
}

// executor/ri/rel.rs
/// RI が必要とするもの。session が実装して RiQueue::new に渡す（executor は session を use しない）
pub trait RiEnv: Send + Sync + std::fmt::Debug {
    /// リレーションロックを取った後に呼ぶ（FK-D17）。カタログ用スナップショットを取り直して最新の定義を読み、
    /// RelHandle・NOT NULL・CHECK・DEFAULT の実行用の式を作って返す。実装は INSERT の計画が使う関数を呼ぶ
    fn open_relation(&self, oid: Oid) -> Result<RiRel>;
    /// 最新のスナップショットを取る（登録される。Drop で外れる）。own は自分の XID、curcid は現在のコマンド ID
    fn latest_snapshot(&self, own: Option<Xid>, curcid: CommandId) -> RegisteredSnapshot;
}
#[derive(Debug)]
pub struct RiRel {
    pub def: Arc<TableDef>,
    pub rel: RelHandle,
    pub not_null: Vec<bool>,
    pub checks: Vec<PhysCheck>,
    pub defaults: Vec<Option<PhysExpr>>,       // 列ごとの DEFAULT（式を持たない列は None）
}

/// 制約ごとの検索の計画。初めて使うときに作ってキューに覚える
#[derive(Debug)]
pub struct FkPlan {
    pub fk: Arc<ForeignKeyDef>,
    pub child: Arc<RiRel>,
    pub parent: Arc<RiRel>,
    pub cmp: Vec<RiKeyCmp>,                    // 列の組ごと
    pub parent_index: IndexHandle,
    /// 親のインデックスの列の位置 i に入れるプローブは、FK のキーの何番目か（FK の列順とインデックスの列順の違いを吸収）
    pub parent_perm: Vec<usize>,
    /// 子で使えるインデックス（FK-D12）。None なら全件走査
    pub child_index: Option<(IndexHandle, Vec<usize>)>,
}

// executor/ri/keys.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RiKeyCmp {
    /// 子と親が同じ型（Datum の変種が同じ）。cmp_datum でそのまま比べる
    Same,
    /// 子と親がどちらも int2 / int4 / int8。cmp_datum が幅の違いを吸収する
    IntCross,
    /// 子の値を親の型へ暗黙キャストしてから比べる（int → numeric、bpchar → text など）。索引を使えない（子の索引を引く向きのとき）
    CastChild { from: SqlType, to: SqlType },
}
pub fn classify_pair(child: SqlType, parent: SqlType) -> Option<RiKeyCmp>;     // None = 42804
pub fn extract_key(row: &[Datum], cols: &[i16]) -> Vec<Datum>;                 // attnum - 1 で引く
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NullClass { NoneNull, AllNull, Mixed }
pub fn null_class(key: &[Datum]) -> NullClass;
/// PG の datum_image_eq 相当。値が cmp_datum で等しくても、表現が違えば false（numeric の 1.0 と 1.00、末尾空白の違う bpchar）。
/// float は to_bits で比べる
pub fn images_equal(a: &[Datum], b: &[Datum]) -> bool;
```

### 4.5 executor との契約（フック）と、RW への依頼

```rust
// executor/ri/mod.rs: 行の書き込みの直後に呼ぶフック。rel.ri.is_empty() ならすぐ戻る（FK の無い表は追加の負担がない）
pub fn after_insert(ctx: &mut ExecCtx<'_>, rel: &RelHandle, tid: Tid, new_row: &[Datum]) -> Result<()>;
pub fn after_update(ctx: &mut ExecCtx<'_>, rel: &RelHandle, old_tid: Tid, old_row: &[Datum], new_tid: Tid, new_row: &[Datum]) -> Result<()>;
pub fn after_delete(ctx: &mut ExecCtx<'_>, rel: &RelHandle, old_row: &[Datum]) -> Result<()>;
/// 文の終わりに 1 回。キューが空なら何もしない。drain 中に呼ばれたら何もしない（action の書き込みが積んだイベントは外側のループが処理する）
pub fn finish_statement(ctx: &mut ExecCtx<'_>) -> Result<()>;
```

呼び出しの責任は **`executor/dml.rs`（RW の持ち物）の 3 つの共有関数に集める**（INSERT・UPDATE・COPY・RI の action が同じ関数を通るので、呼び忘れが起きない）。00 / M4 契約の署名からの変更を §11 に依頼として書く。

```rust
// executor/dml.rs（RW が持つ。FK が依存する形）
/// ヒープ + 全インデックス。成功したら ri::after_insert を呼ぶ
pub fn insert_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid>;
/// 旧版を更新し、新しい TID を全インデックスに入れる。TmResult::Ok のとき ri::after_update を呼ぶ。
/// snap: 更新対象の可視性に使うスナップショット（通常の文は ctx.snapshot、RI の action は RI のスナップショット）。
/// crosscheck は**引数**（02 §4.6 が確定した形。通常の文は None、RI の action は ctx.ri.crosscheck() を渡す）。待ちは ctx.wait で行う
pub fn update_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, old_row: &[Datum],
                           new_row: &[Datum], crosscheck: Option<&Snapshot>) -> Result<UpdateOutcome>;
/// storage.delete を呼び、TmResult::Ok のとき ri::after_delete を呼ぶ。crosscheck と待ちは同じ
pub fn delete_row(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, old_row: &[Datum],
                  crosscheck: Option<&Snapshot>) -> Result<TmResult>;
```

- INSERT・UPDATE・DELETE のノードは、入力を使い切ったら `ri::finish_statement(ctx)` を呼んでから `None` を返す。COPY FROM は CopyDone の処理の終わり（`CommandComplete` の前）で呼ぶ。EXPLAIN ANALYZE は通常の実行と同じ。
- NOT NULL と CHECK の検査は、ノード（M4 の `enforce_constraints`）が `update_with_indexes` を呼ぶ前に行う。RI の action は同じ関数を `RiRel.not_null` / `RiRel.checks` で呼ぶ（§6.5）。

### 4.6 DDL 側の関数（`ddl/constraint.rs`）

```rust
/// CREATE TABLE と ALTER の共通。pg_constraint・pg_depend の行を書く。check_existing は ALTER のとき true（§6.6）
pub fn create_foreign_key(ctx: &mut DdlCtx<'_>, child: &TableDef, fk: &BoundForeignKey, resolved_parent: ResolvedParent, check_existing: bool) -> Result<ForeignKeyDef>;
pub enum ResolvedParent<'a> { Existing(&'a TableDef), SelfTable }
pub fn add_foreign_key(ctx: &mut DdlCtx<'_>, b: BoundAddForeignKey) -> Result<String>;          // "ALTER TABLE"
pub fn drop_constraint(ctx: &mut DdlCtx<'_>, b: BoundDropConstraint) -> Result<String>;         // "ALTER TABLE"
/// 既存の行を全件検査する（ALTER ADD。§6.6）
pub fn initial_check(ctx: &mut DdlCtx<'_>, fk: &ForeignKeyDef, child: &RiRel, parent: &RiRel) -> Result<()>;

// 他の DDL（M4 の DROP TABLE / DROP INDEX / DROP CONSTRAINT、03 章と M4 の TRUNCATE）が呼ぶ
/// DROP TABLE の対象（dropping）を参照している FK のうち、対象に含まれない子の表のもの。空でなければ 2BP01、CASCADE なら削除
pub fn fks_blocking_drop_tables(ctx: &DdlCtx<'_>, dropping: &[Oid]) -> Result<Vec<Arc<ForeignKeyDef>>>;
/// その一意インデックス（conindid）に依存する FK。DROP INDEX と DROP CONSTRAINT（PK / UNIQUE）が使う
pub fn fks_depending_on_index(ctx: &DdlCtx<'_>, index: Oid) -> Result<Vec<Arc<ForeignKeyDef>>>;
/// DROP の DETAIL の 1 行: "constraint c_pid_fkey on table c depends on table p"（on 以降の対象は呼び出し側が渡す）
pub fn describe_dependent(fk: &ForeignKeyDef, child_name: &str) -> String;       // "constraint {name} on table {child}"
/// TRUNCATE の対象（rels）に対する拒否（§6.7）
pub fn check_truncate_fks(ctx: &DdlCtx<'_>, rels: &[Oid], cascade: bool) -> Result<()>;
/// FK を 1 つ消す（行と依存を消し、catalog_dirty を立てる）。CASCADE の NOTICE は呼び出し側が出す
pub fn drop_foreign_key(ctx: &mut DdlCtx<'_>, fk: &ForeignKeyDef) -> Result<()>;
```

---

## 5. 処理の流れ

### 5.1 全体

```
INSERT / UPDATE / DELETE / COPY FROM（1 文）
  ├─ 各行: ヒープへ書く（dml.rs の 3 関数）→ ri::after_* がイベントを積む（§5.2）
  └─ 入力を使い切ったら ri::finish_statement（§5.3）
        CCI → キューが空になるまで:
              pop → CheckParent なら §5.4、ParentKeyGone なら §5.5 / §5.6
        失敗 → Err（文全体が中断される。§5.10）
```

### 5.2 イベントを積む（`ri::after_*`、`queue.rs`）

行ごとに、**被参照側（`rel.ri.incoming`）の制約を OID 昇順、続けて参照側（`rel.ri.outgoing`）の制約を OID 昇順**の順に判定し、積むものを積む（FK-D2）。

```text
after_insert(rel, tid, new_row):
  for fk in outgoing:
     key = extract_key(new_row, fk.child_cols)
     match null_class(key):
        AllNull                      → 積まない（MATCH SIMPLE も FULL も）
        Mixed  if SIMPLE             → 積まない
        Mixed  if FULL               → CheckParent を積む（5.4 で match_full_violation になる）
        NoneNull                     → CheckParent { fk, child_tid: tid, key } を積む

after_delete(rel, old_row):
  for fk in incoming:
     old_key = extract_key(old_row, fk.parent_cols)
     if null_class(old_key) != NoneNull → 積まない        // NULL を含むキーは誰にも参照されない
     ParentKeyGone { fk, op: Delete, old_key, new_key: None } を積む

after_update(rel, old_tid, old_row, new_tid, new_row):
  for fk in incoming:                                          // 親側の判定（PG の RI_FKey_pk_upd_check_required）
     old_key = extract_key(old_row, fk.parent_cols)
     if null_class(old_key) != NoneNull → 積まない
     new_key = extract_key(new_row, fk.parent_cols)
     if images_equal(old_key, new_key)  → 積まない                 // 値が変わっていない。PK 側は画像で比べる（1.0 → 1.00 も変更）
     ParentKeyGone { fk, op: Update, old_key, new_key: Some(new_key) } を積む
  for fk in outgoing:                                          // 子側の判定（PG の RI_FKey_fk_upd_check_required）
     new_key = extract_key(new_row, fk.child_cols)
     （null_class による積まない判定は after_insert と同じ）
     old_key = extract_key(old_row, fk.child_cols)
     if cmp_datum で old_key == new_key（FK 側は値で比べる） かつ 旧版を作ったのが自分のトランザクションでない → 積まない
     CheckParent { fk, child_tid: new_tid, key: new_key } を積む
```

- 「旧版を作ったのが自分のトランザクションか」は、`ctx.storage.fetch(rel, ctx.snapshot, old_tid)` の `xmin` が自分の XID か（**値が等しいとき、かつ new_key が NULL でないときだけ**読む）。自分のトランザクションが作った行を更新すると、挿入時の CheckParent は「版がもう生きていない」で捨てられるので、更新の CheckParent を省けない（【確認】`ri_triggers.c` のコメント。PG:src/backend/utils/adt/ri_triggers.c の `RI_FKey_fk_upd_check_required`）。読めなければ（None）省かない側に倒す。
- イベントは `ctx.mem.charge(estimate)` で数える。超えたら 53200（M4 D-19）。1 文のイベント数が `RI_MAX_EVENTS`（1 億）を超えたら 54001 `too many foreign key events in one statement`（暴走の安全弁）。
- **積む時点で NULL を捨てるのは PG と観測が同じ**: PG は積んだ後に発火時に捨てるが、結果と報告順は変わらない（捨てるものはエラーにならない）。MATCH FULL の NULL 混在だけは、他のエラーとの順序を保つため積む。

### 5.3 文の終わり（`finish_statement`）

```text
finish_statement(ctx):
  if ctx.ri.draining || ctx.ri.events.is_empty() { return Ok(()) }
  ctx.ri.draining = true
  ctx.txn.command_counter_increment()            // 文本体の変更を、これから取るスナップショットから見えるようにする
  loop pop_front → event:
      ctx.check_interrupts()?
      ctx.ri.processed += 1; if processed > RI_MAX_EVENTS → Err(54001)
      match event:
        CheckParent       → check::check_parent(ctx, ...)?                       // §5.4
        ParentKeyGone     → match fk.on_<op>:                                    // op に応じて on_delete / on_update
            NoAction      → check::parent_key_gone(ctx, ..., is_no_action = true)?    // §5.5
            Restrict      → check::parent_key_gone(ctx, ..., is_no_action = false)?
            Cascade       → action::cascade(ctx, ...)?                           // §5.6
            SetNull       → action::set_null_or_default(ctx, ..., SetNull)?
            SetDefault    → action::set_null_or_default(ctx, ..., SetDefault)?
                            check::parent_key_gone(ctx, ..., is_no_action = true)?   // FK-D11
      ctx.ri.release(event のメモリ)
  ctx.ri.draining = false（エラーでも戻す）。ctx.ri.clear()
```

- 各 action が 1 行以上を書いたら、**その直後に `command_counter_increment`** する（PG は SPI の問い合わせごとに CCI する）。次のイベントの検索が前のイベントの変更を見るため。
- **action の書き込みが積んだイベントは、同じキューの末尾に入る**（FK-D3）。drain 中は `draining` が真なので、`dml.rs` が呼ぶ `finish_statement` は何もしない。
- 検査済みキャッシュ `verified`（§6.1）は、action が 1 行でも書いたら空にする。
- 失敗したら、キューに残ったイベントは捨てる（文全体が中断されるので続きは無い）。

### 5.4 子の検査（`CheckParent`。`check::check_parent`）

PG の `RI_FKey_check`（ins / upd 共通）に当たる。

```text
check_parent(ctx, fk, child_tid, key):
  plan = ctx.ri.plan(ctx, fk)?          // 初回: 親に RowShare、子に RowShare を取り（FK-D17: ロック → open_relation）、FkPlan を作る
  snap = ri_snapshot(ctx, SnapKind::Check)?           // §5.7
  // 1. 子の版がまだ生きているか（積んだ後に、同じ文の中の action などで消えたら検査しない）
  if ctx.storage.fetch(plan.child.rel, &snap, child_tid)? is None { return Ok(()) }
  // 2. NULL
  match fk.match_type, null_class(key):
     (_, AllNull)            → return Ok(())
     (Simple, Mixed)         → return Ok(())
     (Full, Mixed)           → Err(match_full_violation(...))
     (_, NoneNull)           → 続ける
  // 3. この文の中で同じキーを確認済みなら省く（FOR KEY SHARE は取得済み）
  if ctx.ri.verified.contains((fk.oid, HashKey(key))) { return Ok(()) }
  // 4. 親を引く（インデックス）
  probe = build_probe(plan, key)?        // 子の値を親のインデックスの列順・型に直す。Ok(None) = どの親とも等しくない（桁あふれ）
  found = match probe { Some(p) => find_and_lock(ctx, &plan.parent, &plan.parent_index, p, &snap, recheck)?, None => false }
  if !found { Err(child_violation(...)) } else { ctx.ri.verified.insert(...); Ok(()) }
```

`find_and_lock` は §6.4。`recheck` は「ロックした最新版の親のキーが、プローブとまだ等しいか」（親の行が別のトランザクションにキー更新されたあとの再確認）。

### 5.5 親側の検査（NO ACTION と RESTRICT。`check::parent_key_gone`）

PG の `ri_restrict(trigdata, is_no_action)` に当たる。

```text
parent_key_gone(ctx, fk, old_key, is_no_action):
  plan = ctx.ri.plan(ctx, fk)?
  if null_class(old_key) != NoneNull { return Ok(()) }
  snap = ri_snapshot(ctx, SnapKind::Latest)?                  // PG の detectNewRows = true
  // NO ACTION だけ: 同じキーの別の親の行が（この文の変更の後に）あるなら、何もしない（ri_Check_Pk_Match）
  if is_no_action && parent_key_exists(ctx, &plan, old_key, &snap)? { return Ok(()) }     // 親の行も KeyShare でロックする
  // 子を引く（インデックス、なければ全件走査）。見つかった行は KeyShare でロックして、まだ条件を満たすか確かめる
  if child_row_referencing(ctx, &plan, old_key, &snap)? { Err(parent_violation(...)) } else { Ok(()) }
```

- **NO ACTION と RESTRICT の違いはこの 1 行だけ**: 同じキーを持つ別の親の行が文の終わりに存在するかの再確認をするか（【確認】§5.7 の最後の実機。`k1`（NO ACTION）は通り、`k2`（RESTRICT）は 23503）。
- 子の行を `KeyShare` でロックするのは、**子の行を別のトランザクションが削除中のときに待つため**（その削除がコミットすれば子は残らない。待たずに「見える」と判断すると偽の違反になる）。

### 5.6 action（`action.rs`）

#### CASCADE

```text
cascade(ctx, fk, op, old_key, new_key):
  plan = ctx.ri.plan(ctx, fk)?                     // 子に RowExclusive（初回）
  loop:                                            // RC の再走査（§6.5）
    snap = ri_snapshot(ctx, SnapKind::Latest)?
    for (tid, row) in child_rows_matching(ctx, &plan, old_key, &snap)? :     // 先に全部集めてから書く
        match op:
          Delete → delete_row(ctx, child.rel, &w, &snap, tid, &row, ctx.ri.crosscheck())     // crosscheck は ctx.ri が持つ値を引数で渡す
          Update → new_row = row with child_cols[i] := assign_cast(new_key[i], 子の列の型)
                   enforce_constraints(child, &new_row)?               // NOT NULL → 23502、CHECK → 23514
                   update_with_indexes(ctx, child.rel, &w, &snap, tid, &row, &new_row, ctx.ri.crosscheck())
        結果の処理は §6.5
    if 競合があって RC なら再走査、なければ break
  CCI（書いたとき）
```

#### SET NULL / SET DEFAULT

```text
set_null_or_default(ctx, fk, op, old_key, kind):
  対象の列 = if op == Delete && !fk.set_cols.is_empty() { fk.set_cols } else { fk.child_cols }
  各子の行について new_row = row の対象の列を NULL（SET NULL）または列の DEFAULT 式の値（SET DEFAULT。式が無ければ NULL）に置き換える
  → cascade の Update と同じ書き込み（NOT NULL・CHECK を通す）
```

- `ON UPDATE SET NULL / SET DEFAULT` は FK の全列（`set_cols` は ON DELETE のときだけ意味を持つ）。
- DEFAULT 式は行ごとに評価する（`nextval` や `now()` を含みうる）。`ctx.eval` は DEFAULT の式を `Var` を含まないものとして評価する。
- SET DEFAULT の結果のキーが親に無いとき、更新された子の行は `after_update` で `CheckParent` を積み、**子側のメッセージ**で 23503 になる（【確認】`Key (pid)=(0) is not present in table "pc".`）。**既定値のキーが、消した親のキーと同じ**ときは、子の `CheckParent` は積まれない（キーが変わらない）ので、FK-D11 の NO ACTION の再検査が親側のメッセージで失敗させる（【確認】`delete from pc where id=0` → `update or delete on table "pc" violates foreign key constraint "cc_pid_fkey" on table "cc"` / `Key (id)=(0) is still referenced from table "cc".`）。

### 5.7 スナップショット（FK-D5）

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SnapKind { Check /* PG の detectNewRows = false */, Latest /* detectNewRows = true */ }

fn ri_snapshot(ctx: &mut ExecCtx<'_>, kind: SnapKind) -> Result<RiSnapshot> {
    let cid = ctx.txn.cid;                                   // CCI の後。自分のここまでの変更が見える
    match (ctx.xact_snapshot, kind) {
        // Repeatable Read の子の検査だけは、トランザクションのスナップショット（curcid だけ進める）
        (Some(x), SnapKind::Check) => Ok(RiSnapshot::Xact(Snapshot { curcid: cid, ..x.clone() })),
        // それ以外は、検査ごとに取り直した最新のスナップショット
        _ => Ok(RiSnapshot::Latest(ctx.ri.env().latest_snapshot(ctx.txn.xid, cid))),
    }
}
```

| 検査 | Read Committed | Repeatable Read |
|---|---|---|
| 子の検査（CheckParent） | 最新 | **トランザクションのスナップショット**。親の行のロックで食い違い（コミット済みの更新・削除）に当たったら 40001 |
| 親側の検査（NO ACTION / RESTRICT） | 最新 | 最新。ロックで食い違いに当たったら 40001 |
| action の書き込み | 最新。当たった競合は再走査（§6.5） | 最新 + **crosscheck = トランザクションのスナップショット**。トランザクションのスナップショットで見えない子の行を書こうとしたら 40001 |
| ALTER ADD の初期検査 | 最新（子と親を ShareRowExclusive で守る） | 同じ |

- **crosscheck**: `RiQueue.crosscheck` は action の実行中だけ `Some(トランザクションのスナップショット)`（RR のとき）、それ以外は `None`。RI の action は `delete_row` / `update_with_indexes` の `crosscheck` 引数に `ctx.ri.crosscheck()` を渡し、`dml.rs` がそれを `TableStore::delete` / `update` の `crosscheck` に渡す（00 §4.5。02 §4.6。通常の文は `None`。レビュー対応 R-05）。ストアは「更新・削除できる版が `crosscheck` で見えない」ときに `TmResult::Updated` を返す。
- **ロックの食い違い**: RR のとき、`lock_tuple` に `follow_updates = false` を渡し、返りが `Updated` / `Deleted` なら 40001（`could not serialize access due to concurrent update`。`Deleted` は action の書き込みのときだけ `... concurrent delete`。00 §3.8）。RC のときは `follow_updates = true`（最新版まで追ってロックする）。
- 【確認】PG17 で次の 6 つを 2 つのセッションで確かめた（`T1` が先にスナップショットを取った RR のトランザクション、`T2` が別のトランザクション）。

| # | シナリオ | 結果（PG17） |
|---|---|---|
| a | RC。T1 が子を INSERT（未コミット）、T2 が親を DELETE（NO ACTION） | T2 は待つ。T1 がコミットすると T2 は 23503 `update or delete on table "pc" violates foreign key constraint "cc_pid_fkey" on table "cc"` / `Key (id)=(1) is still referenced from table "cc".` |
| b | RC。T1 が親を DELETE（未コミット）、T2 が子を INSERT | T2 は待つ。T1 がコミットすると T2 は 23503 `insert or update on table "cc" violates foreign key constraint "cc_pid_fkey"` / `Key (pid)=(1) is not present in table "pc".` |
| c2 | RR。T2 が親を DELETE してコミットした後、T1 が子を INSERT | T1 は **40001** `could not serialize access due to concurrent update` |
| d | RR。T2 が子を INSERT してコミットした後、T1 が親を DELETE（NO ACTION） | T1 は **23503**（`Key (id)=(1) is still referenced from table "cc".`） |
| e | RR。T2 が親を INSERT してコミットした後、T1 がその親を指す子を INSERT | T1 は **23503**（`Key (pid)=(7) is not present in table "pc".`）。トランザクションのスナップショットで親が見えないため |
| f/g/h | RR。T2 が子を INSERT してコミットした後、T1 が親を DELETE（ON DELETE CASCADE / SET NULL）または親のキーを UPDATE（ON UPDATE CASCADE） | T1 は **40001** `could not serialize access due to concurrent update`（crosscheck） |

### 5.8 並行性: 行ロックと D6 の簡易版（FK-D6）

- **取るロック**: 子の検査は親の行に、親側の検査は子の行に、`lock_tuple(.., LockTupleMode::KeyShare, RowWait::Block, follow_updates, &ctx.wait)`。ロックはトランザクションの終わりまで残る（xmax の `LOCK_ONLY | KEYSHR_LOCK`。複数の保持者がいれば D6 の MultiXact）。
- **FK の検査で待つ相手**: (1) その行を更新・削除中の別のトランザクション、(2) その行に `FOR UPDATE` / `FOR NO KEY UPDATE` / `FOR SHARE` を持つトランザクション（D6 の表のとおり。`KeyShare` どうしは共存）。リレーションの AccessExclusive などはリレーションロックの待ち（01 章）。
- **D6 の簡易版が PG と違う点**（FK に現れるもの）:

| 状況 | PG17（【確認】） | yuzhu（D6） |
|---|---|---|
| T1 が子を INSERT 済み（親の行に KeyShare）。T2 が親の**非キー列**を UPDATE | T2 は待たない（KeyShare と NoKeyExclusive は両立） | **T2 は T1 の終了まで待つ**（更新者とロック保持者は常に衝突） |
| 親の行に KeyShare を持つ T1・T3 が、それぞれ親の非キー列を UPDATE | 通る | 互いに待ってデッドロック（40P01） |
| T1 が親の行に KeyShare を持った後、**自分で**その行を UPDATE | 通る | 通る（他の保持者がいなければ）。他の保持者がいれば、その終了を待つ |

- 分離性テストの期待ファイルに「親の非キー UPDATE が待つ / 待たない」を含むものは、共有テストにしない（yuzhu 専用に分け、差を明記する。§7.2）。
- **pgbench の tpcb-like**（`--foreign-keys` つきの初期化の後）: 子（`pgbench_history`）の INSERT が、**同じトランザクションがすでに UPDATE した**親の行（accounts・tellers・branches の新しい版）に KeyShare を取るだけなので、上の差は出ない（推論。`-c 4 -T 30` で確かめる。§7.4）。

### 5.9 ロックの取得（リレーションロックの順序）

| 場面 | ロック | 取る時機 |
|---|---|---|
| CREATE TABLE / `ALTER TABLE ADD FOREIGN KEY` | 子と親を **ShareRowExclusive**（00 §3.4）。自己参照は 1 回 | 文の最初（LK-4 の「生のパース木からの一覧」。REFERENCES の対象を含める。§11） |
| `ALTER TABLE DROP CONSTRAINT`（FK） | 子と親を **AccessExclusive** | 文の最初（子はパース木にあり、親は制約から引く。解決 → ロック → 再解決の手順で、親を引いてからロックし、制約がまだあるか確かめる） |
| DML の対象 | RowExclusive（LK-4。FK と無関係） | 文の最初 |
| 子の検査（CheckParent） | 親に **RowShare** | 最初のイベントの処理時（文の終わり）。FK-D17: ロック → `open_relation` |
| 親側の検査（NO ACTION / RESTRICT） | 子に **RowShare** | 同上 |
| action（CASCADE / SET ...） | 子に **RowExclusive** | 同上 |

- RI のロックは文の終わりに**遅れて**取る（PG の `table_open(.., RowShareLock)` と同じ）。ほかのロックとの順序は動的で、デッドロックは 01 章の検出器が見つける。
- `ri.locked: HashSet<(Oid, LockMode)>` で、同じ文の中の取り直しを省く。
- ロックの待ちに入る前に、ページのラッチ・ピン・コミットゲートを持たない。RI は文の終わりに動くので、通常は何も持っていない（デバッグビルドの検査が守る）。

### 5.10 文の原子性とクラッシュ

- M3 は文単位のロールバックを持たない（サブトランザクションなし）。**文が失敗したら、暗黙のトランザクションは中断、トランザクションブロックの中なら Failed 状態になり、その後の COMMIT は ROLLBACK になる**。したがって CASCADE の途中で失敗した文の書き込みは、トランザクションごと見えなくなる。FK 固有の巻き戻しの処理は無い。
- CASCADE の行は、すべて親の DELETE と同じ XID で書かれ、同じコミットレコードで確定する。**コミットされた CASCADE は「親と子の両方が消えた」状態でしか見えず、クラッシュで途中までになることはない**（コミットレコードが無ければ全部が無かったことになる）。FK 専用の WAL レコードは無い（親の行ロックは 02 章の `HEAP_LOCK`）。

---

## 6. モジュールごとの仕様

### 6.1 `executor/ri/queue.rs`・`mod.rs`

- §5.2 の積む処理、§5.3 の drain のループ、定数（`RI_MAX_EVENTS = 100_000_000`）。
- `verified`: 子の検査で「このキーの親がある」と確かめた `(制約 OID, HashKey(key))` を覚える。**action が 1 行でも書いたら空にする**（親の行を削除・更新しうるのは action だけ）。大きさが 100,000 を超えたら空にする。
- **早期の処理（任意。FK-2 の後半）**: INSERT / COPY が積む `CheckParent` が 10,000 件を超えたら、**親が書き込みの対象の表と違う制約のものだけ**をその場で処理して減らす。理由: そのような `CheckParent` が読むのは、この文が変更しない親の表だけなので、文の終わりまで待っても結果が同じ。メモリ予算（53200）を守る。自己参照の制約と `ParentKeyGone` は文の終わりまで待つ。（M5-FK-Q10）

### 6.2 `executor/ri/keys.rs`

#### 型の組の分類（FK-D15）

```text
classify_pair(child, parent):
   child.oid == parent.oid                                    → Some(Same)
   Datum の変種が同じ（text と varchar など。どちらも Datum::Text）  → Some(Same)
   どちらも int2 / int4 / int8                                → Some(IntCross)
   子から親への暗黙キャスト（castcontext 'i'）が pg_cast にある → Some(CastChild { from: child, to: parent })
   それ以外                                                  → None（DDL は 42804）
```

【確認】PG17 で通る組（この規則で通る）: `bigint` → `int`、`int` → `bigint`、`text` → `varchar(5)`、`int` → `numeric`、`date` → `timestamp`。通らない組: `numeric` → `int`（42804。`Key columns "v" and "id" are of incompatible types: numeric and integer.`）、`text` → `int`（42804）。**PG が演算子族のクロスタイプ演算子だけで通す組（`timestamp` の子が `date` の親を参照するなど）は、yuzhu では 42804**（既知の差）。暗黙キャストの有無は M2 の `pg_cast` の静的な表（`catalog::builtin` のキャスト表。関数名は M2 の実装に合わせる）で引く。

#### プローブの作成

- **子 → 親**（CheckParent）: 子のキーの各値を、親のインデックスの列順に並べ直す（`parent_perm`）。`Same` / `IntCross` はそのまま。`CastChild` は暗黙キャストを適用する（キャストが失敗したら、そのエラーを返す。例外として、整数の幅が親より広い `IntCross` の値が親の型に収まらないときは、キャストせず `cmp_datum` に任せる＝どの親とも等しくならない）。
- **親 → 子**（親側の検査と action）: 親の `old_key` を、子のインデックスの列順に並べ直す（`child_index.1`）。**`Same` / `IntCross` の列だけの制約でインデックスを使う**。`CastChild` が 1 列でもあれば子の全件走査にして、各行の子のキーを親の型へ暗黙キャストして `cmp_datum` で比べる。

#### `images_equal`

`Datum` の変種ごとに、値の表現が同じかを比べる（NULL どうしは等しい）。`Float4` / `Float8` は `to_bits`。`Numeric` は `ndigits` / `weight` / `sign` / `dscale` / `digits` が同じか。`BpChar` は文字列が同じか（空白を含む）。【確認】PG は `datum_image_eq` で比べ、PK 側は 1 ビットでも違えば「変更あり」、FK 側は違っても `=` が真なら「変更なし」とする（`ri_KeysEqual`）。

### 6.3 `executor/ri/rel.rs`

- `ctx.ri.plan(ctx, fk) -> Result<Arc<FkPlan>>`: キューの `plans` になければ作る。手順: (1) 子と親にロック（§5.9。`locked` を見る）、(2) `open_relation`（親と子が同じ表なら 1 回）、(3) `classify_pair` を列ごとに（`None` ならここで内部エラー。DDL が防いでいるはず）、(4) 親のインデックス（`def.indexes` から `fk.parent_index`）と `parent_perm`、(5) 子のインデックス（下記）。
- **子で使えるインデックス（FK-D12）**: `child.def.indexes` の中で、(a) 部分・式のインデックスでない（M5 には無い）、(b) 先頭の `n`（FK の列数）列の attnum の**集合**が `fk.child_cols` の集合と一致、(c) 使う列の `RiKeyCmp` がすべて `Same` / `IntCross`、(d) インデックスの列の `opclass` が、親の値と比べる演算子（`conpfeqop`）の演算子族に属する、を満たす最初のもの（OID 順）。`n` 列の並び（`child_index.1`）は、インデックスの列位置 `i` に入れる値が FK のキーの何番目かを持つ。
- 全件走査は、そのイベントの間ずっと同じ走査にならないよう、**1 イベントにつき 1 回**だけ走査する。

### 6.4 `executor/ri/check.rs`

#### ロック付きの検索 `find_and_lock`

```rust
/// 索引（または全件走査）で候補を集め、1 件ずつ KeyShare でロックして recheck を満たす最初の 1 件を返す
fn find_and_lock(ctx: &mut ExecCtx<'_>, rel: &RiRel, how: Lookup<'_>, snap: &Snapshot,
                 recheck: &dyn Fn(&[Datum]) -> Result<bool>) -> Result<Option<HeapTuple>>;
enum Lookup<'a> { Index { index: &'a IndexHandle, keys: ResolvedScanKeys }, Scan { filter: &'a dyn Fn(&[Datum]) -> Result<bool> } }
```

```text
find_and_lock:
  candidates: Vec<Tid> = IndexStore::begin_scan(...) の全 TID（ブロック順に重複を除く）、
                         または HeapScan の（filter を満たす）可視な行の TID
  for tid in candidates:
     tuple = ctx.storage.fetch(rel, snap, tid)?   // None（この snap で見えない）なら次へ
     match lock_candidate(ctx, rel, tid, snap, recheck)? { Match(t) → return Some(t), Gone → continue }
  None
```

```rust
enum Locked { Gone, Match(HeapTuple) }
fn lock_candidate(ctx, rel: &RiRel, tid: Tid, snap: &Snapshot, recheck: &dyn Fn(&[Datum]) -> Result<bool>) -> Result<Locked> {
    let w = ctx.write_ctx()?;                                      // XID は書き込みの時点で採番済み
    let rr = ctx.xact_snapshot.is_some();
    let out = ctx.storage.lock_tuple(&rel.rel, &w, snap, tid, LockTupleMode::KeyShare, RowWait::Block, !rr, &ctx.wait)?;
    match out.result {
        TmResult::Ok => { let t = out.latest.unwrap_or(fetched); if recheck(&t.row)? { Ok(Match(t)) } else { Ok(Gone) } }
        TmResult::Deleted { .. } | TmResult::Updated { .. } if rr => Err(serialization_failure("concurrent update")),
        TmResult::Deleted { .. } | TmResult::Invisible => Ok(Gone),        // RC: 別のトランザクションが消した
        TmResult::SelfModified { .. } => Ok(Gone),                           // 同じコマンドが自分で変更済み
        other => Err(Error::internal(format!("unexpected lock_tuple result in RI: {other:?}"))),   // Updated（RC では follow_updates で来ない）、BeingModified、WouldBlock
    }
}
```

- 待ちは `lock_tuple` の中（heap 層。D7）で、ページのラッチを外して行われる。RC で相手が更新してコミットしたときは、`follow_updates` により最新版をロックして `latest` に返る。**最新版でもう一度 `recheck`**（キーが変わっていれば Gone）。
- **待つ前のスナップショット**: RI の検索は、ロックを待つ**前に**取ったスナップショットで行う。待った後にコミットされた新しい版は検索の対象にならない（【確認】PG: T1 が親を DELETE して同じキーで INSERT し直す。T2 が子を INSERT して待つ。T1 のコミット後、T2 は 23503。再 INSERT された親は見えない。シナリオ j）。同じ動きにする。

#### `parent_key_exists` と `child_row_referencing`

`find_and_lock` の呼び出し（親のインデックス、子のインデックスまたは全件走査）。`recheck` は、最新版のキーが `old_key` と `cmp_datum`（`CastChild` なら暗黙キャスト後）で等しいか。

#### 違反のエラー（【確認】PG17 の文言。00 §3.8）

```rust
/// 23503。子の検査が親を見つけられなかった
fn child_violation(fk: &ForeignKeyDef, plan: &FkPlan, key: &[Datum], env: &TypeEnv<'_>) -> Error;
/// 23503。MATCH FULL で NULL と非 NULL が混ざっていた
fn match_full_violation(fk: &ForeignKeyDef, plan: &FkPlan) -> Error;
/// 23503。親のキーを消したのに子が残っている
fn parent_violation(fk: &ForeignKeyDef, plan: &FkPlan, old_key: &[Datum], env: &TypeEnv<'_>) -> Error;
```

| 状況 | メッセージ | DETAIL |
|---|---|---|
| 子の検査 | `insert or update on table "<子>" violates foreign key constraint "<制約>"` | `Key (<子の列>, ...)=(<値>, ...) is not present in table "<親>".` |
| MATCH FULL の混在 | 上と同じ | `MATCH FULL does not allow mixing of null and nonnull key values.` |
| 親側 | `update or delete on table "<親>" violates foreign key constraint "<制約>" on table "<子>"` | `Key (<親の列>, ...)=(<値>, ...) is still referenced from table "<子>".` |

- SQLSTATE は 23503。**ErrorResponse の `s` / `t` / `n` は、どのメッセージでも子の表のスキーマ・子の表名・制約名**（【確認】親側の違反でも `TABLE NAME` は子）。`Error::with_table(child_schema, child).with_constraint(fk.name)`。
- 列名は引用符なしで `, ` 区切り。値は `output_text` の文字列で、引用符なし（【確認】`Key (v)=(it's "q") is still referenced from table "c3".`）。複数列は `Key (a, b)=(9, 9)`。値が NULL なら `null`。
- 親側で値に使うのは `old_key`（親の列の値）、子の検査では `key`（子の列の値）。
- `ALTER TABLE ADD` の初期検査の違反も、同じ `child_violation`（【確認】`insert or update on table "b8" violates foreign key constraint "b8_x_fkey"` / `Key (x)=(2) is not present in table "pt".`）。
- `40001` は `Error::new(sqlstate::SERIALIZATION_FAILURE, "could not serialize access due to concurrent update")`（【確認】PG17 の ErrorResponse に HINT は付かない。`CONTEXT` は FK-D14 のとおり出さない）。

### 6.5 `executor/ri/action.rs`

#### 子の行の集め方

`child_rows_matching(ctx, plan, old_key, snap) -> Result<Vec<(Tid, Row)>>`: 子のインデックス（あれば）か全件走査で、`snap` で見える行のうち、子のキーが `old_key` と等しいものを**先にすべて集める**（書く前に走査を終える。メモリは `ctx.mem` で数える）。

#### 書き込みの結果の処理

```text
delete / update の結果:
  Ok                                → 件数に数える
  SelfModified { .. }               → 飛ばす（同じコマンドで自分が変更済み）
  Deleted { .. }   RC               → 飛ばす（別のトランザクションが消した）
  Deleted { .. }   RR               → 40001 "could not serialize access due to concurrent delete"
  Updated { .. }   RC               → 競合あり: 全部の行を処理した後、新しいスナップショットで走査をやり直す（PG の EvalPlanQual と同じ結果）
  Updated { .. }   RR               → 40001 "could not serialize access due to concurrent update"（crosscheck を含む）
  その他                            → Error::internal
```

- **RC の再走査**は、競合が無くなるまで繰り返す（各回で見える版が新しくなるので終わる。`check_interrupts` が止められる）。既に書いた行は CCI で見えなくなるので二重には書かない。
- 新しい子の行は `enforce_constraints(child, &new_row)`（NOT NULL → 23502、CHECK → 23514。【確認】`null value in column "pid" of relation "nn" violates not-null constraint` / `Failing row contains (1, null).`、`new row for relation "ck" violates check constraint "ck_pid_check"`。ErrorResponse の `s` `t` `c`（列）`n` も付く）の後に `update_with_indexes`。子の行の削除は `delete_row`。どちらも `after_*` のフックを通って、子が持つ FK / 子を参照する FK のイベントを積む（自己参照の連鎖、孫の表への連鎖）。
- `assign_cast(new_key[i], 子の型)`: 親の値を子の列の型へ**代入キャスト**する（varchar(n) の長さの切り詰めエラー 22001 を含む）。`Same` / `IntCross` では値のコピー（`IntCross` は幅を合わせる。範囲外は 22003）。

### 6.6 DDL（`ddl/constraint.rs`、`analyzer/ddl_ext/foreign_key.rs`）

#### 解析（`bind_foreign_key`）の順序と、エラー

【確認】PG17 の SQLSTATE とメッセージ。

| # | 検査 | エラー |
|---|---|---|
| 1 | `MATCH PARTIAL`（パーサ） | 0A000 `MATCH PARTIAL not yet implemented` |
| 2 | `DEFERRABLE`、`INITIALLY DEFERRED`、`NOT VALID`（§1.2） | 0A000 |
| 3 | `NOT DEFERRABLE INITIALLY DEFERRED` | 42601 `constraint declared INITIALLY DEFERRED must be DEFERRABLE` |
| 4 | 親の表が無い | 42P01 `relation "x" does not exist` |
| 5 | 親が表でない（インデックス・シーケンス） | 42809 `referenced relation "x" is not a table`（未検証） |
| 6 | 親がシステムカタログ | 42501 `permission denied: "pg_class" is a system catalog` |
| 7 | 子の列・親の列の名前が無い | 42703 `column "zz" referenced in foreign key constraint does not exist` |
| 8 | 親の列を省略したが PK が無い | 42704 `there is no primary key for referenced table "x"` |
| 9 | 子と親の列の数が違う | 42830 `number of referencing and referenced columns for foreign key disagree` |
| 10 | 親の列に重複 | 42830 `foreign key referenced-columns list must not contain duplicates` |
| 11 | 親の列の集合に一致する一意インデックスが無い | 42830 `there is no unique constraint matching given keys for referenced table "x"` |
| 12 | 型が合わない（最初の 1 組） | 42804 `foreign key constraint "<名前>" cannot be implemented` / DETAIL `Key columns "<子の列>" and "<親の列>" are of incompatible types: <子の型> and <親の型>.` |
| 13 | `SET NULL / SET DEFAULT (cols)` の列が FK の列でない | 42P10 `column "y" referenced in ON DELETE SET action must be part of foreign key` |
| 14 | 明示名が同じ表の制約名と重複 | 42710 `constraint "x" for relation "t" already exists` |

- **一意インデックスの選び方**: 親の表の一意インデックス（`indisunique`、部分・式でない）のうち、列の attnum の**集合**が `parent_cols` の集合と一致する最初のもの（OID 昇順。【確認】`(id, k)` の PK に対して `(k, id)` の順で参照できる。PK と UNIQUE の列を混ぜた組は 11）。FK に書いた列順（`confkey`）はそのまま保存する。列を省略したときは PK のインデックス。
- 型名は `format_type`（`integer`、`text`、`numeric` など）。
- `CREATE TABLE` の自己参照: 親は作成中の表。子の列の型は作成中の列から、親のインデックスは `BoundCreateTable.constraints` の PK / UNIQUE から解決する（`BoundFkParent::SelfTable`）。PK が無ければ 8 / 11。

#### 制約名の自動生成（FK-D10）

```text
名前の指定あり → そのまま。同じ表の既存の制約名・この文で決まった名前と重複したら 42710
名前の指定なし → label = "fkey"
   base = make_object_name(<子の表名>, <子の列名を "_" でつないだもの>, "fkey")     // 63 バイトを超えるときの切り詰めは M2 の make_object_name（CHECK と共通）
   候補 = base、重複なら base + "1"、"2"、...（切り詰めを再計算）
   重複 = taken に含まれる、または catalog.constraint_name_exists(子の名前空間, 候補)（名前空間全体。FK-D10）
```

【確認】`c_pid_fkey`、`m3_a_b_fkey`、同じ列に 2 つ付けると 2 つ目が `c_pid_fkey1`、`a(b_c)` と `a_b(c)` は `a_b_c_fkey` と `a_b_c_fkey1`、長い名前は 63 バイトに切り詰め（`averyveryveryveryveryveryvery_averyveryveryveryveryverylon_fkey`）。

#### `create_foreign_key`

```text
1. （ロックは文の最初に取得済み。§5.9）
2. 親の定義（Existing は引数、SelfTable は child 自身）。親のインデックス OID（SelfTable は作成した PK / UNIQUE のインデックス）
3. ForeignKeyDef を組み立てる。oid = get_new_oid、eq_ops（§3.3）
4. CatalogStore::insert_foreign_key（pg_constraint と pg_depend の行）
5. txn.catalog_dirty = true（コミットで両方の表のキャッシュを無効化。M3 §5.3）
6. check_existing なら initial_check（ALTER のとき。CREATE TABLE の新しい表は空）
```

#### `initial_check`（ALTER ADD。FK-D5 の表の最後の行）

```text
initial_check(ctx, fk, child, parent):
  snap = latest_snapshot(own, cid)                 // 子と親を ShareRowExclusive で守っているので、書き込む相手はいない
  for 子の可視な各行（HeapScan。ストリーム）:
      key = extract_key(row, fk.child_cols)
      NULL の扱いは §5.4 と同じ（Full の Mixed は match_full_violation）
      if (制約, key) が検査済みキャッシュにある → 次へ
      親のインデックスを引き（行ロックは取らない）、無ければ Err(child_violation(...))   // 初めに見つかった違反の行を報告
```

- 【確認】`alter table b8 add foreign key (x) references pt(id)` で `b8` に `x = 2` があり `pt` に無いと、23503 `insert or update on table "b8" violates foreign key constraint "b8_x_fkey"` / `Key (x)=(2) is not present in table "pt".`。複数の違反があるときにどの行が報告されるかは、走査の順（PG は結合の計画次第）。
- 失敗したら文全体が中断される。`pg_constraint` と `pg_depend` の行は、中断したトランザクションの行として見えなくなる（§5.10）。そのトランザクションがブロックの中で他の文を続けられない（Failed）のは、通常のエラーと同じ。
- **ロックを取った後にスナップショットを取る**（00 §5.1）。ALTER が待たされた間に別のトランザクションが書き込んだ行も検査の対象になる。

### 6.7 DROP / TRUNCATE との連携

#### DROP TABLE（M4 の `ddl/table.rs` が `fks_blocking_drop_tables` を呼ぶ）

```text
drop_table(targets, cascade):
   blockers = fks_blocking_drop_tables(ctx, targets)    // confrelid が targets に含まれ、conrelid は含まれない FK
   if !blockers.is_empty():
       if !cascade → Err(2BP01)
       else        → 各 FK を drop_foreign_key（その子の表を AccessExclusive でロックしてから）+ NOTICE
   targets の子としての FK（conrelid が targets）は、表の行と一緒に消える（pg_constraint と pg_depend の行）
```

【確認】PG17:

```text
drop table p;
ERROR:  cannot drop table p because other objects depend on it
DETAIL:  constraint c_pid_fkey on table c depends on table p
HINT:  Use DROP ... CASCADE to drop the dependent objects too.

drop table p cascade;
NOTICE:  drop cascades to constraint c_pid_fkey on table c        -- 子の表 c は残る

drop table c;               -- 通る（FK の制約も一緒に消える）
drop table p, c;            -- 通る（依存元も同じ DROP の対象）
```

- 複数の FK があれば、DETAIL は 1 行ずつ `\n` でつなぐ（`describe_dependent` の文字列を並べる。「depends on table p」までを呼び出し側が付ける）。NOTICE は 1 件ずつ `drop cascades to constraint ...`。
- 自己参照の表の DROP は通る（依存元が対象に含まれる）。

#### DROP INDEX と DROP CONSTRAINT（PK / UNIQUE。M4 が `fks_depending_on_index` を呼ぶ）

【確認】PG17:

```text
alter table p drop constraint p_pkey;
ERROR:  cannot drop constraint p_pkey on table p because other objects depend on it
DETAIL:  constraint c_pid_fkey on table c depends on index p_pkey
HINT:  Use DROP ... CASCADE to drop the dependent objects too.

drop index p_pkey;      -- FK が無くても、制約が所有するインデックスは 2BP01（M4）:
ERROR:  cannot drop index p_pkey because constraint p_pkey on table p requires it
HINT:  You can drop constraint p_pkey on table p instead.
```

- 制約が所有しない一意インデックス（`CREATE UNIQUE INDEX`）を FK が使っているときの `DROP INDEX` も 2BP01（`cannot drop index x because other objects depend on it` / `constraint ... on table ... depends on index x`。未検証）。
- `ALTER TABLE DROP CONSTRAINT <PK / UNIQUE / CHECK>` は、FK が依存していれば上の 2BP01、依存がなければ 0A000（FK-D16）。`CASCADE` なら依存する FK を消す（NOTICE つき）が、PK 自体の削除は 0A000 のまま。

#### TRUNCATE（`check_truncate_fks`）

【確認】PG17:

```text
truncate p;
ERROR:  cannot truncate a table referenced in a foreign key constraint
DETAIL:  Table "c" references "p".
HINT:  Truncate table "c" at the same time, or use TRUNCATE ... CASCADE.

truncate p, c;           -- 通る
truncate p cascade;      -- PG は c も切り詰める。yuzhu は 0A000（FK-D18）
truncate c;              -- 通る（子の表の TRUNCATE は自由）
```

```text
check_truncate_fks(rels, cascade):
   for rel in rels:
       for fk in catalog.foreign_keys_referencing(rel):     // 自己参照は fk.child == rel なので rels に含まれる
           if fk.child in rels { continue }
           if cascade → Err(0A000 "TRUNCATE ... CASCADE is not supported")
           else       → Err(0A000 "cannot truncate a table referenced in a foreign key constraint")
                        + DETAIL 'Table "<子>" references "<親>".' + HINT 'Truncate table "<子>" at the same time, or use TRUNCATE ... CASCADE.'
   cascade かつ上のエラーに当たらなかった場合は、cascade の指定自体は無視してよい（対象に子が全部含まれている）
```

- 最初の 1 件だけ報告する（PG も同じ）。**FK は参照の有無だけ見る。表が空でも拒否する**（PG も同じ）。呼び出しは `ddl/truncate.rs`（M4 + 03 章 VC-5）。AccessExclusive のロック（D42）を取った後に呼ぶ。

### 6.8 psql の `\d` と pgbench

#### psql 17 の `\d tbl`（【確認】`psql -E` で実際の問い合わせを見た）

- 子の表: `SELECT true as sametable, conname, pg_get_constraintdef(r.oid, true) as condef, conrelid::regclass AS ontable FROM pg_constraint r WHERE r.conrelid = '<oid>' AND r.contype = 'f' AND conparentid = 0 ORDER BY conname`。→ `Foreign-key constraints:` の行。
- 親の表: `SELECT conname, conrelid::regclass AS ontable, pg_get_constraintdef(oid, true) AS condef FROM pg_constraint c WHERE confrelid IN (SELECT pg_partition_ancestors('<oid>') UNION ALL VALUES ('<oid>'::regclass)) AND contype = 'f' AND conparentid = 0 ORDER BY conname`。→ `Referenced by:` の行。
- `relhastriggers` が `t` のときだけ `pg_trigger` を引く。yuzhu は `f`（FK-D13）なので引かない。【確認】PG は FK のある表（子も親も）で `t`。
- FK 側の責任: `pg_constraint` の `conparentid` 列（0）、`pg_get_constraintdef(oid, bool)` の FK 形式（§3.4）。**この章の範囲外で、05 章（TY）の TY-5c が用意するもの**: `pg_partition_ancestors(regclass)`（【確認】【実機】PG17.11。パーティションでない表には 0 行を返す集合返却関数）。これは SELECT 句の集合返却関数なので、M4 の「SELECT 句の SRF は 0A000」と衝突するが、**05 TY-D17 が「SELECT 句に SRF が 1 つだけで他の出力列も FROM 句もない `SELECT srf(args)`」を `SELECT * FROM srf(args)` に書き換える最小の対応を M5 に足した（+0.5 日。00 D25 の例外）**ので、副問い合わせの腕 `SELECT pg_partition_ancestors('<oid>')` はこの形で通る（レビュー対応 R-28。【実機】`psql -E` で `\d p` の `Referenced by` の問い合わせを採取）。この章の完了条件 9（`\d p` / `\d c`）は TY-5c に依存する。
- 期待する出力の例（【確認】）:

```text
Foreign-key constraints:
    "c1_a_b_fkey" FOREIGN KEY (a, b) REFERENCES p(id, k) MATCH FULL ON UPDATE SET NULL ON DELETE CASCADE
Referenced by:
    TABLE "c1" CONSTRAINT "c1_a_b_fkey" FOREIGN KEY (a, b) REFERENCES p(id, k) MATCH FULL ON UPDATE SET NULL ON DELETE CASCADE
    TABLE "c3" CONSTRAINT "c3_v_fkey" FOREIGN KEY (v) REFERENCES p(v) ON DELETE RESTRICT
```

（`Referenced by` は `conname` の順。`ORDER BY conname`）

#### pgbench の外部キー初期化

`pgbench -i --foreign-keys`（`-F`）の外部キーの作成は、初期データの投入の後に次の 5 本の `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY ... REFERENCES <表>`（列は省略して PK を使う）を流す（【記憶】pgbench 17 の `initCreateFKeys`。実機で確かめる）。

```sql
alter table pgbench_tellers  add constraint pgbench_tellers_bid_fkey  foreign key (bid) references pgbench_branches;
alter table pgbench_accounts add constraint pgbench_accounts_bid_fkey foreign key (bid) references pgbench_branches;
alter table pgbench_history  add constraint pgbench_history_bid_fkey  foreign key (bid) references pgbench_branches;
alter table pgbench_history  add constraint pgbench_history_tid_fkey  foreign key (tid) references pgbench_tellers;
alter table pgbench_history  add constraint pgbench_history_aid_fkey  foreign key (aid) references pgbench_accounts;
```

- `pgbench_accounts`（スケール 1 で 10 万行）の初期検査は、子の全件走査 × 親のインデックスの検索。M4 の B+Tree で数十万行を数秒で処理できること（性能の目標は 10 章）。
- 初期化の後の tpcb-like（`-c 4 -T 30`）が完走することを、FK 付きでも確かめる（FK-5。M4 の D-25 の完了条件は FK なしなので、これは M5 の追加）。

---

## 7. テスト

テストは FK-5。共有テストの期待値は**実機（PG17）で確かめた値**を使う（M1〜M4 と同じ規則）。エラーは `statement error` の文言で照合する（sqllogictest の制約）。

### 7.1 共有 SLT（`tests/slt/m5/fk/*.slt`。PG17 でも通ること）

| ファイル | 内容 |
|---|---|
| `ddl_forms.slt` | 列制約・表制約・複合キー・自己参照・`REFERENCES t`（PK 省略）・`REFERENCES t(cols)`（列順の入れ替え。`(id, k)` の PK に `(k, id)`）の作成。自動命名（`c_pid_fkey`、`c_pid_fkey1`、`m3_a_b_fkey`、名前空間をまたぐ衝突）。`pg_constraint` の全列（`conkey`・`confkey`・`conpfeqop` など。`conindid` は `regclass` にキャスト）、`pg_get_constraintdef` の各形式（§3.4 の例）、`pg_depend` の 3 種類の行 |
| `ddl_errors.slt` | §6.6 の表の 1〜14 の各エラー（文言）。`MATCH PARTIAL`、文法の順序違い、`ON UPDATE SET NULL (cols)` |
| `violations.slt` | 子の INSERT / UPDATE、親の DELETE / UPDATE の違反のメッセージと DETAIL（複数列、特殊文字の値、NULL の値）。MATCH SIMPLE の NULL（検査されない）、MATCH FULL の NULL 混在と全 NULL |
| `actions.slt` | `CASCADE`（DELETE / UPDATE、多段）、`SET NULL`（全列、`(cols)`）、`SET DEFAULT`（既定値の親がある・ない・既定値が消した親と同じ）、`RESTRICT`、`NO ACTION`。NOT NULL / CHECK との衝突（23502 / 23514） |
| `statement_end.slt` | 文末の検査: 自己参照の複数行 INSERT `(2,1),(1,NULL)`、キーのずらしの UPDATE（`id = id + 10, parent = parent + 10`）、**NO ACTION と RESTRICT の差**（行 `(5),(1),(9)` を `case id when 5 then 6 when 1 then 5 when 9 then 1 end` で回す。NO ACTION は通り、RESTRICT は 23503。【確認】）、ブロックの中の DELETE + INSERT（最初の文で 23503） |
| `self_ref.slt` | 自己参照の CASCADE の多段（1 を消して全部消える）、ON UPDATE CASCADE の連鎖（【確認】`update s2 set id = id + 10 where id in (1,2)` で `(3,12) (11,NULL) (12,11)`）、同じ行を更新と CASCADE の両方が対象にする場合 |
| `types.slt` | 子 `bigint` → 親 `int`、子 `int` → 親 `bigint`、`text` → `varchar`、`int` → `numeric`（成功）、`numeric` → `int`・`text` → `int`（42804）。値の桁あふれ（`int8` の 5000000000 は親に無い → 23503） |
| `drop_truncate.slt` | §6.7 のすべて（DROP / DROP CASCADE / 子の DROP / 複数 DROP / PK の DROP CONSTRAINT / TRUNCATE の拒否と `TRUNCATE p, c`）。`NOTICE` は `statement ok` で見ない |
| `alter.slt` | `ALTER TABLE ADD FOREIGN KEY`（既存の行が違反 → 23503、通る場合）、`ADD CONSTRAINT` の重複名（42710）、`DROP CONSTRAINT`・`IF EXISTS`（NOTICE）・存在しない制約（42704 `constraint "x" of relation "t" does not exist`）、自動命名 `a1_x_fkey1` |
| `yuzhu_only.slt` | `onlyif yuzhu`: `DEFERRABLE` / `INITIALLY DEFERRED` / `NOT VALID` / `TRUNCATE ... CASCADE` / FK 以外の `DROP CONSTRAINT` が 0A000。`pg_trigger` に行が無い |

- 期待値の `NOT VALID` / `DEFERRABLE` を使うテストは、PG では通るので `onlyif yuzhu` に隔離する。
- 結果が表の物理順に依存するもの（NO ACTION と RESTRICT の差の UPDATE）は、**行の挿入順を固定**して書く（`INSERT` の順 = 走査の順）。

### 7.2 分離性テスト（`tests/isolation/specs/fk-*.spec`。00 D39 の方式。期待ファイルは PG17 で生成）

PG と同じ結果になるもの（共有）:

| spec | 手順 | 期待 |
|---|---|---|
| `fk-child-insert-parent-delete` | s1: 子を INSERT。s2: 親を DELETE（NO ACTION）→ 待つ。s1: COMMIT | s2 は 23503（`still referenced`） |
| `fk-parent-delete-child-insert` | s1: 親を DELETE。s2: 子を INSERT → 待つ。s1: COMMIT | s2 は 23503（`not present`） |
| `fk-parent-key-update-child-insert` | s1: 親のキーを UPDATE。s2: 旧キーを指す子を INSERT → 待つ。s1: COMMIT | s2 は 23503 |
| `fk-delete-reinsert-same-key` | s1: 親を DELETE して同じキーで INSERT。s2: 子を INSERT → 待つ。s1: COMMIT | s2 は 23503（待つ前のスナップショット。§6.4） |
| `fk-rr-child-insert-parent-deleted` | RR の s1 が SELECT。s2: 親を DELETE してコミット。s1: 子を INSERT | s1 は 40001 |
| `fk-rr-parent-delete-new-child` | RR の s1 が SELECT。s2: 子を INSERT してコミット。s1: 親を DELETE | s1 は 23503（`still referenced`） |
| `fk-rr-child-insert-new-parent` | RR の s1 が SELECT。s2: 親を INSERT してコミット。s1: その親を指す子を INSERT | s1 は 23503（`not present`） |
| `fk-rr-cascade-new-child` | RR の s1 が SELECT。s2: 子を INSERT してコミット。s1: 親を DELETE（`ON DELETE CASCADE`）、SET NULL、ON UPDATE CASCADE の 3 通り | s1 は 40001 |

yuzhu 専用に分ける（PG と差が出る。D6）:

| spec | 手順 | PG | yuzhu |
|---|---|---|---|
| `fk-nonkey-update-parent` | s1: 子を INSERT。s2: 親の非キー列を UPDATE | s2 は待たない | s2 は待つ（s1 の COMMIT 後に完了） |
| `fk-keyshare-deadlock` | s1・s3 が同じ親を参照する子を INSERT。s1・s3 がその親の非キー列を UPDATE | 通る | 40P01 |

- `fk-contention` / `fk-deadlock`（PG の isolation の既存 spec）は、D6 で差が出るので共有にしない（00 の `m5-concurrency` §691）。

### 7.3 Rust のテスト

| 対象 | 内容 |
|---|---|
| `catalog/fk.rs` | `fk_constraint_def` の文字列（§3.4 の例をそのまま固定値に）、引用符・修飾 |
| `parser/fk.rs` | 文法（順序固定、重複、`NOT DEFERRABLE` と `NOT NULL` の先読み、`ON UPDATE SET NULL (cols)`） |
| `analyzer/ddl_ext/foreign_key.rs` | §6.6 の 14 のエラー、`classify_pair`、自動命名（`FakeCatalog` に `constraint_name_exists` を持たせる）、一意インデックスの選択（列順の入れ替え） |
| `ri/keys.rs` | `null_class`、`images_equal`（numeric の 1.0 と 1.00、float の NaN と -0、bpchar の末尾空白）、プローブの作成（`parent_perm`）、整数の幅違い |
| `ri/queue.rs` | 積む判定（§5.2 の全分岐。MATCH SIMPLE / FULL、NULL、キー不変、自分のトランザクションが作った行、被参照側 → 参照側の順、OID 順）。`FakeStore` で `fetch` の xmin を差し替える |
| `ri/check.rs`・`action.rs` | `FakeStore` + `FakeIndexStore` で、検査の分岐（見つかる・見つからない・ロック結果ごと: `Ok` / `Deleted` / `SelfModified` / RR の `Updated`）、action の結果の処理（RC の再走査、RR の 40001）、エラーの文字列と ErrorResponse の `s` `t` `n` |
| 統合（`yuzhu-core/tests/fk_*.rs` と `yuzhu-server/tests/fk.rs`） | 2 セッションで §5.7 の a〜j を再現。`SET NULL` の NOT NULL 違反、CASCADE の多段（30000 段の自己参照鎖。再帰しないこと） |
| 並行ストレス（10 章 TS-2 の一部） | 親子の INSERT / DELETE / 親の非キー UPDATE を複数セッションで繰り返し、子が必ず親を持つ不変条件と、デッドロックの許容（40P01 は再試行） |

### 7.3a 変異テスト（10 章 §6.9 の #12。レビュー対応 R-09）

この章は `DebugKnobs` に **`fk_skip_key_share`**（子の検査で親の行に `FOR KEY SHARE` を取らない。`ri/check.rs` の `lock_tuple` の呼び出しを飛ばして `fetch` だけにする）を足す（既定は無効。外から変えられない。F0 が `DebugKnobs` に項目を足す。§11 の R10）。検出するもの: 10 章の S3・I23、分離性 `fk-parent-delete-race`（親の削除と子の挿入が競合したとき、子が親を失う）。各変異は「既定のシード集合の少なくとも 1 つで検出する」ことをテストにする（M3 §7.5）。

### 7.4 psql・pgbench・クラッシュ

- **psql**: `\d p` と `\d c` が §6.8 の出力と一致（M4 の psql の作業が済んでいること）。`\d` の問い合わせが `pg_partition_ancestors` を含めてすべて通る。
- **pgbench**: `pgbench -i --foreign-keys -s 1` が完走し、`pgbench -c 4 -T 30`（tpcb-like）が FK 付きで完走する（デッドロックの 40P01 が出ないことを確認。出たら §5.8 の D6 の差の見直し）。
- **クラッシュ（TS-3 の一部）**: ① CASCADE の 1 文を、SimVfs の「N 番目の WAL 書き込みでクラッシュ」の全点で試す（コミット前のクラッシュ: 親も子も残る、コミット後: 親も子も消える。中間の状態が見えない）。② `ALTER TABLE ADD FOREIGN KEY` の途中のクラッシュ（制約の行が残らない／コミット後は両方のキャッシュが新しい制約を見る）。③ 子の INSERT の後のクラッシュで、親の行のロックが残らない（クラッシュ後は MultiXact もロックマネージャも空。00 §5.4）。

---

## 8. 実装の分担と工数

00 §7 の ID のまま。担当は 1 エージェントが続けて持つ（FK-1 → FK-2 → FK-3 → FK-4 → FK-5）。FK-4 の一部（DROP / TRUNCATE の連携）は M4 の `ddl/` と 03 章の VC-5 の完成に合わせる。

| ID | 内容 | 主なファイル | 依存 | 日数 |
|---|---|---|---|---|
| FK-1 | DDL: パーサ・AST（`REFERENCES`、`FOREIGN KEY`、MATCH、action、順序の固定）、`bind_foreign_key`（§6.6 のエラー、一意インデックスの選択、型の分類、`eq_ops`、自動命名）、`catalog/fk.rs`・`store_fk.rs`（`pg_constraint` と `pg_depend` の行、`CatalogReader` の 3 メソッド）、`TableDef` / `RelHandle` の FK 一覧、`create_foreign_key`（CREATE TABLE の自己参照を含む）、`fk_constraint_def` | `sql/parser/fk.rs`、`sql/ast.rs`、`analyzer/ddl_ext/foreign_key.rs`、`catalog/{fk,store_fk}.rs`、`ddl/constraint.rs` | TY-5（配列）、M4（`ddl/`、`pg_index`、`opclass`） | 4 |
| FK-2 | RI エンジン: `RiQueue` と積む処理、drain のループ、`check_parent`、`parent_key_gone`、`cascade` / `set_null_or_default`、`keys.rs`、`rel.rs`、エラーの文言、`dml.rs` のフックの結線（RW と調整）、`RiEnv` の session 側の実装 | `executor/ri/*`、（依頼）`executor/dml.rs`、`session/*` | FK-1、RW-3 | 6 |
| FK-3 | 並行性: `lock_candidate`、RR の `SnapKind`、crosscheck の結線、40001、待ちとラッチ、リレーションロックの取得順（§5.9） | `executor/ri/{check,action,mod}.rs` | FK-2、RW-4 | 2 |
| FK-4 | `ALTER TABLE ADD / DROP CONSTRAINT`、`initial_check`、DROP / DROP INDEX / DROP CONSTRAINT（PK）の 2BP01 と CASCADE、TRUNCATE の拒否、`pg_get_constraintdef` の接続、psql `\d` の確認 | `ddl/constraint.rs`、（依頼）`ddl/{table,index,truncate}.rs` | FK-2、VC-5 | 3 |
| FK-5 | テスト（§7）。共有 SLT・分離性 spec・Rust・クラッシュ・pgbench / psql | `tests/slt/m5/fk/`、`tests/isolation/specs/fk-*`、`yuzhu-core/tests/fk_*.rs` | FK-2 | 2 |
| **合計** | | | | **17** |

- 並列にできる部分: FK-1 のパーサ・AST と、`catalog/fk.rs`・`store_fk.rs`・`fk_constraint_def` は別々に進められる。`keys.rs`（FK-2）は FK-1 と並行して書ける（純粋関数）。
- クリティカルパス（00 §1.3）: ... → RW-3 → FK-2 → FK-3。FK-2 が RW-3（`lock_tuple` と EPQ の結線）に依存するので、**FK-2 の前半（`keys.rs`、`queue.rs`、`check.rs` の検索の部分）は RW-3 の完成前に `FakeStore` で進められる**。
- カットライン（遅れたら落とす順。ほかの章の契約を変えずに落とせる）: §6.1 の早期の処理 → 検査済みキャッシュ → `ALTER TABLE DROP CONSTRAINT` の PK / UNIQUE の 2BP01 連携 → 型の分類の `CastChild`（同じ型と整数だけにする）。**落とせないもの**: 5 つの action、MATCH FULL、文末の処理、RR の 40001、ロック付きの検索。

---

## 9. 未検証の点（実装前に確かめるもの）

| # | 内容 | 確かめ方 |
|---|---|---|
| 1 | M4 の `AMOPS` の `integer_ops` にクロスタイプの `=`（`int24eq` 532、`int42eq` 533、`int28eq` 1868、`int82eq` 1869 などの OID）が入っているか（`int48eq` = 15 と `int84eq` = 416 は PG17 で確認済み） | `pg_amop.dat` / `pg_operator.dat` |
| 2 | `ON UPDATE SET DEFAULT (cols)` のエラー文言（SET NULL は確認済み） | PG17 |
| 3 | `referenced relation "x" is not a table` の文言と SQLSTATE（42809 と記憶） | PG17 |
| 4 | 制約が所有しない一意インデックスを FK が使っているときの `DROP INDEX` の文言 | PG17 |
| 5 | `lock_tuple` が `follow_updates = true` のとき、`TmResult::Ok` と `latest` の組を必ず返すか（`latest` が `None` の場合があるか）。02 章の確定を待つ | 02 章 |
| 6 | PG の入れ子の問い合わせの深さ優先（FK-D3）の観測できる差。複数の違反の報告順 | PG17 の実験 |
| 7 | pgbench 17 の `initCreateFKeys` の SQL 5 本 | pgbench のソース |
| 8 | 子の FK 列に同じ列を 2 回書いたとき（`foreign key (x, x) references pt(id, k)`）の PG の挙動 | PG17 |
| 9 | 子の列が 32 個を超える FK の `54011` `cannot have more than 32 keys in a foreign key`（親のインデックスの上限 32 に先に当たるので実用上は届かない） | PG17 |
| 10 | M4 の `HeapTuple` / `fetch` が、更新で xmax が自分になった旧版を `ctx.snapshot`（文のスナップショット）で返すこと（§5.2 の「旧版を作ったのが自分か」） | M4 の実装 |
| 11 | `RelHandle.ri` の追加と `from_table` の変更が M4 の単体テストに与える影響 | M4 のマージ後 |
| 12 | RR のトランザクション内で `ALTER TABLE ADD FOREIGN KEY` を流したときの PG の挙動（初期検査のスナップショット） | PG17 |

---

## 10. 確認事項

仮決めのまま進めてよいものです。ID は `M5-FK-Q<n>`（10 章が集めて通し番号に振り直す）。

- **M5-FK-Q1 `INITIALLY IMMEDIATE` と `DEFERRABLE` の文言**: 依頼文は「`DEFERRABLE` と `INITIALLY` は 0A000」としていましたが、**`INITIALLY IMMEDIATE` と `NOT DEFERRABLE` は受け付け、`DEFERRABLE` と `INITIALLY DEFERRED` だけを 0A000** にしました（FK-D7）。理由: `INITIALLY IMMEDIATE` は既定と同じ意味で、拒否する理由がない。変えたい場合: `INITIALLY IMMEDIATE` も 0A000 にするなら、パーサの 1 分岐を足すだけ（影響は小さい）。文言（`DEFERRABLE constraints are not supported`）は M4 の PK / UNIQUE の `DEFERRABLE` と同じにする必要があり、M4 の 07 章と突き合わせる。
- **M5-FK-Q2 TRUNCATE**: 00 D42 は「参照されている表への TRUNCATE を 0A000」とし、`CASCADE` も 0A000 としました。**参照している表がすべて同じ TRUNCATE に含まれていれば通す**（PG と同じ。FK-D18）。理由: PG の挙動で、実装も小さい。変えたい場合: 常に拒否にするなら `check_truncate_fks` の `continue` を外す（実装は簡単。PG との互換が少し下がる）。
- **M5-FK-Q3 `DROP CONSTRAINT` の範囲**: FK だけ（FK-D16）。PK / UNIQUE / CHECK は、FK の依存があれば PG と同じ 2BP01、なければ 0A000。理由: 00 §1.1 が ALTER の大半を M6 とした。変えたい場合: PK / UNIQUE / CHECK の削除は M4 の `ddl/constraint.rs`（インデックスの削除、`relchecks` の更新）に足す作業になる（+S〜M）。
- **M5-FK-Q4 連鎖の処理順（幅優先）**: FK-D3。理由: 再帰するとスタックが尽きる。PG と違うのは、複数の違反が同時にあるときの報告順だけ。変えたい場合: 深さ優先にするなら、drain を再帰にして `check_stack_depth` で 54001 にする（30000 段の鎖が PG は通るのに yuzhu が 54001 になる差が出る）。
- **M5-FK-Q5 D6 の簡易版の帰結**: 子の INSERT 済みのトランザクションがあると、親の非キー UPDATE が待たされ、デッドロックも増える（§5.8、§1.4）。PG との差で、共有テストには入れない。永続版 MultiXact（00 M5-Q4）にすれば消える。
- **M5-FK-Q6 `CONTEXT` 行**: 出さない（FK-D14）。ORM は `CONTEXT` を使わない（【記憶】）。変えたい場合: SQL の文字列を生成して `with_context` に渡す（`SQL statement "SELECT 1 FROM ONLY ..."`）。+S。
- **M5-FK-Q7 `relhastriggers` と `pg_trigger`**: FK の表でも `f`、行なし（FK-D13）。psql の `\d` には影響しない（【確認】）。`pg_dump` が FK のトリガーを見る場合は M6 の課題。
- **M5-FK-Q8 型の互換の簡約**: PG が演算子族のクロスタイプ演算子だけで通す組（`timestamp` の子が `date` の親、など）は 42804（FK-D15）。理由: 子の索引と親の索引を同じ比較で引くため、値の変換が要る。変えたい場合: 該当の組ごとの比較関数を `RiKeyCmp` に足す（+S。要望が出てから）。
- **M5-FK-Q9 `pg_depend` の行の形**: PG17 の実機と同じ（FK-D9）。M4 契約 §11.6 の「制約 → 表」の簡約形とは違うが、PK / UNIQUE の行は M4 のまま。M4 の DROP の依存の解決が `pg_depend` を一般に引く実装なら、そのまま動く。
- **M5-FK-Q10 早期の処理**: §6.1 の `CheckParent` の早期処理は任意（FK-2 の後半）。入れなければ、数百万行の COPY が FK 付きの表へ入るとき、イベントのメモリ（`yuzhu.query_mem_limit` 256MB）で 53200 になりうる。入れれば PG と同じ規模の COPY が通る。
- **M5-FK-Q11 `\d` の `pg_partition_ancestors`**: psql 17 の `Referenced by` の問い合わせは SELECT 句の集合返却関数を使う。**仮決め**: 05 章 TY-D17（TY-5c。+0.5 日）が「SELECT 句に SRF が 1 つだけの `SELECT srf(args)`」を FROM 句の形に書き換える最小の対応を M5 に足す（持ち主は TY。この章は `pg_constraint` と `pg_get_constraintdef` までを持つ）。**変えたい場合の影響**: TY-5c を外すと FK のある表の `\d tbl`（`\d p` / `\d c`）が M5 で動かず、この章の完了条件 9 と 10 §7.3 の 9 を外す必要がある。
- **M5-FK-Q12 エラーの位置情報（`LINE n:` と `^`）**: PG は 42601 の構文エラーのほか、一部の FK のエラー（`MATCH PARTIAL`、SET NULL の列リスト）に付ける（【確認】）。yuzhu はパーサのエラーの `span` を使う。DDL の意味エラーには付けない（共有テストは文言だけ見る）。

---

## 11. 契約への変更依頼

00 と食い違う点は 2 つある（レビュー対応で明記。R-05、R-07）: (1) **FK-D18**: 00 D42(2) の「参照されている表への TRUNCATE を無条件に 0A000」を「参照元が同じ TRUNCATE に全部含まれていれば通す（PG と同じ）」に変える（00 D42 と 03 VC-D17 は直した）、(2) **`executor/dml.rs` の 3 関数の署名**（M4 契約 §10 の `update_with_indexes(ctx, rel, w, tid, new_row)` に `snap`・`old_row`・`crosscheck` を足す。R1。02 §4.6 が受け入れて確定し、00 §4.6 にも載せた）。そのほか、00 が「FK-2 が決める」とした `RiQueue` と、FK が他の章のファイルに期待することを、依頼として書く。**持ち主の章に取り込んでもらうこと**（この章からは直さない。02・03 は取り込み済み）。

| # | 依頼先 | 内容 | 理由 |
|---|---|---|---|
| R1 | RW（02 章。`executor/dml.rs`） | `insert_with_indexes` / `update_with_indexes` / `delete_row`（§4.5）の 3 関数に、(a) `ri::after_*` の呼び出し、(b) `update_with_indexes` と `delete_row` の引数 `snap: &Snapshot` と `old_row: &[Datum]` の追加、(c) `crosscheck` は引数のまま（`ctx.ri` から読まない。RI の action が `ctx.ri.crosscheck()` を渡す）。**02 §4.6 が受け入れて確定した（R-05）**。INSERT・UPDATE・DELETE のノードは、入力を使い切った後で `ri::finish_statement(ctx)` を呼ぶ。M4 契約 §10 の `update_with_indexes(ctx, rel, w, tid, new_row)` からの署名の変更 | INSERT・UPDATE・COPY・RI の action が同じ書き込みの経路を通る。RI の action は別のスナップショットで書く（`ctx.snapshot` は文の最初のもの） |
| R2 | M4 の COPY（`copy/`、10 章） | COPY FROM は行ごとに `insert_with_indexes`（R1 の経路）を呼び、CopyDone の処理の終わり（`CommandComplete` の前）で `ri::finish_statement` を呼ぶ | COPY も FK を検査する（PG と同じ） |
| R3 | M4（`catalog/mod.rs`、`reader.rs`、`storage/mod.rs`） | `TableDef` に `foreign_keys` と `referenced_by`、`RelHandle` に `ri: Arc<RiRelInfo>`（§4.2）。`CatalogReader` に `foreign_keys_of` / `foreign_keys_referencing` / `constraint_name_exists`（既定実装つき）。実装本体は FK の `store_fk.rs`。`reader.rs` と `cache.rs` には委譲と TableDef 組み立ての数行だけを F0 が足す | RI がテーブルごとの FK 一覧を引く。FK の無い表の追加の負担は `ri.is_empty()` の 1 回の判定だけ |
| R4 | session（F0 が口。`session/simple.rs`・`extended.rs`） | 文ごとに `RiQueue::new(env)` を作って `ExecCtx.ri` に渡す。`RiEnv`（§4.4）を実装する: `open_relation` はリレーションロックの後に呼ばれ、カタログ用スナップショットを取り直して `TableDef` を読み、`RelHandle`・`not_null`・`checks`・`defaults` の実行用の式を INSERT の計画が使う関数で作る。`latest_snapshot` は `TxnManager::take_snapshot` | `executor` が `session` を `use` しない（00 §2）。RI は文の途中で関係を開く |
| R5 | LK（01 章。`session/locking.rs` の「生のパース木からの一覧」） | `CREATE TABLE ... REFERENCES t` と `ALTER TABLE ... ADD FOREIGN KEY ... REFERENCES t` の参照先を、**ShareRowExclusive** でロックする対象に含める。`ALTER TABLE ... DROP CONSTRAINT`（FK）は、子を AccessExclusive でロックし、制約から引いた親も AccessExclusive でロックしてから制約の存在を再確認する | 00 §3.4 の表（ALTER ADD は参照元と参照先の両方が ShareRowExclusive） |
| R6 | M4（`ddl/table.rs`、`ddl/index.rs`、`ddl/constraint.rs` の PK / UNIQUE、`ddl/truncate.rs`）と 03 章（VC-5） | DROP TABLE は `fks_blocking_drop_tables`、DROP INDEX と PK / UNIQUE の DROP CONSTRAINT は `fks_depending_on_index`、TRUNCATE は `check_truncate_fks`（§4.6）を、それぞれのロックを取った後に呼ぶ。2BP01 の DETAIL は FK の依存を `describe_dependent` で 1 行ずつ足す | FK の依存の解決は FK の関数が持つ。M4 の汎用の依存の解決が `pg_depend` を引くなら、同じ結果になる |
| R7 | M4（`deparse/`、`pg_get_constraintdef`） | contype `f` のとき `catalog::fk::fk_constraint_def` を呼ぶ | FK の文字列の形を 1 か所に持つ |
| R8 | M4（`catalog/opclass.rs`） | `integer_ops` のクロスタイプの `=` が `AMOPS` に無ければ追加（§9 の 1）。OID は `.dat` で確認 | `conpfeqop` / `conffeqop` の OID |
| R9 | TS（10 章） | 既知の差（§1.4）を集める。`tests/slt/m5/fk/` と `tests/isolation/specs/fk-*`、pgbench の FK 付きジョブ | — |
| R10 | F0（`debug_knobs.rs`） | `DebugKnobs` に `fk_skip_key_share` を足す（§7.3a。10 章 §6.9 の #12。R-09） | 変異テスト |
