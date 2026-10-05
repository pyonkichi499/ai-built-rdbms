# yuzhu M4 03: 問い合わせの構文とアナライザ

M4 の問い合わせ（JOIN・サブクエリ・WITH・集合演算・集約・`UPDATE ... FROM` / `DELETE ... USING`）の**構文（パーサ）**と、**アナライザ**（名前解決・型付け・エラー）の契約です。`00-contracts.md`（以下「00」）に従います。00 の署名・名前は変えません。変えたいものは第 10 節にまとめました。担当は S1（パーサ。この章の分）、N1（FROM 句・スコープ・DML の FROM）、N2（集約・GROUP BY・ORDER BY・DISTINCT ON）、N3（サブクエリ・集合演算・CTE）です。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 前提（読んだもの）: `m1.md` §5.1・§5.2、`m1-changes.md`、`m2.md` §6.10、`m3.md`（構成と書式）、`QUESTIONS.md`（Q-010〜Q-014）、`spec/research/m4-query.md` §2・§3・§6.5・§8、`research-pg-types.md` §3、`pg-compat-tools.md`、現在の `sql/`（ast.rs、parser/*）と `analyzer/*`（M3 の実装途中の状態を読んだ。この章は M3 が完成した状態を前提にする）、`00-contracts.md` §6〜§7・§11・§17。隣の章の `09-types-functions.md`（`FnKind::Set`、集約の表）と `10-explain-copy-compat.md`（EXPLAIN の AST）も参照した。
- 表記: **【検証済み】** は PostgreSQL 17.11（C ロケール、`sandbox/pg.sh` の 127.0.0.1:55432）に実際に SQL を流して確かめたもの。表中のエラーは `SQLSTATE メッセージ（位置）` の形で書く。位置は `psql -v VERBOSITY=verbose` のカレットで確かめた「1 始まりの文字位置が指すもの」を言葉で書く（「ColumnRef の先頭」など）。**「（未検証）」** は実機で確かめていない。PostgreSQL のソースは REL_17_STABLE を `PG:<path>` と略す。
- 迷ったら PostgreSQL 17 と同じ挙動を選ぶ。判断が分かれる点は推奨案で進め、第 9 節の「確認事項」に書く（`[03-Q番号]`）。

---

## 1. 範囲

### 1.1 この章が決めること

| 分類 | 内容 | 担当 |
|---|---|---|
| 構文 | `WITH [RECURSIVE] [NOT] MATERIALIZED`、FROM 句の関数（`generate_series`）、`ONLY`、`FILTER (WHERE ...)`、`x op ANY\|ALL (subquery)`、`COLLATE`、`OPERATOR(schema.op)`、行値（`IN` の左辺だけ）、`GROUP BY` の特殊形の検出。**拒否する構文（0A000）の文言と位置** | S1 |
| 名前解決 | 範囲表（`Rte`）、名前空間、`levels_up`、修飾あり・なしの探索、`*` の展開、システム列、別名の重複、結合列（USING / NATURAL）の併合 | N1 |
| FROM と DML | JOIN の解析（ON / USING / NATURAL）、FROM 句の関数・VALUES・派生表、`UPDATE ... FROM`、`DELETE ... USING`、`RETURNING` | N1 |
| 集約と並べ替え | 集約の解決と検査、GROUP BY（主キーへの関数従属を含む）、ORDER BY、DISTINCT ON、HAVING、集約の禁止位置 | N2 |
| 副問い合わせ | スカラー・EXISTS・IN / NOT IN・ANY / ALL、`test` の構築、列数エラー、相関 | N3 |
| 集合演算・CTE | UNION / INTERSECT / EXCEPT の型と ORDER BY の制約、WITH のスコープ・別名・MATERIALIZED | N3 |
| 出力 | 出力列名（`FigureColname` を集約・副問い合わせに拡張）、出力列の由来（`table_oid` / `attnum`） | N1、N2、N3 |

### 1.2 M4 で入れる／入れない（問い合わせ側）

00 §3 の範囲の問い合わせ側を次のように具体化する。**入れないものは実行すると `0A000`**（第 5.1.4 節に文言・位置・検出する層の表）。

| 入れる | 入れない（0A000） |
|---|---|
| JOIN 全種（INNER / LEFT / RIGHT / FULL / CROSS、ON / USING / NATURAL、カンマ結合）、FROM 句の副問い合わせ（別名・列別名・別名なし）、`(VALUES ...) AS v(a, b)`、FROM 句の `generate_series`（`AS g` / `AS g(x)`）、`ONLY t` と `t *`（継承がないので無視） | `LATERAL`（明示。および関数引数が左の FROM 項目を参照する暗黙の LATERAL）、`WITH ORDINALITY`、`ROWS FROM`、`TABLESAMPLE`、FROM 句の `generate_series` 以外の関数、`JOIN ... USING (...) AS alias`、括弧付き JOIN への別名 |
| 集約 `count` `sum` `avg` `min` `max` `bool_and` `bool_or` `every`（`DISTINCT`、`FILTER`）、`GROUP BY` / `HAVING`（主キーへの関数従属）、`DISTINCT ON` | ウィンドウ関数（`OVER`、`WINDOW`）、`GROUPING SETS` / `ROLLUP` / `CUBE` / `GROUP BY ()` / `GROUPING()`、集約内の `ORDER BY`、`WITHIN GROUP`、`string_agg` `array_agg` `stddev` ほかの未対応の集約、**外側の問い合わせに属する集約**（`(select max(t.a))`） |
| サブクエリ式（スカラー・`EXISTS`・`IN` / `NOT IN`・`= ANY` / `<> ALL` ほか）、相関サブクエリ、行値の `(a, b) IN (SELECT ...)` | `ANY` / `ALL` の配列形（`x = ANY (ARRAY[...])`）、行値のその他の使い方（`(a, b) = (c, d)`、`(a, b) IN ((1, 2))`）、全行参照（`count(t)`）、`ARRAY(SELECT ...)` |
| `UNION` / `INTERSECT` / `EXCEPT`（`ALL` を含む）、括弧、腕ごとの ORDER BY / LIMIT | `FOR UPDATE` / `FOR SHARE`（PostgreSQL が許す単一表の SELECT でも） |
| `WITH`（非再帰）、`[NOT] MATERIALIZED`、CTE の列別名、`WITH RECURSIVE` と書いても再帰しないものは通常の WITH として動く | `WITH` の再帰参照（自己・前方）、データ変更文を含む CTE、`WITH ... INSERT / UPDATE / DELETE`（文の先頭の WITH）、`SEARCH` / `CYCLE` 句 |
| `UPDATE ... FROM`、`DELETE ... USING`、`INSERT ... SELECT`（WITH・集合演算を含む）、`COLLATE "C" / "POSIX" / "default"`、`OPERATOR(pg_catalog.op)`、正規表現演算子 `~ ~* !~ !~*` | `IS DISTINCT FROM`（M1 から 0A000 のまま。[03-Q12]）、`COLLATE` のその他の照合順序（42704）、`RETURNING`（解析は実装し、解禁は後半。[03-Q14]） |

### 1.3 psql・pgbench が解析側に要求するもの

`pg-compat-tools.md` §3 と、実機の psql 17 に `-E` を付けて取り出した問い合わせ（【検証済み】）から、アナライザとパーサが通せるべき構文を表にする。名前の関数や演算子の本体は 09 の担当。

| ツールの操作 | 送る問い合わせの構文 | この章で受け持つもの |
|---|---|---|
| `\dt`、`\dt+`、`\di`、`\ds`、`\dv`（D-24 の `\dt` `\di`） | `FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace LEFT JOIN pg_am am ON am.oid = c.relam [LEFT JOIN pg_index i ON i.indexrelid = c.oid LEFT JOIN pg_class c2 ON i.indrelid = c2.oid]`、`WHERE c.relkind IN ('r','p','') AND n.nspname <> 'pg_catalog' AND n.nspname !~ '^pg_toast' AND pg_table_is_visible(c.oid)`、`CASE c.relkind WHEN ...`、`ORDER BY 1,2` | 複数の LEFT JOIN（ON は直前までの結合の列と、自分の右辺だけが見える）、自己結合（別名 `c2`）、`ORDER BY` の位置番号、`!~`・`<>`・`IN (...)` |
| `\dt pattern`、`\dn pattern` | `c.relname OPERATOR(pg_catalog.~) '^(foo.*)$' COLLATE pg_catalog.default`、`n.nspname OPERATOR(pg_catalog.~) ...` | `OPERATOR(schema.op)`（`~` の優先順位は「その他の演算子」と同じ）、`COLLATE schema.name`（`'...' COLLATE` は右辺の一部） |
| `\dn`、`\l` | `n.nspname !~ '^pg_'`、`ORDER BY 1`、関数呼び出し | M2 で動作済み。`!~` の演算子行は 09 |
| `\d tbl`（任意） | 相関スカラー副問い合わせ（SELECT 句。`(SELECT pg_get_expr(d.adbin, d.adrelid, true) FROM pg_attrdef d WHERE d.adrelid = a.attrelid AND ...)`）、`ARRAY(SELECT ...)`、`= any(配列)`、`'{0}'` との比較 | 相関スカラー副問い合わせは動く。**配列の 3 点（`ARRAY(SELECT)`、`= ANY(array)`、配列リテラル）は 0A000 のまま**で、psql は該当の 1 本のエラーを表示し、他の問い合わせには影響しない（`pg-compat-tools.md` §3.1 の要点どおり、`\d tbl` の完全な一致は M5） |
| `pgbench -i`（D-25） | `insert into pgbench_accounts(aid, bid, abalance, filler) select aid, (aid - 1) / 100000 + 1, 0, '' from generate_series(1, 100000) as aid`（`-I G` のとき）、`select count(*) from pgbench_branches` | FROM 句の `generate_series`（**関数名の別名 `aid` が列名にもなる**）、INSERT ... SELECT の出力 unknown（`''`）の扱い、集約 `count(*)` |
| `pgbench`（tpcb-like） | `UPDATE pgbench_accounts SET abalance = abalance + 10 WHERE aid = 1`、`SELECT abalance FROM ... WHERE aid = 1`、`INSERT INTO pgbench_history (...) VALUES (..., CURRENT_TIMESTAMP)` | M2 までで動く範囲。パーティション確認の問い合わせ（`CROSS JOIN LATERAL` ほか）は **0A000 で失敗してよい**（pgbench は失敗を許容して続行する） |

### 1.4 他章との境界

| 項目 | 持ち主 | この章の扱い |
|---|---|---|
| 論理プランへの変換、サブクエリの書き換え（SEMI / ANTI への引き上げ）、結合順序、FULL JOIN の 0A000 | `04-planner-optimizer.md` | アナライザは**構造をそのまま渡す**（EXISTS の対象列を捨てない、ON を押し下げない）。D-15 の解析側は 2.2 の D3-5 と 5.7 |
| `RETURNING` の実行、`UPDATE` / `DELETE` の入力の形 | `05-executor.md`、04 | Bound の形（00 §7）と解析の規則だけ決める |
| DDL の構文（`CREATE INDEX`、シーケンス、`ALTER TABLE`、`TRUNCATE`、`VACUUM`）、列定義の `GENERATED` | `07`、`08` | 触れない |
| 型名の修飾子、`CURRENT_TIMESTAMP` などの SQL 値関数の構文、`SessionValueKind` の追加、`KNOWN_UNSUPPORTED_TYPES` の整理、リテラルの経路、`pg_typeof` の畳み込み、`"any"` 引数の受理 | `09-types-functions.md`（実装は S1 / P0 / N1） | 09 が定義し、S1 と N1 が実装する。**この章では重複して定義しない**。ただし `analyzer/resolve.rs` に入る 2 点（`oid::ANY` を受理、`FnKind::Set` を FROM 句以外で拒否）の置き場所だけ 5.4 と 5.12 に書く |
| `EXPLAIN` / `COPY` の構文と `Statement::Explain` の解析 | `10-explain-copy-compat.md` | `analyze` の入口に枝を 1 つ置く（4.1）。内側の文は同じ `analyze` に再帰する |
| 集約の表（`BuiltinAggregate`、`AGGREGATES`）、`generate_series` の行（`FnKind::Set`）、演算子の行（`~` など） | `09` | この章は**引く側**。解決の手順は 5.5 と 5.4 |
| 関係の種類の検査（FROM 句・DML の対象が索引・シーケンスのときの `42809`）、IDENTITY 列の `identity_insert_rule` / `identity_update_rule` の呼び出し | `07-catalog-ddl.md`（D07-18、07-Q14）、`08-sequence-serial.md`（§4.9） | 規則と文言は 07 / 08。**呼び出す位置と実装は N1**（5.3.1、5.10） |

---

## 2. 決定

### 2.1 調査間の食い違いと、この章での決定

| # | 論点 | 資料の記述 | 実機・ソースで確かめた事実 | 決定 |
|---|---|---|---|---|
| R1 | FULL JOIN の 0A000 の条件 | `m4-query.md` §3.2: 「等値条件（ハッシュかマージが可能な条件）でなければ `0A000`」。§6.2: 「FULL: ネステッドループでは対応しない」 | `ON true`、`ON false`、`ON 1=1` は**通る**（Merge Full Join）。`ON t.a = u.a + 0`（式）も通る。`ON t.a < u.a`、`ON t.a + u.a = 3`、`ON t.a = u.a OR t.b = u.e`、`ON t.a = 1`、`ON t.a IS NOT DISTINCT FROM u.a` は `0A000 FULL JOIN is only supported with merge-joinable or hash-joinable join conditions`（`PG:src/backend/optimizer/path/joinrels.c:971`、**プランナが出す**）。`ON t.a = u.a AND t.b < u.e` は Hash Full Join（残りは Join Filter） | 0A000 は**プランナ（04）が出す**。アナライザは ON の型（bool）と集約の禁止しか見ない。`ON` が定数だけの FULL JOIN を通すかは 04 が決める（D3-1、[03-Q1]） |
| R2 | USING の比較の型 | `m4-query.md` §3.2: 「左右の列の共通型（`select_common_type`）で比較する」 | `transformJoinUsingClause`（`PG:parse_clause.c:308`）は元の左右の `Var` で通常の演算子解決をする（`smallint = bigint` は `int28eq`。`smallint = text` は `42883 operator does not exist: smallint = text`）。共通型は**併合列の型**にだけ使う（`buildMergedJoinVar`）。`m1 join m2 using (k)`（`smallint` と `bigint`）の `k` は `bigint`、`s`（`varchar(5)` と `text`）は `character varying`（`select_common_type` が先頭の型を残す） | 実機の方式にする（5.3） |
| R3 | 結合 RTE の表現 | `m4-query.md` §3.1: `RteKind::Join { merged: Vec<MergedColumn> }` | 00 §7: `JoinColSource { Left, Right, Coalesce }` | 00 に従う。さらに、PG は結合の別名 Var を計画時に展開するが、**yuzhu は解析時に展開する**（D3-2） |
| R4 | CTE の展開 | `m4-query.md` §3.7 / M4Q-4: 参照が 2 回以上でもインライン | 00 D-16: PG と同じ規則 | 00 に従う。アナライザは `CteMaterialize` と `CteRef` を作るだけで、展開しない |
| R5 | UNIQUE + NOT NULL への関数従属 | `m4-query.md` §3.3: 「PG も主キーのみ【未検証: UNIQUE + NOT NULL でも不可であること】」 | 【検証済み】`uq(id int unique not null, v int)` で `select id, v from uq group by id` は `42803 column "uq.v" must appear in the GROUP BY clause ...`。複合主キーは全列が要る（`group by a` だけでは不可、`group by a, b` で `c` が通る） | 主キーだけが対象（D3-10） |
| R6 | JOIN の ON から見えない表 | `m4-query.md` §3.2: 「名前空間にはあるが参照できない位置の場合は `invalid reference to ...`【未検証: 発生条件】」 | 【検証済み】`select * from t, u join p on p.id = t.a` は `42P01 invalid reference to FROM-clause entry for table "t"` + `DETAIL: There is an entry for table "t", but it cannot be referenced from this part of the query.`（HINT なし）。派生表・関数が左の項目を参照すると同じ DETAIL に `HINT: To reference that table, you must mark this subquery with LATERAL.` | 名前空間の項目に `lateral_only` を持たせ、PG と同じ診断にする（5.2） |
| R7 | ORDER BY の定数 | `m1.md` §5.2: 整数リテラルは出力列の番号。M1 の実装は `ORDER BY TRUE` を式として通す | 【検証済み】`order by true` / `null` / `1.5` / `'a'` はすべて `42601 non-integer constant in ORDER BY`（`TRUE` も `A_Const` のため） | M1 の実装を直す（`Literal::Bool` を除外している分岐を外す。N2 の最初の作業）。GROUP BY / DISTINCT ON も同じ |
| R8 | `SELECT *` の FROM なし | M1 の文言: `SELECT * with no tables specified` | 【検証済み】`42601 SELECT * with no tables specified is not valid`（位置は `*`） | 文言を直す（N1） |
| R9 | `TableRef::Values` | `00 §7.1`: 「`Subquery` で表せるなら不要」 | `(VALUES ...) v(a, b)` は `TableRef::Subquery` の本体が `QueryBody::Values` で表せる | 追加しない。アナライザが ORDER BY / LIMIT / WITH のない `Values` 本体を `RteKind::Values` にする（5.4） |
| R10 | `UPDATE r SET t.v = 1` | `m2.md` §6.10: 「PostgreSQL の挙動は未検証」 | 【検証済み】`42703 column "t" of relation "r" does not exist`（位置は `t`）。`SET r.v = 1` は同じエラーに `HINT: SET target columns cannot be qualified with the relation name.` | M2 の実装（`bad_set_target`）と一致。変更なし |
| R11 | LIMIT の中の副問い合わせ | `m4-query.md` §3.8: 「許される」 | 【検証済み】現スコープの列を含むと `42P10 argument of LIMIT must not contain variables`（副問い合わせの中の `t.a` も、その副問い合わせから見て現スコープの Var なら同じ）。外側の列は副問い合わせの LIMIT に書ける（`... limit t.a`） | 「現スコープの Var を含まない」を、副問い合わせの深さを数えて検査する（5.12） |
| R12 | UPDATE の解析順 | M2 の実装は SET 列の検査を WHERE より先に行う | 【検証済み】`update r set v = zz where yy = 1` は `yy` のエラー（WHERE が先）。順序は FROM → WHERE → RETURNING → SET（`PG:analyze.c` の `transformUpdateStmt`）。DELETE は USING → WHERE → RETURNING | PG の順序にする（4.7） |

### 2.2 この章で追加で決めたこと

| # | 決定 | 理由 | 確認 |
|---|---|---|---|
| D3-1 | **FULL JOIN の 0A000 はプランナが判定する**。アナライザは ON を `coerce_to_boolean("JOIN/ON")` して `FromItem::Join.on` に置くだけ。文言は PG と同じ `FULL JOIN is only supported with merge-joinable or hash-joinable join conditions`。プランナへの申し送り: 左だけ・右だけを参照する式の等値が 1 本もない FULL は 0A000。`ON` が定数だけ（`true` / `false` / `1=1`）の FULL は PG が通すが、04 §7 は結合キーがなければすべて 0A000 にしている（NestedLoop の FULL を持たないため）。**既知の差**として残し、slt は PG だけが通す行を `skipif yuzhu` + `onlyif yuzhu` の 0A000 の対にする | 判定は述語の分類（結合キーの抽出）の後でないと確定しない。アナライザで判定すると同じ分類を 2 か所に書く。PG もプランナが出す（R1） | [03-Q1] |
| D3-2 | **結合の別名 Var は解析時に展開する**。`Var` が `RteKind::Join` の列を指す `BoundExpr` は作らない。`t JOIN u USING (a)` の `a` は INNER・LEFT なら `t.a`（左の Var）、RIGHT なら `u.a`、FULL なら `COALESCE(t.a, u.a)`（型が違えば暗黙キャストを挟む）に展開した式として `BoundExpr` に入る。`RteKind::Join.sources` は `*` の展開と診断のための情報として残す | PG は `parseCheckAggregates` で GROUP BY・SELECT の両方を同じ形に展開してから比較する。解析時に展開すれば、GROUP BY の検査・出力列の由来・04 の列参照の解決が単純になる（04 は Join RTE の Var を扱わなくてよい） | [03-Q2]（00 への変更提案 P3-1） |
| D3-3 | **名前の解決順**: GROUP BY の単純な名前は**入力列（FROM の列）が先、なければ出力列の別名**。ORDER BY と DISTINCT ON は**出力列の別名が先**。どちらも整数リテラルは出力列の位置、それ以外の式は入力列に対して解析し、同じ式が出力列にあればそれ、なければ resjunk として足す | PG の `findTargetlistEntrySQL92`（`PG:parse_clause.c:2006`）。GROUP BY だけが逆（`select a as b, count(*) from t group by b` は `t.b` に解決され `42803`） | — |
| D3-4 | **`has_agg`**: `BoundSelect.has_agg = 現スコープに属する集約がある \|\| group_by が空でない \|\| having がある`。集約の属するスコープは「引数（と FILTER）の Var の `levels_up` の最小値」で決まり、0 なら現スコープ。**1 以上（外側に属する）は 0A000**。Var が 1 つもない集約（`count(*)`、`sum(1)`）は現スコープ | PG の `agglevelsup`。外側の集約は実行時の意味（外側の GROUP BY の 1 グループの値で内側を評価）が複雑で、実用がほぼない | [03-Q4] |
| D3-5 | **サブクエリの解析側（D-15）**: (1) `SubLink { kind, test, query }` を式として作る。(2) `x IN (SELECT ..)` は `Any`（`test = x = SubLinkOutput(0)`）、`NOT IN` は `Not(Any)`、`x op ANY/ALL (..)` は `Any` / `All`。(3) EXISTS の対象列は解析するが捨てない。(4) 相関は `Var.levels_up` だけで表し、SubLink に「相関あり」の印は付けない（プランナが導出）。(5) 引き上げ（SEMI / ANTI）の可否の判定はプランナ。(6) 副問い合わせの出力列の unknown は text に解決してから比較演算子を解決する | PG の `transformSubLink`。(6) は【検証済み】`1 = any (select null)` が `42883 operator does not exist: integer = text` | — |
| D3-6 | **LATERAL は 0A000**（明示の `LATERAL` はパーサ、関数引数が左の FROM 項目を参照する暗黙の LATERAL はアナライザ）。ただし LATERAL でない参照のエラー（`42P01` / `42703` + DETAIL / HINT）は PG と同じにする | 実装は相関サブクエリの仕組みで可能だが M5〜M6（`m4-query.md` §2.1）。診断を PG と同じにすれば、将来 LATERAL を入れるときに変えるのは 1 か所 | [03-Q5] |
| D3-7 | **`WITH RECURSIVE` の構文は受理**し、再帰参照（自己・前方）だけを 0A000 にする。再帰しない CTE は `RECURSIVE` があっても通常の WITH として動く（PG と同じ） | ORM が `WITH RECURSIVE` を付けて非再帰の CTE を送ることがある | [03-Q7] |
| D3-8 | **`WITH ... INSERT / UPDATE / DELETE` とデータ変更を含む CTE は 0A000**（パーサ）。`INSERT ... SELECT` の SELECT の中の WITH は動く | `BoundUpdate` / `BoundDelete` に `ctes` の欄がない。`INSERT ... SELECT` は `source: BoundQuery` が `ctes` を持てる | [03-Q6] |
| D3-9 | **FROM 句の関数は `FnKind::Set`（集合返却。M4 は `generate_series`）だけ**。他の関数は 0A000。`FnKind::Set` を FROM 句以外で呼ぶと 0A000（09 §10.1 の文言） | `FunctionScan` は集合返却だけを実行する（00 §9.2）。PG は `select * from lower('A')` も通すが、実用がない | [03-Q11] |
| D3-10 | **GROUP BY の関数従属は PRIMARY KEY だけ**。`RteKind::Table` の RTE について、主キーの全列が `group_by` に Var として含まれれば、その RTE の列は何でも参照してよい。参照された（グループ化されていない）列の Var は `BoundSelect.group_by` の末尾に追加する（グループの値は各グループ内で一定なので意味は変わらない。プランナは追加のキーとして扱えばよく、`first_value` 的な集約は要らない） | 【検証済み】R5。PG は依存を `pg_depend` に記録して主キーの DROP を防ぐが、yuzhu は M4 にビューがなく CHECK に集約を書けないので記録しない | [03-Q3] |
| D3-11 | **`JOIN ... USING (...) AS alias` と、括弧付き JOIN への別名 `(t JOIN u ON ...) AS j` は 0A000 のまま**（M1 のパーサの拒否を維持） | 実機では動く（【検証済み】）が、ORM が生成することはまれ。名前空間の「別名が子を隠す」規則だけが増える | [03-Q13] |
| D3-12 | **`ANY` / `ALL` の配列形、行値の一般の使い方、全行参照は 0A000**。行値は `(a, b) [NOT] IN (SELECT ...)` と `(a, b) = ANY (SELECT ...)`、`(a, b) <> ALL (SELECT ...)` の左辺としてだけ受理する（M4 後半・任意） | 配列は M5。`m4-query.md` §2.1 が行値の IN を「M4（任意）」にしている | [03-Q8]、[03-Q9]、[03-Q15] |
| D3-13 | **`COLLATE`** は `"C"` / `"POSIX"` / `"default"`（`pg_catalog.` 修飾可）だけ受理し、**式をそのまま返す**（照合順序は C しかない）。他の名前は `42704 collation "x" for encoding "UTF8" does not exist`。照合順序を持たない型は `42804 collations are not supported by type integer`。2 つの異なる明示照合順序の衝突（`42P21`）は検出しない | 【検証済み】5.12.3 の表。psql が `COLLATE pg_catalog.default` だけを使う | [03-Q10] |
| D3-14 | **ORDER BY のある `VALUES` は `Select` over `RteKind::Values` にする**（M1 と同じ）。ORDER BY のない `VALUES` と、集合演算の腕・`INSERT ... VALUES` は `BoundSetExpr::Values` | `BoundSetExpr::Values` には ORDER BY の resjunk 式を置く場所がない（`VALUES (1) ORDER BY column1 + 1` が PG で通る） | — |
| D3-15 | **`analyze_query` の `resolve_unknowns`**: true = 単独の SELECT・副問い合わせ式・CTE・FROM 句の派生表。false = `INSERT ... SELECT` の本体と集合演算の腕（腕の出力は集合演算が共通型を決める） | M1 の方針（`m1.md` §5.2）と PG の `parse_sub_analyze` の引数 | — |
| D3-16 | **CTE の列別名は解析時に適用する**。`BoundCte.query.columns[i].name` を別名適用後の名前にし、`col_aliases` は宣言どおりの別名（EXPLAIN の参考）として残す | 下流（04）は名前の合成を考えなくてよい | — |
| D3-17 | **RETURNING の解析は完全に実装するが、定数 `RETURNING_ENABLED` が `false` の間は `0A000 RETURNING is not supported yet` を返す**（M2 と同じ文言）。X3 と 04 の対応が終わったら `true` にする。対象は対象表の列・システム列・定数・関数だけで、FROM / USING の列と副問い合わせは 0A000 | 00 D-26（RETURNING は M4 後半・任意）。Bound には欄がある | [03-Q14] |
| D3-18 | **未対応の集約名は `0A000 aggregate function X is not supported yet`**（`aggregates_named` が空で、`string_agg` などの既知の集約名の一覧にあるとき）。それ以外の存在しない関数は `42883` | M1 の `AGGREGATES` 定数（0A000 にする名前の一覧）の引き継ぎ | [03-Q16] |
| D3-19 | **エラーの DETAIL / HINT**: 5.2.4 の表の DETAIL / HINT のうち、`42703` の近い名前の HINT（`Perhaps you meant to reference the column "t.b".`）は**付けない**。それ以外（lateral の DETAIL / HINT、別名の HINT、CTE の前方参照の DETAIL / HINT）は付ける | slt は SQLSTATE と主要な文言だけを見る。近い名前の推測は編集距離の実装が要る | [03-Q17] |
| D3-20 | **`levels_up` の数え方**: `Var.levels_up` は**スコープ**（rtable を持つ `BoundSelect`、DML 文、および rtable が空のスコープとして扱う `BoundSetExpr::Values` の行）の入れ子だけを数え、`BoundQuery` は数えない。`CteRef.levels_up` は `BoundQuery` の入れ子を数える（3.2.1） | 集合演算の腕・CTE の本体・副問い合わせの本体は `BoundQuery` の子で、互いに兄弟。PG は `BoundQuery` に相当する Query も数える（`varlevelsup` が 1 大きい場合がある）が、yuzhu はスコープだけにして 04 の積み方を単純にする | — |

---

## 3. 型とトレイト（契約の具体化）

### 3.1 AST の追加（S1。`sql/ast.rs`）

00 §7.1 は名前だけを固定している。この章が持つ分のフィールドを決める。**既存の変種は名前を変えない**（`TableRef::{Table, Subquery, Join}`、`Expr::{InSubquery, Exists, Subquery}`、`QueryBody::{Select, Values, SetOp, Nested}`、`Distinct::On`、`JoinConstraint`、`Update.from`、`Delete.using`、`returning: Vec<SelectItem>` は M1・M2 のまま使う）。

```rust
// ---- WITH ----
pub struct Query {
    /// 先頭の WITH。`(WITH ... SELECT ...)` の括弧の中も、入れ子の Query がそれぞれ持つ
    pub with: Option<With>,                       // ★ 追加
    pub body: QueryBody,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    pub span: Span,                               // WITH の先頭から
}
pub struct With { pub recursive: bool, pub ctes: Vec<Cte>, pub span: Span }
pub struct Cte {
    pub name: Ident,
    /// `x(a, b) AS (...)` の列別名。空 = なし
    pub columns: Vec<Ident>,
    /// `MATERIALIZED` = Some(true)、`NOT MATERIALIZED` = Some(false)、指定なし = None
    pub materialized: Option<bool>,
    pub query: Box<Query>,
    /// name の先頭から閉じ括弧まで。`WITH query "x" has N columns available ...`（42P10）の位置は span.start
    pub span: Span,
}

// ---- FROM ----
pub enum TableRef {
    Table { name: ObjectName, alias: Option<TableAlias>, span: Span },
    Subquery { query: Box<Query>, alias: Option<TableAlias>, span: Span },   // (VALUES ...) もここ（本体が QueryBody::Values）
    /// ★ `generate_series(1, 3) AS g(x)`。args は a_expr の並び（空もありうる）
    Function { name: ObjectName, args: Vec<Expr>, alias: Option<TableAlias>, span: Span },
    Join { left: Box<TableRef>, right: Box<TableRef>, kind: JoinKind, constraint: JoinConstraint, span: Span },
}

// ---- 式 ----
Expr::Function { name, args, distinct, star,
                 /// ★ `FILTER (WHERE cond)`
                 filter: Option<Box<Expr>>, span }

/// ★ `x op ANY|SOME|ALL (subquery)`。op は演算子の綴り（`=`、`<>`、`<`、`~~`（LIKE）、`!~~`（NOT LIKE）、`~~*`、`!~~*`）。
/// `OPERATOR(pg_catalog.=)` なら op_schema = Some(pg_catalog)。span.start は演算子のトークン（`NOT LIKE` なら NOT）
Expr::QuantifiedSubquery { expr: Box<Expr>, op: String, op_schema: Option<Ident>,
                           quantifier: Quantifier, query: Box<Query>, span: Span }
pub enum Quantifier { Any /* ANY と SOME */, All }

/// ★ `expr COLLATE name`。span.start は COLLATE のトークン（PG の位置と同じ）
Expr::Collate { expr: Box<Expr>, collation: ObjectName, span: Span }

