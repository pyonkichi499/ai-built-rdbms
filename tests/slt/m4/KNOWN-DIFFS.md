# 既知の差分（PostgreSQL 17 と yuzhu M4 の違い）

`# KNOWN-DIFF: KD-<n> 理由` を `skipif yuzhu` / `onlyif yuzhu` の直前の行に書く（`slttools lint` の L06 が検査する）。
yuzhu だけで意味を持つ検査は `# YUZHU-ONLY: 理由`。PG 側 `skipif yuzhu`、yuzhu 側 `onlyif yuzhu` を対で書く。
`skipif yuzhu` は「M5 以降に作る」か「PostgreSQL にしかない機能」だけに使う。ID の追加は K4 に依頼するか、末尾に追記だけする。
正本は `spec/design/m4/11-tests-plan.md` §3.2.7。使われなくなった差は消す（lint が使用箇所のない ID を警告する）。

| ID | 内容（yuzhu の振る舞い） | 出典 |
|---|---|---|
| KD-1 | `interval` / `time` / `timetz` は `0A000`。`timestamp - timestamp`、`ts ± '1 day'` は `42883`（PostgreSQL では動く） | 09-Q2 |
| KD-2 | `FULL JOIN ... ON true`（ハッシュ可能な等値のない FULL）は `0A000`。PostgreSQL は通す | 03-Q1、04-D10 |
| KD-3 | `LATERAL`（明示・暗黙）は `0A000` | 03-Q5 |
| KD-4 | `WITH RECURSIVE` の再帰、データ変更 CTE、`WITH ... INSERT/UPDATE/DELETE` は `0A000` | 03-Q6、03-Q7 |
| KD-5 | `IS [NOT] DISTINCT FROM` は `0A000`（M4 後半の任意項目で入れるかもしれない） | 03-Q12 |
| KD-6 | `FOR UPDATE` / `FOR SHARE` は `0A000` | 03 §8 |
| KD-7 | ウィンドウ関数、`GROUPING SETS` / `ROLLUP` / `CUBE`、集約内の `ORDER BY`、`string_agg` ほか未対応の集約は `0A000` | 00 §3、03-Q16 |
| KD-8 | 外側の問い合わせに属する集約（`(select max(t.a))`）は `0A000` | 03-Q4、02-Q4 |
| KD-9 | `ANY` / `ALL` の配列形、`ARRAY(SELECT ...)`、行値のその他の使い方、全行参照（`count(t)`）は `0A000` | 03-Q8、03-Q9、03-Q15 |
| KD-10 | `JOIN ... USING (...) AS j`、括弧つき JOIN の別名は `0A000` | 03-Q13 |
| KD-11 | `ALTER SEQUENCE` / `TRUNCATE ... RESTART IDENTITY` の状態はロールバックされない。`log_cnt` は ALTER のたびに 0 | 08-Q5、08-Q14 |
| KD-12 | `pg_typeof(1/0)` は `integer`（PostgreSQL は評価してエラー）。`timestamp(7)` の `WARNING` なし。DEFAULT の `'now'::timestamp` が使うたびの時刻 | 09-Q6、09-Q10、09-Q7 |
| KD-13 | 正規表現の後方参照・先読みは `0A000` | 09-Q5 |
| KD-14 | `pg_class.relpages` / `reltuples`、`relhasindex` の更新時機、`pg_type.typmodin` が 0 | 07-Q12、07-Q13、09 §3.2 |
| KD-15 | 圧縮されうる大きなキー（繰り返しの多い 2.7KB 以上）は `54000` | 06-Q19 |
| KD-16 | 索引の列に使えない型（`xid` `cid` `tid` `"char"` `int2vector`）は `42704`。`name` の索引列の `atttypid` が `name` | 07 D07-19、07-Q15 |
| KD-17 | `TEMP` / `UNLOGGED` シーケンス、`ALTER SEQUENCE RENAME` / `SET SCHEMA`、`ALTER TABLE` の `ADD PRIMARY KEY` / `UNIQUE` / `OWNER TO` 以外は `0A000` | 08-Q6、00 D-12 |
| KD-18 | `fillfactor = 50.5`（小数）は `22023`。`autovacuum_enabled` などその他の reloptions は `22023` | 07-Q8 |
| KD-19 | `COPY` の CSV・バイナリ・`TO`・`WHERE`・`ON_ERROR`、`EXPLAIN` の `FORMAT JSON` / `XML` / `YAML` は `0A000` | 10-Q4、10-Q7 |
| KD-20 | `\d tbl` は M4 では動かない（実機の SQL 10 本のうち 3 本（行レベルセキュリティ、拡張統計、出版物）が配列・`regnamespace`・関数のために解析できず、さらに `pg_collation` と空のカタログ表を作る担当がない。レビュー対応 R-18・R-19。動かすには約 7 日の任意 WP） | 10-Q6 |
| KD-21 | `GROUP BY (SELECT ...)` の式を SELECT に書き直すと `42803`（PostgreSQL は構造の等しい副問い合わせを一致とみなす） | 03 §8、02-Q10 |
| KD-22 | 同じ `CREATE TABLE` が作るシーケンスの名前を DEFAULT に書くと `42P01`。同じ文の 2 つの暗黙のシーケンスの名前の衝突を避ける（上位互換） | 08-Q10、08-Q13 |
| KD-23 | 外側の列を参照する CTE のうち、`MATERIALIZED` の明示と揮発性のものは `0A000`（それ以外はインラインで通る。R-05） | 05-Q5、04-Q11、02-Q5 |
| KD-24 | `generate_series(numeric, ...)` / 日時版 / `generate_series(1, 10.5)` は `42883`。FROM 句の `generate_series` 以外の関数は `0A000` | 09、03-Q11 |
| KD-25 | `EXPLAIN` のプランの選び方（インデックスを小さな表でも使う、`Hash Right Join` の向きなど）と SubPlan の番号（InitPlan の位置はその階層の根で PostgreSQL と同じ。R-06） | 04 §2.2、04-Q7 |
| KD-26 | `RETURNING` に FROM / USING の列を使うと `0A000`（`RETURNING` 自体が M4 後半・任意） | 05-Q4、03-Q14 |
| KD-27 | SELECT 句の集合返却関数は `0A000` | 00 §3 |
| KD-28 | `transaction_timeout` は受け付けるだけで強制しない | 10-Q8 |
| KD-29 | `CREATE SCHEMA` / `CREATE DATABASE` は M5。compat テストは `postgres` データベースの `public` だけで動く | 00 §3（§7.1 の C-12） |
| KD-30 | PostgreSQL で成功する索引・制約のオプションのうち、yuzhu では `0A000` のもの: DEFERRABLE / INITIALLY DEFERRED、NULLS NOT DISTINCT、INCLUDE、EXCLUDE 制約、`CREATE INDEX` の `USING hash`・式・`WHERE`（部分索引）・COLLATE・TABLESPACE | 06、07 |
