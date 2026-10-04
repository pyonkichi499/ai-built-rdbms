# difftest 敵対的レビュー: 引継ぎメモ（作業途中）

## 状態
- **ビルドは通らない見込み**: `src/db.rs` だけ書き換え済みで、呼び出し側（`case.rs` / `main.rs` / `db.rs` のテスト）が未更新。
- 未コミット。PG17 コンテナ `difftest-adv-r3k9`（port 55493、DB `r` と `t`）が**残っている**。後で `docker rm -f difftest-adv-r3k9` で消すこと。

## PG17 で確かめた問題（直す対象）
1. **-0 / 0 のどちらが出るかが決まらない**: `DISTINCT`、`GROUP BY`、`min/max`、`ORDER BY .. LIMIT`、スカラーサブクエリ（`ORDER BY 1 LIMIT 1`）では、どちらが残るかが行の格納順や実行計画で変わる。
   例: `-0, 0` の順に入れると `SELECT DISTINCT c` は `-0` を返す。`WHERE c::text='0'` を付けると `0` になる。
   → 正しい実装でも差分として報告される（誤検出）。DISTINCT 付きの TLP では、参照側でも恒等式が破れたと誤って判定される（TlpRef）。
2. **エラーが出るかどうかが評価順で変わる**: PG はトップレベルの AND をコスト順に並べ替え、`x AND FALSE` を畳み込む。結合順やサブクエリの実行も計画しだい。
   再現: `c=0` の行に対して `WHERE (10/c=1 OR ...) AND c IS NULL` は PG ではエラーにならないが、左から順に評価する実装ではエラーになる。
   実験: DB `t` に `enable_hashjoin/mergejoin/hashagg=off`、DB `r` に `enable_sort/nestloop=off` を設定すると、m4 の 2 万クエリで「エラー 22003 と行の結果」が食い違う誤検出が 4 件出た。並列実行を強制した設定でも 6 件出た。m1 では 0 件。
3. **文のタイムアウトがない**: yuzhu がハングすると difftest も止まる。
4. （警告だけ出す予定）参照側が C ロケールでないと、`'a'<'B'` の結果や `upper('é')` が変わる。

## 実装済み（src/db.rs）
- `Worker` スレッドと `recv_timeout` による文のタイムアウト。タイムアウトしたら `Outcome::Lost` とし、次の文の前に再接続する。`Server::connect(label, conninfo, timeout)` に引数を追加し、`connect_timeout` も設定するようにした。
- `Traits { ordered, zero_ambiguous, eval_sensitive }` を追加し、`compare(r, t, Traits, opts)` に変更した。`zero_ambiguous` のときは `-0` を `0` にそろえて比べる。
- `EvalOrder { Skip, Report }` を `CmpOpts.eval_order` に追加し、`eval_order_inconclusive()` を新設した（クラス 22 のエラー対行の結果、またはクラス 22 どうしでコードが違う場合）。
- `tlp_violation` は DISTINCT のとき `-0` を正規化してから重複を除くようにした。

## 残作業
- `ast.rs`: `Select::zero_ambiguous()` を追加する（distinct / grouped / limit / offset / スカラーサブクエリ / 集約のいずれか）。`Select::eval_order_sensitive()` も追加する（FROM が複数、サブクエリ、AND/OR/BETWEEN/IN リストのいずれか）。走査用に `Expr::any_node` を用意する。
- `case.rs`: `Pair` に `inconclusive: u64` を追加する。`check` は `ordered` の代わりに `Traits` を受け取り、`eval_order_inconclusive` が真なら Ok を返してカウンタを増やす。セットアップ文は `Traits::default()` で呼ぶ。
- `main.rs`: `ConnOpts` に `--timeout SECS`（既定 10）と `--eval-order-errors skip|report`（既定 skip）を追加する。実行結果の要約に inconclusive の件数を出す。replay では SQL の文字列から Traits を推定する（DISTINCT / LIMIT / GROUP BY / min( / max( / AND / OR / JOIN / (SELECT）。参照側の datcollate/datctype が C/POSIX 以外なら警告する。
- テストを追加する: compare の -0 正規化、eval_order_inconclusive、tlp_violation で DISTINCT と ±0 が混じる場合。
- README に追記する: 比較の規則、新しいオプション、タイムアウト。
- 確認: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`、および上記の計画差分 DB で m1/m4 を走らせ、誤検出が 0 件になること。