/// ★ 行値 `(a, b)` と `ROW(a, b)`。アナライザは IN / ANY / ALL の副問い合わせの左辺でしか受け付けない（D3-12）。
/// span.start は `(` または ROW。`(a)` は括弧つきの式で Row ではない。`ROW(a)` は explicit = true の 1 要素
Expr::Row { items: Vec<Expr>, explicit: bool, span: Span }

/// ★ BinaryOp / UnaryOp に op_schema を足す（`OPERATOR(pg_catalog.~)`。通常の演算子は None）
Expr::BinaryOp { op: String, op_schema: Option<Ident>, left: Box<Expr>, right: Box<Expr>, span: Span }
Expr::UnaryOp  { op: String, op_schema: Option<Ident>, expr: Box<Expr>, span: Span }
```

- `Expr::InSubquery { expr, query, negated }` は M1 のまま使う（`IN` = `= ANY`）。`ANY` / `ALL` の配列形（`x = ANY (ARRAY[...])`、`x = ANY (expr)`）は AST に載せず、パーサが 0A000 にする。
- `GROUP BY` の特殊形（`()`、`ROLLUP`、`CUBE`、`GROUPING SETS`）は AST に載せず、パーサが 0A000 にする。`Select.group_by: Vec<Expr>` は M1 のまま。
- `TableAlias { name, columns, span }` と `Ident` は M1 のまま。`TableRef::Function` の `alias` が `None` のとき、アナライザが関数名を別名にする。

### 3.2 Bound の補足（00 §7 の具体化）

00 §7 の型はそのまま使う。次の約束を足す（P0・N1〜N3・L1 が共有する）。

#### 3.2.1 `levels_up` の数え方

- **`Var.levels_up`**: 参照が属するスコープまで、**スコープ**をいくつ外へ出るか。スコープになるのは `BoundSelect`（rtable を持つ）、`BoundUpdate` / `BoundDelete`、および **`BoundSetExpr::Values` の行の式**（rtable が空の 1 つのスコープ。FROM なしの `(select t.a)` と同じ。【検証済み】`select (values (t.a)) from t` は通り、`t.a` は 1 つ外）。**`BoundQuery` 自体はスコープではなく、数えない**（WITH・集合演算・ORDER BY / LIMIT の外枠）。`BoundQuery` の子（CTE の本体、本体の SELECT、集合演算の腕。互いに兄弟）は、その `BoundQuery` を含むスコープを親にした**同じスコープ連鎖**を持つ。FROM 句の派生表と副問い合わせ式の `BoundQuery` は、それを含む SELECT のスコープの 1 つ内側。
- **`CteRef.levels_up`**: **`BoundQuery` の入れ子**を数える。CTE を宣言した `BoundQuery` から見て、参照を含む `BoundQuery` が何段内側か（同じ `BoundQuery` の本体から参照するなら 0。その `BoundQuery` の CTE 本体や、集合演算の腕、FROM 句の派生表、副問い合わせ式の本体から参照するなら 1 以上）。

```sql
-- 例: 外側の t.a を、中の副問い合わせの副問い合わせから参照する
select (select (select x.a + y.b) from u y) from t x
--   x.a: levels_up = 2（一番内側の select は FROM なし。u y の select が 1、t x の select が 2）
--   y.b: levels_up = 1
with c as (select 1 a) select (select a from c) from t
--   `from c`: CteRef { levels_up: 1 }（副問い合わせ式の BoundQuery は、WITH を持つ最上位の BoundQuery の 1 つ内側）
```

- FROM 句の派生表の本体から、同じ FROM 句の左の項目を参照するのは LATERAL（0A000）。親の親（さらに外側）は参照できる（上の例の `x.a` のように、連鎖が派生表を通り抜ける）。
- CTE の本体が外側の列を参照する場合（`with y as (select v) select * from y` が副問い合わせの中にあり、`v` が外側の `x.v`）、`levels_up` は **1**（宣言した問い合わせ自身のスコープは数えない。PostgreSQL の `varlevelsup` より 1 小さい）。【検証済み】PG で動く（`select (with y as (select v) select * from y) from x`）。
- 04 への申し送り: 論理プランへの変換は、`BoundSelect` に入るたびにスコープ連鎖へ積み、`BoundQuery` の子（CTE の本体、本体の SELECT、集合演算の腕）はどれも**同じ連鎖**で処理する。
- **`BoundSetExpr::Values` の行を `lower` するときは、rtable が空のスコープ（`SelectScope`）を 1 つ積む**（`select (values (t.a)) from t` の `t.a` は `levels_up = 1` で来る）。04 §4.1・§5.6 の `Values` の処理に反映してもらう（04 は現状 Values で積まない）。Select 本体の副問い合わせ・派生表は 04 §4.1 のとおり 1 段積む（一致している）。CTE の本体は、04 §5.7 のとおり宣言した `BoundQuery` の外側のスコープ（`scope_depth`）で処理する（一致している）。

#### 3.2.2 結合（JOIN）

- `Rte.columns`（`RteKind::Join`）の並び: USING / NATURAL の併合列（USING の指定順、NATURAL は左の列の順）、左の残りの列、右の残りの列。ON 結合と CROSS は左の全列 ++ 右の全列。`sources[i]` は同じ並びで `columns[i]` の出どころ。
- 併合列の型 `common`: 左右の型が同じならその型（typmod も同じなら保持）、違えば `select_common_type(左, 右, "JOIN/USING")`（typmod は -1）。
- 併合列の `JoinColSource`（PG の `buildMergedJoinVar`）:

| 結合の種類 | source | 式（`common` と違う辺は暗黙キャストを挟む） |
|---|---|---|
| INNER | 左の型 == common なら `Left(i)`、そうでなく右の型 == common なら `Right(j)`、どちらでもなければ `Left(i)` | その辺の列 |
| LEFT | `Left(i)` | 左の列 |
| RIGHT | `Right(j)` | 右の列 |
| FULL | `Coalesce(i, j)` | `COALESCE(l, r)`（`Expr::Coalesce`、型 common） |

- **結合列の参照は解析時に展開する**（D3-2）: `Scope::resolve_column` が結合 RTE の列に当たったら、`sources` をたどって子の列の式を作る（子が結合なら再帰）。`levels_up` は結合 RTE のスコープの段数をそのまま子の Var に使う。`*` と `t.*`（結合の別名のない JOIN は子の表の `t.*`）も同様。
- `FromItem::Join { rte, kind, left, right, on }`: ON 結合は `on = Some(ON の式)`。USING は `on = Some(AND(l_i = r_i, ...))`（各 `=` は元の左右の型で解決。5.3）。NATURAL で共通列がなければ `on = None`。CROSS は `kind = JoinType::Cross`、`on = None`。**`on = None` は「常に真」**で、Left / Right / Full でもそう読む。RIGHT は RIGHT のまま渡す（04 が LEFT に直す）。

#### 3.2.3 集約・グループ化

- `BoundSelect.has_agg` は D3-4。`group_by` は **位置番号・別名を解決した式**（結合の別名は展開済み）。主キーへの関数従属で許された Var は `group_by` の**末尾に追加**される（D3-10）。`having` と `targets`（resjunk を含む）は集約を含んでよく、`filter` は含まない。
- `ExprKind::Aggregate(Box<AggCall>)`: `func` は `aggregates_named` の解決結果、`args` は宣言された引数型へ暗黙キャスト済み（`count("any")` の引数は `oid::ANY` なのでキャストしない。unknown の定数は text に解決する）、`filter` は bool に強制済み、`ty` は `func.result`（typmod は -1）。
- 集約の入れ子（引数・FILTER の中の同じスコープの集約）は 42803。外側のスコープに属する集約は 0A000（D3-4）。

#### 3.2.4 DISTINCT ON

`BoundDistinct::On(positions)`: `positions` は `targets` の位置で、5.6.4 の `result`（ORDER BY の先頭から DISTINCT ON の式に一致する項目を順に、続けて、まだ出ていない DISTINCT ON の式を書かれた順に）。性質: (1) `order_by` の先頭の項目は `positions` の先頭から同じ順で一致する部分（`k` 個）で、`order_by` に DISTINCT ON にない項目があるときは `k = positions.len()`（すべて出ている）。(2) `k < positions.len()` の残りは `order_by` が DISTINCT ON の式だけで尽きているときの追加分で、既定の昇順・NULLS LAST。したがって 04 は **`order_by` の後ろに `positions` の残り（`positions[k..]`、既定の向き）を足した並びで 1 回 Sort し**、Unique は `positions` の式の等しさで判定すればよい（PG の `distinct_pathkeys` が `sort_pathkeys` の先頭に一致する。二度目の整列は要らない）。`order_by` が空なら `positions` の順（昇順・NULLS LAST）で Sort する。**04 §5.5 の `On(pos)` の記述（「Sort の keys は order、空なら ON の式」）は、`distinct on (a, b) .. order by a` のように ORDER BY が ON の式の一部だけのとき足りないので、上の「order ++ 未出の ON の式」に直してもらう。**

#### 3.2.5 CTE

- `BoundQuery.ctes[i]`: `name`、`query`（`resolve_unknowns = true` で解析。D3-15）、`materialize`（`MATERIALIZED` = `Always`、`NOT MATERIALIZED` = `Never`、なし = `Default`）、`col_aliases`（宣言どおり。D3-16）。`CteId(i)` はこの並びの添字。
- `RteKind::CteRef { levels_up, cte }`: `Rte.columns` は CTE の（別名適用後の）出力列、`refname` は FROM 句の別名、なければ CTE 名。FROM 句の別名の列リスト（`from x as a(p, q)`）はその上にさらに適用する。同じ CTE を何度参照してもそのたびに別の `Rte`。

#### 3.2.6 集合演算

`BoundSetExpr::SetOp { op, all, left, right, left_coerce, right_coerce, types }`:

- `types[i]`: 列ごとに左から順に 2 つずつ `select_common_type(左, 右, "UNION" | "INTERSECT" | "EXCEPT")`。typmod は左右の（型, typmod）が同じなら保持、違えば -1。腕の出力の unknown 同士は text。
- 腕が `Select` で、出力列が **unknown 型のリテラル**（`'a'`、`NULL`）のとき、共通型への変換は**腕の `targets[i]` を直接書き換えて**（`coerce_type` のリテラルの経路）腕の出力型を直す。エラー位置はそのリテラル（`select 1 union select 'a'` の `22P02` は `'a'` の位置）。unknown の列が非リテラルになることは M4 ではない（文字列リテラルと NULL だけが unknown になる）。
- それ以外の型違い（`int2` と `int8` など）は `left_coerce` / `right_coerce` の式で表す（00 §7 のとおり、腕の出力の `i` 番目を `Var { rte: RteId(0), col: i, levels_up: 0 }` で参照）。型変換が要らない腕は `None`。
- 出力列名（`BoundQuery.columns`）は最も左の腕の名前。`table_oid` / `attnum` は 0。
- 集合演算の ORDER BY（外側の `BoundQuery.order_by`）は出力列の位置を指す。

#### 3.2.7 VALUES

D3-14。`BoundSetExpr::Values { rows, types }` の各列は `coerce_all_to_common(.., "VALUES")`（M1 のまま）。FROM 句の `(VALUES ...)` は ORDER BY / LIMIT / OFFSET / WITH がなければ `RteKind::Values { rows }`（列名は `column1..`、別名の列リストで改名）、あれば `RteKind::Subquery`。

#### 3.2.8 SubLink の `test`

| 構文 | kind | test（`Out(i)` = `SubLinkOutput(i)`） | 型 |
|---|---|---|---|
| `(SELECT ..)` | `Scalar` | なし | 副問い合わせの出力列の型 |
| `EXISTS (SELECT ..)` | `Exists` | なし | bool |
| `x IN (SELECT ..)`、`x = ANY (SELECT ..)` | `Any` | `x = Out(0)`（`=` は元の型で解決。右辺に型変換が要れば `Cast(Out(0))`） | bool |
| `x NOT IN (SELECT ..)` | `Any` を `Not` で包む | 同上 | bool |
| `x op ANY (SELECT ..)` | `Any` | `x op Out(0)` | bool |
| `x op ALL (SELECT ..)` | `All` | `x op Out(0)` | bool |
| `(a, b) IN (SELECT ..)`、`(a, b) = ANY (..)` | `Any` | `And(a = Out(0), b = Out(1))` | bool |
| `(a, b) NOT IN (..)`、`(a, b) <> ALL (..)` | `Any` を `Not` で包む | `And(a = Out(0), b = Out(1))` | bool |

#### 3.2.9 DML

- `BoundUpdate` / `BoundDelete`: `rtable[0]` が対象表（`RteKind::Table`。別名があればその別名が `refname`）、`from` は `FROM` / `USING` の項目（対象表を含まない）、後続の `rtable[1..]` は FROM 句の RTE。`filter`・代入式・`checks` の Var は `rtable` 全体を指してよい（`checks` は `rte = 0` だけ。00 §7）。
- `BoundReturning.columns`・`targets` は D3-17。

#### 3.2.10 出力列の由来

`OutputColumn { name, ty, table_oid, attnum }` の `table_oid` / `attnum` は、`targets[i]` が**Var そのもの**のとき、その由来をたどって決める（PG の `markTargetListOrigin`）: `RteKind::Table` なら `(table.oid, attnum)`（システム列は負の `attnum`）、`Subquery` なら `query.columns[col]` の由来、`CteRef` なら CTE の出力列の由来、`Values` / `Function` は `(0, 0)`。Var 以外の式（結合の `COALESCE`、演算、集約）は `(0, 0)`。

#### 3.2.11 `BoundQuery` の式の走査（担当 A / P0。`analyzer/bound.rs`）

グループ化の検査（5.5.3）、LIMIT の変数検査（5.12.2）、04 の相関の検出が共有する。

```rust
impl BoundQuery {
    /// この問い合わせの中のすべての式を訪れる（先行順）。`depth` は、その式が属するスコープが、
    /// この問い合わせを含む SELECT のスコープから何段内側か（`depth_base` が基準）。
    /// 訪れるもの: CTE の本体（`depth_base`。本体の SELECT と兄弟）、本体の SELECT の filter・targets・group_by・having・
    /// FromItem::Join.on・RteKind::{Values の行, Function の call}（`depth_base`）、RteKind::Subquery の query（`depth_base + 1`）、
    /// 集合演算の腕（`depth_base`）、VALUES の行（`depth_base`）、order_by が指す式（targets に含まれる）、limit / offset（`depth_base`）。
    /// 式の中の SubLink の query は `depth + 1` で再帰する
    pub fn walk_exprs(&self, depth_base: u16, f: &mut dyn FnMut(&BoundExpr, u16));
}
```

「Var が検査対象の SELECT のスコープを指す」条件は、`v.levels_up == depth`（`depth_base = 0` から始めたとき）。

### 3.3 アナライザの内部の型（`analyzer/scope.rs`、`select.rs`、`cte.rs`。N1〜N3 が共有）

M1 の `Scope`（FROM 項目が最大 1 つ）と `ExprKind`（式の文脈）を置き換える。**M1 の `scope::ExprKind` は `ParseExprKind` に改名する**（00 §6.2 の `ExprKind<C, Q>` と紛らわしい）。

```rust
// analyzer/scope.rs（N1）

/// 式の文脈（PostgreSQL の ParseExprKind）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ParseExprKind {
    SelectTarget, Where, Having, GroupBy, OrderBy, DistinctOn, Limit, Offset,
    JoinOn, FromFunction, Filter, Values, UpdateSet, Returning, ColumnDefault, Check,
}
impl ParseExprKind {
    /// `aggregate functions are not allowed in {name}` の name。None = 集約を許す（SelectTarget、Having、OrderBy、DistinctOn）
    pub(super) fn agg_forbidden_in(self) -> Option<&'static str>;
    /// `cannot use subquery in {name}`（0A000）の name。None = 許す（Check と ColumnDefault だけ Some）
    pub(super) fn sublink_forbidden_in(self) -> Option<&'static str>;
    /// `argument of {name} must not contain variables` などの節の名前（"LIMIT"、"OFFSET"）
    pub(super) fn clause_name(self) -> &'static str;
}

/// 名前空間の 1 項目（PostgreSQL の ParseNamespaceItem）
#[derive(Clone, Debug)]
pub(super) struct NsItem {
    pub(super) rte: RteId,
    /// 修飾名 `t.a` の `t` に使える名前。別名なしの JOIN・派生表・VALUES は None
    pub(super) refname: Option<String>,
    /// 別名のない表だけ: スキーマ名（`public.t.a` を許す）
    pub(super) schema: Option<String>,
    /// 別名で隠された元の表名（診断用。`select t.a from t as t1`）
    pub(super) hidden_name: Option<String>,
    /// 修飾名で見えるか（別名なし JOIN の子は true、別名ありの JOIN の子は false。M4 は JOIN の別名がないので常に true）
    pub(super) rel_visible: bool,
    /// 修飾なしの列名の探索対象か（別名なし JOIN の子の表は false。JOIN の RTE 自身は true）
    pub(super) cols_visible: bool,
    /// 同じ FROM 句の左にある項目（右辺の副問い合わせ・関数・JOIN の右辺・UPDATE の対象表から見たとき）。
    /// 参照すると PG と同じ診断のエラー（LATERAL なら通る）。M4 は通る場合がない
    pub(super) lateral_only: bool,
    /// lateral_only の項目が「LATERAL を付ければ参照できる」位置にあるか（診断の HINT を出すかだけに使う）。
    /// FROM 句のカンマの左の項目と、INNER / LEFT / CROSS JOIN の左の項目は true。
    /// UPDATE / DELETE の対象表（FROM 句から）と、RIGHT / FULL JOIN の左の項目は false
    pub(super) lateral_ok: bool,
}

/// 1 つの SELECT（または UPDATE / DELETE の文）のスコープ。FROM 句が確定した後の読み取り専用のビュー
pub(super) struct Scope<'a> {
    pub(super) parent: Option<&'a Scope<'a>>,     // 外側の SELECT。levels_up = 1 がこれ
    pub(super) rtable: &'a [Rte],
    pub(super) ns: &'a [NsItem],                  // FROM の順（`*` の展開の順）
    /// 関数引数の解析中だけ true（暗黙の LATERAL）。lateral_only の項目に当たったら 0A000（D3-6）
    pub(super) lateral_active: bool,
    pub(super) state: &'a ScopeState,
}
#[derive(Default)]
pub(super) struct ScopeState {
    /// このスコープに属する集約があったか（has_agg の元）
    pub(super) agg_seen: Cell<bool>,
    /// 集約の引数・FILTER を解析している間は > 0（同じスコープの集約の入れ子の検出）
    pub(super) agg_arg_depth: Cell<u32>,
}

#[derive(Clone, Copy)]
pub(super) struct ExprCtx<'a> {
    pub(super) scope: &'a Scope<'a>,
    pub(super) ctes: &'a CteScope<'a>,
    pub(super) kind: ParseExprKind,
}

/// 問い合わせ 1 つ（BoundQuery）の解析環境
#[derive(Clone, Copy)]
pub(super) struct QueryEnv<'a> {
    /// 相関参照の解決先。この問い合わせの本体の SELECT から見て levels_up = 1 の SELECT のスコープ
    pub(super) outer: Option<&'a Scope<'a>>,
    /// この BoundQuery の 1 つ外側の CTE スコープ
    pub(super) ctes: &'a CteScope<'a>,
    /// D3-15
    pub(super) resolve_unknowns: bool,
}

// analyzer/from.rs（N1）。FROM 句の解析中の状態（RteId は rtable の添字。4.5）
pub(super) struct FromBuilder { pub(super) rtable: Vec<Rte>, pub(super) ns: Vec<NsItem> }

// analyzer/cte.rs（N3）
/// BoundQuery の入れ子と 1 対 1 の連鎖（WITH がなくても 1 段作る。CteRef.levels_up を数えるため）
pub(super) struct CteScope<'a> {
    pub(super) parent: Option<&'a CteScope<'a>>,
    /// 解析済み（参照できる）項目。1 項目ずつ足す
    pub(super) visible: RefCell<Vec<CteEntry>>,
    /// この WITH の全項目名（前方参照の診断と RECURSIVE の判定に使う）
    pub(super) all_names: Vec<String>,
    pub(super) recursive: bool,
}
pub(super) struct CteEntry {
    pub(super) name: String,
    pub(super) id: CteId,
    /// 別名適用後の出力列（名前と型）と、それぞれの由来
    pub(super) columns: Vec<RteColumn>,
    pub(super) origins: Vec<(Oid, i16)>,
}
pub(super) enum CteLookup { Found { levels_up: u16, entry: CteEntryRef }, /** 再帰の WITH で宣言済みだが未解析（自己・前方）。0A000 */ Recursive, NotFound }

// analyzer/select.rs（N2）。ORDER BY / GROUP BY / DISTINCT ON が共有する出力列のリスト
pub(super) struct TargetList {
    pub(super) exprs: Vec<BoundExpr>,           // 先頭 n_visible 個が可視、以降は resjunk
    pub(super) names: Vec<String>,              // 出力名（resjunk は空）
    pub(super) origins: Vec<(Oid, i16)>,
    pub(super) n_visible: usize,
}
```

### 3.4 P0 が置く呼び出し口（スタブ）の署名

00 §17 のとおり、P0 が下の関数を**署名つきのスタブ**として置き、`expr.rs`・`select.rs`・`from.rs` の分岐点に呼び出しを足す。以降の担当はスタブの中身だけを書く（呼び出し側は変えない）。

| 呼び出し口 | 署名 | 持ち主 | 呼ぶ側 | スタブの挙動 |
|---|---|---|---|---|
| `agg::analyze_agg_call` | `fn(&self, e: &Expr /* Expr::Function */, cx: &ExprCtx<'_>) -> Result<Option<BoundExpr>>`。集約でなければ `Ok(None)`（呼び出し側が通常の関数として解決する） | N2 | `expr.rs` の `Expr::Function` 枝（関数名が `aggregates_named` にあるか未対応の集約名のとき、または `star` / `distinct` / `filter` のとき） | M1 と同じ `0A000 aggregate functions are not supported yet` |
| `agg::check_grouping` | `fn(&self, sel: &mut BoundSelect, scope: &Scope<'_>) -> Result<()>` | N2 | `select.rs` の `analyze_select` の最後 | `Ok(())` |
| `select::resolve_group_by` | `fn(&self, items: &[Expr], tl: &mut TargetList, cx: &ExprCtx<'_>) -> Result<Vec<BoundExpr>>` | N2 | 同上 | 空でなければ `0A000 GROUP BY is not supported yet` |
| `select::resolve_distinct_on` | `fn(&self, exprs: &[Expr], keys: &[BoundSortKey], tl: &mut TargetList, cx: &ExprCtx<'_>) -> Result<Vec<usize>>` | N2 | 同上 | `0A000 SELECT DISTINCT ON is not supported yet` |
| `sublink::analyze_sublink` | `fn(&self, e: &Expr /* Subquery / Exists / InSubquery / QuantifiedSubquery */, cx: &ExprCtx<'_>) -> Result<BoundExpr>` | N3 | `expr.rs` の該当の枝 | `0A000 subqueries are not supported yet` |
| `setop::analyze_set_operation` | `fn(&self, body: &QueryBody /* SetOp */, q: &Query, env: &QueryEnv<'_>) -> Result<BoundQuery>`（`q` の ORDER BY / LIMIT / OFFSET を外枠に付ける） | N3 | `select.rs` の `analyze_query` | `0A000 UNION/INTERSECT/EXCEPT is not supported yet` |
| `cte::analyze_with` | `fn<'e>(&self, with: &With, env: &QueryEnv<'e>) -> Result<WithResult<'e>>`（`WithResult { ctes: Vec<BoundCte>, scope: CteScope<'e> }`） | N3 | `select.rs` の `analyze_query` | `with` があれば `0A000 WITH is not supported yet` |
| `cte::find_cte` | `fn(&self, name: &str, ctes: &CteScope<'_>) -> CteLookup` | N3 | `from.rs`（FROM 句の表名の解決） | 常に `NotFound` |
| `from::analyze_from_clause` | `fn(&self, from: &[TableRef], fb: &mut FromBuilder, env: &QueryEnv<'_>) -> Result<Vec<FromItem>>` | N1 | `select.rs` / `dml.rs` | 単一の `TableRef::Table`（M1 と同じ）だけ受理 |
| `Scope::resolve_column` / `Scope::expand_star` | 3.3 のとおり（5.2） | N1 | `expr.rs` / `select.rs` | 単一の表（`rte = 0`）だけ |
| `resolve::qualified_operator`、`resolve::collate` | `fn(&self, schema: Option<&Ident>, name: &str, ..)`、`fn(&self, e: BoundExpr, collation: &ObjectName, span: Span) -> Result<BoundExpr>` | N1 | `expr.rs` の `BinaryOp` / `UnaryOp` / `Collate` 枝 | `op_schema` があれば 0A000、`Collate` は 0A000 |

`Expr::Row` は `expr.rs` の一般の分岐では 0A000（`row constructors is not supported yet`）。`sublink.rs` だけが左辺の Row を受け取る。

### 3.5 カタログへの前提

| 前提 | 出どころ |
|---|---|
| `CatalogReader::aggregates_named(name) -> Vec<&'static BuiltinAggregate>`、`BuiltinAggregate { oid, name, args, result, kind }`、`AggKind` | 00 §11.2・§11.3（09 §8 が 43 行の表を書く） |
| `TableDef::primary_key() -> Option<&Arc<IndexDef>>`、`IndexDef.columns[i].attnum` | 00 §11.1（関数従属の判定） |
| `FnKind::Set(SetFn)` と `SetFn.column_name`、`oid::ANY`（2276） | 09 §10.1、09 の P-5・P-7 |
| `CatalogReader::functions_named` / `operators_named` / `find_cast` / `type_by_oid` | M2 のまま |
| 存在するスキーマの判定（`OPERATOR(nosuch.+)` の `3F000`、`COLLATE nosuch."C"`） | M2 の `analyzer/expr.rs` のとおり `pg_catalog` / `public` を既知とする小さな関数 `schema_exists(&str)`（M4 には `CREATE SCHEMA` がない。`pg_toast` も既知。N1 が `resolve.rs` に置く） |

---

## 4. 処理の流れ

### 4.1 `analyze` の入口

```rust
// analyzer/mod.rs（署名は M1 から変えない）
pub fn analyze(stmt: &Statement, catalog: &dyn CatalogReader) -> Result<BoundStatement>;

pub(crate) struct Analyzer<'a> {
    pub(crate) catalog: &'a dyn CatalogReader,
    /// スカラー副問い合わせ式の出力名（FigureColname が使う。5.11）。キーは AST の Subquery ノードの span
    pub(crate) sublink_names: RefCell<HashMap<(u32, u32), String>>,
}
```

| 文 | 処理 |
|---|---|
| `Statement::Query(q)` | `analyze_query(q, &QueryEnv::root(true))` → `BoundStatement::Select(Box<BoundQuery>)`（`QueryEnv::root(resolve_unknowns)` は `outer = None`、空の `CteScope` を根にした環境） |
| `Insert` | `source`（`InsertSource::Query`）を `analyze_query(q, env { resolve_unknowns: false })`。DEFAULT VALUES と VALUES は M1 のまま（VALUES は `BoundSetExpr::Values`）。`returning` は 4.7 |
| `Update` / `Delete` | 4.7 |
| `Explain(e)` | `10-explain-copy-compat.md` が枝を書く。内側の `e.statement` に同じ `analyze` を再帰する |
| `CreateTable` ほかの DDL | `07`、`08`、`09` |
| CHECK・DEFAULT | `analyze_table_checks` と `analyze_column_default` は署名を変えない。スコープは「`rte = 0` の表 1 つ」の `Scope`（`ParseExprKind::Check` / `ColumnDefault`）。出力は `rte = 0` の Var だけ（`lower_single_rel` の前提。00 §6.5） |

### 4.2 `analyze_query`

```rust
impl Analyzer<'_> {
    /// WITH + 本体 + ORDER BY / LIMIT / OFFSET → BoundQuery（N3 が setop・cte を書き、N2 が select の本体を書く）
    pub(super) fn analyze_query(&self, q: &Query, env: &QueryEnv<'_>) -> Result<BoundQuery>;
}
```

1. `check_stack_depth()?`（`sql::stack`。副問い合わせの入れ子が深いときの `54001`。パーサと同じ上限の考え方）。
2. WITH があれば `cte::analyze_with(with, env)` で `ctes` を作り、本体の `CteScope` を得る。なければ空の `CteScope::level(env.ctes)`（`CteRef.levels_up` を数えるため、WITH がなくても 1 段作る）。`q.body` が `Nested(inner)` で `q.with` があるときは、`inner` に外側の WITH を移して解析する（両方に WITH があれば `0A000 WITH on a parenthesized query that has its own WITH is not supported yet`）。
3. 本体で分岐する。
   - `Select(s)` → `analyze_select(s, q, &env2)`（4.3）。
   - `Values(v)` → `analyze_values_query(v, q, &env2)`（D3-14。M1 の `analyze_values` を改造）。
   - `SetOp { .. }` → `setop::analyze_set_operation(&q.body, q, &env2)`。
   - `Nested(inner)` → 外枠の ORDER BY / LIMIT / OFFSET が空なら `analyze_query(inner, &env2')`。`inner` 側が空なら外枠を付けた `inner` の本体を解析（M1 の `analyze_query` と同じ付け替え）。両方にあるのはパーサが `multiple ORDER BY clauses not allowed` で弾くので来ない（来たら `Error::internal`）。
4. `BoundQuery { ctes, body, order_by, limit, offset, columns }` を返す。

### 4.3 `analyze_select` の手順（PostgreSQL の `transformSelectStmt` の順序）

エラーが複数あるときにどれが先に出るかは PG の順序で決まる。順序を守る。

```rust
fn analyze_select(&self, s: &Select, q: &Query, env: &QueryEnv<'_>) -> Result<BoundQuery> {
    // 1. FROM（RTE を積み、名前空間を作る）
    let mut fb = FromBuilder::default();
    let from = self.analyze_from_clause(&s.from, &mut fb, env)?;
    let state = ScopeState::default();
    let ns = fb.ns_for_expressions();                 // lateral_only をすべて false にした写し
    let scope = Scope { parent: env.outer, rtable: &fb.rtable, ns: &ns, lateral_active: false, state: &state };
    // 2. SELECT 句（`*` の展開、別名、出力名、出力列の由来。env.resolve_unknowns なら unknown を text に）
    let mut tl = self.transform_target_list(&s.targets, &scope, env)?;
    // 3. WHERE → coerce_to_boolean("WHERE")
    // 4. HAVING → coerce_to_boolean("HAVING")（HAVING は GROUP BY より先に解析する。検査は 9 でまとめて行う）
    // 5. ORDER BY（出力名が先。resjunk を tl に足す）
    // 6. GROUP BY（入力列が先。D3-3）
    // 7. DISTINCT（42P10 の検査）/ DISTINCT ON（5.6）
    // 8. LIMIT / OFFSET（int8 に強制。現スコープの Var を含まない。5.12）
    // 9. has_agg = state.agg_seen || !group_by.is_empty() || having.is_some()。true なら check_grouping（5.5）
    // 10. BoundSelect { rtable, from, filter, group_by, having, has_agg, targets, n_visible, distinct } と
    //     BoundQuery { ctes: vec![] /* analyze_query が WITH の結果で置き換える */, body: Select, order_by, limit, offset, columns } を組み立てる
}
```

- 2〜8 の式の解析は `ExprCtx { scope: &scope, ctes: env.ctes, kind }`（`env` は `analyze_query` が本体用に作った `QueryEnv`）。`kind` は節ごとに `SelectTarget`、`Where`、`Having`、`OrderBy`、`GroupBy`、`DistinctOn`、`Limit`、`Offset`。
- SELECT 句の後で WHERE / HAVING を解析する（PG と同じ。`select zz from t where yy = 1` は `zz` のエラーが先）。
- ORDER BY を GROUP BY より先に解析する（PG と同じ。どちらも resjunk の式を `tl` に足すので、先に解析した方の位置が小さくなる）。
- `UNKNOWN` の解決は SELECT 句の直後（M1 のまま。`resolve_unknown`）。ORDER BY の式が unknown の定数なら M1 と同じく text にする。

### 4.4 FROM 句の処理

```rust
impl Analyzer<'_> {
    /// カンマ区切りの項目を左から順に解析し、RTE を fb.rtable に積み、名前空間を fb.ns に足す。
    /// 戻り値は FromItem の並び（カンマ区切りごと）
    pub(super) fn analyze_from_clause(&self, from: &[TableRef], fb: &mut FromBuilder, env: &QueryEnv<'_>) -> Result<Vec<FromItem>>;
}
```

1. 項目 `i` を解析する直前に、それまでに積まれた `fb.ns`（左の項目）を **`lateral_only = true` にした写し**を作り、その写しと `env.outer` から、右辺の副問い合わせ・関数引数・JOIN の右辺のための `Scope`（`lateral_active` は関数引数の解析中だけ true）を作る。
2. 項目の種類で分岐する（5.3、5.4）。表（CTE を含む）・派生表・関数・VALUES は RTE を 1 つ積んで `FromItem::Scan(rte)`。JOIN は左・右を再帰してから結合の RTE を積む（5.3）。
3. 項目の名前空間 `new_ns` を、**既存の `fb.ns` と衝突しないか**検査する（`checkNameSpaceConflicts`。`refname` が同じ項目が 2 つあれば `42712 table name "t" specified more than once`。位置なし）。通れば `fb.ns` に足す。JOIN の ON 句の中の衝突検査は、その JOIN の左右の名前空間だけを見る。
4. 全部の項目が済んだら、`fb.ns` の `lateral_only` をすべて false にした写しを `ns_for_expressions()` で作り、式の解析に使う。

UPDATE / DELETE では、対象表の `NsItem` を最初に `fb.ns` に置いて `lateral_only = true`（FROM 句の副問い合わせ・関数・JOIN の ON から見えない。【検証済み】`update r set v = 1 from t join u on u.a = r.id` は `42P01 invalid reference to FROM-clause entry for table "r"` + DETAIL）にし、FROM 句の解析が終わってから `false` にして WHERE 以降に見せる（4.7）。

### 4.5 `RteId` の採番

`RteId(n)` は `rtable` の添字（u16。65535 を超えたら `54000 too many range table entries`。00 §4.3 の 6）。**子を先に、JOIN の RTE を後に**積む（後行順。`from t join u on ..` の RTE は `t` = 0、`u` = 1、JOIN = 2）。DML は対象表が 0。派生表の RTE は、本体の解析が終わってから積む（本体の `BoundQuery` は別のスコープなので RteId を消費しない）。CTE の参照も 1 回ごとに RTE を 1 つ積む。

### 4.6 CTE と派生表の積み方

スコープ連鎖は 3.2.1 のとおり 2 本ある。`QueryEnv` が両方を持ち回る。

```
analyze_query(Q1: WITH c AS (..) SELECT .. FROM (SELECT .. FROM c) d, t WHERE x IN (SELECT .. FROM c))
 ├ CteScope(Q1)（parent = Q1 の外側の CteScope）。c の本体は CteScope(Q1) の 1 段内側で解析し（前の CTE は見える。c 自身は未登録なので見えない）、解析が済んでから c を足す
 ├ Q1 の本体の Scope（FROM: d, t）
 │   ├ 派生表 d の BoundQuery Q2: CteScope(Q2, parent = CteScope(Q1))   `from c` → CteRef { levels_up: 1 }
 │   │   Scope(Q2 の本体, parent = Q1 の Scope を lateral_only で見せたもの)
 │   └ WHERE の副問い合わせ Q3: CteScope(Q3, parent = CteScope(Q1))      `from c` → CteRef { levels_up: 1 }
 │       Scope(Q3 の本体, parent = Q1 の Scope)    Q1 の列は Var { levels_up: 1 }
 └ 同じ BoundQuery の本体から `from c` → CteRef { levels_up: 0 }
```

- CTE `i` の本体は、`env.outer`（Q1 の外側のスコープ）を `outer` にして `analyze_query` する。**Q1 自身のスコープ（Q1 の FROM の項目）は連鎖に入れない**。Var の `levels_up` は 3.2.1 のとおり Q1 の外側から数える。
- `analyze_with` は CTE を**宣言順に 1 つずつ**解析し、解析が済んだものを `CteScope.visible` に足す（次の CTE から見える）。自己参照・前方参照は `all_names` にあって `visible` にない名前として検出する（非再帰: `42P01` + DETAIL / HINT、`RECURSIVE` 付き: `0A000`。5.9）。

### 4.7 DML の手順

**UPDATE**（`PG:analyze.c` の `transformUpdateStmt` の順序。R12）

1. 対象表を `resolve_table`（42P01）し、`check_writable`（システムカタログは `42501`。M2 のまま）。`rtable[0]` に積み、`NsItem` を `lateral_only = true` で `fb.ns` に置く。
2. FROM 句を `analyze_from_clause`。
3. 対象表の `lateral_only` を false に戻し、`Scope` を作る。
4. WHERE → `coerce_to_boolean("WHERE")`。
5. RETURNING（`RETURNING_ENABLED` が false のときは、手順 1 の前に `0A000 RETURNING is not supported yet` を返す。D3-17）。
6. SET 句: 各代入について、列を探し（なければ `42703 column "x" of relation "r" does not exist`）、重複は `42601 multiple assignments to same column "v"`（位置は 2 つ目の列名）、システム列は `0A000 cannot assign to system column "ctid"`、右辺を `ParseExprKind::UpdateSet` で解析して `coerce_assignment`（M2 のまま）。`DEFAULT` は `UpdateSource::Default`。
7. `checks`（対象表だけ）と `not_null`。

**DELETE**: 対象表 → USING → WHERE → RETURNING。

**INSERT**: 対象表 → 列リスト → `source`（`resolve_unknowns = false`）→ 代入の型検査（M2 のまま）→ RETURNING。`INSERT ... SELECT` の SELECT が ORDER BY / DISTINCT / LIMIT / OFFSET / 集合演算を持つときの `coercions` は M2 のとおり（`source` の出力列を `Var { rte: 0, col: i }` で参照する式を、`BoundQuery` の外側に置く）。

---

## 5. モジュールごとの仕様

### 5.1 パーサ（S1。`sql/ast.rs`、`sql/parser/{select,expr,dml,mod}.rs`）

#### 5.1.1 現状の調査結果（M3 実装途中の `sql/` を読んだ結果）

パーサは JOIN・副問い合わせ式・集合演算・`DISTINCT ON`・`GROUP BY` / `HAVING`・`UPDATE ... FROM`・`DELETE ... USING`・`RETURNING` を**すでに AST に載せて受理**している。拒否は 2 種類ある: (a) パーサが `0A000`（`not_supported`）にするもの、(b) AST に載るが**アナライザが `0A000`** にするもの。

(a) パーサが拒否しているもの:

| 構文 | 場所（関数） | 現状の文言 | M4 |
|---|---|---|---|
| `WITH` | `select.rs` `parse_query_level` | `WITH is not supported yet` | **実装**（5.1.2 の 1） |
| `LATERAL` | `select.rs` `parse_table_primary_level` | `LATERAL is not supported yet` | 維持（D3-6） |
| `ONLY` | 同上 | `ONLY is not supported yet` | **受理**（DML の `parse_relation_expr` と同じ。`ONLY t` と `ONLY (t)`。継承がないので無視） |
| FROM 句の関数 | 同上 | `functions in FROM is not supported yet` | **実装**（5.1.2 の 2） |
| `JOIN ... USING (...) AS alias` | `select.rs` `parse_joins_level` | `JOIN USING aliases is not supported yet` | 維持（D3-11） |
| 括弧付き JOIN への別名 | `parse_table_primary_level` | `aliases for parenthesized joins is not supported yet` | 維持（D3-11） |
| `TABLESAMPLE` | 同上 | `TABLESAMPLE is not supported yet` | 維持 |
| `FILTER (WHERE ...)` | `expr.rs` `parse_function_call` | `FILTER is not supported yet` | **実装**（5.1.2 の 3） |
| `COLLATE` | `expr.rs` `parse_keyword_infix` | `COLLATE is not supported yet` | **実装**（5.1.2 の 5） |
| 行値 `(a, b)`、`ROW(..)` | `expr.rs` `parse_paren_expr`、`parse_keyword_primary` | `row constructors is not supported yet`、`ROW is not supported yet` | **`Expr::Row` を作る**（アナライザが IN / ANY / ALL の左辺以外を拒否。5.1.2 の 7） |
| 集約内の `ORDER BY`、`WITHIN GROUP`、`VARIADIC`、名前付き引数 | `parse_function_call` | 各 `... is not supported yet` | 維持 |
| `OVER`（ウィンドウ関数）、`WINDOW` 句 | `parse_function_call`、`select.rs` `parse_select` | `window functions is not supported yet`、`WINDOW is not supported yet` | 維持（文言は 5.1.4 の表の形に直してよい） |
| `FOR UPDATE / SHARE` | `select.rs` `finish_query` | `FOR UPDATE/SHARE is not supported yet` | 維持 |
| `FETCH ... WITH TIES`、`ORDER BY ... USING`、`SELECT INTO` | `select.rs` | 各 `... is not supported yet` | 維持 |
| 配列の添字、`ARRAY[..]`、フィールド選択 | `expr.rs` | 各 `... is not supported yet` | 維持（M5） |
| `GREATEST` / `LEAST` / `OVERLAY` / `TREAT` / `NORMALIZE` / `GROUPING` / `MERGE_ACTION`、`xml*` / `json*` | `parse_keyword_primary` | 各 `... is not supported yet` | 維持（`GROUPING` は 5.1.4 の表） |
| `CURRENT_DATE`、`CURRENT_TIMESTAMP`、`LOCALTIMESTAMP` | `parse_keyword_primary` | `... is not supported yet` | **09 の依頼**（`09-types-functions.md` §6.4）。S1 が 09 の仕様で実装する。この章は触れない |
| `UPDATE` の複数列 `SET (a, b) = ...`、`WHERE CURRENT OF`、`ON CONFLICT`、添字・フィールドへの代入 | `dml.rs` | 各 `... is not supported yet` | 維持。`OVERRIDING` は 08 |

(b) AST に載るがアナライザが `0A000` にするもの（M1・M2 の `analyzer/select.rs`、`expr.rs`、`dml.rs`）:

| 構文 | 現状の文言 | M4 |
|---|---|---|
| `SELECT DISTINCT ON` | `SELECT DISTINCT ON is not supported yet` | **実装**（N2。5.6） |
| `GROUP BY`、`HAVING` | `GROUP BY is not supported yet`、`HAVING ...` | **実装**（N2） |
| JOIN、FROM 句の副問い合わせ、2 つ以上の FROM 項目 | `JOIN is not supported yet`、`subquery in FROM ...`、`JOIN (more than one table in FROM) ...` | **実装**（N1。5.3） |
| `UNION` / `INTERSECT` / `EXCEPT`、`Nested` | `UNION/INTERSECT/EXCEPT is not supported yet`、`nested ORDER BY / LIMIT ...` | **実装**（N3。5.8） |
| 集約関数（`count(*)`、`DISTINCT`、`AGGREGATES` の名前） | `aggregate functions are not supported yet` | **実装**（N2。5.5）。未対応の集約名は D3-18 |
| 副問い合わせ式（`IN (SELECT)`、`EXISTS`、スカラー） | `subqueries are not supported yet` | **実装**（N3。5.7） |
| `IS DISTINCT FROM` | `IS DISTINCT FROM is not supported yet` | **維持**（[03-Q12]） |
| `UPDATE ... FROM`、`DELETE ... USING` | `UPDATE ... FROM is not supported yet`、`DELETE ... USING ...` | **実装**（N1。5.10） |
| `RETURNING` | `RETURNING is not supported yet` | 解析は**実装**し、`RETURNING_ENABLED` が false の間は同じ文言で 0A000（D3-17） |

M1 のパーサの性質で M4 が前提にするもの: 式の `span.start` は PG の `location`（中置・後置の演算子は演算子のトークン、`x::t` は `::`）。構文エラーは `syntax error at or near "..."` か `at end of input`。`GROUP BY ALL` / `GROUP BY DISTINCT` は受理して無視（PG17 も `set_quantifier` として受理する）。`(SELECT ...) LIMIT 1` のように括弧つき問い合わせに外側の ORDER BY / LIMIT / OFFSET を付けると内側に統合し、両方にあると `42601 multiple ORDER BY clauses not allowed`（PG と同じ）。

#### 5.1.2 追加する文法

gram.y（`PG:src/backend/parser/gram.y`）の規則を括弧で示す。

**1. WITH**（`with_clause`、`common_table_expr`、`opt_materialized`）

```
query      := [with_clause] set_expr [sort_clause] [limit_clause | offset_clause | fetch_clause]*
with_clause:= WITH [RECURSIVE] cte (',' cte)*
cte        := ColId ['(' ColId (',' ColId)* ')'] AS [MATERIALIZED | NOT MATERIALIZED] '(' query ')'
```

- `parse_query_level` の先頭で `with` を見たら `With` を作り、続けて `set_expr` 以降を従来どおり解析する。WITH は**集合演算全体**にかかる（【検証済み】`with x as (..) select v from x union all select v from x` が通る）。括弧の中の問い合わせにも付けられる（`parse_set_primary` の `(` → `parse_query` が再帰するので追加の作業なし）。
- `AS` の後: `materialized` なら `Some(true)`、`not` + `materialized` なら `Some(false)`（`not` の次が `materialized` のときだけ。`NOT_LA`）。その後 `(` `query` `)`。CTE の中身が `insert` / `update` / `delete` / `merge` なら `0A000 data-modifying statements in WITH is not supported yet`（位置はそのキーワード）。
- 閉じ括弧の次が `search` / `cycle` なら `0A000 SEARCH and CYCLE clauses are not supported yet`（位置はそのキーワード）。
- WITH 句の次が `insert` / `update` / `delete` / `merge` なら `0A000 WITH clause on INSERT, UPDATE or DELETE is not supported yet`（位置はそのキーワード）。`INSERT INTO t WITH x AS (..) SELECT ...` は `parse_insert` の中の `parse_query` が WITH を受け取るので動く。
- `RECURSIVE` は受理して `With.recursive = true`（D3-7）。
- `Query.span` は WITH の先頭から。`Cte.span` は CTE 名の先頭から閉じ括弧まで。

**2. FROM 句の関数**（`func_table`、`table_ref`）

```
table_primary := ... | func_name '(' [a_expr (',' a_expr)*] ')' [alias_clause]
```

`parse_table_primary_level` の `parse_object_name` の直後に `(` があれば、実引数を `parse_expr_list`（空もありうる）で読む。その後に `with` `ordinality` なら `0A000 WITH ORDINALITY is not supported yet`（位置は `with`）。`rows` `from` なら `0A000 ROWS FROM is not supported yet`（位置は `rows`。`rows` は表名にも使える非予約語なので、**直後が `from`** のときだけ）。別名は従来の `parse_opt_alias`（`AS g(x)` の列別名リストを含む。`g(x int)` のような列定義は構文エラーのまま）。

**3. FILTER**（`filter_clause`）

`func_application [filter_clause] [over_clause]`。`parse_function_call` の閉じ括弧の後で、`filter` の次が `(` のとき: `(` `where` a_expr `)` を読んで `Expr::Function.filter` に入れる（`where` がなければその位置の構文エラー）。その後に `over` なら従来の 0A000。**パーサは関数が集約かどうかを知らない**ので、集約でない関数への FILTER はアナライザが `42809` にする（5.5）。

**4. `x op ANY | SOME | ALL (query)`**（`a_expr subquery_Op sub_type select_with_parens`）

`parse_infix` の `TokenKind::Op` 枝で、演算子を読んだ**直後**に `any` / `some` / `all` があり、その次が `(` なら右辺は `( query )` に固定する（優先順位に関わらない右辺）。`(` の後ろが問い合わせの先頭（`select` `values` `with` `table`、括弧の入れ子を許す `parens_then_query`）なら `Expr::QuantifiedSubquery`、そうでなければ **`0A000 ANY/ALL with an array is not supported yet`**（位置は `any` / `some` / `all`）。`LIKE` / `ILIKE` / `NOT LIKE` / `NOT ILIKE`（`parse_predicate`）の直後も同じ（`op` は `~~`、`~~*`、`!~~`、`!~~*`）。`OPERATOR(schema.op)` の後も同じ（`op_schema` を持つ）。左辺は演算子のトークンの優先順位で決まる（`1 + 2 = ANY (..)` の左辺は `1 + 2`）。`IN (query)` / `NOT IN (query)` は M1 の `InSubquery` のまま。

**5. COLLATE**（`a_expr COLLATE any_name`）

`parse_keyword_infix` の `"collate"` を `Expr::Collate` を作る枝にする。`collation` は `parse_object_name`（`ColId ('.' ColLabel)*`。`collate default` は `default` が予約語なので**構文エラー**【検証済み】だが、`collate "default"` と `collate pg_catalog."default"` は通る）。優先順位は 14（`+ -` より上、単項マイナスより下）。列定義の `COLLATE`（`ddl.rs`）は 07 の担当で、この章では触れない。

**6. `OPERATOR(schema.op)`**（`qual_Op`）

`OPERATOR` `(` [`ColId` `.`]* 演算子トークン `)`。**中置**（`infix_prec` の Word 枝で `operator` の次が `(` なら優先順位 9）と**前置**（`parse_prefix` で `operator` + `(`）の両方、および `ANY` / `ALL` の前（4）に置ける。スキーマ部が 1 つなら `op_schema = Some(..)`、0 個（`OPERATOR(+)`）なら `None`、2 つ以上は `0A000 OPERATOR with a database-qualified name is not supported yet`。優先順位は「その他の演算子」の段（9。比較の 5 より高く、`+ -` の 10・`*` の 11 より低い）。【検証済み】`1 operator(pg_catalog.+) 2 * 3` は 7（`*` が先）、`1 + 2 operator(pg_catalog.*) 3` は 9（`+` が先）。`OPERATOR` は非予約語なので、**`(` が続くときだけ**この構文とみなす。

演算子トークン `~ ~* !~ !~*`（正規表現）は M1 の字句解析がすでに 1 つの `Op` として切り出し、`op_prec` が `P_OP` を返す（`!~*` の末尾の `*` は `+` / `-` の分割規則の対象外）。**パーサの追加作業はない**。演算子の行は 09。

**7. 行値**（`row`、`implicit_row`）

`parse_paren_expr` で最初の式の後に `,` が来たら、残りの式を読んで `Expr::Row { explicit: false }`（`(a, b)` は 2 要素以上）。`parse_keyword_primary` の `row` + `(` は `Expr::Row { explicit: true }`（`ROW()` の空も可）。`Row` をどこに置けるかはアナライザが決める（5.7、5.12）。

**8. FROM の `ONLY` と `*`**

`only` + 表名、`only` + `(` 表名 `)`、表名 + `*` を、DML の `parse_relation_expr` と同じく受理して無視する（`TableRef::Table` をそのまま返す）。

**9. GROUP BY の特殊形**

`parse_select` の `GROUP BY` の項目の先頭が `(` + `)`、`rollup` + `(`、`cube` + `(`、`grouping` + `sets` のどれかなら、`0A000 GROUPING SETS / ROLLUP / CUBE / empty grouping sets are not supported yet`（位置はその項目の先頭）。リストの途中の項目（`GROUP BY a, ROLLUP (b)`）も同じ。

#### 5.1.3 優先順位と結合性（gram.y に従う。低い順）

M1 の表（`parser/expr.rs` の冒頭）に、この章の追加を入れる。`P_` は `expr.rs` の定数。

| 段 | 演算子 | 結合性 | M4 の追加 |
|---|---|---|---|
| — | `UNION` `EXCEPT` < `INTERSECT`（集合演算） | 左 | 変更なし |
| 1 | `OR` | 左 | |
| 2 | `AND` | 左 | |
| 3 | `NOT`（前置） | 右 | |
| 4 | `IS` `ISNULL` `NOTNULL` | 非結合 | |
| 5 | `< > = <= >= <>` | 非結合 | `x op ANY\|ALL (query)` の左辺の優先順位はこの段（演算子のトークンの段） |
| 6 | `BETWEEN` `IN` `LIKE` `ILIKE` `SIMILAR`（と前の `NOT`） | 非結合 | `LIKE ANY (query)` も同じ段 |
| 9 | その他の演算子（`\|\|`、`~`、`!~*` など）と `OPERATOR(...)` | 左 | `OPERATOR(...)` を足す |
| 10 | `+ -` | 左 | |
| 11 | `* / %` | 左 | |
| 12 | `^` | 左 | |
| 13 | `AT` | 左 | |
| 14 | `COLLATE` | 左 | **足す**（`'x' COLLATE "C"` は `+` より強く結びつく: `'a' OPERATOR(pg_catalog.~) 'a' COLLATE pg_catalog.default` の `COLLATE` は右辺の一部） |
| 15 | 単項 `+ -` | 右 | |
| 16 | `[ ]` | 左 | |
| 18 | `::` | 左 | |

#### 5.1.4 0A000 の一覧（文言・位置・検出する層）

文言は `Error::not_supported(<全文>)`。SQLSTATE は `0A000`。位置は `with_span` で付ける。slt は `statement error (0A000)` で SQLSTATE だけを照合する（文言は yuzhu 独自）。

| 構文 | 文言 | 位置 | 層 |
|---|---|---|---|
| `LATERAL` | `LATERAL is not supported yet` | `lateral` | パーサ |
| 関数引数が左の FROM 項目を参照（暗黙の LATERAL） | `implicit LATERAL reference in a function in FROM is not supported yet` | その列参照 | アナライザ（5.2） |
| `WITH ORDINALITY` | `WITH ORDINALITY is not supported yet` | `with` | パーサ |
| `ROWS FROM` | `ROWS FROM is not supported yet` | `rows` | パーサ |
| FROM 句の `generate_series` 以外の関数 | `function "lower" in FROM is not supported yet` | 関数名 | アナライザ（5.4） |
| `TABLESAMPLE` | `TABLESAMPLE is not supported yet` | `tablesample` | パーサ |
| `JOIN ... USING (...) AS alias` | `JOIN USING aliases is not supported yet` | `as` | パーサ |
| 括弧付き JOIN への別名 | `aliases for parenthesized joins is not supported yet` | 別名 | パーサ |
| `WITH RECURSIVE` の再帰参照 | `WITH RECURSIVE is not supported yet` | 参照している表名 | アナライザ（5.9） |
| データ変更文を含む CTE | `data-modifying statements in WITH is not supported yet` | `insert` 等 | パーサ |
| `WITH ... INSERT/UPDATE/DELETE` | `WITH clause on INSERT, UPDATE or DELETE is not supported yet` | `insert` 等 | パーサ |
| CTE の `SEARCH` / `CYCLE` | `SEARCH and CYCLE clauses are not supported yet` | キーワード | パーサ |
| `OVER`（ウィンドウ関数） | `window functions (OVER clause) is not supported yet` | `over` | パーサ |
| `WINDOW` 句 | `WINDOW clause is not supported yet` | `window` | パーサ |
| `GROUP BY ()` / `ROLLUP` / `CUBE` / `GROUPING SETS` | `GROUPING SETS / ROLLUP / CUBE / empty grouping sets are not supported yet` | 項目の先頭 | パーサ |
| `GROUPING(...)` | `GROUPING is not supported yet` | `grouping` | パーサ（M1 のまま） |
| `FOR UPDATE` / `FOR SHARE` | `FOR UPDATE/SHARE is not supported yet` | `for` | パーサ（M1 のまま） |
| 集約内の `ORDER BY` | `ORDER BY in aggregate calls is not supported yet` | `order` | パーサ（M1 のまま） |
| 外側のスコープに属する集約 | `aggregate functions of an outer query level are not supported yet` | 集約の呼び出し | アナライザ（5.5） |
| 未対応の集約名 | `aggregate function string_agg is not supported yet` | 関数名 | アナライザ（5.5） |
| `ANY` / `ALL` の配列形 | `ANY/ALL with an array is not supported yet` | `any` / `all` | パーサ |
| `IN` / `ANY` / `ALL` の左辺以外の行値 | `row constructors is not supported yet` | 行値の先頭 | アナライザ（5.12） |
| 行値のその他の演算子（`(a,b) < ANY (..)` など） | `row comparison with this operator in a subquery is not supported yet` | 演算子 | アナライザ（5.7） |
| 全行参照（`count(t)`、`select t from t`） | `whole-row reference to "t" is not supported yet` | その名前 | アナライザ（5.2） |
| `IS DISTINCT FROM` | `IS DISTINCT FROM is not supported yet` | 演算子 | アナライザ（M1 のまま） |
| 副問い合わせを含む RETURNING、他の FROM 項目を参照する RETURNING（`UPDATE ... FROM ... RETURNING *` を含む） | `subquery in RETURNING is not supported yet`、`RETURNING referencing other tables is not supported yet` | 式 | アナライザ（5.10） |
| `RETURNING`（解禁前） | `RETURNING is not supported yet` | `returning` の項目 | アナライザ（D3-17） |
| 集合返却関数を FROM 句以外で使用 | `set-returning functions are only supported in the FROM clause`（09 §10.1） | 関数名 | アナライザ（5.12） |

### 5.2 スコープと名前解決（N1。`analyzer/scope.rs`）

#### 5.2.1 列参照の解決

```rust
impl Scope<'_> {
    /// `a`、`t.a`、`schema.t.a`、`db.schema.t.a` を解決する。結合列は展開した式（D3-2）、それ以外は Var
    pub(super) fn resolve_column(&self, a: &Analyzer<'_>, parts: &[Ident], span: Span) -> Result<BoundExpr>;
}
```

1. 5 部以上は `42601 improper qualified name (too many dotted names): a.b.c.d.e`（位置は `span.start`）。4 部は最初が `catalog.current_database()` でなければ `0A000 cross-database references are not implemented: a.b.c.d`、一致すれば 3 部として続ける（【検証済み】`ch03.public.t.a` は通り、`other.public.t.a` は 0A000）。
2. **修飾なし `a`**: 内側から外側へスコープを順に見る（`level = 0, 1, ..`）。各スコープで `cols_visible` の項目の RTE の列を名前で探す。**`lateral_only` の項目は探索から外す**（診断のためだけ記録）。`RteKind::Table` の RTE は、同名のユーザー列がなければシステム列（`ctid` `xmin` `cmin` `xmax` `cmax` `tableoid`）も候補になる（ユーザー列が優先。**RTE ごとに判定する**ので `select ctid from t, u` は両方に候補があり曖昧。【検証済み】）。同じスコープ内で候補が 1 つならそれ（`Var { levels_up: level }`）。2 つ以上なら `42702 column reference "a" is ambiguous`（位置は `span.start`。同じ RTE に同名の列が 2 つある場合も同じ）。0 個なら次のスコープへ。全スコープで 0 個なら 3 へ。
3. **見つからない**: `lateral_active`（関数引数の解析中）で、`lateral_only` の項目に同名の列があれば `0A000 implicit LATERAL reference ...`（5.1.4）。単一名で、いずれかのスコープの `rel_visible` な項目の `refname` に一致すれば全行参照として `0A000 whole-row reference to "t" is not supported yet`。そうでなければ `42703 column "a" does not exist`（位置は `span.start`）に診断を付ける（5.2.4）。
4. **修飾あり `t.a`**: 内側のスコープから、`rel_visible` で `refname == t` の項目があるスコープを探す（**最初に見つかったスコープで決まり、そこに列がなくても外側へは行かない**）。その項目が `lateral_only` なら 5.2.4 の `invalid reference` のエラー（`lateral_active` かつ関数引数の解析中なら `0A000`）。列は RTE の列（結合 RTE なら展開）か、`Table` のシステム列（`t.ctid`）。なければ `42703 column t.zz does not exist`（位置は `span.start`）。`schema.t.a` は項目の `schema == Some(s)` も条件にする（別名つきの表は `schema` が None なので、`select public.t.a from t as t1` は 5.2.4 の別名の診断）。
5. 項目が見つからなければ `42P01`（5.2.4）。

#### 5.2.2 `*` の展開

```rust
pub(super) struct StarColumn { pub name: String, pub expr: BoundExpr, pub origin: (Oid, i16) }
impl Scope<'_> {
    /// `*`（qual = None）と `t.*`。FROM 句の順
    pub(super) fn expand_star(&self, a: &Analyzer<'_>, qual: Option<&ObjectName>, span: Span) -> Result<Vec<StarColumn>>;
}
```

- `*`: `ns` の項目のうち `cols_visible`（かつ `lateral_only` でない）ものを順に展開する（PG の `ExpandAllTables`）。別名なし JOIN の子の表は `cols_visible = false` なので、結合の列（併合列が先頭）だけが出る。項目がなければ `42601 SELECT * with no tables specified is not valid`（位置は `*`。R8）。
- `t.*`: `rel_visible` で `refname == t` の項目を、修飾あり列参照と同じ規則（内側のスコープから）で探す。**結合の子の表の `t.*` はその表の全列**（併合列でも元の列）。なければ `42P01 missing FROM-clause entry for table "t"`（位置は `t`）。システム列は展開しない。
- 結合 RTE の列は 3.2.2 のとおり展開した式。列名は `Rte.columns[i].name`。

#### 5.2.3 システム列

`ctid`（tid）、`xmin`（xid）、`cmin`、`xmax`、`cmax`（cid）、`tableoid`（oid）は `Var::system(rte, sc)`。FROM 句の表（`RteKind::Table`）のうち**名前が見える項目**の分だけ。`RteKind::Subquery` / `Values` / `Function` / `CteRef` / `Join` にはない（`select ctid from (select * from t) q` は `42703`）。結合の子の表のシステム列は、修飾すれば使える（`t.ctid`）が、修飾なしでは見えない（結合 RTE だけが `cols_visible`。【検証済み】`select ctid from t join p on t.a = p.id` は `42703 column "ctid" does not exist` + `DETAIL: There are columns named "ctid", but they are in tables that cannot be referenced from this part of the query.` + `HINT: Try using a table-qualified name.`）。`ctid` を FROM 句に 1 つだけ持つ `select ctid from t, (select 1 a) s` は通る。

#### 5.2.4 エラーの表（【検証済み】。位置は 1 始まりの文字位置が指すもの）

| 状況 | SQLSTATE | メッセージ・DETAIL・HINT | 位置 |
|---|---|---|---|
| 修飾なしの列が複数の項目にある | 42702 | `column reference "a" is ambiguous` | ColumnRef の先頭 |
| 同じ RTE に同名の列が 2 つ（`s.a` が `(select 1 a, 2 a) s` を指す） | 42702 | 同上 | 同上 |
| 列がない（修飾なし） | 42703 | `column "zz" does not exist` | ColumnRef の先頭（M1 の実装は末尾の名前。位置は slt で比較しないので合わせるのは任意だが、N1 が直す） |
| 列がない（修飾あり） | 42703 | `column t.zz does not exist` | ColumnRef の先頭 |
| 同名の列が lateral_only の項目にある（FROM 句の左の項目が右辺の副問い合わせから） | 42703 | `column "a" does not exist` + `DETAIL: There is a column named "a" in table "t", but it cannot be referenced from this part of the query.` + `HINT: To reference that column, you must mark this subquery with LATERAL.`（`lateral_ok` のとき。UPDATE の対象表や RIGHT / FULL JOIN の左からでは DETAIL だけ） | 同上 |
| 同名の列が `cols_visible = false` の項目にある（JOIN の子の表の列を修飾なしで） | 42703 | `column "ctid" does not exist` + `DETAIL: There are columns named "ctid", but they are in tables that cannot be referenced from this part of the query.` + `HINT: Try using a table-qualified name.` | 同上 |
| 修飾名の表がない | 42P01 | `missing FROM-clause entry for table "x"` | 修飾名の先頭（`x.a` の `x`） |
| 別名で隠された元の表名を使った（`select t.a from t as t1`、`public.t.a` も） | 42P01 | `invalid reference to FROM-clause entry for table "t"` + `HINT: Perhaps you meant to reference the table alias "t1".` | 同上 |
| 同じ `rtable` にあるが参照できない位置の表（FROM 句の左の項目を派生表・関数が、JOIN の ON が兄弟の項目を、UPDATE の FROM が対象表を） | 42P01 | `invalid reference to FROM-clause entry for table "t"` + `DETAIL: There is an entry for table "t", but it cannot be referenced from this part of the query.`（`lateral_ok` で派生表・関数からなら `HINT: To reference that table, you must mark this subquery with LATERAL.`） | 同上 |
| 関数引数が左の FROM 項目を参照（暗黙の LATERAL） | 0A000 | 5.1.4 の表 | その列参照 |
| 別名の重複（`from t, t`、`from t x, u x`、関数の既定名の重複、CTE の参照の重複） | 42712 | `table name "t" specified more than once` | なし |
| 列別名が多すぎる（表・派生表・VALUES・関数・CTE の参照） | 42P10 | `table "x" has 3 columns available but 4 columns specified` | なし |
| 列別名が多すぎる（WITH の項目） | 42P10 | `WITH query "x" has 2 columns available but 4 columns specified` | CTE 名の先頭 |
| 3 部以上の表名（`FROM a.b.c.d`） | 42601 | `improper relation name (too many dotted names): a.b.c.d` | 表名の先頭（M1 のまま） |
| `*` で FROM なし | 42601 | `SELECT * with no tables specified is not valid` | `*` |
| 修飾 `*` の表がない | 42P01 | `missing FROM-clause entry for table "x"` | 修飾名の先頭 |
| 存在しない表 | 42P01 | `relation "x" does not exist`（スキーマ付きは `relation "s.x" does not exist`） | 表名の先頭 |
| スキーマ付きの名前で CTE を引く（`select * from public.x`、`x` は CTE） | 42P01 | `relation "public.x" does not exist` | 表名の先頭 |
| 他の DB 名（`other.public.t`） | 0A000 | `cross-database references are not implemented: "other.public.t"` | 表名の先頭 |

`errorMissingRTE` 相当の診断は、**現スコープの `rtable`（FROM 句の途中なら `FromBuilder.rtable`）を全部**探す（`ns` ではなく）。そのため ON の中で `rtable` にあるが `ns` にない表（兄弟の項目）も `invalid reference`（DETAIL）になる。

### 5.3 FROM 句と JOIN（N1。`analyzer/from.rs`）

#### 5.3.1 表・CTE の参照

- `TableRef::Table { name, alias }`: 名前が 1 部（スキーマなし）なら、まず `cte::find_cte(name, env.ctes)`。`Found` → `RteKind::CteRef`、`Recursive` → `0A000 WITH RECURSIVE is not supported yet`（5.9.2）、`NotFound` → `resolve_table`（M1 の関数。実表がなければ `42P01`。5.9.2 の 4）。2 部以上は CTE を引かない（【検証済み】`public.x` は CTE `x` を引かない）。
- `resolve_table` は `CatalogReader::table()` が `None` のとき `relation_kind` を引き、名前が**索引**なら `42809 cannot open relation "x"` + `DETAIL: This operation is not supported for indexes.`（07 D07-18。M4 で `pg_class` の名前空間は表・索引・シーケンスで共有される）。シーケンスは `table()` が返す（`select * from s` は動く。00 §11.1）。
- `Rte { kind: Table { table }, refname: alias か表名, columns: 属性順のユーザー列（別名の列リストで先頭から改名）, span }`。`NsItem { refname: Some(alias か表名), schema: 別名なしなら Some(table.schema), hidden_name: 別名ありなら Some(table.name), rel_visible: true, cols_visible: true, lateral_only: false, .. }`。
- 別名の列リストが列数より多ければ 42P10（5.2.4）。少なければ先頭から改名し、残りは元の名前（【検証済み】）。
- `ONLY` / `*` は AST に出ない（パーサが捨てる）。

#### 5.3.2 JOIN

```rust
/// TableRef::Join を解析し、FromItem と、この結合が外へ出す名前空間を返す
fn analyze_join(&self, j: &TableRef, fb: &mut FromBuilder, env: &QueryEnv<'_>) -> Result<(FromItem, Vec<NsItem>)>;
```

手順（PG の `transformFromClauseItem` の JOIN 枝）:

1. **左**を `analyze_from_item`（`l_item`、`l_ns`）。続けて、**右**を解析する間は `l_ns` を `lateral_only = true`（`lateral_ok = kind が Inner / Left / Cross`）にして右辺の `Scope` に見せる（右辺が派生表・関数のとき診断のため）。
2. 右を `analyze_from_item`（`r_item`、`r_ns`）。
3. `l_ns` と `r_ns` の間で `refname` の衝突があれば `42712`。
4. **結合列の決定**。`l_cols` / `r_cols` は**左右の項目それぞれの最上位の RTE の列**（左が JOIN ならその結合 RTE の列。名前空間全体ではない）。
   - `NATURAL`: `l_cols` の名前のうち `r_cols` にもあるものを左の順に USING の名前にする。共通列がなければ結合列なし（`on = None` の直積。**エラーにならない**。【検証済み】）。
   - `USING (a, b)`: 指定順。
   - `ON` / `CROSS`: なし。
5. USING の各名前 `n` について（指定順）:
   a. USING の中の重複は `42701 column name "a" appears more than once in USING clause`（位置なし）。
   b. `l_cols` で `n` を探す。0 個: `42703 column "n" specified in USING clause does not exist in left table`。2 個以上: `42702 common column name "n" appears more than once in left table`。`r_cols` も同様（`right table`）。位置はなし。
   c. 左右の列の式 `l_expr` / `r_expr`（結合列は展開した式）。`cond = make_op("=", l_expr, r_expr)`（**元の左右の型で演算子を解決**。R2。なければ `42883 operator does not exist: smallint = text` + HINT。位置は USING の位置がないので `None`）。結果型が bool でなければ起きない。
   d. 併合列の型 `common`（3.2.2）。型が違い `select_common_type(.., "JOIN/USING")` が失敗したら `42804 JOIN/USING types smallint and text cannot be matched`（実際は c で先に 42883 になる組が多い）。
6. `ON`: `ExprCtx { scope: Scope { parent: env.outer, rtable: &fb.rtable, ns: l_ns ++ r_ns, lateral_active: false }, kind: JoinOn }` で解析し `coerce_to_boolean("JOIN/ON")`（`42804 argument of JOIN/ON must be type boolean, not type integer`、位置は式の先頭）。集約は `42803 aggregate functions are not allowed in JOIN conditions`（位置は集約の呼び出し）。ON から見えるのは**この JOIN の左右の項目だけ**。
7. 結合 RTE を積む。`columns` と `sources` は 3.2.2 の並び。`on` は ON の式、USING の `cond` の `And`（1 つならそのまま）、NATURAL で結合列なしなら `None`。
8. 返す名前空間: `l_ns`・`r_ns` の各項目を `cols_visible = false`（`rel_visible` は true のまま）にしたものと、結合 RTE の `NsItem { refname: None, rel_visible: false, cols_visible: true }`。

#### 5.3.3 JOIN の振る舞いの表（【検証済み】）

| 観点 | 実機の挙動 |
|---|---|
| `SELECT *` の列順 | 併合列（先頭）→ 左の残り → 右の残り。`t join u using (a)`（`t(a,b,c)`、`u(a,e,c)`）は `a, b, c, e, c` |
| 併合列と修飾 | `select a, t.a, u.a from t full join u using (a)` は `COALESCE(t.a, u.a)`、`t.a`、`u.a`（元の列はそれぞれ NULL になりうる）。INNER・LEFT の `a` は `t.a` と同じ |
| 型 | `m1(k smallint) join m2(k bigint) using (k)` の `k` は bigint、`m1.k` は smallint、`m2.k` は bigint |
| NATURAL | 共通名の列すべてが USING。`natural join` の連鎖（`t natural join u natural join p`）は直前の結合の列に対して共通列を見る。共通列がなければ直積 |
| 3 つの結合（`t join u using (a) join u u2 using (a, e)`）| 2 つ目の USING の左は結合 RTE。`a` は 1 つに見える（`42702 common column name ... more than once` になるのは `c` のように結合の左右に同名の列が残っている名前） |
| FROM 句のカンマと JOIN | `from t, u join p on p.id = u.a` は通る（`u` と `p` だけが ON から見える）。`from t, u join p on p.id = t.a` は `42P01 invalid reference to FROM-clause entry for table "t"` + DETAIL（HINT なし） |
| 結合の左を右の関数・派生表が参照 | LATERAL なら通る（M4 は 0A000。派生表は `invalid reference` + DETAIL + HINT。関数引数は 0A000） |
| FULL JOIN の ON | アナライザは見ない（D3-1）。`ON` が定数だけの FULL（`ON true`）は PG が通す |
| ON の型 | bool でなければ `42804 argument of JOIN/ON must be type boolean, not type integer` |
| ON の中の集約 | `42803 aggregate functions are not allowed in JOIN conditions` |
| JOIN の別名（USING の `AS`、括弧付き） | 実機は通る。yuzhu は 0A000（D3-11） |
| 結合の左右で同じ表を 2 回 | 別名がなければ `42712 table name "t" specified more than once`（`from t join t using (a)`） |

---

### 5.4 FROM 句の派生表・VALUES・関数（N1。`analyzer/from.rs`）

#### 5.4.1 派生表（`TableRef::Subquery`）

- 本体を `analyze_query(query, QueryEnv { outer: Some(&child_scope), ctes: env.ctes, resolve_unknowns: true })` で解析する。`child_scope` は 4.4 の 1 の `Scope`（親は `env.outer`、左の項目は `lateral_only`）。
- `Rte { kind: Subquery { query }, refname: alias.map(name), columns: query.columns を (name, ty) に、別名の列リストで先頭から改名, span }`。別名なしの派生表は `refname = None`（PG16 以降は別名を省略できる。【検証済み】）。`NsItem.refname = alias`（なければ `None`）、`cols_visible = true`。**エラー文言では別名なしの `refname` を `unnamed_subquery` と書く**（【検証済み】`column "unnamed_subquery.b" must appear in the GROUP BY clause ...`）。
- 本体が ORDER BY / LIMIT / OFFSET / WITH を持たない `VALUES` なら `Rte { kind: Values { rows } }`（列名は `column1..`、型は `coerce_all_to_common`）。ORDER BY などがあれば `Subquery`。
- 列別名が多ければ `42P10 table "s" has 1 columns available but 2 columns specified`（位置なし）。【検証済み】`(values (1),(2)) v(a, b)` も同じ。
- 別名なしの重複: `(select 1), (select 2)` は `refname = None` なので衝突しない。

#### 5.4.2 関数（`TableRef::Function`）

```rust
fn analyze_from_function(&self, name: &ObjectName, args: &[Expr], alias: Option<&TableAlias>, span: Span,
                         fb: &mut FromBuilder, env: &QueryEnv<'_>) -> Result<(FromItem, NsItem)>;
```

1. 引数を `ExprCtx { scope: Scope { lateral_active: true, ns: 左の項目（lateral_only） }, kind: FromFunction }` で解析する。左の FROM 項目に当たったら `0A000 implicit LATERAL reference ...`（D3-6。【検証済み】PG は `t, generate_series(1, t.a)` を通す）。集約は `42803 aggregate functions are not allowed in functions in FROM`。副問い合わせ（`generate_series(1, (select 2))`）は通る。
2. 関数名: スキーマなしまたは `pg_catalog`。他のスキーマ（`public.generate_series`）は `42883 function public.generate_series(integer, integer) does not exist`（引数の型を並べる）、存在しないスキーマは `3F000 schema "x" does not exist`（M2 の関数呼び出しと同じ）。
3. `make_func_call(name, args)`（M1 の関数解決。完全一致 → 暗黙キャストで到達できる候補 → `func_select_candidate`）。解決できなければ `42883 function nosuchfunc(integer) does not exist`（+ HINT。位置は関数名）、曖昧なら `42725`。【検証済み】`generate_series(1::smallint, 3::smallint)` は int4 版と int8 版が同点で `42725 function generate_series(smallint, smallint) is not unique`。**M4 の表には numeric 版がない**（09 §10.1）ので `generate_series(1, 10.5)` は `42883`（PG は numeric 版で通す。既知の差分）。
4. 解決した関数の `kind` が `FnKind::Set(set_fn)` でなければ `0A000 function "lower" in FROM is not supported yet`（D3-9）。
5. 列と名前: `refname` は別名があればその名前、なければ `set_fn.column_name`（`generate_series`）。**列名**は、別名の列リストがあれば先頭の名前、なければ別名（あれば）、なければ関数名。【検証済み】`select g from generate_series(1,3) g`、`select * from generate_series(1,3) as g(x)` は列 `x`、`select generate_series from generate_series(1,3)` が通る（`pgbench -i` の `from generate_series(1, N) as aid` の `aid` が列名になる根拠）。列リストが 2 つ以上は `42P10 table "g" has 1 columns available but 2 columns specified`。
6. `Rte { kind: Function { call }, columns: [RteColumn { name, ty: func.result }] }`。`call` は `ExprKind::Function { func, args }`（引数は宣言型へ暗黙キャスト済み）。
7. 同じ名前の関数を 2 回（`generate_series(1,2), generate_series(1,2)`）は `42712 table name "generate_series" specified more than once`。
8. システム列はない（`select ctid from generate_series(1,2)` は `42703`）。

#### 5.4.3 エラーの表（【検証済み】）

| 状況 | SQLSTATE | メッセージ | 位置 |
|---|---|---|---|
| 派生表・VALUES・関数・表の列別名が多い | 42P10 | `table "s" has 1 columns available but 2 columns specified` | なし |
| VALUES の行の長さが違う | 42601 | `VALUES lists must all be the same length` | 長さの違う行の最初の式 |
| VALUES の値が変換できない | 22P02 | `invalid input syntax for type integer: "a"` | リテラル |
| 関数がない | 42883 | `function nosuchfunc(integer) does not exist` + `HINT: No function matches the given name and argument types. You might need to add explicit type casts.` | 関数名 |
| 引数の個数が違う（`generate_series(1)`、`()`、4 個） | 42883 | `function generate_series(integer) does not exist` | 関数名 |
| 関数のスキーマ違い | 42883 | `function public.generate_series(integer, integer) does not exist` | 関数名 |
| 候補が曖昧（`smallint, smallint`、`'1', '3'`） | 42725 | `function generate_series(smallint, smallint) is not unique` + HINT | 関数名 |
| `FnKind::Set` でない関数 | 0A000 | `function "lower" in FROM is not supported yet` | 関数名 |
| `step` が 0（実行時） | 22023 | `step size cannot equal zero` | なし |
| 関数引数に集約 | 42803 | `aggregate functions are not allowed in functions in FROM` | 集約 |
| 関数引数が左の FROM 項目を参照 | 0A000 | 5.1.4 の表 | 列参照 |
| 関数の別名の重複 | 42712 | `table name "g" specified more than once` | なし |
| 関数・VALUES・派生表のシステム列 | 42703 | `column "ctid" does not exist` | ColumnRef の先頭 |

### 5.5 集約（N2。`analyzer/agg.rs`）

#### 5.5.1 呼び出しの認識と解決

`expr.rs` の `Expr::Function` 枝は、**関数名の解決より先に** `agg::analyze_agg_call` を呼ぶ（3.4）。

1. 関数名のスキーマは、なし・`pg_catalog` なら続行、`public` などは `42883 function public.count() does not exist`（【検証済み】。引数の型を並べた形。`*` は `()`）、存在しないスキーマは `3F000`（M2 の関数呼び出しと同じ）。
2. `candidates = catalog.aggregates_named(name)`。空なら: `filter` / `distinct` / `star` が付いていれば、通常の関数 `functions_named(name)` の有無で分ける。**関数なら** `distinct` は `42809 DISTINCT specified, but abs is not an aggregate function`、`filter` は `42809 FILTER specified, but abs is not an aggregate function`（位置は関数名。【検証済み】）、`star` は `42883 function abs() does not exist`。**関数でもなければ** `42883`（通常の関数解決に任せる: `Ok(None)`）。付いていなければ、名前が D3-18 の未対応の集約名の一覧にあれば `0A000 aggregate function X is not supported yet`、なければ `Ok(None)`。
3. 集約として続ける。`entry_depth = state.agg_arg_depth.get()` を控え、**引数と FILTER を解析する**（その間 `agg_arg_depth` を 1 増やす。引数は `cx.kind` のまま、FILTER は `ParseExprKind::Filter` で解析する。PG と同じく、引数の中の集約は同じ文脈の検査を受けるので、`where sum(count(*)) > 1` は内側の `count(*)` が先に `42803 aggregate functions are not allowed in WHERE` になる。【検証済み】）。
4. **集約の属するスコープ**（D3-4）: 解析した引数と FILTER の Var の `levels_up - その Var がある副問い合わせの深さ` の最小値 `r`（Var がなければ 0）。`r > 0` なら `0A000 aggregate functions of an outer query level are not supported yet`（位置は集約の呼び出し）。
5. **文脈の検査**: `cx.kind.agg_forbidden_in()` が `Some(name)` なら `42803 aggregate functions are not allowed in {name}`（位置は集約の呼び出し。【検証済み】WHERE、GROUP BY、LIMIT、OFFSET、JOIN conditions、functions in FROM、FILTER、VALUES、UPDATE、RETURNING、check constraints、DEFAULT expressions）。
6. **入れ子の検査**: `r == 0` かつ `entry_depth > 0`（このスコープの別の集約の引数・FILTER の中にいる）なら `42803 aggregate function calls cannot be nested`（位置は**内側**の集約。【検証済み】`sum(count(*))`、`sum(count(*) filter (where a > 0))`）。副問い合わせの中の集約（別の `Scope`）は `agg_arg_depth` が別なので入れ子にならない（`sum((select count(*) from u))` は通る）。
7. **関数の解決**（`func_get_detail` と同じ。`m1.md` §5.2 の規則）: 引数の型の並び `inputs`（`star` は空）で、`candidates` を引数の数で絞る。
   - 完全一致（`args == inputs`）があればそれ。
   - なければ `func_match_argtypes`（暗黙キャストで到達できる候補。`oid::ANY` は何でも受ける。unknown は常に通る）→ 1 件ならそれ、複数なら `func_select_candidate`（M1 のもの）。
   - 0 件なら `42883 function sum(text) does not exist` + `HINT: No function matches the given name and argument types. You might need to add explicit type casts.`（位置は関数名）。曖昧なら `42725 function sum(unknown) is not unique` + `HINT: Could not choose a best candidate function. You might need to add explicit type casts.`。
   - 引数なし（`count()`、`star` でない）: `42809 count(*) must be used to call a parameterless aggregate function`（【検証済み】。0 引数の集約は `count(*)` だけ）。`sum(*)` / `sum()` は 0 引数の候補がないので `42883 function sum() does not exist`。
8. 引数を宣言型へ `coerce_arg`（暗黙キャスト）。`count("any")` の `ANY` は変換しない（unknown の定数は text に解決する）。FILTER は `coerce_to_boolean("FILTER")`（`42804 argument of FILTER must be type boolean, not type integer`、位置は式）。
9. `state.agg_seen.set(true)`、`BoundExpr { kind: Aggregate(AggCall { func, args, distinct, filter }), ty: SqlType::of(func.result), span: 呼び出しの先頭 }`。

【検証済み】の解決の例（実機）: `sum('1')` と `sum(null)` は `42725`（`sum` の候補が多く unknown では絞れない）。`max('a')` は text（候補の中で文字列カテゴリの推奨型が選ばれる）、`min(null)` は text、`count('a')` と `count(null)` は通る。`max(varchar(5))` は text、`max(char(5))` は character。`min(true)` は `42883 function min(boolean) does not exist`（bool に `min` / `max` はない）。`count(distinct a, b)` は `42883 function count(integer, integer) does not exist`。`pg_catalog.count(*)` は通る。

#### 5.5.2 集約の結果型（PG17.11 の `pg_proc` で確認。09 §8 の表が正）

| 集約 | 引数 | 結果 |
|---|---|---|
| `count(*)`、`count("any")` | — / 何でも | int8 |
| `sum` | int2、int4 / int8 / float4 / float8 / numeric | int8 / **numeric** / float4 / float8 / numeric |
| `avg` | int2、int4、int8、numeric / float4、float8 | **numeric** / **float8** |
| `min`、`max` | 型ごとの行（int2 int4 int8 float4 float8 numeric text bpchar date timestamp timestamptz oid ほか。varchar は text の行に解決される） | 引数と同じ（varchar → text、typmod は -1） |
| `bool_and`、`bool_or`、`every` | bool | bool |

#### 5.5.3 グループ化の検査（`parseCheckAggregates` / `check_ungrouped_columns`。`PG:parse_agg.c:1131`、`:1328`）

```rust
/// has_agg のときだけ呼ぶ（4.3 の 9）。targets（resjunk を含む）と having を検査する。
/// 関数従属で許された Var は sel.group_by の末尾に追加する（D3-10）
pub(super) fn check_grouping(&self, sel: &mut BoundSelect, scope: &Scope<'_>) -> Result<()>;
```

アルゴリズム（再帰関数 `walk(node, sublevels)`。`sublevels` は検査対象の SELECT から見た入れ子の深さで、`BoundQuery::walk_exprs`（3.2.11）の `depth` と同じ）:

1. `node` が現スコープの集約（`sublevels == 0` の `Aggregate`）なら、引数・FILTER に降りずに終わる（集約の中は未グループの列を含んでよい）。
2. `sublevels == 0` で `node` が Var でなく、`group_by` のどれかと**構造が等しい**（`same_expr`。span を無視。結合の別名は展開済みなので別名とそのもとの列は同じ式になる）なら、子に降りずに終わる。
3. `node` が Var で `v.levels_up == sublevels`（検査対象のスコープの列）なら:
   - `group_by` に同じ Var（`rte`・`col`・`levels_up = 0`）があれば OK。
   - **関数従属**: `scope.rtable[v.rte]` が `RteKind::Table` で、`table.primary_key()` があり、主キーの全列（`IndexColumn.attnum - 1`）が `group_by` に `Var { rte: v.rte, col, levels_up: 0 }` として含まれていれば OK（`v` を `deps` に記録）。
   - どちらでもなければ `42803`: `sublevels == 0` なら `column "t.b" must appear in the GROUP BY clause or be used in an aggregate function`、`sublevels > 0`（副問い合わせの中から）なら `subquery uses ungrouped column "t.b" from outer query`。`t.b` は `{RTE の refname（別名なしの派生表・VALUES は unnamed_subquery）}.{RTE の列名}`。位置は Var の位置。
4. `node` が `SubLink` なら、`query` の式を `BoundQuery::walk_exprs(sublevels + 1)` で走査し、各式に `walk(e, depth)` を適用する（副問い合わせの中の集約は、そのスコープに属するので 1 で止まらない: `depth > 0` の `Aggregate` は引数・FILTER に降りる。引数が外側の未グループの列を含みうるため。【検証済み】`(select max(u.e + t.b) from u)` は `42803 subquery uses ungrouped column "t.b" from outer query`）。
5. それ以外は子に降りる。

【検証済み】の振る舞い: `select a, count(*) from t`（GROUP BY なし）は `42803`、`select count(*) from t having a > 1` も `42803`、`select 1 from t having exists (select 1 from u where u.a = t.a)` は `42803 subquery uses ungrouped column "t.a" from outer query`、`select 1 having false` は 0 行（集約がなくても HAVING があれば集約の問い合わせ）、GROUP BY のない HAVING は全体を 1 グループ、`select count(*) from t group by a having a in (select a from u)` は通る（`a` はグループ化済み）、`select a from t group by a order by count(*)` は通る（ORDER BY の集約も `has_agg` に数える）、`select a from t order by count(*)` は `42803 column "t.a" must appear ...`（位置は `a`）。

#### 5.5.4 関数従属（D3-10）

【検証済み】

| 問い合わせ | 結果 |
|---|---|
| `select id, v from p group by id`（`id` が主キー） | 通る |
| `select p.v, t.c from p join t on t.a = p.id group by p.id` | `42803 column "t.c" must appear ...`（`t` に主キーの情報がない。従属は RTE ごとに判定する） |
| `select a, c from pk2 group by a`（主キー `(a, b)`） | `42803 column "pk2.c" must appear ...` |
| `select a, c from pk2 group by a, b` | 通る |
| `select id, v from uq group by id`（`id` が UNIQUE NOT NULL） | `42803 column "uq.v" must appear ...`（主キーだけ） |
| `select v from (select id, v from p) s group by id` | `42803 column "s.v" ...`（派生表は対象外） |
| `select x.id, x.v from p x group by x.id` | 通る（別名つきでも） |
| `select id, (select p.v) from p group by id` | 通る（副問い合わせの中の `p.v` も従属で許される） |
| `p` から主キーを外した後の `group by id` | `42803`（アナライザは**解析時のカタログ**を見る。依存は記録しない） |

従属で許された Var を `group_by` の末尾に足す（重複なし）。これで Aggregate の出力（グループのキー ++ 集約の結果）から `targets` の Var を解決できる。

### 5.6 GROUP BY / ORDER BY / DISTINCT ON / DISTINCT（N2。`analyzer/select.rs`）

#### 5.6.1 項目の解決（`findTargetlistEntrySQL92` と `SQL99`。`PG:parse_clause.c:2006`）

```rust
enum ClauseKind { OrderBy, GroupBy, DistinctOn }       // 文言は "ORDER BY" / "GROUP BY" / "DISTINCT ON"
/// 項目 e を targets の位置にする。ORDER BY と DISTINCT ON は必要なら resjunk の式を tl に足す。
/// GROUP BY は式そのものを返す（group_by は式の並びなので junk を足さない）
fn find_target_entry(&self, e: &Expr, kind: ClauseKind, tl: &mut TargetList, cx: &ExprCtx<'_>) -> Result<Resolved>;
enum Resolved { Target(usize), Group(BoundExpr) }
```

1. **単純な名前**（`Expr::Column` の部分が 1 つ）:
   - `GroupBy` のときだけ、まず FROM の列として解決を試す（`Scope::lookup_unqualified`）。見つかれば（または曖昧なら `42702 column reference "a" is ambiguous`）、**入力列として 3 へ**（出力列の別名は見ない）。【検証済み】`select a as b, count(*) from t group by b`（`t` に `b` がある）は `t.b` に解決され `42803`。`select t.a as a from t, u group by a` は `42702`（`a` が FROM で曖昧）。
   - それ以外は、`tl.names[..n_visible]` から名前が等しいものを探す。見つかった複数が**同じ式でなければ** `42702 ORDER BY "x" is ambiguous`（`GROUP BY "x"`、`DISTINCT ON "x"`。位置は名前）。同じ式（`select a as x, a as x`）なら先頭。見つかればその位置。
2. **整数リテラル**（`Literal::Integer`。M1 は括弧なしの負数も `Integer("-1")` に畳む）: `pos < 1 || pos > n_visible` なら `42P10 ORDER BY position 5 is not in select list`（`GROUP BY position 0`、`DISTINCT ON position 3`。位置はリテラル。【検証済み】0・-1・範囲外すべて 42P10）。範囲内ならその位置。**`GroupBy` のときはその対象が集約を含むと `42803 aggregate functions are not allowed in GROUP BY`**（位置は対象の式の先頭。【検証済み】`select count(*) from t group by 1`）。**整数以外のリテラル**（`TRUE`、`NULL`、`1.5`、`'a'`）は `42601 non-integer constant in ORDER BY`（R7。位置はリテラル）。
3. **それ以外の式**（SQL99）: `cx.kind` を節の種別にして `transform_expr`、`resolve_unknown`。`tl.exprs`（resjunk を含む全部）に `same_expr` で等しいものがあればその位置。なければ `OrderBy` / `DistinctOn` は resjunk として `tl` の末尾に足してその位置、`GroupBy` は `Resolved::Group(式)`。

#### 5.6.2 ORDER BY

各項目を 5.6.1 で解決し、`BoundSortKey { target, descending, nulls_first }`。`nulls_first` は明示（`NULLS FIRST` / `LAST`）があればそれ、なければ `descending`（PG と同じ。昇順は NULLS LAST）。`USING` はパーサが 0A000。

【検証済み】の例: `select a as b, b as a from t order by a`（出力列 `a` = `t.b` が先）、`select a+b as s from t order by s+1` は `42703 column "s" does not exist`（式の中の別名は見ない）、`select t.a, u.a from t, u order by a` は `42702 ORDER BY "a" is ambiguous`、`order by 4`（範囲外）は `42P10 ORDER BY position 4 is not in select list`、`select a from t group by a order by a+1` は通る（式 `a+1` は resjunk。`a` は grouped）。

#### 5.6.3 GROUP BY

各項目を 5.6.1 で解決して `group_by: Vec<BoundExpr>`（出力列の位置なら `tl.exprs[pos]` の複製）。集約・ウィンドウ関数は文脈の検査で `42803`。同じ式の重複は残してよい（`group by a, a`）。**`GROUP BY` は ORDER BY の後に解析する**（4.3）。

【検証済み】`select a+0 as zz, count(*) from t group by zz` は別名 `zz` に解決されて通る（`t` に `zz` がないので）。`select (a + 1) as k, count(*) from t group by k + 1` は `42703 column "k" does not exist`（式の中の別名は見ない）。

#### 5.6.4 DISTINCT と DISTINCT ON

- `SELECT DISTINCT`: ORDER BY の各キーの `target` が `n_visible` 以上（resjunk）なら `42P10 for SELECT DISTINCT, ORDER BY expressions must appear in select list`（位置は ORDER BY の式の**先頭**。`order by a+1` なら `a`。`PG:parse_clause.c:3013`）。`select distinct a+1 from t order by a+1` は通る（式が出力列と同じ）。
- `SELECT DISTINCT ON (e1, e2, ..)`（`transformDistinctOnClause`。`PG:parse_clause.c:3069`）。アルゴリズム:
  1. 各 `e_i` を 5.6.1（`DistinctOn`）で `targets` の位置 `refs[i]` にする（式なら resjunk を足す）。
  2. `skipped = false`、`result = []`。ORDER BY のキーを順に見る。キーの `target` が `refs` に含まれるなら、`skipped` が true であれば `42P10 SELECT DISTINCT ON expressions must match initial ORDER BY expressions`（位置はそのキーに対応する DISTINCT ON の式の先頭）、そうでなければ `result` に足す。含まれないなら `skipped = true`。
  3. DISTINCT ON の式を書かれた順に見る。`refs[i]` が `result` にあれば何もしない。なければ、`skipped` が true なら同じ `42P10`（位置はその式の先頭）、そうでなければ `result` に足す（既定の昇順・NULLS LAST）。
  4. `BoundDistinct::On(result)`（3.2.4）。
  - 帰結: ORDER BY が空なら `result` は DISTINCT ON の式の並びそのもの。ORDER BY があるなら、**先頭の一部が DISTINCT ON の式の部分集合と一致していなければならず**、ORDER BY に DISTINCT ON にない項目があるなら、DISTINCT ON の式は**すべて**ORDER BY の先頭に出ていなければならない。
  - 【検証済み】`distinct on (a) .. order by a` OK、`order by a, b desc` OK、`order by a, c, b`（`b` は DISTINCT ON にない）OK、`order by b`（`a` が先頭にない）は 42P10、`order by b, a` も 42P10、`distinct on (a, b) .. order by a, c` は 42P10（`b` が足せない）、`order by a` と `order by a, b, c` は OK、`order by b desc`（`a` が後ろに足される）は OK で `b` の降順、`distinct on (b, a) .. order by a, b` と `order by b, a` はどちらも OK、`distinct on (a, a) .. order by a` OK、`distinct on (a+1) a from t order by a+1` OK、`distinct on (count(*)) a from t` は `42803`（DISTINCT ON の式の集約が `has_agg` にし、`a` が未グループ）、`distinct on (a) count(*) from t group by a order by a` OK、`distinct on () a` は `42601`（パーサ）。

#### 5.6.5 HAVING

`ParseExprKind::Having` で解析して `coerce_to_boolean("HAVING")`。**GROUP BY の別名・位置番号は見ない**（【検証済み】`select count(*) c from t having c > 1` は `c` が `t.c`（text の列）に解決されて `42883 operator does not exist: text > integer`）。

---

### 5.7 サブクエリ（N3。`analyzer/sublink.rs`）

#### 5.7.1 解析の手順（`transformSubLink`。`PG:parse_expr.c`）

```rust
impl Analyzer<'_> {
    pub(super) fn analyze_sublink(&self, e: &Expr, cx: &ExprCtx<'_>) -> Result<BoundExpr>;
}
```

対象の AST は `Subquery`（スカラー）、`Exists`、`InSubquery { negated }`、`QuantifiedSubquery`。**sublink の位置**は AST の `span.start`（スカラーは `(`、`EXISTS` は `exists`、`IN` は `in`（`NOT IN` は `not`）、`ANY` / `ALL` は演算子のトークン）。エラーの位置はすべてこれ。

1. **文脈の検査**: `cx.kind.sublink_forbidden_in()` が `Some(name)` なら `0A000 cannot use subquery in {name}`（`check constraint`、`DEFAULT expression`。【検証済み】`create table z1(a int check (a in (select 1)))`）。`ParseExprKind::Returning` なら `0A000 subquery in RETURNING is not supported yet`（D3-17）。
2. **副問い合わせの本体を先に**解析する（PG と同じ。本体のエラーが左辺のエラーより先に出る）: `analyze_query(query, QueryEnv { outer: Some(cx.scope), ctes: cx.ctes, resolve_unknowns: true })`。`cx.scope` を `outer` にするので、本体の最上位の SELECT から見て `levels_up = 1` が `cx.scope`（3.2.1）。
3. **種類ごと**:

| 種類 | 手順 |
|---|---|
| スカラー | 可視出力列が 1 つでなければ `42601 subquery must return only one column`（0 列の `(select)` も同じ。【検証済み】位置は `(`）。`SubLink { kind: Scalar, test: None }`、型は出力列の型（typmod も）。`state` の `sublink_names` に出力列名を記録（5.11） |
| `EXISTS` | `SubLink { kind: Exists }`、型は bool。対象列は何列でもよい（`exists (select)` も通る。【検証済み】）。解析した対象列は捨てない |
| `IN` / `= ANY` / `op ANY` / `op ALL` | 次の 4 の手順（左辺・列数・演算子） |

4. **左辺・列数・演算子**（`Any` / `All`）:
   a. 左辺を `transform_expr`（`Expr::Row` なら要素の並び `lhs[]`、それ以外は 1 要素。1 要素の `ROW(a)` も 1 要素）。
   b. 列数の比較: `lhs.len() < n_visible` なら `42601 subquery has too many columns`、`lhs.len() > n_visible` なら `42601 subquery has too few columns`（【検証済み】`a in (select a, e from u)` は too many、`(1,2) in (select 1)` は too few。`a in (select)` は too few）。
   c. 各 `i` について、右辺 `Out(i) = BoundExpr { kind: SubLinkOutput(i), ty: 副問い合わせの i 番目の出力列の型 }`（unknown は本体の解析で text に解決済み）。`cmp_i = make_op(op, lhs[i], Out(i), op の位置)`。`op` は `IN` と `Quantifier` の `=`、`ANY` / `ALL` の演算子（`op_schema` があればその解決。5.12）。解決できなければ `42883 operator does not exist: integer = text` + HINT（位置は sublink の位置。【検証済み】`a = any (select c from u)`、`a in (select c from u)`、`1 in (select null)`（`null` が text に解決されるため）も同じ）。左辺が unknown の定数なら `Out(i)` の型の入力関数で評価される（`'a' in (select 1)` は `22P02 invalid input syntax for type integer: "a"`、位置は `'a'`）。
   d. 各 `cmp_i` の結果型が bool でなければ `42804 row comparison operator must yield type boolean, not type integer`（【検証済み】`1 + any (select 1)`。単一列でもこの文言。位置は演算子）。
   e. `test`: 1 列なら `cmp_0`、複数列なら `And(cmp_0, cmp_1, ..)`。**行値**（`lhs.len() > 1`）の演算子は、`IN` と `= ANY`（`Any`）、`<> ALL`（`Not(Any(test = And(=)))` に書き換える。3.2.8）だけを受け付け、それ以外（`(a, b) < ANY (..)`、`(a, b) <> ANY (..)`）は `0A000 row comparison with this operator in a subquery is not supported yet`（位置は演算子）。
   f. `kind`: `IN` と `= ANY` と `op ANY` は `Any`、`op ALL` は `All`。`NOT IN` は `Any` を `Not` で包む。型は bool。

#### 5.7.2 エラーと挙動の表（【検証済み】）

| 状況 | SQLSTATE | メッセージ | 位置 |
|---|---|---|---|
| スカラー副問い合わせの列が 0 または 2 以上 | 42601 | `subquery must return only one column` | `(` |
| IN / ANY / ALL で右の列が多い・少ない | 42601 | `subquery has too many columns` / `subquery has too few columns` | `in` / 演算子 |
| スカラー副問い合わせが 2 行以上（実行時） | 21000 | `more than one row returned by a subquery used as an expression` | なし |
| スカラー副問い合わせが 0 行（実行時） | — | NULL | — |
| 演算子の型が合わない | 42883 | `operator does not exist: integer = text` | 演算子 / `in` |
| 演算子が bool を返さない | 42804 | `row comparison operator must yield type boolean, not type integer` | 演算子 |
| CHECK・DEFAULT の中 | 0A000 | `cannot use subquery in check constraint` / `... in DEFAULT expression` | sublink |
| FROM 句の派生表の中の副問い合わせが、同じ FROM 句の左の項目を参照 | 42P01 | 5.2.4（`invalid reference ... LATERAL`） | 列参照 |
| `ANY` / `ALL` の配列形 | 0A000 | パーサ（5.1.4） | `any` |
| `x IN (list)` に副問い合わせが混ざる（`1 in (1, (select 2))`） | — | 通常の `InList`（スカラー副問い合わせが要素）。通る | — |
| グループ化していない外側の列を副問い合わせが使う | 42803 | `subquery uses ungrouped column "t.b" from outer query` | Var |

実行時の意味（05 の担当。テストで固定する）: `x IN (空)` は false、`x NOT IN (空)` は true、`NULL IN (空でない集合)` は NULL、`x IN (..NULL を含む..)` は一致がなければ NULL、`x = ALL (空)` は true、`x = ANY (空)` は false。

#### 5.7.3 プランナへの申し送り（D-15 の解析側）

- `SubLink` は式の中にあり、Bound ではまだ書き換えない。EXISTS の対象列、ORDER BY、LIMIT は残してある（PG も `convert_EXISTS_sublink_to_join` が捨てる）。
- 相関の有無は Var の `levels_up`（SubLink の `query` の中で、`levels_up` が副問い合わせの深さ以上のもの）から 04 が求める。**SubLink に印はない**。
- `IN` は `Any` + `test = (x = Out(0))`、`NOT IN` は `Not(Any(..))`（PG と同じく SEMI / ANTI に書き換えない）。

### 5.8 集合演算（N3。`analyzer/setop.rs`）

#### 5.8.1 手順（`transformSetOperationStmt`、`transformSetOperationTree`。`PG:analyze.c:1815`、`:2003`）

```rust
impl Analyzer<'_> {
    /// body は QueryBody::SetOp。q の ORDER BY / LIMIT / OFFSET は外枠
    pub(super) fn analyze_set_operation(&self, body: &QueryBody, q: &Query, env: &QueryEnv<'_>) -> Result<BoundQuery>;
}
```

1. **腕の解析**（左から右。再帰）: `QueryBody::SetOp { op, all, left, right }` の `left` と `right` を `analyze_set_arm`（下）。
   - `Select(s)` → `analyze_select`（外枠の ORDER BY などは空の `Query`。`env.resolve_unknowns = false`。D3-15）。
   - `Values(v)` → `BoundQuery { body: BoundSetExpr::Values }`（列の型は VALUES の共通型。全部 unknown なら text）。
   - `SetOp` → 再帰（`BoundQuery { body: SetOp }`）。
   - `Nested(inner)` → `analyze_query(inner, env { resolve_unknowns: false })`（腕自身の ORDER BY / LIMIT / WITH を持てる）。
2. **列数**: 左右の可視列数が違えば `42601 each UNION query must have the same number of columns`（`INTERSECT` / `EXCEPT` も同じ形。`UNION ALL` は `UNION`。位置は**右の腕の最初の出力式**。【検証済み】`select a, b from t union select a from u` は右の `a`）。
3. **列ごとの型**（左から順に 2 つずつ）: 左の列の型 `lt` と右の列の型 `rt`。両方 unknown なら text。それ以外は `select_common_type([l, r], context)`（`context` は `UNION` / `INTERSECT` / `EXCEPT`）。分類が違えば `42804 UNION types integer and text cannot be matched`（位置は**右の腕のその列の式**。【検証済み】`select null union select null union select 1` は `UNION types text and integer cannot be matched`、位置は `1`）。typmod は左右が同じ（型, typmod）なら保持、違えば -1。
4. **変換**: 腕が `Select` で、その列が unknown のリテラルなら、腕の `targets[i]` を共通型へ `coerce_type`（リテラルを入力関数で評価。`select 1 union select 'a'` は `22P02 invalid input syntax for type integer: "a"`、位置は `'a'`）して腕の `columns[i].ty` を直す。型が違う他の列は `left_coerce` / `right_coerce` の式（暗黙キャスト）。暗黙キャストできなければ `42846 UNION could not convert type X to Y`（位置は列の式）。
5. `BoundSetExpr::SetOp { op, all, left, right, left_coerce, right_coerce, types }`。`BoundQuery.columns` は左の最初の腕の名前、`table_oid` = `attnum` = 0。
6. **外枠の ORDER BY**: 名前空間は結果の列だけ（修飾名では見えない `NsItem { rel_visible: false, cols_visible: true }`）で、`TargetList`（`exprs` は結果の列の位置を指す Var 風のプレースホルダ）に対して 5.6.1 を適用する。解決の結果 **resjunk の式を足す必要があるものは** `0A000 invalid UNION/INTERSECT/EXCEPT ORDER BY clause` + `DETAIL: Only result column names can be used, not expressions or functions.` + `HINT: Add the expression/function to every SELECT, or move the UNION into a FROM clause.`（位置は式の先頭。【検証済み】`order by a+1`、`order by 1+0`）。修飾名 `t.a` は `42P01 missing FROM-clause entry for table "t"`（位置は `t`）。範囲外の位置は `42P10 ORDER BY position 2 is not in select list`。名前が右の腕の別名にしかない（`select a as x from t union select e as y from u order by y`）は `42703 column "y" does not exist`（PG は DETAIL に `There is a column named "y" in table "*SELECT* 2", but it cannot be referenced from this part of the query.` を付ける。D3-19 の方針で省略可）。
7. LIMIT / OFFSET は 5.12.2。さらに、**外枠の LIMIT / OFFSET が外側の列（`levels_up >= 1` の Var）を含むときは `0A000 correlated LIMIT or OFFSET on a set operation is not supported yet`**（位置は Var。04 §4.1 の前提「集合演算の ORDER BY / LIMIT は出力列の位置と定数・パラメータだけを参照する」を守るため。腕自身の LIMIT は通常の SELECT と同じ）。

#### 5.8.2 括弧・優先順位・出力名

- 優先順位は `INTERSECT` が `UNION` / `EXCEPT` より強く、同じ強さは左結合（パーサ。M1 のまま）。【検証済み】`select a from t union select e from u intersect select 3` は `intersect` が先。`... except select e from u except select 1` は左から。
- 括弧つきの腕は ORDER BY / LIMIT を持てる（`(select a from t order by a limit 1) union (select e from u order by e desc limit 1) order by 1`）。括弧なしの `select ... order by a union select ...` は構文エラー（位置は `union`）。
- 出力列の名前は最初の（最も左の）腕。`select 1 as a union select 2 as b` の列は `a`。`VALUES` が左なら `column1`。
- `UNION`（`all = false`）は重複を除く（NULL どうしは等しい）。`INTERSECT ALL` / `EXCEPT ALL` は個数の演算（05 の `HashSetOp`）。

#### 5.8.3 エラーの表（【検証済み】）

| 状況 | SQLSTATE | メッセージ | 位置 |
|---|---|---|---|
| 列数が違う | 42601 | `each UNION query must have the same number of columns`（`INTERSECT` / `EXCEPT`） | 右の腕の最初の出力式 |
| 型が合わない | 42804 | `UNION types integer and text cannot be matched` | 右の腕のその列の式 |
| unknown のリテラルが変換できない | 22P02 | `invalid input syntax for type integer: "a"` | リテラル |
| 暗黙キャストできない | 42846 | `UNION could not convert type X to Y` | 列の式 |
| ORDER BY の表の修飾 | 42P01 | `missing FROM-clause entry for table "t"` | 修飾名 |
| ORDER BY の式 | 0A000 | 5.8.1 の 6 | 式の先頭 |
| ORDER BY の位置の範囲外 | 42P10 | `ORDER BY position 2 is not in select list` | リテラル |
| `FOR UPDATE` と併用 | 0A000 | パーサ（5.1.4） | `for` |

### 5.9 CTE（N3。`analyzer/cte.rs`）

#### 5.9.1 `analyze_with`

```rust
pub(super) struct WithResult<'e> { pub(super) ctes: Vec<BoundCte>, pub(super) scope: CteScope<'e> }
impl Analyzer<'_> {
    pub(super) fn analyze_with<'e>(&self, with: &With, env: &QueryEnv<'e>) -> Result<WithResult<'e>>;
    /// 表名（スキーマなし）を CTE として引く。3.3 の CteLookup
    pub(super) fn find_cte(&self, name: &str, ctes: &CteScope<'_>) -> CteLookup;
    /// 表も CTE も見つからなかったとき、宣言済みだが未解析の CTE の名前か（42P01 の DETAIL / HINT 用）
    pub(super) fn is_future_cte(&self, name: &str, ctes: &CteScope<'_>) -> bool;
}
```

1. **名前の重複**: 全項目を先に見て、同じ名前が 2 度あれば `42712 WITH query name "x" specified more than once`（位置は**後の項目の名前**。【検証済み】）。
2. `scope = CteScope { parent: Some(env.ctes), visible: [], all_names, recursive: with.recursive }`。
3. 各項目 `i` を**宣言順に**解析する:
   a. 本体: `analyze_query(&cte.query, QueryEnv { outer: env.outer, ctes: &scope, resolve_unknowns: true })`。本体は `BoundQuery`（CTE の本体が WITH を持つ入れ子もよい）。
   b. 列別名: `cte.columns.len() > query.columns.len()` なら `42P10 WITH query "x" has 2 columns available but 4 columns specified`（位置は CTE 名。【検証済み】）。先頭から `query.columns[i].name` を別名に置き換える（少なければ残りは元の名前。重複した別名も許す。【検証済み】`x(p, p)`）。
   c. `BoundCte { name, query, materialize, col_aliases }` を作り、`CteEntry { name, id: CteId(i), columns, origins }` を `scope.visible` に足す（**次の項目から見える**）。
4. `WithResult { ctes, scope }`。

使われない CTE も解析する（型エラーなどは出る）。実行はしない。

#### 5.9.2 参照の解決（FROM 句の表名）

`from.rs` の表名の解決（5.3.1）が、スキーマなしの名前に対して次の順で探す。

1. `find_cte`: `ctes` の連鎖を内側から見て、`visible` に同名があれば `Found { levels_up, entry }`（**最も内側の宣言が勝つ**。【検証済み】`with x as (..) select * from (with x as (..) select * from x) s` は内側。実表と同名なら CTE が勝つ）。`levels_up` は連鎖の段数（`BoundQuery` の入れ子。3.2.1）。
2. 見つからず、**ある段が `recursive` で `all_names` に同名（未解析）がある**なら `CteLookup::Recursive` → `0A000 WITH RECURSIVE is not supported yet`（位置は表名）。
3. 実表を引く（`resolve_table`）。あれば実表。
4. 実表もなければ `42P01 relation "x" does not exist`（位置は表名）。`is_future_cte` が真なら `DETAIL: There is a WITH item named "x", but it cannot be referenced from this part of the query.` と `HINT: Use WITH RECURSIVE, or re-order the WITH items to remove forward references.` を付ける（【検証済み】自己参照 `with x as (select * from x) ..` も前方参照も同じ）。実表と同名の前方参照は実表に解決される（PG と同じ）。

`Found` なら `Rte { kind: CteRef { levels_up, cte: entry.id }, refname: 別名か CTE 名, columns: entry.columns を別名の列リストで改名 }`、`NsItem { schema: None, hidden_name: 別名ありなら CTE 名 }`。同じ CTE を `from x, x` と 2 回（別名なし）書けば `42712`、`from x a, x b` は通る。

#### 5.9.3 挙動の表（【検証済み】）

| 観点 | 挙動 |
|---|---|
| 後の CTE が前の CTE を参照 | 通る（`CteRef.levels_up = 1`） |
| 前の CTE が後を参照 | `42P01` + DETAIL / HINT |
| `WITH RECURSIVE` でも再帰しない | 通常の WITH として動く。再帰参照は 0A000 |
| 内側の WITH が外側と同名 | 内側の問い合わせの**本体**では内側が優先。内側の CTE の**宣言の中**（`with x as (select a + 10 as a from x)`）の同名参照は、宣言中の内側ではなく**外側**の `x` に解決される（`find_cte` が `visible` だけを見るため。【検証済み】結果は 11） |
| スキーマ付き `public.x` | CTE を引かない（`42P01 relation "public.x" does not exist`） |
| 副問い合わせの中の WITH | 動く（`select (with y as (select v) select * from y) from x` は `y` の本体が外側の `x.v` を参照。`Var.levels_up = 1`） |
| `WITH ... VALUES`、`WITH ... TABLE x` | 動く（`TABLE x` は CTE も引く） |
| 列別名 | 先頭から改名。重複も可。多ければ 42P10 |
| MATERIALIZED / NOT MATERIALIZED | 結果は同じ（`CteMaterialize`。EXPLAIN は 04・10） |
| 使われない CTE の型エラー | 出る（解析する） |
| 出力の unknown | text（`with x as (select 'a')` の列は text） |
| `WITH ... INSERT / UPDATE / DELETE`、データ変更 CTE | 実機は通るが yuzhu は 0A000（パーサ） |
| `INSERT INTO p WITH x AS (..) SELECT ...` | 動く（`source` の `BoundQuery.ctes`） |

### 5.10 `UPDATE ... FROM`、`DELETE ... USING`、`RETURNING`、`INSERT`（N1。`analyzer/dml.rs`）

手順は 4.7。M2 の `analyze_update` / `analyze_delete` を作り替える。

#### 5.10.1 UPDATE ... FROM / DELETE ... USING

- `rtable[0]` = 対象表、`from` = FROM / USING 項目、`rtable[1..]` = その RTE。名前空間は `NsItem` の並び（対象表が先頭）。**同じ表を 2 回**: `update r set v = 1 from r` は `42712 table name "r" specified more than once`、`update r set v = 1 from r as r2 where r2.id = r.id` は通る。対象表に別名があれば、元の表名での参照は `42P01 invalid reference to FROM-clause entry for table "r"` + `HINT: Perhaps you meant to reference the table alias "x".`（【検証済み】）。
- FROM 句の中から対象表は見えない（4.4。DETAIL 付きの `invalid reference` / `column ... does not exist`。【検証済み】`update r set v = 1 from (select id) s`）。WHERE・SET・RETURNING から見える。
- SET の右辺は `rtable` 全体を参照してよい（`update r set v = t.a * 100 from t where t.a = r.id`）。`UpdateSource::Expr` は `coerce_assignment`（M2 のまま。`column "v" is of type integer but expression is of type text`、位置は式）。
- システム列: 対象表の `ctid` は `Var::system(RteId(0), Ctid)`。FROM の表にも `ctid` があるので修飾なしの `ctid` は `42702`（【検証済み】）、`update r set v = 1 where ctid = '(0,1)'` は FROM がなければ通る。
- 複数の FROM 行が同じ対象行に一致するときは、その行を**1 回だけ**更新する（PG と同じ。05 の `Update`・`Delete`）。テストは件数と、一致が一意なものの値だけを見る（どの一致が選ばれるかは不定）。
- 集約: `42803 aggregate functions are not allowed in UPDATE`（SET の右辺）、`... in WHERE`。副問い合わせは SET の右辺・WHERE に書ける（`update r set v = (select max(e) from u where u.a = r.id)`）。
- `WHERE CURRENT OF` と複数列の `SET (a, b) = ...` はパーサが 0A000。
- 関係の種類: 対象が索引なら `42809 cannot open relation "x"` + `DETAIL: This operation is not supported for indexes.`（07 D07-18 / 07-Q14）、シーケンスなら `42809 cannot change sequence "s"`（08 §4.6。`relation_kind` で判定する）。FROM 句に索引を書いたときも `42809 cannot open relation "x"`（同上）。シーケンスを FROM 句で SELECT するのは動く。
- 対象がシステムカタログなら `42501 permission denied for table pg_class`（M2 のまま）。

#### 5.10.2 RETURNING（`RETURNING_ENABLED`。D3-17）

```rust
/// analyzer/dml.rs。X3 と 04 が RETURNING に対応したら true にする
const RETURNING_ENABLED: bool = false;
```

`false` の間は、`returning` が空でなければ最初に `0A000 RETURNING is not supported yet`（位置は最初の項目。M2 と同じ）。`true` のとき:

- `ParseExprKind::Returning` で各項目を解析する（`*`、`t.*`、式、別名。出力列名は 5.11、型・由来は `OutputColumn`）。スコープは INSERT なら対象表だけ、UPDATE / DELETE なら（FROM / USING があっても）`rtable` 全体だが、**対象表以外の RTE の Var が出てきたら** `0A000 RETURNING referencing other tables is not supported yet`（`UPDATE ... FROM ... RETURNING *` は `*` が他の表の列を含むので 0A000。【検証済み】PG は通す）。
- 集約は `42803 aggregate functions are not allowed in RETURNING`（【検証済み】）。副問い合わせは `0A000 subquery in RETURNING is not supported yet`（`lower_single_rel` が SubLink を持てないため。PG は通す）。存在しない列は `42703`、`insert .. returning t.a`（表がない）は `42P01 missing FROM-clause entry for table "t"`。
- 対象表の列は**更新・挿入後の値**（`BoundReturning.targets` の Var は `rte = 0`）。システム列も使える（`returning ctid`）。
- `BoundReturning { targets, columns }`。`INSERT ... SELECT` の `returning` も同じ（SELECT の FROM は見えない）。

#### 5.10.3 INSERT の補足（M2 からの変更点だけ）

- `source` は `BoundQuery`。`InsertSource::Query(q)` が `VALUES`（ORDER BY / LIMIT なし）なら `BoundSetExpr::Values`（M2 の `insert_values`）、それ以外は `analyze_query(q, env { resolve_unknowns: false })`（M2 の `insert_select`。`coercions` の約束は 00 §7 のとおり）。集合演算・WITH を含む SELECT も同じ経路。
- 対象がシーケンスなら `42809 cannot change sequence "s"`、索引なら `42809 cannot open relation "x"`。IDENTITY 列の `identity_insert_rule` / `identity_update_rule`（08 §4.9）を、列ごとの代入の解析から呼ぶ（呼び出し位置だけこの章。N1）。

#### 5.10.4 エラーの表（【検証済み】）

| 状況 | SQLSTATE | メッセージ | 位置 |
|---|---|---|---|
| 対象表がない | 42P01 | `relation "nosuch" does not exist` | 表名 |
| FROM / USING の表がない | 42P01 | 同上 | 表名 |
| 同じ表を別名なしで 2 回 | 42712 | `table name "r" specified more than once` | なし |
| 対象表の元の名前を、別名があるのに使う | 42P01 | `invalid reference to FROM-clause entry for table "r"` + `HINT: Perhaps you meant to reference the table alias "x".` | 修飾名の先頭 |
| FROM 句の中から対象表（修飾あり） | 42P01 | `invalid reference to FROM-clause entry for table "r"` + `DETAIL: There is an entry for table "r", but it cannot be referenced from this part of the query.` | 修飾名の先頭 |
| FROM 句の中から対象表（修飾なし） | 42703 | `column "id" does not exist` + `DETAIL: There is a column named "id" in table "r", but it cannot be referenced from this part of the query.` | ColumnRef の先頭 |
| 修飾なしの列が曖昧 | 42702 | `column reference "a" is ambiguous` | ColumnRef の先頭 |
| SET の列がない | 42703 | `column "nosuch" of relation "r" does not exist` | 列名 |
| `SET t.v`、`SET r.v` | 42703 | `column "t" of relation "r" does not exist`（`r.v` は + `HINT: SET target columns cannot be qualified with the relation name.`） | `t` / `r` |
| 同じ列を 2 回代入 | 42601 | `multiple assignments to same column "v"` | なし（PG は位置を付けない。M2 の実装は 2 つ目の列名に付けているが比較しない） |
| システム列への代入 | 0A000 | `cannot assign to system column "ctid"` | 列名 |
| 代入の型（式が型を持つ） | 42804 | `column "v" is of type integer but expression is of type text` + `HINT: You will need to rewrite or cast the expression.` | 式 |
| 代入の型（unknown のリテラル） | 22P02 | `invalid input syntax for type integer: "x"` | リテラル |
| 集約 | 42803 | `aggregate functions are not allowed in UPDATE` / `... in WHERE` | 集約 |
| SET の副問い合わせの列数 | 42601 | `subquery must return only one column` | `(` |
| SET の副問い合わせが 2 行以上（実行時） | 21000 | `more than one row returned by a subquery used as an expression` | なし |
| 索引・シーケンスが対象 | 42809 | 5.10.1 の「関係の種類」 | 表名 |
| 解析の順序 | — | `update r set v = zz where yy = 1` は `yy`（WHERE が SET より先）。`... returning qq` は SET の列のエラーより先 | — |

### 5.11 出力列名と出力列の由来（N1・N2・N3）

`FigureColname`（`PG:parse_target.c`）を、集約・副問い合わせまで拡張する。M1 の `figure_colname`（`analyzer/expr.rs`）に次を足す。強さは M1 のまま（0 = 名前なし、1 = 弱い（キャストの型名・`CASE`）、2 = 強い）。

| 式 | 名前 | 強さ |
|---|---|---|
| 列参照 | 最後の名前 | 2 |
| 関数呼び出し（**集約を含む**。`FILTER` / `DISTINCT` は影響しない） | 関数名の最後の部分（`count`、`sum`） | 2 |
| `CASE` | `case` | 1 |
| `COALESCE` / `NULLIF` | `coalesce` / `nullif` | 2 |
| `CAST` / `::` | 内側の式の名前が強さ 2 ならそれ（内側が強さ 1 以下なら型名 `int4`、強さ 1） | — |
| `EXISTS (..)` | `exists` | 2 |
| **スカラー副問い合わせ** `(SELECT ..)` | **副問い合わせの最初の出力列の名前**（別名か `FigureColname`。名前がなければ `?column?`）。強さ 2 | 2 |
| `x IN (SELECT ..)`、`x op ANY\|ALL (..)`、`NOT EXISTS (..)`、算術・比較・`AND` / `OR` / `NOT` | `?column?` | 0 |
| `expr COLLATE c` | 内側の式の名前 | 内側のまま |
| 行値 | `row` | 2（Row は 0A000 のため実際には出ない） |

【検証済み】`select (select max(a) from t), (select 1 as k), (select 1), exists(select 1), a in (select 1), not exists(select 1), (select (select 2))` の列名は `max, k, ?column?, exists, ?column?, ?column?, ?column?`。`(select 1)::int` は `?column?`（サブクエリの名前が強さ 2 なのでキャストの型名より優先）、`(select max(a) from t)::text` は `max`。**スカラー副問い合わせの名前は、副問い合わせを解析した結果が要る**ので、`analyze_sublink` が `Analyzer.sublink_names` に `(span.start, span.end) -> 名前` を記録し、`figure_colname` がその表を引く（キャストなどで包まれていても、内側の `Expr::Subquery` の span で引く）。

出力列の由来（`table_oid` / `attnum`）は 3.2.10。`*` の展開は列名と由来を `Rte` から取る。

### 5.12 式の文脈・LIMIT・COLLATE・OPERATOR・その他（N1・N2）

#### 5.12.1 文脈ごとの禁止（`ParseExprKind`）

| 文脈 | 集約 | 副問い合わせ | 備考 |
|---|---|---|---|
| `SelectTarget`、`OrderBy`、`DistinctOn`、`Having` | 許す | 許す | |
| `Where` | `42803 ... in WHERE` | 許す | |
| `GroupBy` | `... in GROUP BY` | 許す | |
| `Limit`、`Offset` | `... in LIMIT` / `... in OFFSET` | 許す | 現スコープの Var は不可（下） |
| `JoinOn` | `... in JOIN conditions` | 許す | |
| `FromFunction` | `... in functions in FROM` | 許す（非相関のみ。左の項目は 0A000） | |
| `Filter` | `... in FILTER` | 許す | |
| `Values` | `... in VALUES` | 許す | |
| `UpdateSet` | `... in UPDATE` | 許す | |
| `Returning` | `... in RETURNING` | 0A000（D3-17） | |
| `ColumnDefault` | `... in DEFAULT expressions` | 0A000 `cannot use subquery in DEFAULT expression` | 列参照は M2 のとおり 0A000 |
| `Check` | `... in check constraints` | 0A000 `cannot use subquery in check constraint` | |

集約のエラーはすべて `42803 aggregate functions are not allowed in {name}`（位置は集約の呼び出し。【検証済み】）。

#### 5.12.2 LIMIT / OFFSET

`transform_limit`（M1）を次のように直す: `ParseExprKind::Limit` / `Offset` で解析し、**現スコープの Var を含まない**ことを検査する。「現スコープの Var」は、`levels_up` が（その Var が副問い合わせの中にあるときはその深さ）に等しいもの（3.2.11 の走査の `depth` と比較する）。違反は `42P10 argument of LIMIT must not contain variables`（`OFFSET`。位置は Var。【検証済み】`select a from t limit a`、`limit (select a from t t2 where t2.a = t.a)`）。外側の Var は通る（`select a from t where a in (select a from u limit t.a)`）。型は `coerce_to_specific_type(INT8, "LIMIT")`（M1 のまま。`limit 1.5` は 2 行、`limit 'x'` は `22P02 invalid input syntax for type bigint: "x"`）。`limit null` は制限なし。負の値は実行時に `2201W LIMIT must not be negative`（05）。

#### 5.12.3 COLLATE（D3-13。`resolve::collate`）

【検証済み】

| 式 | 結果 |
|---|---|
| `'a' COLLATE "C"`、`"POSIX"`、`"default"`、`pg_catalog."default"` | 通る。**式をそのまま返す**（型も変えない。unknown のまま） |
| `'a' COLLATE nosuch` | `42704 collation "nosuch" for encoding "UTF8" does not exist`（位置は `COLLATE`） |
| `'a' COLLATE pg_catalog.nosuch`、`public."C"` | `42704 collation "pg_catalog.nosuch" for encoding "UTF8" does not exist`（`public.C`） |
| `'a' COLLATE nosuch."C"` | `3F000 schema "nosuch" does not exist` |
| `1 COLLATE "C"`、`a COLLATE "C"`（int 列） | `42804 collations are not supported by type integer` |
| `'a' COLLATE "C" = 'b' COLLATE "POSIX"` | PG は `42P21 collation mismatch between explicit collations "C" and "POSIX"`。**yuzhu は検出しない**（通す。既知の差分） |
| `'a' COLLATE "en_US"` | `42704`（M4 に C 以外はない） |

照合順序を持つ型（text、varchar、bpchar、name）か unknown かは、型の `typcategory = 'S'`（または `COLLATE` を受ける型）で判定する。ORDER BY の `c COLLATE "C"` は式として解析する（resjunk の等しさは `Collate` を外した式で比べる。結果は同じ）。

#### 5.12.4 `OPERATOR(schema.op)`（`resolve::qualified_operator`）

`BinaryOp` / `UnaryOp` / `QuantifiedSubquery` の `op_schema` が `Some(s)` のとき: `s` が `pg_catalog` なら通常の `oper(op, ..)`（候補は `operators_named(op)`）。`schema_exists(s)` が偽なら `3F000 schema "nosuch" does not exist`（位置は `operator` キーワード）。存在するが `pg_catalog` でない（`public`）なら候補が空なので `42883 operator does not exist: integer public.+ integer`（演算子名に `schema.` を付けて表示。【検証済み】）。存在しない演算子名（`OPERATOR(pg_catalog.nosuchop)`）はパーサが演算子トークンを読めないので構文エラー。優先順位は 5.1.3。psql の `n.oid OPERATOR(pg_catalog.=) c.relnamespace`、`c.relname OPERATOR(pg_catalog.~) '^(foo)$' COLLATE pg_catalog.default` が通る（`~` の行は 09）。

#### 5.12.5 その他

| 項目 | 規則 |
|---|---|
| `Expr::Row` | `expr.rs` の一般の枝は `0A000 row constructors is not supported yet`（位置は行値の先頭）。`sublink.rs` が IN / ANY / ALL の左辺としてだけ受け取る |
| 全行参照 | 5.2.1 の 3 |
| `IS DISTINCT FROM` | `0A000 IS DISTINCT FROM is not supported yet`（M1 のまま。[03-Q12]） |
| `oid::ANY` を引数に取る関数・集約 | 引数の型が何でもよい（`can_coerce` の `target == ANY` は真。**09 の依頼（T3）で `resolve.rs` の該当 1 行を足す**。N1 が反映） |
| `pg_typeof(x)` | `Literal(Datum::Oid(x の型))`（regtype）に畳む（09 D-9-9。引数は評価しない。N1 が `make_func_call` の後処理に足す） |
| `FnKind::Set` の関数を FROM 句以外で | `0A000 set-returning functions are only supported in the FROM clause`（位置は関数名。`make_func_call` に `allow_set: bool` を足し、5.4.2 だけ true） |
| `SELECT` 句の `generate_series(..)` | 上と同じ 0A000（PG は通す。`m4-query.md` の範囲外） |
| 未解決の `$n` | M1 のまま（`42P02`） |
| 解析の再帰 | `analyze_query` と `transform_expr` の入口で `check_stack_depth()?`（`54001`） |

---

## 6. テスト

### 6.1 パーサの単体テスト（S1。`sql/parser/tests_query.rs`。既存の `tests.rs` は触らず、この章の分を別ファイルにする）

構文木の形と、エラーの SQLSTATE・文言・位置（`Error::resolve_position` した 1 始まりの位置）を確かめる。

| 観点 | ケース |
|---|---|
| WITH | `WITH x AS (SELECT 1) SELECT * FROM x`（`Query.with`、`recursive = false`、`materialized = None`）、`WITH RECURSIVE`、`AS MATERIALIZED` / `AS NOT MATERIALIZED`（`Some(true)` / `Some(false)`）、列別名 `x(a, b)`、複数の CTE、入れ子（CTE の中の WITH、副問い合わせの中の WITH、括弧の中の WITH）、WITH + 集合演算・VALUES・`TABLE x`、`INSERT INTO t WITH ... SELECT` |
| WITH の 0A000 | `WITH x AS (INSERT ...)`（位置は `insert`）、`WITH x AS (SELECT 1) INSERT ...` / `UPDATE` / `DELETE`、`... AS (SELECT 1) SEARCH ...` / `CYCLE`。`WITH x AS SELECT 1`（括弧なし）は構文エラー（位置は `select`）。`WITH x AS (SELECT 1)` だけ（本体なし）は `syntax error at end of input` |
| FROM の関数 | `generate_series(1, 3)`、`pg_catalog.generate_series(1,2) AS g(x)`、引数なし `f()`、引数に式・副問い合わせ、`WITH ORDINALITY`（0A000、位置は `with`）、`ROWS FROM (..)`（0A000）、`rows` という名前の表（`FROM rows`、`FROM rows r`）は表のまま、`g(x int)` の列定義は構文エラー |
| ONLY / `*` | `FROM ONLY t`、`FROM ONLY (t)`、`FROM t *`、`DELETE FROM ONLY t`（M2 のまま） |
| FILTER | `count(*) FILTER (WHERE a > 1)`、`sum(a) FILTER (WHERE ..)` + `OVER`（OVER が 0A000）、`FILTER` なしの `filter` という名前の列、`FILTER (a > 1)`（`where` がない構文エラー） |
| ANY / ALL | `a = ANY (SELECT ..)`、`SOME`、`a < ALL (VALUES (1))`、`a <> ALL ((SELECT 1))`（二重括弧）、`1 + 2 = ANY (SELECT ..)` の左辺が `1 + 2`、`a LIKE ANY (SELECT ..)`（`op = "~~"`）、`a NOT LIKE ALL (..)`（`!~~`）、`a OPERATOR(pg_catalog.=) ANY (..)`（`op_schema`）、`a = ANY (ARRAY[1])` と `a = ANY (1)`（0A000、位置は `ANY`） |
| COLLATE | `'a' COLLATE "C"`、`pg_catalog."default"`、優先順位（`a \|\| 'x' COLLATE "C"` の COLLATE は `'x'` に付く）、`a OPERATOR(pg_catalog.~) 'x' COLLATE pg_catalog.default`、`COLLATE default`（構文エラー） |
| OPERATOR | 中置・前置・`OPERATOR(+)`（スキーマなし）、`OPERATOR(a.b.+)`（0A000）、`OPERATOR` という名前の列（`select operator from t`）は列、優先順位（`1 OPERATOR(pg_catalog.+) 2 * 3` と `1 + 2 OPERATOR(pg_catalog.*) 3` の木） |
| 行値 | `(a, b)`、`ROW(a, b)`、`ROW()`、`ROW(a)`（`explicit = true`）、`(a)`（Row にならない）、`(a, b) IN (SELECT ..)` |
| GROUP BY の特殊形 | `GROUP BY ()`、`GROUP BY ROLLUP (a)`、`GROUP BY a, CUBE (b)`、`GROUP BY GROUPING SETS ((a))`（0A000、位置は項目の先頭）。`GROUP BY ALL` / `DISTINCT` は受理 |
| 位置 | 各 AST ノードの `span.start`（Collate は `COLLATE`、QuantifiedSubquery は演算子、Row は `(` / `ROW`、Cte は名前）。式の `span.end` |
| 回帰 | M1 の既存テストがすべて通る。`SELECT ... FOR UPDATE`、`LATERAL`、`TABLESAMPLE`、`JOIN ... USING (..) AS j`、`(a JOIN b ON ..) AS j`、`OVER`、`WITHIN GROUP` の 0A000 が変わらない |

### 6.2 アナライザの単体テスト（N1〜N3。`analyzer/tests_from.rs`、`tests_agg.rs`、`tests_sub.rs`。`FakeCatalog`）

M1 の `analyzer/tests.rs` のヘルパ（`run`、`err`、`select`、`create`、`names`、`types`）を、P0 が `pub(super)` にして新しいテストファイルから使えるようにする。**主キーつきの表**が要るので、`catalog/fake.rs` に `put_table_with_primary_key(oid, name, columns, pk_attnums)`（`IndexDef` を 1 つ持つ `TableDef`。C1 と共同）を足す。`err(c, sql)` は SQLSTATE を返す（M1 のとおり）。**エラーの文言・位置も確かめるケース**には `err_full` を足す（`(sqlstate, message, position)`）。

**N1（`tests_from.rs`）**

| 観点 | ケース |
|---|---|
| RTE と RteId | `from t join u on ..` の rtable は `[t, u, join]`（後行順）、`from t, u` の `from.len() == 2`、副問い合わせの RTE は `RteKind::Subquery` で `columns` が本体の列、`(VALUES ..)` は `RteKind::Values`、ORDER BY つきは `Subquery`。UPDATE の `rtable[0]` が対象表 |
| 列参照 | 単一・複数の FROM 項目、修飾あり・なし、`public.t.a`、別名で隠された元の表名、4 部名（DB 違いは 0A000、一致は通る）。`levels_up`（2 段の副問い合わせの `x.a` が 2、`y.b` が 1。3.2.1 の例）。システム列（`Var::system`）、`select ctid from t, u`（42702）、`from (select ..) q` の `ctid`（42703）、結合の子の表の `ctid` の修飾あり・なし（42703 + DETAIL） |
| エラーの表 | 5.2.4 の全行（SQLSTATE・文言・位置・DETAIL / HINT） |
| `*` | `select *`、`t.*`、結合の `*`（併合列が先頭）、`t.*` が結合の子の表の全列、FROM なしの `*`、別名つきの列リスト |
| JOIN | USING / NATURAL / ON / CROSS、併合列の `JoinColSource`（INNER は Left、型違いの INNER は Right、LEFT は Left、RIGHT は Right、FULL は Coalesce）、併合列の型（smallint と bigint、varchar と text）、`on` の形（USING は `AND` の等号）、展開された式（INNER の `a` が `t.a` の Var と等しい、FULL は `Coalesce`）、5.3.3 の表の全行、USING のエラー 4 種 |
| 関数・VALUES | `generate_series` の `RteKind::Function`、別名と列名（5.4.2 の 5）、`FnKind::Set` を FROM 外で使うと 0A000、集約の禁止、暗黙の LATERAL の 0A000、`42725`、`42883`（`generate_series(1)`、`public.generate_series`） |
| DML | UPDATE FROM / DELETE USING の rtable と `from`、自己結合（別名あり・なし）、SET の右辺が FROM を参照、WHERE が SET より先にエラー（R12）、FROM から対象表が見えない、`RETURNING_ENABLED` の切り替え（false なら 0A000、true なら 5.10.2 の全ケース） |
| 出力列 | `OutputColumn` の由来（表の列、派生表を通した列、結合の `COALESCE`（由来なし）、CTE の列） |
| `COLLATE` / `OPERATOR` | 5.12.3、5.12.4 の全行（`~` の演算子行が 09 で入るまでは `=` で確かめる） |

**N2（`tests_agg.rs`）**

| 観点 | ケース |
|---|---|
| 集約の解決 | 5.5.1 の例すべて（結果型、`count(*)`、`count(x)`、`DISTINCT`、`FILTER`、`42809` ×3、`42883` ×4、`42725`）。引数の暗黙キャスト（`sum(1::smallint)` は int2 の行、`avg('1'::text)` は `42883`、`max(varchar)` は text の行へのキャストを挟む） |
| 禁止位置 | 5.12.1 の表の全行（WHERE、GROUP BY、LIMIT、OFFSET、JOIN conditions、functions in FROM、FILTER、VALUES、UPDATE、RETURNING（有効時）、check constraints、DEFAULT expressions） |
| 入れ子・外側の集約 | `sum(count(*))`（42803）、`(select max(t.a))`（0A000）、`sum((select count(*) from u))`（通る。副問い合わせの集約は内側のスコープ）、`(select max(u.e + t.b) from u)`（内側の集約。通る） |
| グループ化の検査 | 5.5.3 の【検証済み】の全行。副問い合わせの中の未グループ列の文言（`subquery uses ungrouped column`）。GROUP BY の式の一致（`a + 1`）、結合の別名の展開（`select t.a from t join u using (a) group by a` が通り、`select u.a ... group by a` が 42803）、FULL JOIN の `COALESCE` |
| 関数従属 | 5.5.4 の全行。`group_by` の末尾に Var が足されること（重複なし）。主キーなしの表・複合主キーの一部・UNIQUE |
| 解決順 | 5.6.1〜5.6.4 の全行（GROUP BY が入力列先、ORDER BY が出力列先、整数・非整数の定数、曖昧、`DISTINCT ON` の 12 ケース、`DISTINCT` + ORDER BY）。`ORDER BY TRUE` が 42601（R7） |
| `has_agg` | 集約なし・GROUP BY なし・HAVING なし = false、`having false` だけ = true、ORDER BY の集約だけ = true、DISTINCT ON の式の集約 = true |

**N3（`tests_sub.rs`）**

| 観点 | ケース |
|---|---|
| SubLink の形 | 3.2.8 の表の全行（`test` の木、`Not` で包む位置、`Cast(SubLinkOutput)`）、行値 IN の `And`、`<> ALL` の行値 |
| エラー | 5.7.2 の全行。副問い合わせ本体のエラーが左辺のエラーより先（`select zz in (select yy)` は `yy`） |
| 相関 | `Var.levels_up` が 3.2.1 のとおり（副問い合わせの中の派生表、集合演算の腕、CTE の本体、VALUES の行（`select (values (t.a)) from t` の `t.a` が 1））。ON・HAVING・ORDER BY・SET の中 |
| 集合演算 | 5.8 の全ケース（型の決定が左から 2 つずつ、unknown リテラルの書き換え、`left_coerce` の形、列名、`types` の typmod、外枠の ORDER BY の 5 種のエラー、括弧、優先順位、`ALL` のフラグ、VALUES の腕、腕に WITH） |
| CTE | 5.9 の全ケース（`CteRef.levels_up` の値、`col_aliases` と `query.columns` の改名、MATERIALIZED の対応、前方・自己参照のエラー、RECURSIVE の 0A000 と非再帰、同名の優先、実表を隠す、スキーマ付き、`WITH` + `Nested`） |
| 出力名 | 5.11 の表の全行（スカラー副問い合わせ、キャストで包んだ副問い合わせ、`NOT EXISTS`、入れ子の副問い合わせ） |

### 6.3 共通の SQL テスト（`tests/slt/m4/{join,agg,subquery,setop,cte,dml}/*.slt`。K）

- **PostgreSQL 17 で通ること**が条件（`tests/run.sh --target pg`）。ドラフトは実機で全件通した（付録 A）。
- 行の順序が決まらないものは `ORDER BY` か `query ... rowsort` を付ける（ハッシュ結合・ハッシュ集約・集合演算の出力は不定）。NULL を混ぜる（キー・集約の入力・`IN` の集合）。
- 表名は**ファイルごとの接頭辞**（`jn_`、`ag_`、`sq_`、`so_`、`ct_`、`dm_`）を付け、**ファイルの最後で DROP する**（M2-Q22。同じ DB への再実行が通る）。
- エラーは `statement error (SQLSTATE)`（完全一致）を基本にし、PG と文言が同じものは文言の regex（`statement error <regex>`。1 行の部分一致）を使ってよい。yuzhu 独自の 0A000 は `(0A000)` だけ。**PG で成功するが yuzhu が 0A000 にするものは、PG 側を `skipif yuzhu`、yuzhu 側を `onlyif yuzhu` + `statement error (0A000)` の対で書く**。
- EXPLAIN は使わない（04・10 が `tests/slt/m4/explain` に置く）。

| ファイル | 内容 | ドラフト |
|---|---|---|
| `join/cross_inner.slt` | INNER / CROSS / カンマ結合、自己結合（別名）、3 表の結合、式での結合、NULL キーは一致しない、結合と WHERE の併用、派生表との結合、`count(*)` の検算 | なし（K） |
| `join/using_natural.slt` | 5.3.3 の表: 併合列の位置・型・修飾、FULL の `COALESCE`、複数列、NATURAL（共通名・なし・LEFT）、連鎖、エラー 6 種 | **付録 A-1** |
| `join/outer_and_names.slt` | ON と WHERE の違い、右辺だけの ON、非等値の LEFT、RIGHT、FULL（等値 + 残り、NULL キー）、入れ子の外部結合、ON から見える名前、5.2.4 のエラー、FULL JOIN の 0A000、`ON true` の FULL（`skipif yuzhu`） | **付録 A-2** |
| `join/from_items.slt` | 派生表・列別名・別名なし、VALUES、`generate_series`（別名が列名）、システム列、出力列名、5.4 のエラー | **付録 A-3** |
| `agg/basic.slt` | 結果型、NULL・空入力、DISTINCT、FILTER、解決のエラー、禁止位置、未対応構文（`onlyif yuzhu`） | **付録 A-4** |
| `agg/group_resolution.slt` | GROUP BY が入力列先、ORDER BY が出力列先、位置・定数のエラー、HAVING、DISTINCT / DISTINCT ON | **付録 A-5** |
| `agg/functional_dep.slt` | 5.5.4 の全行、HAVING だけの集約問い合わせ、副問い合わせの未グループ列 | **付録 A-6** |
| `subquery/null_semantics.slt` | IN / NOT IN / ANY / ALL / EXISTS の三値論理と空集合、スカラー副問い合わせ（0 行・2 行）、行値 IN、列数・型のエラー | **付録 A-7** |
| `subquery/correlated.slt` | 2 段の相関、派生表を通り抜ける参照、ON・HAVING・ORDER BY・SET・DELETE の中、未グループ列、LIMIT の変数、CHECK / DEFAULT の禁止、外側の集約（`skipif` / `onlyif`） | **付録 A-8** |
| `subquery/row_in.slt` | 行値の `IN` / `NOT IN` / `= ANY` / `<> ALL`（NULL を含む）、その他の行値演算子（`onlyif yuzhu` で 0A000）。**M4 後半・任意** | なし（K） |
| `setop/union_types.slt` | 型の決定、unknown、列名、外枠の ORDER BY / LIMIT、優先順位・左結合、括弧、ALL、VALUES の腕、エラー | **付録 A-9** |
| `setop/in_contexts.slt` | 集合演算を FROM の派生表・副問い合わせ・INSERT ... SELECT・CTE の本体に置く、腕の中の GROUP BY / DISTINCT / 集約、腕に副問い合わせ | なし（K） |
| `cte/scope.slt` | 5.9.3 の表: 前の CTE、2 回の参照、列別名、同名、副問い合わせの中、MATERIALIZED、WITH + 集合演算 / VALUES / TABLE、INSERT ... SELECT の中、エラー | **付録 A-10** |
| `cte/recursive_syntax.slt` | `WITH RECURSIVE` の非再帰は通る。再帰の本体は PG だけ（`skipif yuzhu`）、yuzhu は `(0A000)`（`onlyif yuzhu`）。WITH + INSERT / UPDATE / DELETE とデータ変更 CTE も同じ対 | なし（K） |
| `dml/update_from.slt` | UPDATE ... FROM / DELETE ... USING: 結合・複数一致・自己結合・派生表・VALUES・generate_series、SET の副問い合わせ、解析の順序、エラー | **付録 A-11** |
| `dml/insert_select.slt` | `INSERT ... SELECT` に結合・集約・集合演算・CTE・generate_series（`pgbench -i` と同じ形）、`INSERT ... VALUES` の副問い合わせ、unknown の列の代入（`''` を char / text へ）、シーケンス・索引への DML の 42809 | なし（K） |
| `dml/returning.slt` | `RETURNING`（INSERT / UPDATE / DELETE、`*`、式、別名、システム列、集約の 42803、FROM 列を含む RETURNING の 0A000（`onlyif yuzhu`））。**`RETURNING_ENABLED` が true になった時点で有効にする** | なし（K） |

### 6.4 差分ランダムテスト（Z）との接続

`yuzhu-fuzz-sql` の生成器は、第 1.2 節の「入れる」の構文だけを生成する（0A000 になるものは生成しない）。この章に関する特に有効な生成: 等値結合（INNER / LEFT / RIGHT / FULL）の連鎖、`GROUP BY` + 集約 + `HAVING`、`IN` / `NOT IN` / `EXISTS` / スカラー（NULL を含む表）、`UNION` / `INTERSECT` / `EXCEPT`（`ALL` あり・なし）、CTE の参照 0 回・1 回・2 回、`DISTINCT ON`、相関副問い合わせ。出力はソートして比べ、エラーは SQLSTATE を比べる。

---

## 7. 実装の分担と工数

00 §17 の担当と範囲に、この章の分を具体化する（日数は AI の実装エージェント 1 本の粗い見積もり。00 の合計の内数）。**P0 が 3.4 のスタブを置いた後に N1〜N3 は並列に進める**。同じファイルを 2 人が触らない。

| 担当 | 編集してよいファイル | 中身と内訳 | 日数 |
|---|---|---|---|
| **S1 パーサ**（この章の分） | `sql/ast.rs`（3.1）、`sql/parser/select.rs`（WITH、FROM の関数・ONLY、GROUP BY の特殊形）、`parser/expr.rs`（FILTER、ANY / ALL、COLLATE、OPERATOR、Row）、`parser/tests_query.rs` | WITH と 0A000（0.4）、FROM の関数・ONLY（0.2）、FILTER・ANY / ALL（0.3）、COLLATE・OPERATOR・Row（0.4）、GROUP BY の特殊形・位置の確認（0.1）、テスト（0.4）。07〜10 の構文は各章の分（00 §17 の S1 の 3 日の内数） | 1.8 |
| **N1 解析: FROM** | `analyzer/{scope,from}.rs`、`analyzer/dml.rs`（UPDATE FROM / DELETE USING / RETURNING / 解析順序 / 関係の種類の 42809）、`analyzer/resolve.rs`（`qualified_operator`、`collate`、`allow_set`、`ANY` 引数、`pg_typeof` の畳み込み、`schema_exists`）、`analyzer/tests_from.rs` | `Scope` と名前解決・診断（1.2）、`*` の展開・システム列（0.3）、JOIN（USING / NATURAL / ON、併合列の展開）（1.0）、派生表・VALUES・関数（0.5）、DML（0.6）、COLLATE・OPERATOR・その他（0.2）、テスト（0.2） | 4 |
| **N2 解析: 集約** | `analyzer/agg.rs`、`analyzer/select.rs`（`analyze_select` の手順 4.3、GROUP BY / ORDER BY / DISTINCT ON / DISTINCT、LIMIT、`FigureColname` の拡張）、`analyzer/tests_agg.rs` | 集約の解決・禁止位置・入れ子（0.8）、グループ化の検査と関数従属（1.0）、`find_target_entry` と ORDER BY の作り直し（0.8）、DISTINCT ON（0.4）、`analyze_select` の統合・has_agg・LIMIT の変数検査・M1 の差異（R7、R8）（0.5）、テスト（0.5） | 4 |
| **N3 解析: サブクエリ** | `analyzer/{sublink,setop,cte}.rs`、`analyzer/tests_sub.rs` | sublink（本体・列数・test・行値・文脈）（1.5）、集合演算（型の決定・リテラルの書き換え・外枠の ORDER BY）（1.5）、CTE（スコープ・別名・参照の解決・診断）（1.0）、`BoundQuery` の入れ子の統合（`analyze_query`、WITH + Nested、VALUES）（0.5）、テスト（0.5） | 5 |
| K（共有テスト。この章の分） | `tests/slt/m4/{join,agg,subquery,setop,cte,dml}/*.slt` | 付録 A のドラフトを整える（ほぼそのまま）、「なし」の 6 ファイルを書く、`onlyif` / `skipif` の整理、PG での再確認 | 3（00 §17 の K の 10 日の内数） |

- **依存**: N1〜N3 は P0（`BoundQuery`・`Var`・スタブ）、N2 は C1 の `primary_key()`（`TableDef.indexes`）、09 の `AGGREGATES` と `FnKind::Set`（それまでは `FakeCatalog` に最小の表を置く）。N3 の CTE の参照（`find_cte`）を N1 の `from.rs` が呼ぶので、**N3 は最初の半日で `find_cte` / `is_future_cte` の実装を済ませる**（N1 の結合テストが CTE を使うため）。
- **統合の確認**: P0 の完了条件（`tests/slt/m1`〜`m3` が通る）を壊さない。`analyzer/tests.rs`（M1・M2・M3 のテスト）が通り続けること。
- 最長経路: P0 → N3（5 日）→ 04 の CTE・サブクエリのルール → 結合。N1 と N2 は 04 の結合・集約の物理化より先に終える。

---

## 8. 未検証の点（実装前に確かめるもの）

- `42703` の「近い名前の HINT」（`Perhaps you meant to reference the column "t.b".`）が出る条件の細部（編集距離の閾値）。この章は出さない（D3-19）。slt は `statement error` の文言に HINT を含めない。
- `errorMissingRTE` / `errorMissingColumn` の DETAIL・HINT の全分岐（`rte_visible_if_lateral`、`rte_visible_if_qualified`）。表にあるもの（FROM 句のカンマの左、INNER / LEFT JOIN の左、UPDATE の対象表、JOIN の ON）は実機で確かめた。RIGHT / FULL JOIN の左（`lateral_ok = false`）で HINT が付かないことは**未検証**（ソースの読みによる）。
- 集合演算の列数エラーの位置（右の腕が `VALUES` のとき、`SELECT` 句が空のとき）。`select` 腕は右の腕の最初の出力式で確かめた。
- `WITH` と `INSERT ... SELECT` の組み合わせ（`INSERT INTO t WITH ... SELECT ...` は確かめた。`WITH ... INSERT` は 0A000）で `ctes` の `levels_up` が `source` の `BoundQuery` を基準にすること。
- 同じ式の SubLink 同士の GROUP BY との一致。【検証済み】PG は構造が等しい副問い合わせを GROUP BY の式と一致と見なす（`select (select max(e) from u) from t group by (select max(e) from u)` は通る）。yuzhu の `same_expr` は SubLink を常に不一致にするので、**未グループの Var を含む SubLink を GROUP BY に書いたときだけ**（PG は通り、yuzhu は `42803`）差が出る。実用がほぼないので許容し、必要なら `same_expr` に SubLink の構造比較を足す（0.3 日）。
- `COLLATE` を持つ式の `ORDER BY` / `DISTINCT` / GROUP BY の一致（`Collate` を外して比べるとしたが、PG は照合順序も比べる。C しかないので結果は同じ）。
- 数値の `generate_series(numeric, numeric)` と日時（`interval`）の版が M4 の表にないことによる差（`generate_series(1, 10.5)` が 42883）。
- `FOR UPDATE` を 0A000 にしてよいか（PG は単一表の SELECT では通す）。ORM が `SELECT ... FOR UPDATE` を送る（Django の `select_for_update`）。M5（複数ライターと行ロック）で扱う。

---

## 9. 確認事項

ユーザーの不在中に仮決めしたことです。ディスク形式に関わるものはありません（この章は構文と解析だけ）。変えたい場合の影響を書いた。`11-tests-plan.md` が `M4-Q` の通し番号に振り直す。

- **[03-Q1] FULL JOIN の 0A000 はプランナが出す（D3-1）**
  - 仮決め: アナライザは判定しない。プランナ（04）が「左だけ・右だけを参照する式の等値が 1 本もない FULL」を 0A000（PG と同じ文言）にする。`ON true` などの定数条件の FULL も、04 は 0A000 にしている（PG は通す。既知の差）。
  - 理由: PG もプランナが出す。`ON true`・`ON false`・`ON t.a = u.a + 0` は PG が通す（実機）。判定には述語の分類が要る。
  - 変えたい場合: アナライザで判定するなら、ON の式を `AND` で分解して「両辺が異なる側だけを参照する `=`」を探す処理（04 と重複）をこの章に足す（0.5 日）。
- **[03-Q2] 結合の別名 Var は解析時に展開する（D3-2）**
  - 仮決め: Bound に `RteKind::Join` の列を指す `Var` は現れない。`JoinColSource` は `*` の展開と診断のために残す。
  - 理由: GROUP BY の検査・出力列の由来・04 の列参照の解決が単純になる。PG も GROUP BY の検査の前に展開する。
  - 変えたい場合: 04 が Join RTE の Var を `sources` で展開する処理を持つ必要があり（1 日）、`check_grouping` が展開後の式を作る（0.5 日）。
- **[03-Q3] 関数従属は主キーだけ。許された Var を `group_by` の末尾に足す（D3-10）**
  - 仮決め: 上のとおり。`pg_depend` には記録しない。
  - 理由: 実機の挙動（UNIQUE NOT NULL は対象外）。グループ内で値が一定なので、キーに足しても意味が変わらず、04 の Aggregate が追加の集約（`first_value`）を持たなくて済む。
  - 変えたい場合: 追加しないなら、04 が「グループ化されていない列は各グループの最初の行の値」を実装する（05 の `Aggregate` に代表行の保持が要る。1 日）。
- **[03-Q4] 外側のスコープに属する集約は 0A000（D3-4）**
  - 仮決め: `(select max(t.a)) from t` のように、集約の引数が外側の列だけのとき 0A000。
  - 理由: PG は集約を外側の問い合わせで評価する（【検証済み】`select (select max(t.a)) from t` は 3）。実装には Bound の集約を外側の `Aggregate` ノードへ持ち上げる処理が要り、実用がほぼない。黙って内側で評価すると結果が違う。
  - 変えたい場合: 04 が「SubLink の中の外側の集約」を外側の集約リストへ移す書き換え（`agglevelsup` 相当。2 日）。
- **[03-Q5] LATERAL は 0A000。診断は PG と同じ（D3-6）**
  - 仮決め: 明示の `LATERAL` はパーサ、関数引数の暗黙の LATERAL はアナライザが 0A000。派生表の LATERAL でない参照は PG と同じ `42P01` / `42703` + DETAIL / HINT。
  - 理由: `m4-query.md` §2.1 が LATERAL を M5〜M6 にしている。pgbench のパーティション確認は失敗してよい。
  - 変えたい場合: `lateral_only` の項目を参照可能にし、04 が相関パラメータ付きの NestedLoop で実行する（3〜4 日）。
- **[03-Q6] `WITH ... INSERT/UPDATE/DELETE` とデータ変更 CTE は 0A000（D3-8）**
  - 仮決め: パーサが拒否する。`INSERT ... SELECT` の中の WITH は動く。
  - 理由: `BoundUpdate` / `BoundDelete` に `ctes` の欄がない。データ変更 CTE は RETURNING を前提にする（M4 後半・任意）。
  - 変えたい場合: `BoundUpdate` / `BoundDelete` に `ctes: Vec<BoundCte>` を足し（00 の変更）、`WITH` を持つ DML の解析（4.7 に 1 手順）と、04・05 の対応（各 1 日）。
- **[03-Q7] `WITH RECURSIVE` の構文は受理し、再帰参照だけ 0A000（D3-7）**
  - 仮決め: 上のとおり。
  - 理由: ORM が `WITH RECURSIVE` を付けて非再帰の CTE を送る。再帰は M5。
  - 変えたい場合: 全面的に 0A000 にするなら、`recursive == true` のパーサの分岐を 1 つ足す（0.1 日）。
- **[03-Q8] 行値は IN / ANY / ALL サブクエリの左辺だけ（D3-12。M4 後半・任意）**
  - 仮決め: `(a, b) [NOT] IN (SELECT ..)`、`(a, b) = ANY (..)`、`(a, b) <> ALL (..)` を受理。他は 0A000。
  - 理由: 契約（00 §6.2）が `test` の形を決めている。`m4-query.md` §2.1 が任意としている。
  - 変えたい場合: 受理しないなら `Expr::Row` の作成をやめて従来の 0A000 に戻す（パーサ 0.1 日、アナライザの分の 0.3 日が減る）。
- **[03-Q9] `ANY` / `ALL` の配列形は 0A000（D3-12）**
  - 仮決め: M5（配列型）まで。psql の `\d tbl`（任意）の 2 本が失敗する。
  - 変えたい場合: `int2[]` / `oid[]` の最小の配列と `ANY(array)` の `Expr` 変種が要る（00 の変更。09 と調整。3 日）。
- **[03-Q10] `COLLATE` は "C" / "POSIX" / "default" だけ受理して無視する（D3-13）**
  - 仮決め: 上のとおり。`42P21`（明示照合順序の衝突）は検出しない。
  - 理由: 照合順序は C しかない。psql が `COLLATE pg_catalog.default` を使う。
  - 変えたい場合: 照合順序を式に持たせるなら `Expr` に collation の欄が要る（M5 以降）。
- **[03-Q11] FROM 句の関数は `FnKind::Set` だけ（D3-9）**
  - 仮決め: `generate_series` だけ。`select * from lower('A')` は 0A000。
  - 理由: `FunctionScan` は集合返却だけを実行する。
  - 変えたい場合: スカラー関数を 1 行の関数スキャンにする（アナライザ 0.2 日、04 / 05 に 0.5 日）。
- **[03-Q12] `IS DISTINCT FROM` は 0A000 のまま**
  - 仮決め: M1 の拒否を維持。
  - 理由: 00 の `ExprKind` に変種がない。ORM が使うことがあるが、M4 の完了条件に影響しない。
  - 変えたい場合: `ExprKind::IsDistinctFrom { left, right, eq_op, negated }` を 00 に足す（式の走査・評価・deparse で各 0.2 日）。**M4 後半の任意項目として推奨**（`ON a IS NOT DISTINCT FROM b` の結合も要る。ハッシュ結合のキーにはならない）。
- **[03-Q13] JOIN の別名（`USING ... AS j`、`(t JOIN u ON ..) AS j`）は 0A000（D3-11）**
  - 仮決め: M1 の拒否を維持。
  - 理由: ORM がまれにしか生成しない。名前空間の「別名が子を隠す」規則だけが増えるが、実装は小さい（0.5 日）ので、M4 の余力があれば入れてよい。
  - 変えたい場合: AST に `Join.alias` と `JoinConstraint::Using` の別名を足し、`NsItem.rel_visible` を使う（N1 に 0.5 日）。
- **[03-Q14] RETURNING は解析を実装し、`RETURNING_ENABLED` で解禁する（D3-17）**
  - 仮決め: 解禁は X3 と 04 の対応後。対象表の列だけ。FROM / USING の列と副問い合わせは 0A000。
  - 理由: 00 D-26。`BoundReturning` が `lower_single_rel` を前提にしている。
  - 変えたい場合: FROM の列を許すなら `BoundReturning` を `Vec<BoundExpr>`（rtable 全体の Var）にし、04 が Update / Delete ノードの出力に FROM の列を載せる（2 日）。
- **[03-Q15] 全行参照（`count(t)`、`select t from t`）は 0A000**
  - 仮決め: M5 以降（複合型が要る）。
  - 変えたい場合: `record` 型と `Datum::Record` が要る（M5 以降）。
- **[03-Q16] 未対応の集約名は 0A000（D3-18）**
  - 仮決め: `string_agg`、`array_agg`、`json_agg`、`stddev`、`variance`、`var_*`、`stddev_*`、`corr`、`covar_*`、`regr_*`、`bit_and`、`bit_or`、`bit_xor`、`percentile_*`、`mode`、`any_value`、`range_agg` ほか、PG の組み込みの集約名のうち `AGGREGATES` にないもの。
  - 理由: 「関数が存在しない」（42883）と区別でき、M5 での追加が分かりやすい。
- **[03-Q17] エラーの DETAIL / HINT は主要なものだけ（D3-19）**
  - 仮決め: 近い名前の HINT は出さない。lateral・別名・CTE の DETAIL / HINT は出す。
  - 変えたい場合: 編集距離の実装（`varstr_levenshtein` 相当）と、`searchRangeTableForCol` の候補探索（1 日）。

---

## 10. 00 への変更提案（統合時に反映）

| # | 提案 | 理由 |
|---|---|---|
| P3-1 | 00 §7 に「**`BoundExpr` の `Var` は `RteKind::Join` の列を指さない**（結合の別名は解析時に展開済み）」を追記する。`RteKind::Join.sources` は `*` の展開と診断のためのメタ情報。04 §5.3 の `fill_join_columns`（`sources` から結合の列の式を作る）は不要になる（残しても害はない。`FromItem::Join.on` の USING の条件は 03 が作る） | 04 が Join RTE の Var を扱う必要がなくなる（D3-2） |
| P3-2 | 00 §7 の `BoundSelect.group_by` の注に「関数従属で許された Var が末尾に追加されうる」を足す。04 §5.4 が「GROUP BY の式と一致しない列を従属列として key に足す」処理を持つので、03 の追加と**重複するが無害**（どちらか一方でよい。03 が足すので 04 の補完は不要にしてもよい） | D3-10 |
| P3-3 | 00 §7 の `BoundDistinct::On` の注を「`positions` は distinct の順序（ORDER BY に一致した項目の順、続けて未出の DISTINCT ON の式）。04 は `order_by` の後ろに未出の分を足した並びで 1 回 Sort する」に直す | 3.2.4 |
| P3-4 | `analyzer/bound.rs` に `BoundQuery::walk_exprs(&self, depth_base: u16, f: &mut dyn FnMut(&BoundExpr, u16))`（問い合わせの中の全式を、属するスコープの深さつきで走査。SubLink・派生表・集合演算の腕・CTE・VALUES の行・ON を含む）を足す（担当 A / P0）。`expr/walk.rs` の `Expr::walk` が SubLink の `query` に降りるときに使う | 5.5.3（グループ化の検査）・5.12.2（LIMIT の変数検査）・04（相関の検出）が共有する |
| P3-5 | 00 §6.1 / §7 に `levels_up` の数え方（3.2.1）を追記する。`BoundSetExpr::Values` の行は rtable が空の 1 つのスコープ | 04 の列参照の解決とスコープの積み方が従う |
| P3-6 | 00 §7 の `BoundCte.col_aliases` の注に「宣言どおりの別名。`query.columns[i].name` は別名適用後」を足す | D3-16 |
| P3-7 | 00 §7.1 の AST の表に、この章の 3.1 の具体化を反映する（`Query.with`、`With`、`Cte`、`TableRef::Function`、`Expr::Function.filter`、`Expr::QuantifiedSubquery`、`Expr::Collate`、`Expr::Row`、`BinaryOp` / `UnaryOp` の `op_schema`、`Quantifier`）。`TableRef::Values` は不要 | R9 |
| P3-8 | この章が使う SQLSTATE は、00 §15.3 の追加（`DUPLICATE_ALIAS` 42712）以外すべて `error.rs` に既存（`AMBIGUOUS_COLUMN` 42702、`INVALID_COLUMN_REFERENCE` 42P10、`GROUPING_ERROR` 42803、`DUPLICATE_COLUMN` 42701、`WRONG_OBJECT_TYPE` 42809、`AMBIGUOUS_FUNCTION` 42725、`CANNOT_COERCE` 42846、`INVALID_SCHEMA_NAME` 3F000、`UNDEFINED_OBJECT` 42704 ほか）。**追加の SQLSTATE は不要**と明記する | 確認 |
| P3-9 | `catalog/builtin.rs` に `is_set_returning(&BuiltinFunction) -> bool`（`FnKind::Set` か）を足す（09 の担当）。この章は `func.kind` を直接見てもよい | 5.4.2 |
| P3-10 | 00 §4 のモジュール構成に `analyzer/tests_{from,agg,sub}.rs`、`sql/parser/tests_query.rs` を足す（テスト用） | 7 |
| P3-11 | `analyzer/scope.rs` の `ExprKind`（M1）を `ParseExprKind` に改名する（P0） | 3.3 |


### 10.1 他章への申し送り（この章の決定が前提になるもの）

| 相手 | 内容 |
|---|---|
| `04-planner-optimizer.md`（L1、L2） | (1) `Var.levels_up` と `CteRef.levels_up` の数え方は 3.2.1（04 §4.1 の前提と一致。**`BoundSetExpr::Values` の行は rtable が空の 1 つのスコープとして積む**ことだけが追加）。(2) 結合の別名 Var は Bound に来ない（P3-1）。(3) 関数従属で許された列は 03 が `group_by` の末尾に足す（P3-2。04 §5.4 の補完と重複するが無害）。(4) `BoundDistinct::On` の Sort の keys は「`order_by` ++ 未出の ON の式」（3.2.4。04 §5.5 の記述を直す）。(5) FULL JOIN の 0A000 は 04 が出す（D3-1。`ON true` も 0A000）。(6) 外側の集約・LATERAL は 03 が 0A000。(7) 集合演算の外枠の LIMIT / OFFSET に外側の列があれば 03 が 0A000（5.8.1 の 7）。(8) `FromItem::Join.on = None` は「常に真」。USING の `on` は 03 が作る（`AND(l = r, ..)`）。(9) `BETWEEN` の分解、`IN (list)` の `InList`、`x IN (SELECT ..)` の `SubLink::Any`（04 §4.1 の前提どおり） |
| `05-executor.md`（X1〜X3） | (1) `UPDATE ... FROM` / `DELETE ... USING` で対象行が複数回当たったら 1 回だけ更新（05 §950 の記述と一致）。(2) `RETURNING` の列は更新・挿入後の値、`rte = 0` の Var だけ。(3) スカラー副問い合わせが 2 行以上なら実行時 `21000`、`LIMIT` が負なら `2201W` / `2201X`。(4) `generate_series` の `FnKind::Set` は FROM 句だけ（アナライザが他を 0A000） |
| `09-types-functions.md`（T1〜T3） | (1) `AGGREGATES` の 43 行、`aggregates_named`。(2) `FnKind::Set(SetFn)` と `column_name`、`oid::ANY`。(3) `~ ~* !~ !~*` の演算子行（`OPERATOR(pg_catalog.~)` が引く）。(4) 09 の依頼（`pg_typeof` の畳み込み、`ANY` 引数、`FROM generate_series(1,5) AS aid` の列名）は 5.4.2、5.12.5 に反映した |
| `07-catalog-ddl.md`、`08-sequence-serial.md`（C1、Q1） | `resolve_table` の `relation_kind` による `42809`（索引・シーケンス。N1 が実装）、`identity_insert_rule` / `identity_update_rule` の呼び出し位置（5.10.3）、`TableDef::primary_key()`（関数従属） |
| `10-explain-copy-compat.md`（E1、S） | `analyze` の `Statement::Explain` の枝は P0 が置き E1 が書く。`count_rtes` は `Rte` を再帰で数える（派生表の `BoundQuery` の中も）。EXPLAIN の `COSTS OFF` で式を出すとき、結合の別名は展開済みの式（`COALESCE(t.a, u.a)`）で来る |
| `11-tests-plan.md`（K、Z） | 6.3 の slt、付録 A のドラフト、6.4 の生成範囲。確認事項は 9 節の `[03-Q1]`〜`[03-Q17]` |

---

## 付録 A. 共通 slt のドラフト（PostgreSQL 17.11 で全件通過を確認済み）

`tests/run.sh` と同じランナー（sqllogictest 0.29.1、`--engine postgres --label pg`）で、各ファイルを単独で PostgreSQL 17.11 に流して通ることを確かめた（11 ファイル、合計約 1500 行）。`onlyif yuzhu` / `skipif yuzhu` の行は PG では読み飛ばされる。K はこれを `tests/slt/m4/<dir>/<name>.slt` に置き、yuzhu で通る状態にする（yuzhu で落ちるものは、実装の不備か、この章の仕様との食い違いなので、報告して直す）。表名の接頭辞はディレクトリごと（`jn_`、`ag_`、`sq_`、`so_`、`ct_`、`dm_`）。`statement error` は、PG と文言が同じものは文言の regex、`0A000` は `(0A000)` だけで照合する。`query` の型文字は `I`（整数）、`T`（文字列）、`R`（浮動小数）、`?`（任意。bool の `t` / `f`）。

### A-1. `tests/slt/m4/join/using_natural.slt`

<details><summary>join/using_natural.slt（153 行）</summary>

```
# JOIN ... USING / NATURAL JOIN: 併合列の位置・型・修飾、FULL の COALESCE、連鎖、エラー

statement ok
CREATE TABLE jn_t (a int, b int, c text)

statement ok
CREATE TABLE jn_u (a int, e int, c text)

statement ok
CREATE TABLE jn_k1 (k smallint, s varchar(5))

statement ok
CREATE TABLE jn_k2 (k bigint, s text)

statement ok
INSERT INTO jn_t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, NULL, 'z')

statement ok
INSERT INTO jn_u VALUES (1, 100, 'x'), (3, 300, 'q'), (4, 400, 'w')

statement ok
INSERT INTO jn_k1 VALUES (1, 'a'), (2, 'b')

statement ok
INSERT INTO jn_k2 VALUES (1, 'a'), (3, 'c')

# 併合列は先頭に 1 回だけ。その後に左の残り、右の残り
query IITIT
SELECT * FROM jn_t JOIN jn_u USING (a) ORDER BY a
----
1 10 x 100 x
3 NULL z 300 q

# 修飾すれば元の列（t.* / u.* は USING の列も含む）
query IITIIT
SELECT jn_t.*, jn_u.* FROM jn_t JOIN jn_u USING (a) ORDER BY 1
----
1 10 x 1 100 x
3 NULL z 3 300 q

# FULL JOIN の併合列は COALESCE。t.a と u.a は元の列（NULL になりうる）
query IIII
SELECT a, jn_t.a, jn_u.a, e FROM jn_t FULL JOIN jn_u USING (a) ORDER BY 1
----
1 1 1 100
2 2 NULL NULL
3 3 3 300
4 NULL 4 400

# LEFT / RIGHT の併合列
query II
SELECT a, e FROM jn_t LEFT JOIN jn_u USING (a) ORDER BY a
----
1 100
2 NULL
3 300

query II
SELECT a, b FROM jn_t RIGHT JOIN jn_u USING (a) ORDER BY a
----
1 10
3 NULL
4 NULL

# 複数列の USING。指定順に先頭へ
query ITII
SELECT * FROM jn_t JOIN jn_u USING (a, c) ORDER BY a
----
1 x 10 100

# NATURAL JOIN は共通名（a と c）がすべて USING になる
query ITII
SELECT * FROM jn_t NATURAL JOIN jn_u ORDER BY a
----
1 x 10 100

query ITII
SELECT * FROM jn_t NATURAL LEFT JOIN jn_u ORDER BY a
----
1 x 10 100
2 y 20 NULL
3 z NULL NULL

# 共通名がなければ直積（エラーにならない）
statement ok
CREATE TABLE jn_p (id int, v int)

statement ok
CREATE TABLE jn_kt (k text)

statement ok
INSERT INTO jn_p VALUES (1, 1), (2, 2)

query I
SELECT count(*) FROM jn_t NATURAL JOIN jn_p
----
6

# 型が違う列の USING: 併合列の型は共通型、元の列は元の型
query TTT
SELECT pg_typeof(k), pg_typeof(jn_k1.k), pg_typeof(jn_k2.k) FROM jn_k1 JOIN jn_k2 USING (k)
----
bigint smallint bigint

query TTT
SELECT pg_typeof(k), pg_typeof(jn_k1.k), pg_typeof(jn_k2.k) FROM jn_k1 FULL JOIN jn_k2 USING (k) LIMIT 1
----
bigint smallint bigint

# varchar と text: 併合列は左（varchar）の型が残る
query T
SELECT pg_typeof(s) FROM jn_k1 JOIN jn_k2 USING (s) LIMIT 1
----
character varying

# 連鎖: 2 つ目の USING の左は結合。a は 1 つに見える
query IIITTT
SELECT * FROM jn_t JOIN jn_u USING (a) JOIN jn_u jn_u2 USING (a, e) ORDER BY a
----
1 100 10 x x x
3 300 NULL z q q

# WHERE / ORDER BY / GROUP BY で併合列を修飾なしで使う
query II
SELECT a, e FROM jn_t JOIN jn_u USING (a) WHERE a > 1 ORDER BY a
----
3 300

# エラー
statement error specified in USING clause does not exist in left table
SELECT * FROM jn_t JOIN jn_u USING (zz)

statement error does not exist in left table
SELECT * FROM jn_t JOIN jn_u USING (e)

statement error does not exist in right table
SELECT * FROM jn_t JOIN jn_u USING (b)

statement error appears more than once in USING clause
SELECT * FROM jn_t JOIN jn_u USING (a, a)

# 結合の左に同名の列が 2 つ残っている（c）
statement error common column name "c" appears more than once in left table
SELECT * FROM jn_t JOIN jn_u USING (a) JOIN jn_u jn_u2 USING (c)

statement error operator does not exist: smallint = text
SELECT * FROM jn_k1 JOIN jn_kt USING (k)

statement error column reference "b" is ambiguous
SELECT b FROM jn_t JOIN jn_t jn_t2 USING (a)

statement ok
DROP TABLE jn_t, jn_u, jn_k1, jn_k2, jn_p, jn_kt
```

</details>

### A-2. `tests/slt/m4/join/outer_and_names.slt`

<details><summary>join/outer_and_names.slt（159 行）</summary>

```
# 外部結合（ON と WHERE の違い、FULL の非等値の条件、入れ子）、名前空間のエラー

statement ok
CREATE TABLE jn_t (a int, b int, c text)

statement ok
CREATE TABLE jn_u (a int, e int, c text)

statement ok
CREATE TABLE jn_p (id int, v int)

statement ok
INSERT INTO jn_t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, NULL, 'z')

statement ok
INSERT INTO jn_u VALUES (1, 100, 'x'), (3, 300, 'q'), (4, 400, 'w')

statement ok
INSERT INTO jn_p VALUES (1, 1), (2, 2)

# ON の条件は結合の一致の判定、WHERE は結合の後の絞り込み
query IIT
SELECT jn_t.a, jn_u.e, jn_t.c FROM jn_t LEFT JOIN jn_u ON jn_t.a = jn_u.a AND jn_u.e > 150 ORDER BY 1
----
1 NULL x
2 NULL y
3 300 z

query IIT
SELECT jn_t.a, jn_u.e, jn_t.c FROM jn_t LEFT JOIN jn_u ON jn_t.a = jn_u.a WHERE jn_u.e > 150 ORDER BY 1
----
3 300 z

# 右辺だけの条件を ON に書いた外部結合: 右の行を絞るだけで左の行は残る
query II
SELECT jn_t.a, jn_u.a FROM jn_t LEFT JOIN jn_u ON jn_u.a = 3 ORDER BY 1, 2
----
1 3
2 3
3 3

# 非等値の LEFT JOIN
query II
SELECT jn_t.a, jn_u.a FROM jn_t LEFT JOIN jn_u ON jn_t.a < jn_u.a ORDER BY 1, 2
----
1 3
1 4
2 3
2 4
3 4

# RIGHT は左右を入れ替えた LEFT と同じ集合
query II rowsort
SELECT jn_t.a, jn_u.a FROM jn_t RIGHT JOIN jn_u ON jn_t.a = jn_u.a
----
1 1
3 3
NULL 4

# FULL: 等値 + 残りの条件
query IIII
SELECT jn_t.a, jn_t.b, jn_u.a, jn_u.e FROM jn_t FULL JOIN jn_u ON jn_t.a = jn_u.a AND jn_t.b < jn_u.e ORDER BY 1, 3
----
1 10 1 100
2 20 NULL NULL
3 NULL NULL NULL
NULL NULL 3 300
NULL NULL 4 400

# 等値のキーが NULL の行は一致しない
statement ok
INSERT INTO jn_t VALUES (NULL, 0, 'n')

statement ok
INSERT INTO jn_u VALUES (NULL, 0, 'n')

query II rowsort
SELECT jn_t.a, jn_u.a FROM jn_t FULL JOIN jn_u ON jn_t.a = jn_u.a
----
1 1
2 NULL
3 3
NULL 4
NULL NULL
NULL NULL

# 3 つの結合と、ON は直前の結合の列と自分の右辺が見える
query III
SELECT jn_t.a, jn_u.e, jn_p.v FROM jn_t JOIN jn_u ON jn_t.a = jn_u.a JOIN jn_p ON jn_p.id = jn_u.a ORDER BY 1
----
1 100 1

query III
SELECT jn_t.a, jn_u.e, jn_p.v FROM jn_t LEFT JOIN jn_u ON jn_t.a = jn_u.a LEFT JOIN jn_p ON jn_p.id = jn_t.a ORDER BY 1 NULLS LAST
----
1 100 1
2 NULL 2
3 300 NULL
NULL NULL NULL

# カンマと JOIN の混在: JOIN の ON から見えるのはその JOIN の左右だけ
query I
SELECT count(*) FROM jn_t, jn_u JOIN jn_p ON jn_p.id = jn_u.a
----
4

statement error invalid reference to FROM-clause entry for table "jn_t"
SELECT * FROM jn_t, jn_u JOIN jn_p ON jn_p.id = jn_t.a

# 参照できない位置の表（派生表から左の項目: LATERAL がないとエラー）
statement error invalid reference to FROM-clause entry for table "jn_t"
SELECT * FROM jn_t, (SELECT jn_t.a) s

statement error column "a" does not exist
SELECT * FROM jn_t, (SELECT a) s

# 別名で隠された名前
statement error invalid reference to FROM-clause entry for table "jn_t"
SELECT jn_t.a FROM jn_t AS x

statement error table name "jn_t" specified more than once
SELECT * FROM jn_t, jn_t

statement error table name "x" specified more than once
SELECT * FROM jn_t x, jn_u x

statement error missing FROM-clause entry for table "zz"
SELECT zz.a FROM jn_t

statement error column "zz" does not exist
SELECT zz FROM jn_t

statement error column jn_t.zz does not exist
SELECT jn_t.zz FROM jn_t

statement error column reference "a" is ambiguous
SELECT a FROM jn_t, jn_u

statement error argument of JOIN/ON must be type boolean, not type integer
SELECT * FROM jn_t JOIN jn_u ON jn_t.a

statement error aggregate functions are not allowed in JOIN conditions
SELECT * FROM jn_t JOIN jn_u ON count(*) > 1

statement error table "x" has 3 columns available but 4 columns specified
SELECT * FROM jn_t AS x(p, q, r, s)

# FULL JOIN の 0A000（結合キーがない）。ON が定数だけの FULL は PG だけが通す（yuzhu の 04 は結合キーがなければ 0A000）
statement error FULL JOIN is only supported with merge-joinable or hash-joinable join conditions
SELECT * FROM jn_t FULL JOIN jn_u ON jn_t.a < jn_u.a

skipif yuzhu
query I
SELECT count(*) FROM jn_t FULL JOIN jn_u ON true
----
16

onlyif yuzhu
statement error (0A000)
SELECT count(*) FROM jn_t FULL JOIN jn_u ON true

statement ok
DROP TABLE jn_t, jn_u, jn_p
```

</details>

### A-3. `tests/slt/m4/join/from_items.slt`

<details><summary>join/from_items.slt（166 行）</summary>

```
# FROM 句の派生表・VALUES・generate_series・別名と列別名・システム列・出力列名

statement ok
CREATE TABLE jn_t (a int, b int, c text)

statement ok
INSERT INTO jn_t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, NULL, 'z')

# 派生表: 別名あり・列別名・別名なし（PG16 以降）
query II
SELECT s.x, s.y FROM (SELECT a, b FROM jn_t) AS s(x, y) ORDER BY 1
----
1 10
2 20
3 NULL

# 列別名は先頭から改名する。残りは元の名前
query II
SELECT p, b FROM (SELECT a, b FROM jn_t) AS s(p) ORDER BY 1
----
1 10
2 20
3 NULL

query I
SELECT * FROM (SELECT 1)
----
1

query IT
SELECT * FROM (SELECT 1 AS a, 'k' AS b) s
----
1 k

# 派生表の中の派生表、派生表と表の結合
query II
SELECT t.a, q.m FROM jn_t t JOIN (SELECT a, max(b) AS m FROM jn_t GROUP BY a) q ON q.a = t.a ORDER BY 1
----
1 10
2 20
3 NULL

# 出力列に同名が 2 つあっても * は両方出す。修飾して参照すると曖昧
query II
SELECT * FROM (SELECT 1 AS a, 2 AS a) s
----
1 2

statement error column reference "a" is ambiguous
SELECT s.a FROM (SELECT 1 AS a, 2 AS a) s

statement error table "s" has 1 columns available but 2 columns specified
SELECT * FROM (SELECT 1 AS a) s(x, y)

# VALUES を FROM に: 列名は column1.. か列別名
query IT
SELECT column1, column2 FROM (VALUES (1, 'a'), (2, 'b')) v ORDER BY 1
----
1 a
2 b

query IT
SELECT a, b FROM (VALUES (1, 'a'), (2, 'b')) AS v(a, b) WHERE a > 1
----
2 b

query T
SELECT pg_typeof(column1) FROM (VALUES (1), (2.5)) v LIMIT 1
----
numeric

statement error VALUES lists must all be the same length
SELECT * FROM (VALUES (1, 2), (3)) v

# generate_series: 別名が列名にもなる（pgbench -i の `from generate_series(1, N) as aid`）
query I
SELECT g FROM generate_series(1, 3) AS g ORDER BY g
----
1
2
3

query I
SELECT x FROM generate_series(1, 2) AS g(x) ORDER BY x
----
1
2

query I
SELECT generate_series FROM generate_series(1, 2) ORDER BY 1
----
1
2

query T
SELECT pg_typeof(g) FROM generate_series(1::bigint, 2) g LIMIT 1
----
bigint

query II
SELECT a, g FROM jn_t, generate_series(1, 2) g WHERE a = g ORDER BY a
----
1 1
2 2

query I
SELECT count(*) FROM generate_series(1, 10000)
----
10000

query I
SELECT g FROM generate_series(3, 1, -1) g ORDER BY g
----
1
2
3

statement error step size cannot equal zero
SELECT * FROM generate_series(1, 3, 0)

statement error function generate_series\(integer\) does not exist
SELECT * FROM generate_series(1)

statement error table name "g" specified more than once
SELECT * FROM generate_series(1, 2) AS g, generate_series(1, 2) AS g

statement error aggregate functions are not allowed in functions in FROM
SELECT * FROM generate_series(1, count(*))

# システム列: 表だけにある。結合の子の表は修飾すれば使える
query I
SELECT count(*) FROM jn_t WHERE ctid IS NOT NULL
----
3

query I
SELECT count(DISTINCT jn_t.ctid) FROM jn_t JOIN jn_t j2 USING (a)
----
3

statement error column "ctid" does not exist
SELECT ctid FROM (SELECT * FROM jn_t) q

statement error column reference "ctid" is ambiguous
SELECT ctid FROM jn_t, jn_t j2

# 出力列名（派生表の列名として参照して確かめる）: 集約は関数名、副問い合わせは最初の出力列の名前、CASE は case、EXISTS は exists
query IIIII
SELECT count, max, "?column?", "case", int8 FROM (SELECT count(*), (SELECT max(a) FROM jn_t), a + 1, CASE WHEN a > 1 THEN 1 END, 1::bigint FROM jn_t GROUP BY a) s ORDER BY "?column?"
----
1 3 2 NULL 1
1 3 3 1 1
1 3 4 1 1

query IT
SELECT exists, "?column?" FROM (SELECT EXISTS (SELECT 1), (SELECT 1)::int) s
----
t 1

query IT
SELECT k, "?column?" FROM (SELECT (SELECT 5 AS k), (SELECT (SELECT 2))) s
----
5 2

statement ok
DROP TABLE jn_t
```

</details>

### A-4. `tests/slt/m4/agg/basic.slt`

<details><summary>agg/basic.slt（133 行）</summary>

```
# 集約: 結果型、NULL と空入力、DISTINCT、FILTER、解決のエラー、禁止位置

statement ok
CREATE TABLE ag_t (a int, b int, c text, f bool)

statement ok
INSERT INTO ag_t VALUES (1, 10, 'x', true), (2, 20, 'y', false), (3, NULL, 'x', NULL), (3, 30, NULL, true)

# 結果型（sum(int4) = int8、sum(int8) = numeric、avg(整数) = numeric、avg(float) = float8）
query TTTTTT
SELECT pg_typeof(count(*)), pg_typeof(sum(a)), pg_typeof(sum(a::bigint)), pg_typeof(avg(a)), pg_typeof(avg(a::float8)), pg_typeof(max(c)) FROM ag_t
----
bigint bigint numeric numeric double precision text

query IIRI
SELECT count(*), count(b), avg(b::float8), sum(b) FROM ag_t
----
4 3 20 60

query ITT
SELECT min(a), max(c), min(c) FROM ag_t
----
1 y x

# 空入力: GROUP BY なしは 1 行（count は 0、他は NULL）。GROUP BY ありは 0 行
query IIIT
SELECT count(*), count(a), sum(a), max(c) FROM ag_t WHERE false
----
0 0 NULL NULL

query II
SELECT a, count(*) FROM ag_t WHERE false GROUP BY a
----

# DISTINCT と FILTER
query IIII
SELECT count(DISTINCT a), count(DISTINCT c), sum(DISTINCT a), count(*) FILTER (WHERE a > 1) FROM ag_t
----
3 2 6 3

query ??? 
SELECT bool_and(f), bool_or(f), every(f) FROM ag_t
----
f t f

query ?
SELECT sum(a) FILTER (WHERE false) IS NULL FROM ag_t
----
t

# NULL を含むキーのグループ化（NULL どうしは同じグループ）
query TI
SELECT c, count(*) FROM ag_t GROUP BY c ORDER BY c NULLS LAST
----
x 2
y 1
NULL 1

# 集約の入った式・HAVING・CASE
query IIR
SELECT a, sum(b) + 1, avg(b::float8) FROM ag_t GROUP BY a HAVING count(*) >= 1 ORDER BY a
----
1 11 10
2 21 20
3 31 30

query T
SELECT CASE WHEN count(*) > 3 THEN 'many' ELSE 'few' END FROM ag_t
----
many

# 解決のエラー（5.5.1）
statement error DISTINCT specified, but abs is not an aggregate function
SELECT abs(DISTINCT a) FROM ag_t

statement error FILTER specified, but abs is not an aggregate function
SELECT abs(a) FILTER (WHERE true) FROM ag_t

statement error function abs\(\) does not exist
SELECT abs(*) FROM ag_t

statement error count\(\*\) must be used to call a parameterless aggregate function
SELECT count() FROM ag_t

statement error function sum\(text\) does not exist
SELECT sum(c) FROM ag_t

statement error function sum\(unknown\) is not unique
SELECT sum('1') FROM ag_t

statement error function min\(boolean\) does not exist
SELECT min(f) FROM ag_t

statement error function count\(integer, integer\) does not exist
SELECT count(a, b) FROM ag_t

statement error argument of FILTER must be type boolean, not type integer
SELECT count(*) FILTER (WHERE a) FROM ag_t

# 禁止位置と入れ子
statement error aggregate functions are not allowed in WHERE
SELECT count(*) FROM ag_t WHERE count(*) > 1

statement error aggregate functions are not allowed in GROUP BY
SELECT 1 FROM ag_t GROUP BY count(*)

statement error aggregate functions are not allowed in LIMIT
SELECT a FROM ag_t LIMIT count(*)

statement error aggregate functions are not allowed in VALUES
VALUES (count(*))

statement error aggregate functions are not allowed in FILTER
SELECT count(*) FILTER (WHERE count(*) > 1) FROM ag_t

statement error aggregate function calls cannot be nested
SELECT sum(count(*)) FROM ag_t

# 未対応の構文（yuzhu は 0A000）
onlyif yuzhu
statement error (0A000)
SELECT string_agg(c, ',') FROM ag_t

onlyif yuzhu
statement error (0A000)
SELECT count(*) OVER () FROM ag_t

onlyif yuzhu
statement error (0A000)
SELECT a FROM ag_t GROUP BY ROLLUP (a)

statement ok
DROP TABLE ag_t
```

</details>

### A-5. `tests/slt/m4/agg/group_resolution.slt`

<details><summary>agg/group_resolution.slt（152 行）</summary>

```
# GROUP BY は入力列が先、ORDER BY / DISTINCT ON は出力列の別名が先。位置番号・定数のエラー

statement ok
CREATE TABLE ag_t (a int, b int, c text)

statement ok
INSERT INTO ag_t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, NULL, 'z'), (3, 30, 'z')

# 位置番号と別名
query II
SELECT a, count(*) FROM ag_t GROUP BY 1 ORDER BY 1
----
1 1
2 1
3 2

query II
SELECT a + 0 AS zz, count(*) FROM ag_t GROUP BY zz ORDER BY zz
----
1 1
2 1
3 2

# 別名が入力列と同名のとき GROUP BY は入力列（b）に解決される: a がグループ化されていない
statement error column "ag_t.a" must appear in the GROUP BY clause or be used in an aggregate function
SELECT a AS b, count(*) FROM ag_t GROUP BY b

# 同じ名前でも別名と入力列が同じ列なら通る
query II
SELECT a AS a, count(*) FROM ag_t GROUP BY a ORDER BY a
----
1 1
2 1
3 2

# ORDER BY は出力列の別名が先（出力列 a は ag_t.b、入力列 a ではない）
query II
SELECT a AS b, b AS a FROM ag_t ORDER BY a NULLS LAST, b
----
1 10
2 20
3 30
3 NULL

# 式の中の別名は見ない
statement error column "s" does not exist
SELECT a + b AS s FROM ag_t ORDER BY s + 1

statement error column "k" does not exist
SELECT a + 1 AS k, count(*) FROM ag_t GROUP BY k + 1

# 同名の出力列が 2 つで式が違えば曖昧
statement error ORDER BY "x" is ambiguous
SELECT a AS x, b AS x FROM ag_t ORDER BY x

query II
SELECT a AS x, a AS x FROM ag_t WHERE a = 1 ORDER BY x
----
1 1

# FROM で曖昧な名前は GROUP BY でも曖昧
statement ok
CREATE TABLE ag_u (a int)

statement error column reference "a" is ambiguous
SELECT ag_t.a FROM ag_t, ag_u GROUP BY a

# 範囲外の位置と、整数以外の定数
statement error GROUP BY position 2 is not in select list
SELECT a FROM ag_t GROUP BY 2

statement error ORDER BY position 0 is not in select list
SELECT a FROM ag_t ORDER BY 0

statement error GROUP BY position -1 is not in select list
SELECT a FROM ag_t GROUP BY -1

statement error non-integer constant in ORDER BY
SELECT a FROM ag_t ORDER BY 1.5

statement error non-integer constant in ORDER BY
SELECT a FROM ag_t ORDER BY true

statement error non-integer constant in GROUP BY
SELECT a FROM ag_t GROUP BY 'a'

# GROUP BY の位置が集約
statement error aggregate functions are not allowed in GROUP BY
SELECT count(*) FROM ag_t GROUP BY 1

# HAVING は別名を見ない（c は入力列の text）
statement error operator does not exist: text > integer
SELECT count(*) AS c FROM ag_t HAVING c > 1

# ORDER BY の集約・グループ化した式
query I
SELECT a FROM ag_t GROUP BY a ORDER BY count(*) DESC, a
----
3
1
2

query I
SELECT a FROM ag_t GROUP BY a ORDER BY a + 1
----
1
2
3

statement error column "ag_t.a" must appear in the GROUP BY clause or be used in an aggregate function
SELECT a FROM ag_t ORDER BY count(*)

# DISTINCT / DISTINCT ON
query I
SELECT DISTINCT a + 1 FROM ag_t ORDER BY a + 1
----
2
3
4

statement error for SELECT DISTINCT, ORDER BY expressions must appear in select list
SELECT DISTINCT a FROM ag_t ORDER BY b

query IT
SELECT DISTINCT ON (a) a, c FROM ag_t ORDER BY a, b DESC NULLS LAST
----
1 x
2 y
3 z

query IIT
SELECT DISTINCT ON (a) a, b, c FROM ag_t ORDER BY a, b DESC NULLS LAST
----
1 10 x
2 20 y
3 30 z

statement error SELECT DISTINCT ON expressions must match initial ORDER BY expressions
SELECT DISTINCT ON (a) a, b FROM ag_t ORDER BY b

statement error SELECT DISTINCT ON expressions must match initial ORDER BY expressions
SELECT DISTINCT ON (a, b) a, b FROM ag_t ORDER BY a, c

query I
SELECT DISTINCT ON (a, a) a FROM ag_t ORDER BY a
----
1
2
3

statement ok
DROP TABLE ag_t, ag_u
```

</details>

### A-6. `tests/slt/m4/agg/functional_dep.slt`

<details><summary>agg/functional_dep.slt（113 行）</summary>

```
# GROUP BY の主キーへの関数従属と、グループ化の検査（42803）

statement ok
CREATE TABLE ag_p (id int PRIMARY KEY, v int)

statement ok
CREATE TABLE ag_pk2 (a int, b int, c text, PRIMARY KEY (a, b))

statement ok
CREATE TABLE ag_uq (id int UNIQUE NOT NULL, v int)

statement ok
CREATE TABLE ag_t (a int, b int, c text)

statement ok
INSERT INTO ag_p VALUES (1, 10), (2, 20), (3, NULL)

statement ok
INSERT INTO ag_pk2 VALUES (1, 1, 'x'), (1, 2, 'y'), (2, 1, 'z')

statement ok
INSERT INTO ag_uq VALUES (1, 10), (2, 20)

statement ok
INSERT INTO ag_t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, NULL, 'z')

# 主キーでグループ化すれば、同じ表の他の列を参照できる
query II
SELECT id, v FROM ag_p GROUP BY id ORDER BY id
----
1 10
2 20
3 NULL

# 別名つきでも、集約と一緒でも、HAVING・ORDER BY でも
query III
SELECT x.id, x.v, count(*) FROM ag_p x GROUP BY x.id HAVING x.v IS NOT NULL ORDER BY x.v DESC
----
2 20 1
1 10 1

# 結合の片方の主キー: 主キーのある表の列だけが従属する
query II
SELECT p.v, p.id FROM ag_p p JOIN ag_t t ON t.a = p.id GROUP BY p.id ORDER BY p.id
----
10 1
20 2
NULL 3

statement error column "t.c" must appear in the GROUP BY clause or be used in an aggregate function
SELECT p.v, t.c FROM ag_p p JOIN ag_t t ON t.a = p.id GROUP BY p.id

# 複合主キーは全列が要る
statement error column "ag_pk2.c" must appear in the GROUP BY clause or be used in an aggregate function
SELECT a, c FROM ag_pk2 GROUP BY a

query IIT
SELECT a, b, c FROM ag_pk2 GROUP BY a, b ORDER BY a, b
----
1 1 x
1 2 y
2 1 z

# UNIQUE NOT NULL は対象外（主キーだけ）
statement error column "ag_uq.v" must appear in the GROUP BY clause or be used in an aggregate function
SELECT id, v FROM ag_uq GROUP BY id

# 副問い合わせ・派生表の列は対象外
statement error column "s.v" must appear in the GROUP BY clause or be used in an aggregate function
SELECT v FROM (SELECT id, v FROM ag_p) s GROUP BY id

# 副問い合わせの中の参照も従属で許される
query II
SELECT id, (SELECT ag_p.v) FROM ag_p GROUP BY id ORDER BY id
----
1 10
2 20
3 NULL

# 主キーのない表
statement error column "ag_t.b" must appear in the GROUP BY clause or be used in an aggregate function
SELECT a, b FROM ag_t GROUP BY a

# GROUP BY のない集約問い合わせ
statement error column "ag_t.a" must appear in the GROUP BY clause or be used in an aggregate function
SELECT a, count(*) FROM ag_t

# HAVING だけでも集約問い合わせ（全体が 1 グループ）
query I
SELECT count(*) FROM ag_t HAVING count(*) > 1
----
3

statement error column "ag_t.a" must appear in the GROUP BY clause or be used in an aggregate function
SELECT 1 FROM ag_t HAVING a = 1

query I
SELECT 1 HAVING false
----

# 副問い合わせが未グループの外側の列を使う
statement error subquery uses ungrouped column "ag_t.b" from outer query
SELECT (SELECT count(*) FROM ag_p WHERE ag_p.v = ag_t.b), count(*) FROM ag_t GROUP BY ag_t.a

query II
SELECT (SELECT count(*) FROM ag_p WHERE ag_p.id = ag_t.a), count(*) FROM ag_t GROUP BY ag_t.a ORDER BY 1, 2
----
1 1
1 1
1 1

statement ok
DROP TABLE ag_p, ag_pk2, ag_uq, ag_t
```

</details>

### A-7. `tests/slt/m4/subquery/null_semantics.slt`

<details><summary>subquery/null_semantics.slt（139 行）</summary>

```
# IN / NOT IN / ANY / ALL / EXISTS の三値論理と空集合、列数と型のエラー

statement ok
CREATE TABLE sq_t (a int, b int, c text)

statement ok
CREATE TABLE sq_u (a int, e int, c text)

statement ok
INSERT INTO sq_t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, NULL, 'z'), (NULL, 40, 'n')

statement ok
INSERT INTO sq_u VALUES (1, 100, 'x'), (3, 300, 'q'), (NULL, 400, 'w')

# 一致があれば true、なくて集合に NULL があれば NULL、なければ false
query I
SELECT a FROM sq_t WHERE a IN (SELECT a FROM sq_u) ORDER BY a
----
1
3

query I
SELECT a FROM sq_t WHERE a NOT IN (SELECT a FROM sq_u) ORDER BY a
----

# NOT IN の集合に NULL がなければ（WHERE で NULL を除く）
query I
SELECT a FROM sq_t WHERE a NOT IN (SELECT a FROM sq_u WHERE a IS NOT NULL) ORDER BY a
----
2

# 式の値として（NULL が見える）
query I? rowsort
SELECT a, a IN (SELECT a FROM sq_u) FROM sq_t
----
1 t
2 NULL
3 t
NULL NULL

# 空集合: IN は false、NOT IN は true、ANY は false、ALL は true。左辺が NULL でも
query ????
SELECT NULL IN (SELECT 1 WHERE false), NULL NOT IN (SELECT 1 WHERE false),
       NULL = ANY (SELECT 1 WHERE false), NULL = ALL (SELECT 1 WHERE false)
----
f t f t

# 左辺が NULL で集合が空でなければ NULL
query ??
SELECT NULL IN (SELECT 1), NULL NOT IN (SELECT 1)
----
NULL NULL

# ALL / ANY の比較
query I
SELECT a FROM sq_t WHERE a < ALL (SELECT a FROM sq_u WHERE a IS NOT NULL) ORDER BY a
----

query I
SELECT a FROM sq_t WHERE a <= ANY (SELECT a FROM sq_u WHERE a IS NOT NULL) ORDER BY a
----
1
2
3

query I
SELECT a FROM sq_t WHERE a <> ALL (SELECT a FROM sq_u WHERE a IS NOT NULL) ORDER BY a
----
2

# EXISTS は NULL を返さない。相関と非相関
query I
SELECT a FROM sq_t WHERE EXISTS (SELECT 1 FROM sq_u WHERE sq_u.a = sq_t.a) ORDER BY a
----
1
3

query I
SELECT a FROM sq_t WHERE NOT EXISTS (SELECT 1 FROM sq_u WHERE sq_u.a = sq_t.a) ORDER BY a
----
2
NULL

query ?
SELECT EXISTS (SELECT 1 WHERE false)
----
f

# 列のない EXISTS の対象
query I
SELECT count(*) FROM sq_t WHERE EXISTS (SELECT)
----
4

# スカラー副問い合わせ: 0 行は NULL、2 行以上は 21000
query II
SELECT a, (SELECT e FROM sq_u WHERE sq_u.a = sq_t.a) FROM sq_t ORDER BY a
----
1 100
2 NULL
3 300
NULL NULL

statement error (21000)
SELECT (SELECT a FROM sq_u)

query ?
SELECT (SELECT a FROM sq_u WHERE false) IS NULL
----
t

# 行値の IN
query I
SELECT a FROM sq_t WHERE (a, c) IN (SELECT a, c FROM sq_u) ORDER BY a
----
1

# エラー: 列数
statement error subquery must return only one column
SELECT (SELECT a, e FROM sq_u)

statement error subquery has too many columns
SELECT a FROM sq_t WHERE a IN (SELECT a, e FROM sq_u)

statement error subquery has too few columns
SELECT a FROM sq_t WHERE (a, c) IN (SELECT a FROM sq_u)

# エラー: 型（副問い合わせの出力の unknown は text に解決される）
statement error operator does not exist: integer = text
SELECT a FROM sq_t WHERE a = ANY (SELECT c FROM sq_u)

statement error operator does not exist: integer = text
SELECT 1 IN (SELECT NULL)

statement error invalid input syntax for type integer: "a"
SELECT 'a' IN (SELECT 1)

statement ok
DROP TABLE sq_t, sq_u
```

</details>

### A-8. `tests/slt/m4/subquery/correlated.slt`

<details><summary>subquery/correlated.slt（133 行）</summary>

```
# 相関副問い合わせ: 2 段以上の入れ子、派生表を通り抜ける参照、HAVING / ORDER BY / LIMIT / SET の中、禁止される位置

statement ok
CREATE TABLE sq_t (a int, b int)

statement ok
CREATE TABLE sq_u (a int, e int)

statement ok
CREATE TABLE sq_p (id int PRIMARY KEY, v int)

statement ok
INSERT INTO sq_t VALUES (1, 10), (2, 20), (3, NULL)

statement ok
INSERT INTO sq_u VALUES (1, 100), (1, 101), (3, 300)

statement ok
INSERT INTO sq_p VALUES (1, 1), (3, 3)

# 外側の列を 2 段内側から参照する
query II
SELECT a, (SELECT (SELECT sq_t.a + sq_u.e) FROM sq_u WHERE sq_u.a = sq_t.a ORDER BY e LIMIT 1) FROM sq_t ORDER BY a
----
1 101
2 NULL
3 303

# 副問い合わせ式の中の派生表が、さらに外側の列を参照する（FROM の兄弟は参照できない）
query II
SELECT a, (SELECT x FROM (SELECT sq_t.a AS x) q) FROM sq_t ORDER BY a
----
1 1
2 2
3 3

statement error invalid reference to FROM-clause entry for table "sq_t"
SELECT * FROM sq_t, (SELECT sq_t.a AS x) q

# 相関 EXISTS / IN の入れ子
query I
SELECT a FROM sq_t WHERE a IN (SELECT a FROM sq_u WHERE a IN (SELECT id FROM sq_p WHERE sq_p.id = sq_t.a AND sq_p.v = sq_u.a)) ORDER BY a
----
1
3

# SELECT 句・ORDER BY・HAVING の中の相関
query II
SELECT a, (SELECT count(*) FROM sq_u WHERE sq_u.a = sq_t.a) FROM sq_t ORDER BY 2 DESC, 1
----
1 2
3 1
2 0

query I
SELECT a FROM sq_t ORDER BY (SELECT max(e) FROM sq_u WHERE sq_u.a = sq_t.a) DESC NULLS LAST, a
----
3
1
2

query I
SELECT a FROM sq_t GROUP BY a HAVING EXISTS (SELECT 1 FROM sq_u WHERE sq_u.a = sq_t.a) ORDER BY a
----
1
3

# GROUP BY した列は副問い合わせの中で使える。しない列は 42803
query II
SELECT a, (SELECT max(e) FROM sq_u WHERE sq_u.a = sq_t.a) FROM sq_t GROUP BY a ORDER BY a
----
1 101
2 NULL
3 300

statement error subquery uses ungrouped column "sq_t.b" from outer query
SELECT (SELECT max(e) FROM sq_u WHERE sq_u.e > sq_t.b) FROM sq_t GROUP BY a

# UPDATE の SET の右辺と DELETE の WHERE
statement count 3
UPDATE sq_t SET b = (SELECT coalesce(sum(e), 0) FROM sq_u WHERE sq_u.a = sq_t.a)

query II
SELECT a, b FROM sq_t ORDER BY a
----
1 201
2 0
3 300

statement count 1
DELETE FROM sq_t WHERE NOT EXISTS (SELECT 1 FROM sq_u WHERE sq_u.a = sq_t.a) AND b = 0

# LIMIT / OFFSET: 外側の列は書ける、現スコープの列は書けない
query I
SELECT a FROM sq_t WHERE a IN (SELECT a FROM sq_u ORDER BY e LIMIT sq_t.a) ORDER BY a
----
1
3

statement error argument of LIMIT must not contain variables
SELECT a FROM sq_t LIMIT a

statement error argument of LIMIT must not contain variables
SELECT a FROM sq_t LIMIT (SELECT sq_u.a FROM sq_u WHERE sq_u.a = sq_t.a)

query I
SELECT a FROM sq_t ORDER BY a LIMIT (SELECT 1)
----
1

# CHECK / DEFAULT に副問い合わせは書けない
statement error cannot use subquery in check constraint
CREATE TABLE sq_bad1 (a int CHECK (a IN (SELECT 1)))

statement error cannot use subquery in DEFAULT expression
CREATE TABLE sq_bad2 (a int DEFAULT (SELECT 1))

statement error aggregate functions are not allowed in check constraints
CREATE TABLE sq_bad3 (a int CHECK (count(*) > 1))

# 外側の問い合わせに属する集約は yuzhu では 0A000（PG は外側の集約として評価する）
skipif yuzhu
query I
SELECT (SELECT max(sq_t.a)) FROM sq_t
----
3

onlyif yuzhu
statement error (0A000)
SELECT (SELECT max(sq_t.a)) FROM sq_t

statement ok
DROP TABLE sq_t, sq_u, sq_p
```

</details>

### A-9. `tests/slt/m4/setop/union_types.slt`

<details><summary>setop/union_types.slt（135 行）</summary>

```
# 集合演算: 型の決定（左から 2 つずつ）、unknown の扱い、列名、ORDER BY / LIMIT、優先順位、エラー

statement ok
CREATE TABLE so_t (a int, b text)

statement ok
CREATE TABLE so_u (a bigint, e text)

statement ok
INSERT INTO so_t VALUES (1, 'x'), (2, 'y'), (2, 'y'), (NULL, 'n')

statement ok
INSERT INTO so_u VALUES (2, 'y'), (3, 'q'), (NULL, 'n')

# UNION は重複を除く（NULL どうしは等しい）。UNION ALL は除かない
query IT
SELECT a, b FROM so_t UNION SELECT a, e FROM so_u ORDER BY a, b
----
1 x
2 y
3 q
NULL n

query I
SELECT count(*) FROM (SELECT a FROM so_t UNION ALL SELECT a FROM so_u) s
----
7

# 共通型: int と bigint は bigint、int と numeric は numeric
query T
SELECT pg_typeof(a) FROM (SELECT a FROM so_t UNION SELECT a FROM so_u) s LIMIT 1
----
bigint

query T
SELECT pg_typeof(x) FROM (SELECT 1 AS x UNION SELECT 2.5) s LIMIT 1
----
numeric

# unknown 同士は text。unknown と int は int（リテラルの入力関数で評価）
query T
SELECT pg_typeof(x) FROM (SELECT 'a' AS x UNION SELECT 'b') s LIMIT 1
----
text

query I
SELECT x FROM (SELECT 1 AS x UNION SELECT '2') s ORDER BY x
----
1
2

statement error invalid input syntax for type integer: "a"
SELECT 1 UNION SELECT 'a'

# 左から 2 つずつ: (NULL UNION NULL) は text になるので、後の 1 とは合わない
statement error UNION types text and integer cannot be matched
SELECT NULL UNION SELECT NULL UNION SELECT 1

# 列名は最も左の腕
query I
SELECT a AS first FROM so_t WHERE a = 1 UNION SELECT a FROM so_u WHERE a = 3 ORDER BY first
----
1
3

# 集合演算全体の ORDER BY は出力列の名前か位置だけ
query I
SELECT a FROM so_t UNION SELECT a FROM so_u ORDER BY 1 DESC NULLS LAST LIMIT 2
----
3
2

statement error missing FROM-clause entry for table "so_t"
SELECT a FROM so_t UNION SELECT a FROM so_u ORDER BY so_t.a

statement error invalid UNION/INTERSECT/EXCEPT ORDER BY clause
SELECT a FROM so_t UNION SELECT a FROM so_u ORDER BY a + 1

statement error ORDER BY position 2 is not in select list
SELECT a FROM so_t UNION SELECT a FROM so_u ORDER BY 2

# 優先順位: INTERSECT が UNION / EXCEPT より強い。同じ強さは左結合
query I
SELECT a FROM so_t WHERE a = 1 UNION SELECT a FROM so_u INTERSECT SELECT 2 ORDER BY a
----
1
2

query I
SELECT a FROM so_t EXCEPT SELECT a FROM so_u EXCEPT SELECT 1 ORDER BY a
----

# 括弧つきの腕は ORDER BY / LIMIT を持てる
query I
(SELECT a FROM so_t WHERE a IS NOT NULL ORDER BY a LIMIT 1) UNION ALL (SELECT a FROM so_u WHERE a IS NOT NULL ORDER BY a DESC LIMIT 1) ORDER BY 1
----
1
3

# ALL: INTERSECT ALL は min(nl, nr)、EXCEPT ALL は max(nl - nr, 0)
query I
SELECT a FROM so_t INTERSECT ALL SELECT a FROM so_u ORDER BY a
----
2
NULL

query I
SELECT a FROM so_t EXCEPT ALL SELECT a FROM so_u ORDER BY a
----
1
2

# VALUES の腕
query IT
VALUES (1, 'a') UNION SELECT a, b FROM so_t WHERE a = 1 ORDER BY 1, 2
----
1 a
1 x

# 列数が違う
statement error each UNION query must have the same number of columns
SELECT a, b FROM so_t UNION SELECT a FROM so_u

statement error each INTERSECT query must have the same number of columns
SELECT a FROM so_t INTERSECT SELECT a, e FROM so_u

statement error UNION types integer and text cannot be matched
SELECT a FROM so_t UNION SELECT e FROM so_u

# 括弧なしの腕の ORDER BY は構文エラー
statement error (42601)
SELECT a FROM so_t ORDER BY a UNION SELECT a FROM so_u

statement ok
DROP TABLE so_t, so_u
```

</details>

### A-10. `tests/slt/m4/cte/scope.slt`

<details><summary>cte/scope.slt（132 行）</summary>

```
# WITH: スコープ、前の CTE の参照、重複名、列別名、実表との同名、副問い合わせの中の WITH

statement ok
CREATE TABLE ct_t (a int, b int)

statement ok
INSERT INTO ct_t VALUES (1, 10), (2, 20), (3, NULL)

# 前の CTE を後の CTE が参照できる。同じ CTE を 2 回参照できる
query II
WITH x AS (SELECT a FROM ct_t), y AS (SELECT a + 1 AS a FROM x) SELECT x.a, y.a FROM x JOIN y ON y.a = x.a + 1 ORDER BY 1
----
1 2
2 3
3 4

query II
WITH x AS (SELECT a FROM ct_t) SELECT p.a, q.a FROM x AS p, x AS q WHERE p.a = q.a - 1 ORDER BY 1
----
1 2
2 3

# 列別名は先頭から改名（残りは元の名前）。重複した別名も許す
query II
WITH x(p) AS (SELECT a, b FROM ct_t) SELECT p, b FROM x ORDER BY p
----
1 10
2 20
3 NULL

query II
WITH x(p, p) AS (SELECT a, b FROM ct_t) SELECT * FROM x ORDER BY 1
----
1 10
2 20
3 NULL

# 実表と同名の CTE は実表を隠す。スキーマ付きなら実表
query I
WITH ct_t AS (SELECT 5 AS a) SELECT a FROM ct_t
----
5

query I
WITH ct_t AS (SELECT 5 AS a) SELECT count(*) FROM public.ct_t
----
3

# 内側の WITH が同名なら本体では内側が優先。内側の宣言の中の同名参照は外側
query I
WITH x AS (SELECT 1 AS a) SELECT * FROM (WITH x AS (SELECT a + 10 AS a FROM x) SELECT * FROM x) s
----
11

query I
WITH x AS (SELECT 1 AS a) SELECT * FROM x WHERE EXISTS (WITH x AS (SELECT 2 AS a) SELECT 1 FROM x WHERE x.a = 2)
----
1

# 副問い合わせの中の WITH が外側の列を参照する
query I
WITH x AS (SELECT 1 AS v) SELECT (WITH y AS (SELECT v) SELECT * FROM y) FROM x
----
1

# MATERIALIZED / NOT MATERIALIZED で結果は変わらない
query I
WITH x AS MATERIALIZED (SELECT a FROM ct_t) SELECT count(*) FROM x p, x q WHERE p.a = q.a
----
3

query I
WITH x AS NOT MATERIALIZED (SELECT a FROM ct_t) SELECT count(*) FROM x p, x q WHERE p.a = q.a
----
3

# WITH は集合演算全体にかかる。WITH ... VALUES / TABLE
query I
WITH x AS (SELECT 1 AS v) SELECT v FROM x UNION ALL SELECT v + 1 FROM x ORDER BY 1
----
1
2

query I
WITH x AS (SELECT 7 AS v) TABLE x
----
7

# CTE の出力の unknown は text
query T
WITH x AS (SELECT 'a' AS s) SELECT pg_typeof(s) FROM x
----
text

# INSERT ... SELECT の中の WITH
statement count 3
INSERT INTO ct_t WITH x AS (SELECT a + 100 AS a, b FROM ct_t) SELECT * FROM x

query II
SELECT a, b FROM ct_t WHERE a > 100 ORDER BY a
----
101 10
102 20
103 NULL

# 使われない CTE も解析される（型エラーは出る）
statement error operator does not exist: boolean \+ integer
WITH x AS (SELECT true + 1) SELECT 1

# 重複名・前方参照・自己参照・存在しない列別名の数
statement error WITH query name "x" specified more than once
WITH x AS (SELECT 1), x AS (SELECT 2) SELECT * FROM x

statement error There is a WITH item named|relation "y" does not exist
WITH x AS (SELECT * FROM y), y AS (SELECT 1) SELECT * FROM x

statement error relation "x" does not exist
WITH x AS (SELECT * FROM x) SELECT 1

statement error WITH query "x" has 2 columns available but 4 columns specified
WITH x(p, q, r, s) AS (SELECT a, b FROM ct_t) SELECT * FROM x

# 同じ CTE を別名なしで 2 回
statement error table name "x" specified more than once
WITH x AS (SELECT 1) SELECT * FROM x, x

# スキーマ付きの名前では CTE を引かない
statement error relation "public.x" does not exist
WITH x AS (SELECT 1) SELECT * FROM public.x

statement ok
DROP TABLE ct_t
```

</details>

### A-11. `tests/slt/m4/dml/update_from.slt`

<details><summary>dml/update_from.slt（129 行）</summary>

```
# UPDATE ... FROM / DELETE ... USING: 結合、SET の右辺、複数一致、別名、解析の順序、エラー

statement ok
CREATE TABLE dm_r (id int PRIMARY KEY, v int, w text)

statement ok
CREATE TABLE dm_t (a int, c text)

statement ok
CREATE TABLE dm_u (a int, e int)

statement ok
INSERT INTO dm_r VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 3, 'c'), (4, 4, 'd')

statement ok
INSERT INTO dm_t VALUES (1, 'x'), (2, 'y'), (3, 'z'), (3, 'zz'), (NULL, 'n')

statement ok
INSERT INTO dm_u VALUES (1, 100), (3, 300), (4, 400)

# FROM の列を SET の右辺に使う。一致しない行は変わらない
statement count 3
UPDATE dm_r SET v = dm_u.e FROM dm_u WHERE dm_u.a = dm_r.id

query IIT
SELECT id, v, w FROM dm_r ORDER BY id
----
1 100 a
2 2 b
3 300 c
4 400 d

# 複数の FROM 項目（結合）。ON に FROM の項目だけが見える
statement count 2
UPDATE dm_r SET v = 0 FROM dm_t JOIN dm_u ON dm_u.a = dm_t.a WHERE dm_u.a = dm_r.id AND dm_t.c IN ('x', 'z')

# 対象の行が複数の FROM 行に一致しても 1 回だけ更新する（どの値になるかは不定なので件数だけ）
statement count 1
UPDATE dm_r SET w = dm_t.c FROM dm_t WHERE dm_t.a = dm_r.id AND dm_r.id = 3

# 自己結合（別名）。FROM の中で古い値を読む
statement count 4
UPDATE dm_r SET v = n.id FROM dm_r AS n WHERE n.id = dm_r.id

query II
SELECT id, v FROM dm_r ORDER BY id
----
1 1
2 2
3 3
4 4

# 派生表・VALUES・generate_series を FROM に
statement count 2
UPDATE dm_r SET v = s.x * 10 FROM (SELECT id AS i, id AS x FROM dm_r WHERE id <= 2) s WHERE s.i = dm_r.id

statement count 2
UPDATE dm_r SET v = vv.x FROM (VALUES (3, 33), (4, 44)) AS vv(i, x) WHERE vv.i = dm_r.id

statement count 1
UPDATE dm_r SET v = g FROM generate_series(1, 2) AS g WHERE g = dm_r.id AND g = 1

query II
SELECT id, v FROM dm_r ORDER BY id
----
1 1
2 20
3 33
4 44

# SET の右辺に副問い合わせと集約（FROM と併用しない）
statement count 4
UPDATE dm_r SET v = (SELECT max(e) FROM dm_u WHERE dm_u.a = dm_r.id)

query II
SELECT id, v FROM dm_r ORDER BY id
----
1 100
2 NULL
3 300
4 400

# DELETE ... USING
statement count 2
DELETE FROM dm_r USING dm_t WHERE dm_t.a = dm_r.id AND dm_t.c IN ('x', 'y')

statement count 1
DELETE FROM dm_r USING dm_t JOIN dm_u ON dm_u.a = dm_t.a WHERE dm_t.a = dm_r.id

query I
SELECT count(*) FROM dm_r
----
1

# 解析のエラー
# 同じ表を別名なしで 2 回
statement error table name "dm_r" specified more than once
UPDATE dm_r SET v = 1 FROM dm_r

# 対象表に別名があれば元の名前では参照できない
statement error invalid reference to FROM-clause entry for table "dm_r"
UPDATE dm_r x SET v = 1 WHERE dm_r.id = 1

# FROM の中から対象表は見えない
statement error invalid reference to FROM-clause entry for table "dm_r"
UPDATE dm_r SET v = 1 FROM (SELECT dm_r.id) s

statement error invalid reference to FROM-clause entry for table "dm_r"
UPDATE dm_r SET v = 1 FROM dm_t JOIN dm_u ON dm_u.a = dm_r.id

# 修飾なしの列が曖昧
statement error column reference "a" is ambiguous
UPDATE dm_r SET v = 3 FROM dm_t, dm_u WHERE a = dm_r.id

# 解析の順序は FROM → WHERE → SET（WHERE の列のエラーが先）
statement error column "yy" does not exist
UPDATE dm_r SET v = zz WHERE yy = 1

statement error aggregate functions are not allowed in UPDATE
UPDATE dm_r SET v = count(*)

statement error aggregate functions are not allowed in WHERE
DELETE FROM dm_r WHERE count(*) > 1

statement error relation "nosuch" does not exist
DELETE FROM dm_r USING nosuch

statement ok
DROP TABLE dm_r, dm_t, dm_u
```

</details>

