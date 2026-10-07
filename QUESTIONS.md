# 確認事項（ユーザーの不在中にまとめたもの）

ユーザーの不在中に作業を続けるため、判断が必要な点は推奨案で仮決めして先に進めています。各項目について、仮決めのままでよいか、変更したいかを確認してください。変更する場合は手戻りが発生します（許容済み）。

## 作業の進め方

- **Q-001 コミットについて**: 「いちいち許可取らなくていい」との指示を受け、`dev` ブランチを作ってマイルストーンの区切りごとにローカルでコミットします（push はしません）。
- **Q-002 基本設計書の置き場所**: 要件定義書は Claude Docs に置きましたが、基本設計書は実装エージェントが直接読めるよう、リポジトリ内の `spec/design/` に Markdown で置きました。Docs にも出したい場合は教えてください。

## M1 の設計で仮決めしたこと（`spec/design/m1.md`）

- **Q-003 M1 の ROLLBACK**: M1 にはまだ MVCC がないため、メモリ上のテーブルに undo ログを持たせて ROLLBACK と「文の途中で失敗したときの巻き戻し」を実現します。他のセッションからはコミット前の行も見えます（ダーティリード）。これは M3 で MVCC を入れて解消します。
- **Q-004 server_version の値**: psql の `\d` 系コマンドが報告された版数でカタログの参照先を変えるため、`server_version` は `16.0` と名乗ります。
- **Q-005 既定のデータベース名**: ドライバやテストランナーの既定値に合わせ、M1 の唯一のデータベース名は `postgres` にしました（要件の「yuzhu」という名前は使っていません）。
- **Q-006 DEFAULT・CHECK 式の保存形式**: カタログには SQL テキストのまま保存し、使うたびにパースし直します。PostgreSQL は内部の木を直列化して保存しますが、M1 の単純さを優先しました。
- **Q-007 PRIMARY KEY / UNIQUE**: M1 では構文だけ受け付け、実行すると「未対応（0A000）」エラーを返します（黙って無視はしません）。M4 で対応します。
- **Q-008 M1 の型**: 要件の 5 型（int、bigint、text、bool、double）に、smallint、real、varchar(n) を加えました。varchar(n) は、typmod の処理経路を M1 のうちに通しておくためです。
- **Q-009 既定のポート**: PostgreSQL と同じ 5432 にしました。テスト用の本物の PostgreSQL は 55432 で動かします。

## 先のマイルストーンの調査で出た、範囲に関わる提案（`spec/research/m3-*.md`、`m4-*.md`、`m5-*.md`、`pg-compat-tools.md`）

各マイルストーンの設計書を作るときに、ここに挙げた推奨案で仮決めします。詳細な確認事項は各調査レポートの末尾にあります。

- **Q-010 トランザクション ID の幅**: 64 ビットにして、PostgreSQL のような周回対策（凍結処理）をなくします。外から見える `xmin` 列などは、PostgreSQL と同じく下位 32 ビットを返します。
- **Q-011 numeric の前倒し**: `1.5` のような小数リテラルや `avg(int)` の結果は、PostgreSQL では numeric 型になります。numeric がないと集約（M4）の互換性が保てないため、numeric の基本部分を M5 から M4 の前半に前倒しします。
- **Q-012 COPY・char(n)・timestamp の前倒し**: 性能目標の測定に使う pgbench の初期化に、COPY FROM STDIN、char(n)、timestamp が必要です。これらを M4 に前倒しします。
- **Q-013 M4 での内部構造の大きな変更**: JOIN とサブクエリに対応するため、M4 の冒頭で列参照の表現と、論理プラン・物理プランの分け方を作り直します（M1〜M3 のコードに手が入ります）。
- **Q-014 行ロックの方式（M5）**: PostgreSQL の MultiXact（複数のトランザクションが同じ行を共有ロックする仕組み）は、ディスクに永続化しないメモリ上だけの簡易版にします。これにより、外部キーのチェックで取る行ロックが、非キー列の UPDATE と衝突する場合があります（PostgreSQL 9.2 以前と同じ挙動）。

## M2 の設計で仮決めしたこと（`spec/design/m2.md` 末尾の「確認事項」に全 21 件）

特に重要なものを挙げます。ディスク形式に関わるもの（★）は、M2 の実装後に変えるとやり直しが大きくなります。

- **M2-Q1 ROLLBACK の方式**: M1 の undo ログをやめ、M2 から xmin/xmax とコミットログで実現します。M1 のダーティリードは M2 で解消します。
- **★M2-Q2 タプルヘッダ**: XID を 64 ビットにするため、タプルヘッダは PostgreSQL の 23 バイトより大きい 35 バイトになります。1 ページに入る最大の行数は 291 から 185 に減ります。
- **M2-Q3 単一ライター**: 書き込むトランザクションは同時に 1 つだけです（複数ライターは M5）。
- **M2-Q4 クラッシュ時の保証**: M2 は正常停止の後だけを保証します（WAL は M3）。
- **★M2-Q5 ページチェックサム**: 常に有効にします（PostgreSQL の既定は無効）。
- **M2-Q6 依存の追加**: SIGTERM で正常停止するために、`signal-hook` クレートを追加します。
- **M2-Q7 server_version**: カタログを PostgreSQL 17 にそろえるので、名乗る版を 17.0 に上げます（Q-004 の改訂）。
- **M2-Q8 psql の対応範囲**: M2 では `\l` まで動かします。`\dt` は M4 です。`!~`（正規表現）を手書きするか `regex` クレートを使うかは、M4 で決めます。
- **M2-Q11 TOAST なし**: 1 行が約 8KB を超えるとエラーになります。
- **M2-Q20 データディレクトリの互換性**: M5 までは、マイルストーンをまたいだデータディレクトリの互換を保証しません（形式が変われば initdb のやり直し）。

## M2 の完了時に追加した確認事項

- **M2-Q22 slt の後始末**: `m2/catalog/pg_attribute`・`constraint_attrdef`・`m2/ddl/drop_cleanup` が末尾でテーブルを消さず、同じ DB への再実行が失敗する。テスト側に DROP を足すのが推奨（M3 のテスト整備時に対応）。
- **M2-Q23 CHECK 制約の重複エラー文言**: PG は `check constraint "c" already exists`、yuzhu は `constraint "c" for relation "v" already exists`。PG にそろえるのが推奨（位置の有無は PG で要確認）。
- **M2-Q24 `finish_pending_unlinks` の残骸**: 失敗して再起動後に残った 0 バイトのファイルは、OID 採番時の `storage_exists` 確認で避ける（m2.md の D13）。回収は M5 の VACUUM か起動時掃除で検討。

## M3 の仮決め（`spec/design/m3.md` 第 10 節の M3-Q1〜Q22 は設計書を参照。ここには実装・レビューで追加で決めたものを記す）

- **M3-Q23 PG 17 の実測を m3.md より優先する**: (1) FROM のないものを含め、最初の文でスナップショットを取るため、その後に現在と異なる分離レベル・READ ONLY→READ WRITE・DEFERRABLE の変更をすると 25001（同じレベルへの変更は成功）。未対応の REPEATABLE READ / SERIALIZABLE も、スナップショット後なら 0A000 より先に 25001 を返す。(2) 失敗状態からの `ROLLBACK AND CHAIN` は READ ONLY を引き継がない。(3) 暗黙のトランザクションブロック（1 つの Simple Query に複数文）では `SET LOCAL` が成功し、Query の終了で元に戻る。m3.md の該当注記は未更新。
- **M3-Q24 READ ONLY の 25006 は解析後**: INSERT/UPDATE/DELETE は解析・plan のあとで 25006 を出す（42P01 などが先）。読み取り専用では書き込みロックを取らない。CREATE/DROP TABLE は先頭で 25006。定数畳み込みがないため `SET a = 1/0` の 22012 は PG と順序が違う。
- **M3-Q25 RESET と transaction_* パラメータ**: `transaction_isolation` / `transaction_read_only` / `transaction_deferrable` への RESET と `SET ... TO DEFAULT` は 0A000（`cannot be reset`）。
- **M3-Q26 追加した M3 パラメータの扱い**: `deadlock_timeout` は最小 1ms。`synchronous_commit` は true/yes/1 を on に、false/no/0 を off に正規化する。`full_page_writes`・`wal_sync_method`・`max_wal_size` は 55P02（再起動が要る設定）、`wal_segment_size` は変更不可。
- **M3-Q27 排他バリアの待ちとファイルの削除**: DROP のコミットや CREATE を含む ROLLBACK の unlink は、排他バリアが取れなければキューに積み、文の終了後に再試行する（他セッションの文を待たない）。ポーリングで取るので、読み手が途切れない間は遅れる。クラッシュでキューが失われると孤立ファイルが残る（M3-Q5 と同じ扱い）。待ちの間は cancel / statement_timeout が効かない。
- **M3-Q28 1 レコードあたりのリレーション数の上限**: `MAX_RELS_PER_RECORD` を CREATE/DROP TABLE で検査し、超えると 54000。
- **M3-Q29 チェックポイントスレッドの panic**: `Cluster::tick` の `run_checkpoint` を `catch_unwind` で包み、panic したら警告を出してクラスタを poison する（panic する経路は未確認の防御）。
- **M3-Q30 `idle_in_transaction_session_timeout` はメッセージ途中も対象**: 待ち開始時刻からの期限をメッセージ本体の読み取りにも適用する（25P03 / 57P05 の FATAL）。
- **M3-Q31 RR 用の分離性 spec は yuzhu で失敗する**: `tests/isolation/specs` の `lost-update` と `write-skew-rr` は REPEATABLE READ を使うので、M5 まで yuzhu では失敗する（PG では通る）。
- **M3-Q32 slt の調整**: `tests/slt/m3/txn/isolation_level_after_select.slt` の後半は、yuzhu が REPEATABLE READ を受け付けないので `READ UNCOMMITTED` に変えた（PG でも通る）。

## M4 の設計で仮決めしたこと（`spec/design/m4/`）

出典は `spec/design/m4/99-questions.md`（99-questions）。以下は §1〜§5 の転記（ID は `M4-Q1`〜`M4-Q158` のまま）。

- (a) M4 の設計は `spec/design/m4/`（README.md、00〜11 章、98・99）。M4 は Q-010〜Q-014 を含む。Q-010（64 ビット XID）は M2 で実現済みで M4 の作業なし、Q-011（numeric）は D-3、Q-012（COPY・char(n)・timestamp）は D-4〜D-6 と D-25、Q-013（作り直し）は D-1・D-2、Q-014（MultiXact の簡易版）は M5。
- (b) 工数は約 142 日（00 の見積りの 115.5 日より増加。最長経路は約 21〜23 日）。
- (c) 00 を変える提案が 102 件あり、11 §7.4 と §9 の表のとおり採否を決めた。
- (d) 章の間の食い違い 30 件を 11 §7.1 で決めた（うち 10 件はレビュー対応）。
- (e) 完了判定は `tests/done-check.sh` のローカル実行。

**ディスク形式に関わる ★（実装の前に決めるのが望ましい）**: M4-Q52・Q53・Q54・Q55・Q57・Q60（B+Tree、06）、M4-Q72・Q73（カタログ、07）、M4-Q90・Q91・Q92（シーケンス、08）、M4-Q104（numeric、09）の 12 件。

### 1. ユーザーに最初に見てほしいもの（要約）

重要度の高い順（実装の方針・工数・ディスク形式に効くもの）。

| 順 | ID | 論点 | 仮決め | 変えた場合の主な影響 |
|---|---|---|---|---|
| 1 | M4-Q142 | 完了の判定 | `tests/done-check.sh` のローカル実行（夜間の連続 7 回は求めない） | 夜間連続合格を条件にすると完了が push 後のホスト作業になる |
| 2 | M4-Q52〜Q55・Q57・Q60 ★ | B+Tree の形（ピボットの境界の向き、構造変更の 1 レコード、ブロック数 `3h + 1`、切り詰めなし） | 06 のとおり（M5 の suffix truncation と矛盾しない） | 実装の前なら半日、後なら initdb のやり直し |
| 3 | M4-Q72・Q73 ★ | カタログの追加と `int2[]` / `int2vector` のディスク形式 | 9 カタログ、`Datum::Int2Vector`（PG の配列ヘッダなし） | M5 で PG の形式にすると読み直しと initdb |
| 4 | M4-Q90〜Q92 ★ | シーケンスのページ・WAL・SERIAL の DEFAULT の保存形式 | PG と同じ構造、`nextval('<oid>'::regclass)` | 形を変えると別の実行ノードが要る（+1 日） |
| 5 | M4-Q104 ★ | numeric のディスク形式 | 固定ヘッダ（PG の short / long ヘッダは採らない） | PG 形式にすると約 +0.5 日 |
| 6 | M4-Q105 | `interval` を M5 に回す | `timestamp - timestamp` は `42883` | 入れるなら約 +3〜4 日 |
| 7 | M4-Q119（M4-Q145） | `\d tbl` は M4 では動かない | 任意の約 7 日の WP | 動かすなら配列の最小実装（約 5 日）ほか |
| 8 | M4-Q62 | 項目を消さないインデックス | pgbench は完走するが TPS が低い | 簡易削除で +2〜3 日 |
| 9 | M4-Q127 | 差分ランダムテストは既存の `tests/tools/difftest` を使う | `yuzhu-fuzz-sql` は作らない | ワークスペースに作ると +0.5 日 |
| 10 | M4-Q128 | 工数は 141.8 日（00 の見積りの 115.5 日より増加）、期間は約 21〜23 日 | 並列度約 19 | 担当を絞ると期間が延びる |
| 11 | M4-Q121 | `transaction_timeout` を受け付けて保存だけにする | M3 の `42704` を上書き | `42704` のままだと pg_dump 17 が接続できない |
| 12 | M4-Q143 | 後半・任意の項目（RETURNING ほか）を完了条件に入れない | M5 の最初に RETURNING | 必須にすると +2〜3 日 |

---

### 2. 各章の確認事項（通し番号）

**02 パイプラインの作り直し（M4-Q1〜Q10）**

- **M4-Q1 [02-Q1] 外部結合の NULL 側の `ColId` を再発行しない**: 仮決め: `Join` の NULL 側の列は同じ `ColId`。結合より上の `Column(c)` は null 拡張後の値を指す。理由: 再発行すると結合の上のすべての式の `ColId` を書き換える。PG の `varnullingrels` 相当は結合の位置から判定できる。影響: `Join.null_cols` を足す方式は build・押し下げ・外部結合の簡約・刈り込み・`validate` が増え約 +1.5 日。
- **M4-Q2 [02-Q2] 移行を下から行う（executor → analyzer → planner）**: 仮決め: 各段階の隣に一時アダプタ（`planner/legacy.rs`）。理由: アダプタが自明な構造変換で済む。影響: 上からは約 +1 日（旧型向けの `physicalize` が使い捨て）。
- **M4-Q3 [02-Q3] `uses_params()` は `SubLink` を含む部分木で常に true**: 仮決め: 00 の定義に足す。理由: `PhysExpr` の `SubLink` は `SubPlanId` だけで `Param` 参照が見えない。影響: `uses_params(&self, q)` に署名を変えると +0.5 日。**→ C-6**（executor の溜めた結果の再利用は 05 の `free_params` で決める。`uses_params()` は単体テスト用の保守的な判定として残る）。
- **M4-Q4 [02-Q4] 外側レベルの集約は `0A000`**: 仮決め: `(SELECT sum(t.a) FROM u) FROM t` の形は `0A000`。理由: PG は集約を外側の問い合わせに所属させる。必要性が低く複雑。影響: 集約の所属レベルを決めて外側の `has_agg` を立てる処理を 03・04 に足す約 +3 日（KD-8）。
- **M4-Q5 [02-Q5] 外側の列を参照する共有 CTE は `0A000`（`MATERIALIZED` の明示と揮発性に限る）**: 仮決め: `MATERIALIZED` の明示、または揮発性の相関 CTE は `0A000`。`materialize = Default` で参照が複数・非揮発の相関 CTE と、非 `MATERIALIZED` で参照 1 回のものはインライン展開で通る（04-D4。レビュー対応 R-05 で 04 と 02・05 を一致させた）。理由: 1 回だけ実行して共有する設計と `Param` による再実行は両立しない。共有に意味がない場合は結果が同じなので通す。影響: `CteStates` を `Param` の変化で作り直す約 +1.5 日（KD-23）。
- **M4-Q6 [02-Q6] 導出表の列名は内側の名前のまま、`Subquery Scan` ノードを持たない**: 仮決め: EXPLAIN の式が `s.x` でなく内側の名前で出る。理由: PG もプルアップされた導出表は内側の名前。影響: 論理・物理に `SubqueryScan` を足す約 +1.5 日（04・10 に波及）。
- **M4-Q7 [02-Q7] デバッグビルドで `validate` を常時実行し、隠し設定 `yuzhu.validate_plans`**: 仮決め: リリースは既定 off。`PlannerSettings.validate_plans`。理由: 全テストで不変条件が検査される。影響: 環境変数にする・常時 on にする（性能の計測が要る）。CI の `slt-yuzhu` は on で起動する。
- **M4-Q8 [02-Q8] Join RTE を指す `Var` を Bound に残さず、アナライザが展開する**: 仮決め: USING / NATURAL の併合列と `j.*` は式に展開。理由: `build` と `validate` が単純。影響: `build` が展開する方式は 03・04 に波及し約 +1 日。03-Q2 と同じ決定。
- **M4-Q9 [02-Q9] `levels_up` の数え方**: 02 の当初の仮決め: 1 つの `BoundQuery`（本体が Values / SetOp でも）が 1 レベル。**→ C-3: 03 の D3-20（rtable を持つスコープだけを数える。`Values` の行を 1 スコープとして数える）を採る**。02 の §3.4.2・D1・`validate` の B1 は直し済み。理由: 生成側の 03 と消費側の 04 を一致させる（レビュー対応 R-04: 04 §4.1・§5.6 が Values でスコープを積まず食い違っていたのを直した）。影響: 02 の方式に揃えると 03（N3）の書き直し約 +0.5 日と 04 の `scopes` の積み方の変更。
- **M4-Q10 [02-Q10] `SubLink` を含む式の `same_as` が常に不一致**: 仮決め: `GROUP BY (SELECT ...)` を SELECT に書き直すと `42803`（PG は通す。KD-21）。理由: 副問い合わせの木の等価判定が M4 の必要性に見合わない。影響: `BoundQuery` の構造的な等価判定を 03 に足す約 +1 日。

**03 パーサとアナライザ（M4-Q11〜Q27）**

- **M4-Q11 [03-Q1] FULL JOIN の `0A000` はプランナが出す**: 仮決め: アナライザは判定せず、プランナが「ハッシュ可能な等値がない FULL」を `0A000`（PG と同じ文言）。`ON true` の FULL は PG が通すが yuzhu は通さない（KD-2。**→ C-19**）。理由: PG もプランナが出す。影響: アナライザで判定するなら 04 と重複する分類を足す約 +0.5 日。
- **M4-Q12 [03-Q2] 結合の別名 `Var` は解析時に展開する**: 仮決め: Bound に Join RTE を指す `Var` は現れない。理由: GROUP BY の検査・出力列の由来・04 が単純。影響: 04 が展開する約 +1 日 + 約 +0.5 日。
- **M4-Q13 [03-Q3] 関数従属は主キーだけ。許された `Var` を `group_by` の末尾に足す**: 仮決め: `pg_depend` には記録しない（UNIQUE NOT NULL は対象外。実測）。理由: 実機の挙動。影響: 足さないなら 05 の `Aggregate` に代表行の保持（約 1 日）。
- **M4-Q14 [03-Q4] 外側のスコープに属する集約は `0A000`**: M4-Q4 と同じ決定（KD-8）。影響: 約 +2 日。
- **M4-Q15 [03-Q5] LATERAL は `0A000`。診断は PG と同じ**: 仮決め: 明示はパーサ、関数引数の暗黙の LATERAL はアナライザ。理由: LATERAL は M5〜M6。pgbench のパーティション確認は失敗してよい。影響: 参照を許して 04 が相関パラメータ付き NestedLoop で実行する 3〜4 日（KD-3）。
- **M4-Q16 [03-Q6] `WITH ... INSERT/UPDATE/DELETE` とデータ変更 CTE は `0A000`**: 仮決め: パーサが拒否。`INSERT ... SELECT` の中の WITH は動く。理由: `BoundUpdate` / `BoundDelete` に `ctes` の欄がない。影響: `ctes` を足し 04・05 に各 1 日（KD-4）。
- **M4-Q17 [03-Q7] `WITH RECURSIVE` の構文は受理し、再帰参照だけ `0A000`**: 理由: ORM が `RECURSIVE` を付けて非再帰の CTE を送る。影響: 全面的に `0A000` にするなら 0.1 日。
- **M4-Q18 [03-Q8] 行値は IN / ANY / ALL サブクエリの左辺だけ（M4 後半・任意）**: 理由: 00 §6.2 が `test` の形を決めている。影響: 受理しないなら 0.1 日 + 0.3 日が減る。
- **M4-Q19 [03-Q9] `ANY` / `ALL` の配列形は `0A000`**: 理由: 配列は M5。psql の `\d tbl` の 2 本が失敗（KD-9・KD-20）。影響: 最小の配列と `ANY(array)` の `Expr` 変種が要る 3 日（00 の変更）。
- **M4-Q20 [03-Q10] `COLLATE` は `"C"` / `"POSIX"` / `"default"` だけ受理して無視**: 理由: 照合順序は C だけ。psql が `COLLATE pg_catalog.default` を使う。`42P21` は検出しない。影響: 式に collation の欄が要る（M5 以降）。
- **M4-Q21 [03-Q11] FROM 句の関数は `FnKind::Set`（`generate_series`）だけ**: 理由: `FunctionScan` は集合返却だけ。影響: スカラー関数を 1 行の関数スキャンにする約 0.7 日（KD-24）。
- **M4-Q22 [03-Q12] `IS DISTINCT FROM` は `0A000` のまま**: 理由: 00 の `ExprKind` に変種がない。影響: `ExprKind::IsDistinctFrom` を足す（式の走査・評価・deparse で各 0.2 日。**M4 後半の任意項目として推奨**。KD-5）。
- **M4-Q23 [03-Q13] JOIN の別名（`USING ... AS j`、`(t JOIN u ON ..) AS j`）は `0A000`**: 理由: ORM がまれにしか生成しない。影響: 約 0.5 日（余力があれば入れてよい。KD-10）。
- **M4-Q24 [03-Q14] RETURNING は解析を実装し、`RETURNING_ENABLED` で解禁する**: 仮決め: 解禁は X3 と 04 の対応後。対象表の列だけ（KD-26）。理由: 00 D-26（M4 後半・任意）。影響: FROM の列を許すなら `BoundReturning` を作り直し 04 が入力に載せる 2 日。
- **M4-Q25 [03-Q15] 全行参照（`count(t)`、`select t from t`）は `0A000`**: 理由: 複合型が要る（M5 以降）。
- **M4-Q26 [03-Q16] 未対応の集約名は `0A000 aggregate function X is not supported yet`**: 仮決め: `string_agg` `array_agg` `stddev` ほか PG の組み込みで `AGGREGATES` にないもの（KD-7）。理由: `42883` と区別でき、M5 での追加が分かりやすい。
- **M4-Q27 [03-Q17] エラーの DETAIL / HINT は主要なものだけ**: 仮決め: 近い名前の HINT は出さない。理由: 編集距離の実装が要る。影響: 約 1 日。

**04 プランナとルールベース最適化（M4-Q28〜Q41）**

- **M4-Q28 [04-Q1] `ColId` をパススルーで引き継ぐ**: 仮決め: `Get` → `Project` の単純な参照 → `Aggregate` の group key は同じ `ColId`。計算列だけ新しい ID。理由: 押し下げ・派生表の展開・Semi 化・相関参照が付け替えなしでできる。影響: 「ノードごとに新しい ID」にすると R3 が置換表を持ち回り、L1・L2 の大半に影響。
- **M4-Q29 [04-Q2] 相関のある `IN` を `EXISTS` の形で結合化する**: 理由: PG 17 も Semi 結合にする（実測）。M4 は LATERAL がないので `EXISTS` の形に直せるものだけ。影響: 変換しないなら分岐を消すだけ（結果は同じ）。
- **M4-Q30 [04-Q3] 左結合の ON の中の `EXISTS` を引き上げない**: 影響: `try_pull` に右の子を足す約 +0.3 日。
- **M4-Q31 [04-Q4] 内側 Index Scan の Nested Loop の採否の定数**（`ROWS_PER_BLOCK_EST = 100`、`probe_cost`）: 理由: 統計がないので `nblocks` と述語の個数だけ。影響: 定数だけ（結果は変わらない）。
- **M4-Q32 [04-Q5] ビルド側は `estimate` の小さいほう、同点は右**: 影響: 定数・規則だけ。
- **M4-Q33 [04-Q6] ソートの省略は条件つき（M4 後半・任意）**: 仮決め: 述語で選んだ索引の順序で足りるとき、直上が `Limit` のとき、`enable_sort = off` のときだけ。理由: 全索引走査はヒープへのランダムアクセスが増える。影響: 04 §7.4 の (b) の条件。
- **M4-Q34 [04-Q7] InitPlan / SubPlan の EXPLAIN の位置と番号**: 仮決め（レビュー対応 R-06 で 10 に合わせた）: InitPlan は PostgreSQL と同じくその問い合わせ階層の根の `ExplainNode` の子、SubPlan は式を表示したノードの子。番号は物理化した順（PG は内側が先で CTE と番号を共有するので SubPlan の番号は一致しない。KD-25）。影響: `assemble` の置き場所と採番だけ（`onlyif yuzhu` の期待値）。
- **M4-Q35 [04-Q8] `ExplainNode` に `plan_id`（と `width`）を足し、`PhysicalPlan` と同形にしない**: 仮決め: `Hash` の合成・`Filter` の併合・`Project` の透過。**→ C-1: 名前は `exec_id`（10 に従う）**。影響: 同形にすると EXPLAIN の見た目が PG から少し離れる（`onlyif yuzhu` の期待値だけ）。
- **M4-Q36 [04-Q9] リテラルのキャストを（Stable でも）計画時に畳む**: 理由: DateStyle・TimeZone・カタログは文の間は固定。影響: M5 の Extended Query でプランをキャッシュするなら設定が変わったときの再計画が要る。
- **M4-Q37 [04-Q10] プランをキャッシュしない**: M4 は文ごとに計画。M5 で再計画の仕組みを足す。
- **M4-Q38 [04-Q11] 外側の列を参照する CTE: `MATERIALIZED` の明示と揮発性は `0A000`、それ以外の共有はインライン**（M4-Q5 と同じ決定。R-05）: 理由: M4 の executor は `CteScan` の再実行を持たない。共有が意味を持つ場合だけ拒否する。影響: 共有を許すなら `CteScan` の作り直しと `uses_params` への CTE の算入。
- **M4-Q39 [04-Q12] 計画の再帰の深さの上限 `MAX_PLAN_DEPTH = 500`（`54001`）**: 影響: 定数だけ。スタックサイズ（J）と合わせる。
- **M4-Q40 [04-Q13] 従属列は group key に足す**: 理由: グループ内で一定なので意味が変わらず、追加の集約が要らない（03-Q3 と同じ）。
- **M4-Q41 [04-Q14] `EXISTS` を `ANY` に直してハッシュ化しない**: 仮決め: 引き上げられない相関 `EXISTS` は `Rescan`。PG は等値の相関ならハッシュ化 SubPlan。影響: `plan_sublink` に変換を足す約 +0.5 日。

**05 実行ノードと DML（M4-Q42〜Q51）**

- **M4-Q42 [05-Q1] メモリの課金は文の終わりまで保持し、`rewindable = false` のノードだけ枯渇で返す**: 理由: 二重計上を避ける複雑な管理を作らず、安全側に数える。影響: 厳密な追跡は `Executor` に `release` を足し全ノード変更（+1 日）。
- **M4-Q43 [05-Q2] 溜めた結果の再利用判定に `free_params` を使う（00 の `uses_params()` は使わない）**: 理由: `uses_params()` は `SubLink` の `SubPlanDef.params` を見られず、再利用を誤る。**→ C-6**。影響: `uses_params()` を使うなら `SubLink` を含むノードを保守的に「依存」にする（正しさは保たれるが遅い）。
- **M4-Q44 [05-Q3] 23505 の DETAIL を `dml.rs` が補う**: 仮決め: B+Tree は `23505` と `s` / `t` / `n` だけ（06-Q8 と同じ決定）。理由: `IndexStore::insert` に `TypeEnv` がない。影響: B+Tree が作るなら `IndexStore::insert` に `&TypeEnv` を足す（00 §13.2 の変更）。
- **M4-Q45 [05-Q4] RETURNING は対象表の列だけ**: 仮決め: FROM / USING の列を参照する RETURNING は `0A000`（KD-26）。影響: RETURNING の式を入力の `Project` に出す形に変える +1 日（02・04・X3）。
- **M4-Q46 [05-Q5] 相関のある共有 CTE（`MATERIALIZED` の明示・揮発性）を拒否する**: 仮決め: planner が `0A000`（04-D4）。executor は内部エラー（KD-23）。影響: `CteSlot` に世代番号 +1 日。
- **M4-Q47 [05-Q6] `NestedLoopJoin` の outer は left、inner は right**: 理由: 「出力は左 ++ 右」を NLJ にも適用。影響: INNER の入れ替え用に `outer_is_right` を足す。
- **M4-Q48 [05-Q7] `HashSetOp` に UNION も実装する**: planner は使わない。影響: 削除してよい。
- **M4-Q49 [05-Q8] `HashAggregate` の出力はグループの初出順**: 理由: テストが安定する（PG は不定なので PG と比べる slt は `ORDER BY`）。影響: 順序を変えると yuzhu だけの期待値が変わる。
- **M4-Q50 [05-Q9] 相関のある副問い合わせの結果を、パラメータが前回と同じなら再利用する最適化はしない**: 理由: volatile 関数があると誤る。影響: planner が immutable / stable だけを `SubPlanDef` に持たせれば 1 エントリのキャッシュを足せる +0.5 日。
- **M4-Q51 [05-Q10] 計測の対応づけのための `ExplainNode.phys_id`**: **→ C-1・C-2: 採用しない（10 の `exec_id` と `set_counters` に統一）**。05 の仮決め: 合成ノードを持つ場合だけ必要。影響: 05 §9 の `BuildOptions.instrument` / `PlanScope` / `NodeKey` / `extra_stats` を 10 の方式に置き換える（X1〜X3 の各ノードが `set_counters` を実装。約 +0.3 日）。

**06 B+Tree（M4-Q52〜Q71）**

- **M4-Q52 [06-Q1] ★ ピボットの境界の向き（左の全項目 < 区切り <= 右の全項目）**: 仮決め: 区切り = 右ページの最初の項目そのもの。PG は逆。理由: 切り詰めない M4 では「TID の最下位を 1 引く」特例が要らず、検査器が単純。M5 の suffix truncation とも矛盾しない。影響: 降下・moveright・検査器を反転。ディスク形式（ピボットの TID の意味）が変わるので実装の前なら半日、後なら initdb のやり直し。
- **M4-Q53 [06-Q2] ★ 空のインデックスでもルート葉を作る（メタの `root = 1`）**: 仮決め: 00 D-7 のとおり（`init_index` がメタ + 空の葉の 2 ページを `BTREE_PAGES`（`INIT`）で書く）。理由: 挿入の途中でルートを作る分岐と REDO が要らない。影響: 遅延作成にすると `BTREE_PAGES` の理由が 1 つ増え、07 の `EMPTY_INDEX_STATS` が変わる（+0.5 日）。
- **M4-Q54 [06-Q3] ★ 構造変更は全画像の 1 レコード `BTREE_PAGES`。ブロック数は `3h + 1`、静的な高さの上限は作らない**: 仮決め: 必要なブロック数が 32 を超えたときだけ `54000`。00 §13.4 の `2h + 3` と調査の `3h + 2` は不正確（**→ C-13**）。理由: 未完了の分割という状態がなくクラッシュ試験の分岐が増えない。影響: PG 方式（`INCOMPLETE_SPLIT`）は +M。
- **M4-Q55 [06-Q4] ★ ピボットの形（切り詰めなし、葉タプル + 8 バイト、TID は末尾の 6 バイト、high key の `block = 0`、-∞ ピボットは 8 バイト）**: 理由: 大きさが `S + 8` と定まり、`BT_MAX_ITEM_SIZE` で「high key + データ 2 件」が保証できる。影響: 切り詰めを入れるのは +3 日（ページ形式は変わらない）。
- **M4-Q56 [06-Q5] 分割点の規則（バイト数が均等。右端の葉への末尾への追記だけ左を 90% まで詰める）**: 理由: 単純で決定的。影響: `choose_split` だけ（ディスク形式は変わらない）。
- **M4-Q57 [06-Q6] ★ 一括構築の詰め方（葉を全部作ってから上のレベル、葉 90%・内部 70%、32 枚ずつ `BTREE_PAGES`）**: 理由: 確保が常に末尾への追加で、レコードが連続したブロックになる。影響: nbtsort のように全レベル並行は +1 日（ブロック番号が飛ぶ）。
- **M4-Q58 [06-Q7] バッファプールへの依頼（`read_tree` / `write_tree` / `extend_tree`）**: 仮決め: M3 の debug 検査（ブロック番号の昇順）を B+Tree には適用しない口を足す（0.3 日）。影響: 昇順に合わせるには分割で全ページを昇順にラッチし直す +1 日。
- **M4-Q59 [06-Q8] 23505 の DETAIL は B+Tree が付けない**: M4-Q44 と同じ決定。影響: B+Tree が作るなら `UniqueCheck::Check` と `BuildUnique::Yes` に `&TypeEnv` を足す。
- **M4-Q60 [06-Q9] ★ `datetime_ops` に型をまたぐ行を入れない**: 仮決め: `date` `timestamp` `timestamptz` の同じ型どうしだけ（`pg_amop` が PG より 30 行、`pg_amproc` が 6 行少ない）。理由: 型をまたぐ比較は `TimeZone` に依存し `CmpFn`（純粋）に環境を渡せない。09 がその演算子を作らない。影響: 作るなら 06 に最大 30 行と `CmpFn` の署名変更（+1.5 日。09-Q4 と同時）。
- **M4-Q61 [06-Q10] `fetch_dirty` / `tuple_state` は「自分以外の実行中のトランザクション」を持たない**: 理由: 単一ライターなので clog が実行中の他者の XID は中断の残骸。影響: M5 で実行中の判定を足す（B+Tree の呼び出し側は変わらない）。
- **M4-Q62 [06-Q11] 項目を消さないことの影響**: 仮決め: 同じキーの UPDATE を繰り返すと死んだ版の項目が増え続け、一意性検査と等値スキャンが全部をヒープで確かめる。理由: 削除は WAL・スキャンとの連動・ヒントビットが要り M5 の VACUUM と一緒。影響: pgbench の完走はするが TPS が低い。緩和は簡易削除（+2〜3 日）。
- **M4-Q63 [06-Q12] `BuildStats.levels` はルートの `level`（葉だけの木は 0）**: 理由: 07 の `EMPTY_INDEX_STATS` に合わせる。影響: 表示（`pg_class`）だけ。
- **M4-Q64 [06-Q13] `pg_opclass` の OID（`bool_ops` 10003 など 10000 番台）**: 仮決め: PG 17.11 の実機の値を写す。テストは名前で結合して比べる。影響: なし（`builtin_hash` が変わるので initdb のやり直し）。
- **M4-Q65 [06-Q14] ページの破損の SQLSTATE は `XX001`**: 理由: `error.rs` に `XX002` がなく REINDEX もない。影響: `INDEX_CORRUPTED` を足す 0.1 日。
- **M4-Q66 [06-Q15] CREATE INDEX は全件をメモリに持つ**: 理由: 外部ソートは M6。影響: 上限を設けるなら 07 の `build_from_heap` が `53200` を返す。
- **M4-Q67 [06-Q16] 分割の原子性を壊す変異用の `DebugKnobs`（任意）**: 理由: 「1 レコードで書く」ことをクラッシュ試験で守る。影響: 作らないなら変異テストの 1 行を省く（B1 +0.3 日）。
- **M4-Q68 [06-Q17] 一意性検査: 最初の葉の保持と、等値の連続の全件確認**: 理由: M5 の形に合わせつつ M4 は単純に。影響: M5 で右の葉を取ってから最初の葉を外す・`WaitFor` で待つ・簡易削除。
- **M4-Q69 [06-Q18] 比較は項目を `Datum` に復号してから行う**: 理由: 型ごとの比較規則を `cmp_datum` に一本化（00 §4.3 の 2）。影響: 性能が問題なら整数・text だけバイト列比較を足す。
- **M4-Q70 [06-Q19] PostgreSQL が圧縮して入る値が `54000` になる**: 理由: M2-Q11（TOAST なし）の帰結。差として許容し、共有テストは圧縮されない値だけ（KD-15）。影響: 圧縮は TOAST と一緒に M5 以降。
- **M4-Q71 [06-Q20] `default_opclass` にバイナリ互換の解決を含める**: 仮決め: `varchar` → `text_ops`、`regclass` / `regtype` / `regproc` → `oid_ops`。理由: 型と opclass の対応を 1 か所に。影響: 07 の `resolve_opclass` が補うだけ。

**07 カタログと DDL（M4-Q72〜Q89）**

- **M4-Q72 [07-Q1] ★ カタログの追加と行の値**: 仮決め: 9 カタログ（`pg_index` `pg_depend` `pg_sequence` `pg_language` `pg_opfamily` `pg_opclass` `pg_amop` `pg_amproc` `pg_description`（空））。`reltype` は 0。`pg_depend` は §3.6 の表だけ。理由: 00 D-23。M4 に `DROP SCHEMA` と外部キーがない。影響: 名前空間と CHECK の列の依存を足すと書き込みが増える（後から足せる。`CATALOG_VERSION_NO` を上げて initdb）。
- **M4-Q73 [07-Q2] ★ `int2[]` / `int2vector` のディスク形式**: 仮決め: `Datum::Int2Vector`（varlena + `i16` LE。PG の配列ヘッダなし）。理由: M4 は 1 次元の `int2` 配列だけ。影響: M5 で PG の形式にすると `conkey` / `indkey` の読み直しと initdb。
- **M4-Q74 [07-Q3] 1 コマンドで同じカタログ行を 2 度更新しない**: 理由: `cid` が文の終わりでしか進まず、同じコマンドで挿入した行は見えない。影響: `command_counter_increment` を DDL の途中で呼べるようにする +0.5 日（M2 / M3 のコマンド ID の不変条件に触る）。
- **M4-Q75 [07-Q4] 一意索引の構築の重複検出は C1 が行う（`BuildUnique::No`）**: 理由: 死んだ版を索引に入れるので、隣接比較だけの検出は UPDATE した行を誤検出する。影響: `build` に `live: bool` を足す（06 の変更。工数は同じ）。
- **M4-Q76 [07-Q5] `OWNER TO` を実装する**: 仮決め: 00 D-12 の「何もしない」を変える（ロールを検証し `relowner` を更新）。理由: `\dt` の Owner が誤る。実装は小さい（+0.25 日）。影響: 何もしない（ロールの存在検査だけ）に戻せる。
- **M4-Q77 [07-Q6] `ANALYZE` はトランザクションブロックの中でも成功する**: 仮決め: 00 D-11 の「両方 `25001`」を変える（実測は `VACUUM` だけ `25001`）。影響: なし（PG と同じにしてあるだけ）。
- **M4-Q78 [07-Q7] DROP の依存: `pg_depend` の閉包と二重の網**: 影響: 直接のキーだけにすると `DROP TABLE ... CASCADE` の連鎖と M5 の外部キーを後で作り直す。
- **M4-Q79 [07-Q8] `WITH (...)` の reloptions は `fillfactor`（表・索引）と `deduplicate_items`（索引）だけ**: 仮決め: ほかは `22023`。`fillfactor=50.5` は `22023`（KD-18）。影響: `autovacuum_enabled` などを受け付けて捨てる名前の一覧 +0.1 日（M5 の pg_dump のリストアで必要になる可能性が高い）。
- **M4-Q80 [07-Q9] `TRUNCATE ... RESTART IDENTITY` のシーケンスの更新はトランザクショナルでない**: 理由: 08 のその場の上書き（M4-Q94 と同じ決定。KD-11）。影響: `SequenceStore::reset` を新しい relfilenode 方式にする（08 +0.5 日）。
- **M4-Q81 [07-Q10] `ALTER TABLE ... DROP CONSTRAINT` を M4 に入れない**: 仮決め: PRIMARY KEY の索引は `DROP TABLE` でしか消せない。影響: `DROP CONSTRAINT [IF EXISTS] name` は +0.5 日（CHECK の削除は +0.25 日）。
- **M4-Q82 [07-Q11] `CREATE INDEX CONCURRENTLY` / `DROP INDEX CONCURRENTLY` を通常と同じに動かす**: 理由: ORM のマイグレーションが付けても動く。影響: `0A000` にすると ORM が失敗する。
- **M4-Q83 [07-Q12] `relhasindex` を `DROP INDEX` で下ろさない**: 理由: PG は VACUUM が下ろす。M4 の VACUUM は何もしない。影響: 下ろすなら PG と違う値が見える（KD-14）。
- **M4-Q84 [07-Q13] 索引の `relpages` は空でも 2、`reltuples` は構築した件数。表の `relpages` / `reltuples` は更新しない**: 影響: `ANALYZE` が更新する（M5）。
- **M4-Q85 [07-Q14] 索引・シーケンスの名前を `table()` で引くと `None`**: 仮決め: DML / SELECT で索引を指定したときの `42809 cannot open relation "x"`（実測）を返すのが望ましい（アナライザ側 = N1 の作業）。
- **M4-Q86 [07-Q15] `name` 型の列の索引の `atttypid` は `name` のまま（PG は `cstring`）**: 影響: `\d` には影響しない（KD-16）。
- **M4-Q87 [07-Q16] 自動名を実行時に決める**: 仮決め: 00 の `BoundIndexConstraint.name: String` を `Option<String>` にする。理由: 同じ文の中の衝突の解決。影響: 解析時に決めるとアナライザが文内の名前を覚える責任が増える。
- **M4-Q88 [07-Q17] CHECK の自動名の衝突判定を名前空間の全制約に広げる**: 理由: PG の `ChooseConstraintName`。影響: 同じ表の中だけに戻せる（まれな差）。
- **M4-Q89 [07-Q18] クラッシュで中断した DDL の孤児ファイルを掃除しない**: 理由: M3 の D15、M3-Q5。影響: 大きな表への CREATE INDEX の途中のクラッシュで数百 MB の孤児ファイル。起動時の掃除は M5。

**08 シーケンスと SERIAL / IDENTITY（M4-Q90〜Q103）**

- **M4-Q90 [08-Q1] ★ シーケンスのページとタプルの形**: 仮決め: PG と同じ構造（special 8 バイト `SEQ_MAGIC = 0x1717`、タプル 1 個 57 バイト、`xmin = Xid::FROZEN`、`t_ctid = (0,1)`）。理由: `SELECT * FROM シーケンス` が通常のヒープ走査で動き、`xmin = 2` と `ctid = (0,1)` も PG と同じ。影響: 形を変えると別の実行ノードが要る（+1 日）。
- **M4-Q91 [08-Q2] ★ `SEQ_LOG` の形**: 仮決め: ブロック 1 個、`WILL_INIT`、タプル全体、メインデータなし。REDO は無条件に上書き。理由: PG と同じ。REDO が単純で FPW の対象外。影響: 差分レコードにすると REDO が LSN の順序に依存し FPW が要る（+1 日）。
- **M4-Q92 [08-Q3] ★ SERIAL の DEFAULT の保存形式**: 仮決め: `pg_attrdef.adbin` に `nextval('<oid>'::regclass)`。`pg_get_expr` が名前に戻す。利用者が書いた DEFAULT は M2 のとおり。理由: 名前の変更・検索パスの影響を受けない。影響: すべての DEFAULT を正規形にすると deparse に OID 形式のモード（+0.5 日）。
- **M4-Q93 [08-Q4] ROLLBACK では flush しない**: 理由: 中断した値は外から見えない前提（PG と同じ保証）。影響: 中断でも `finish_without_xid` 相当で flush する（工数ほぼ 0。I15 が強まる）。
- **M4-Q94 [08-Q5] `ALTER SEQUENCE` はその場で書き換える（状態はロールバックされない）**: 仮決め: `log_cnt` は常に 0。`TRUNCATE ... RESTART IDENTITY` も同じ（KD-11）。理由: 00 の `reset` と D-9。PG の方式は M5 の表ロックなしでは払い出しが失われうる。影響: 新しい relfilenode 方式（+2 日）は M5 のロックが要る。
- **M4-Q95 [08-Q6] `TEMPORARY` / `UNLOGGED` シーケンス、`ALTER SEQUENCE RENAME` / `SET SCHEMA` は `0A000`**: 影響: RENAME は +0.25 日（KD-17）。
- **M4-Q96 [08-Q7] 他のセッションの先取りを全部捨てる（`reset_generation`）**: 影響: シーケンスごとの世代にできる +0.25 日。
- **M4-Q97 [08-Q8] 払い出した値を覆う WAL の LSN をページの LSN で決める（PostgreSQL の穴を塞ぐ）**: 理由: PG の方式は他トランザクションの未 flush の `SEQ_LOG` に依存した払い出しがクラッシュで重複しうる。影響: 「自分が書いた分」に戻すと PG と同じ穴（層 1 の変異試験が検出する）。
- **M4-Q98 [08-Q9] ユーザーが書いた DEFAULT の `regclass` は書いたままのテキスト（遅延束縛）**: 理由: M2 の「DEFAULT は SQL テキスト」（Q-006）を変えない。影響: 検索パスを変えると別のシーケンスを引きうる（M4-Q92 の正規形で直る）。
- **M4-Q99 [08-Q10] 同じ `CREATE TABLE` が作るシーケンスの名前を DEFAULT に書くと `42P01`**: 理由: DEFAULT の解析を `ddl` の途中に移す必要があり価値が低い（KD-22）。影響: +0.5 日（07 の構造に影響）。
- **M4-Q100 [08-Q11] `pg_get_serial_sequence` は任意（M4 後半）**: 影響: 必須にすると +0.25 日。
- **M4-Q101 [08-Q12] IDENTITY の `SEQUENCE NAME` は採用、`OWNED BY` / `LOGGED` は `0A000`**: 理由: pg_dump の出力が使うのは `SEQUENCE NAME`。
- **M4-Q102 [08-Q13] 同じ文の 2 つの暗黙のシーケンスの名前の衝突を避ける**: 理由: PG の上位互換（KD-22）。影響: `taken` を空にすれば PG と同じ。
- **M4-Q103 [08-Q14] `ALTER SEQUENCE` のあとの `log_cnt` は常に 0**: 理由: PG の方式はクラッシュで払い出した値が戻りうる。影響: `SELECT * FROM s` の `log_cnt` が一致する代わりにその穴を持つ。

**09 型・関数・集約・正規表現（M4-Q104〜Q113）**

- **M4-Q104 [09-Q1] ★ numeric のディスク形式は 00 §12.3 の固定ヘッダ**: 仮決め: `ndigits` `weight` `sign` `dscale` + 桁。PG の short / long ヘッダ形式は採らない（調査との食い違いは 00 を採る）。理由: M2-Q2 でタプルヘッダが PG と違いサイズ互換の意味がない。影響: PG 形式にすると `encode_numeric` / `decode_numeric` の書き直し約 0.5 日と `tuple.rs` のテストの更新。
- **M4-Q105 [09-Q2] `interval` を M5 に回し、`timestamp - timestamp` を `42883` にする**: 理由: M4 の完了条件が使わず、`interval` の連鎖（`Datum`・比較・ハッシュ・ディスク形式・`IntervalStyle`・演算子・集約）が大きい（KD-1）。影響: 入れるなら約 +3 日（`extract` なども入れて約 +4 日）。
- **M4-Q106 [09-Q3] 日時リテラルはアナライザでなくプランナの畳み込みで評価する**: 理由: `analyze(stmt, catalog)` の署名に `TypeEnv` がない（署名変更は全担当に波及）。影響: `analyze` に `&TypeEnv` を足すと解析時に評価できる（呼び出しは `session.rs` と `testing.rs` だけ）。
- **M4-Q107 [09-Q4] 型をまたぐ日時の比較演算子を作らない**: 理由: `BuiltinOperator.func` が純粋関数。影響: `OpFn::{Pure, Env}` を足し 30 行を入れる（`ts_col < now()` が索引で使える。06-Q9 と同時。約 +1.5 日）。
- **M4-Q108 [09-Q5] 正規表現の後方参照と先読みは `0A000`**: 理由: 後方参照は線形時間で解けない（KD-13）。影響: ステップ数の上限つきバックトラック版の併設約 +2 日。
- **M4-Q109 [09-Q6] `pg_typeof` は引数を評価しない**: 理由: `BuiltinFn` が型を受け取らない（KD-12）。影響: 約 +0.5 日。
- **M4-Q110 [09-Q7] DEFAULT の `'now'::timestamp` は毎回評価される**: 理由: 保存時に畳むには deparse した定数を書き戻す必要（Q-006）。影響: 約 +1 日。
- **M4-Q111 [09-Q8] `TypeEnv.names` で reg* の名前表示を一本化する（00 の P-1）**: 理由: `regclass::text` が出力段以外でも動く必要。影響: 出力段だけだと `regclass::text` が数字になり `\d tbl` と ORM が壊れる。
- **M4-Q112 [09-Q9] `round(float8)` などの float8 版を入れる**: 理由: `round(2)` が PG では `double precision`。影響: 入れないと numeric を返して PG と型が違う（費用は約 0.3 日）。
- **M4-Q113 [09-Q10] `timestamp(p)` の p が 7 以上のときの `WARNING` を出さない**: 理由: アナライザに通知の経路がない（KD-12）。影響: アナライザの出力に `warnings` を足す約 0.5 日。

**10 EXPLAIN・deparse・COPY・互換（M4-Q114〜Q126）**

- **M4-Q114 [10-Q1] `ExplainNode` を PostgreSQL の表示用の木にする**: 仮決め: `PhysicalPlan` と同形にせず、`Project` / `Filter` を吸収し `Hash` を足す。計測は `exec_id`（**→ C-1 で全章の基準にする**）。影響: 同形にすると EXPLAIN の期待値の大半を yuzhu 専用にする。
- **M4-Q115 [10-Q2] ANALYZE の出力を `actual` と `Rows Removed` だけにする**: 理由: `Sort Method` `Buckets` `Memory Usage` は実装依存。影響: 各ノードが追加の計測を持つ約 1〜2 日。
- **M4-Q116 [10-Q3] `BUFFERS` `WAL` `SETTINGS` `MEMORY` `SERIALIZE` を受け付けて無視する**: 理由: pgAdmin・DBeaver が付ける。影響: `0A000` にするとそれらのツールの EXPLAIN が使えない。
- **M4-Q117 [10-Q4] `FORMAT JSON` / `XML` / `YAML` を `0A000` にする**: 影響: JSON を足す約 2 日（KD-19）。
- **M4-Q118 [10-Q5] COPY の失敗の後に即座に `E` と `Z` を返し、後続の `d` `c` `f` を無視する**: 理由: PG と同じ。影響: 「CopyDone まで受信して捨ててから `E`」はデータを送らないクライアントがハングする。
- **M4-Q119 [10-Q6] `\d tbl` は M4 の完了条件に入れず、M4 では動かない**: 仮決め（レビュー対応 R-18・R-19）: 実機の SQL 10 本のうち 3 本（行レベルセキュリティ、拡張統計、出版物）が M4 の型・関数で解析できず、`pg_collation` と空のカタログ表を作る担当もないので、列の表示の時点で失敗する（KD-20。以前の「索引・CHECK・既定値の表示までは動く」は誤り）。影響: 動かすには約 7 日（配列の最小実装 約 5 日（09 の範囲）、空のカタログ表 約 1.0 日（C1b）、`regnamespace` と 2 関数 約 0.5 日、結合 約 0.5 日）。
- **M4-Q120 [10-Q7] COPY の CSV・バイナリ・`TO`・`WHERE`・`ON_ERROR` を M5 にする**: 影響: CSV 約 3 日、`TO STDOUT` 約 1〜2 日、`WHERE` 約 0.5 日（KD-19）。
- **M4-Q121 [10-Q8] `transaction_timeout` を受け付けて保存だけにする**: 仮決め: M3 の「`42704`」を上書き。理由: pg_dump 17 が `SET transaction_timeout = 0` を送る（KD-28）。影響: `42704` のままだと pg_dump 17 と `psql -f` のダンプが通らない。
- **M4-Q122 [10-Q9] 混合幅の整数演算子の有無で式の表示が変わる**: 仮決め: `int2` / `int4` / `int8` の混合幅の演算子がある前提で `bi > 5` は `(bi > 5)`。**実測: 既存の `builtin.rs` にすでにある（45 行）ので 09 の追加は不要**。影響: なければ `Cast` が入って表示が PG と違う。
- **M4-Q123 [10-Q10] `pretty`（括弧の最小化）を作る**: 理由: psql の `\d tbl` と SQLAlchemy の reflection が使う。影響: 作らなければ `pretty = true` でも非 pretty の出力（約 1 日の節約）。
- **M4-Q124 [10-Q11] pgbench のパーティション確認の失敗に頼る**: 仮決め: `CROSS JOIN LATERAL` が `0A000` でも pgbench は続行（偽のサーバで確認）。影響: 将来の版が中止するようになったら LATERAL の最小実装が要る。
- **M4-Q125 [10-Q12] COPY の 1 行の長さの上限を 64 MiB にする（`54000`）**: 影響: 値を変えるだけ。
- **M4-Q126 [10-Q13] `tests/slt/m4/psql/` は自分の表に絞った問い合わせ、`tests/compat/psql/` は psql の出力そのもの**: 理由: slt の DB は共有され他のテストの表が残る（M2-Q22）。

**11 テスト全体・実装計画（M4-Q127〜Q141）**

- **M4-Q127 [11-Q1] 差分ランダムテストは新しい `yuzhu-fuzz-sql` ではなく既存の `tests/tools/difftest` を仕上げて使う**（D11-6、**→ C-5**）: 仮決め: 00 §4 の `yuzhu-fuzz-sql` は作らない。理由: 約 4,300 行の独立したツールがすでにあり、言語非依存のテストツールは `tests/` に置く（CLAUDE.md）。ただしコミットされた状態はビルドが通らない（実測）。影響: 00 のとおりワークスペースの `yuzhu-fuzz-sql` にするなら `tests/tools/difftest` の移動と `postgres` クレートのワークスペースへの追加（+0.5 日）。difftest を捨てると +約 6 日。
- **M4-Q128 [11-Q2] 工数の増加（115.5 → 141.8 日）と K の 4 分割**（D11-11）: 仮決め: K を K1〜K4（24.3 日。141.8 はレビュー対応の +1.5 日を含む）、S1 5 日、R2 5 日、L2 6.9 日、C1 8.2 日、Q1 5.8 日、E1 6 日。理由: 各章が書いたテスト・機能の量が 00 の見積りを超えた。影響: 最長経路は変わらない（約 21〜23 日）が、担当を絞ると期間が延びる。
- **M4-Q129 [11-Q3] slt のディレクトリに `ddl/` `mem/` `z_final/` を足す**（D11-2）: 理由: `consistency.slt` を最後に流す、`onlyif yuzhu` 専用の `mem/` を見落とさない。影響: 00 §18 の一覧に 3 つ足すだけ。
- **M4-Q130 [11-Q4] 既知の差分の規約**（D11-3）: 仮決め: `# KNOWN-DIFF: KD-<n>` を必須とし、`KNOWN-DIFFS.md`（初期 29 項目）と lint で管理する。理由: M4 は既知の差が多く、`skipif` の外し忘れを防ぐ。影響: 規約をやめると `skipif yuzhu` が増え続ける。
- **M4-Q131 [11-Q5] `plan_variants` はテンプレートから 2 段階で生成し、生成物をコミットする**（D11-4）: 理由: 同じ問い合わせ群を 10 通りの設定で流すので手書きは保守できない。期待値は PostgreSQL で 1 回だけ作る。影響: 手書きにすると保守が破綻する。実行時に生成するとランナーに依存する。
- **M4-Q132 [11-Q6] 再起動シナリオの `mode` ファイルと `NN-*.mode`、`tests/run.sh` の拡張**（D11-5、**→ C-9**）: 理由: 08 のシーケンスは正常停止とクラッシュで期待値が違い、`seq-mixed-restart` は 1 つのシナリオの中で停止の方法が変わる。影響: ディレクトリを `restart` / `crash` で分けるだけにすると mixed が書けない。
- **M4-Q133 [11-Q7] PostgreSQL 回帰テストの取り込みは `tests/imported/pg_regress/`（`tests/slt` の外）に生成し、夜間だけ流して合否にしない**（D11-7）: 理由: 数千文で毎回の CI が遅くなる。取り込みは一致した文だけを機械的に選ぶ。影響: 合否にすると PostgreSQL の挙動の細部への追従で M4 が終わらない。
- **M4-Q134 [11-Q8] クラッシュ試験にワークロード 8（DDL）と不変条件 I16（カタログの整合）、`LossyIndexStore` の変異を足す**（D11-8）: 理由: 07 の依頼。どの不変条件にも検出する変異が最低 1 つある状態にする。影響: 足さないと DDL の WAL の抜けを層 1 が検出しない。
- **M4-Q135 [11-Q9] CI のゲート: pg 側は常に必須、yuzhu 側はフェーズ 4 の開始から必須**（D11-9）: 理由: 実装前に yuzhu 側を必須にすると毎回赤くなる。影響: 早く必須にすると PR が通らない期間ができる。
- **M4-Q136 [11-Q10] 環境: tzdata を 3 か所に明示、psql / pgbench は 17 系、Python を使わず Rust のツール**（D11-10、D11-13）: 理由: 実行用 `Dockerfile` に tzdata がない、sandbox に `python3` と `uv` がない（実測）。影響: `sandbox/Dockerfile` に `python3` を足せば 09 の `.py` のままでよい（イメージの再ビルドが要る）。
- **M4-Q137 [11-Q11] compat テストは `postgres` データベースの `public` だけで動かす**（**→ C-12**、KD-29）: 理由: `CREATE DATABASE` / `CREATE SCHEMA` は M5。10 §7.4 の `createdb` と「複数スキーマ」は動かない。影響: M5 で `createdb` を使う構成に戻せる。
- **M4-Q138 [11-Q12] isolation の 2 本と `pgbench/crash.sh` を足す**（D11-14、D11-15）: 理由: 読み手が書き手を待たないこと・シーケンスが戻らないこと・実プロセスの `kill -9` で pgbench の不変条件（原子性）が保たれること。影響: 足さなくても完了条件は満たせるが、索引・シーケンスの結合の検証が薄くなる。
- **M4-Q139 [11-Q13] M4 の完了条件は 9 項目（正本は `01-scope-decisions.md` §3。11 §1.3 は同じ表）**: 仮決め: 機械的に判定でき、ローカルで実行できる形（`tests/done-check.sh`）。理由: 00 の D-24・D-25 と各章の範囲から組み立て、01 で確定した（レビュー対応 R-01、R-29）。影響: 完了条件を変えるときは 01 を直す。
- **M4-Q140 [11-Q14] 担当不在だった作業の割り当て**（§7.3 の G-1〜G-4）: 仮決め: CREATE INDEX / DROP INDEX / TRUNCATE / VACUUM / ALTER TABLE の解析（`analyzer/ddl_index.rs`）は C1 に +1.0 日、`nextval` 系の関数の行は Q1、`pg_get_*` の関数の行は E1、`\dt+` 用の関数は T3（任意）。理由: どの章も担当を書いていなかった。影響: 別の担当に移すと日数が移る。
- **M4-Q141 [11-Q15] 章間の食い違いの決め方と、その結果（C-1〜C-30）**（D11-1。C-21〜C-30 はレビュー対応で追加）: 仮決め: 実機 > 実装の持ち主の章 > ディスク形式の定義の持ち主 > 横断契約（00・02）> 00。理由: 実装者が迷わない。影響: 決定を変えたい食い違いは §7.1 の「変えたい場合」に書いた。

---

### 3. 01 章（範囲・決定・完了条件・保証）の確認事項（M4-Q142〜Q147）

`01-scope-decisions.md` §7 の `[01-Q1]`〜`[01-Q6]` を振り直した（11 §6.3 が予約していた M4-Q142 以降）。

- **M4-Q142 [01-Q1] 完了の判定を「ローカルで実行できるスクリプトの結果」にする**（D01-1、D01-2）: 仮決め: `tests/done-check.sh`。夜間（CI）の連続 7 回は条件から外し、固定シード 1〜32（各 2,000 問い合わせ）で差分 0 件、長時間実行（シード 1001〜1004、各 200,000）を各 1 回完走して未解決の差分 0 件。理由: 実装側は push も CI の起動もできず（CLAUDE.md）、連続 7 夜は 1 回の失敗で途切れ、シードが日付で変わって再現できない。影響: 夜間の連続合格を条件に戻すと完了判定が push 後のホスト作業になり、フェーズ 4（約 5 日）に 7 夜分の余裕が要る。
- **M4-Q143 [01-Q2] 後半・任意の項目（RETURNING、ソートの省略、内側 Index Scan の NLJ、`IS DISTINCT FROM`、行値 IN ほか）を完了条件に入れない**（§1.2、D01-3）: 仮決め: 余力があれば入れる。`RETURNING` は M5 の最初。理由: 最長経路（P0 → L2）を守る。影響: `RETURNING` を必須にすると X3・L1・L2・N1 に約 2〜3 日、ソートの省略は約 0.4 日、NLJ は約 0.5 日。
- **M4-Q144 [01-Q3] `\ds` と `\d シーケンス` を完了条件 5 に含める**（D01-4、D-24 の拡張）: 仮決め: 実機（psql 17.11、`psql -E`）で SQL を採取済み（10 §6.1）で、必要な機能は M4 で作るものだけ。理由: シーケンスの互換の検証になる。影響: 任意に戻しても作業は減らない（slt は残る）。
- **M4-Q145 [01-Q4] `\d tbl` は M4 では動かず、動かすなら約 7 日の任意 WP**（R-18、R-19。M4-Q119 と同じ決定）: 仮決め: 実施しない。理由: 実機の SQL 10 本のうち 3 本が M4 の型・関数で解析できず、`pg_collation` と空のカタログ表の担当がなかった。影響: 空のカタログ表（C1b。約 1.0 日）、配列の最小実装（約 5 日。09 の範囲）、`regnamespace` と 2 関数（約 0.5 日）、結合（約 0.5 日）。
- **M4-Q146 [01-Q5] M4 の工数は 141.8 日、期間は約 21〜23 日**: 仮決め: 並列度約 19 まで使う。理由: 11 §4。影響: 担当を半分にすると期間はおよそ 1.7 倍。
- **M4-Q147 [01-Q6] M4 の着手は M3 の `dev` への統合後**（D01-5）: 仮決め: A・S1・K・Z と B1・B2・T1〜T3 の新規ファイルだけは先に始める。P0 は待つ。理由: P0 が `eval.rs` と `session.rs` を作り直す。影響: M3 の統合前に P0 を始めると、M3 の持ち主との衝突を解決する作業が増える（約 +1〜2 日と不確実性）。

---

### 4. レビュー対応で追加した確認事項（M4-Q148〜Q158）

`98-review-response.md` のレビュー対応で、実在を確認して設計を直した際に、**仮決めを増やした・変えた**もの。

- **M4-Q148 [R-14] 内側 Index Scan で INNER の左右を入れ替えるときは、上に並べ直しの `Project` を置く**: 仮決め: `NestedLoopParam` は常に `outer ++ inner`（outer = 論理の left）。入れ替えたときは `Project` で論理の左 ++ 右に戻し、`Phys.layout` は常に左 ++ 右（04 §7.5.3、00 §9.2、05 D5-8）。理由: 00・05・02 の契約と一致し、`validate` とルールが結合の向きを知らなくてよい。影響: 入れ替えを禁止すると内側が左の表のときにインデックスが使えない（任意の NLJ なので結果は変わらない）。入れ替えた `layout`（右 ++ 左）を許すと `Project` の費用（出力 1 行あたり列の並べ替え 1 回）は省けるが、`validate` が結合ごとに向きを持つ。
- **M4-Q149 [R-11] `check_interrupts` はループを持つ全ノードが入力 1 行ごとに呼ぶ**: 仮決め: 00 §4.3 の 4・05 §5.0 の (b) に従い、葉のノードだけでなく `Filter` / `Sort` / `Materialize` / `HashAggregate` / `HashJoin` の build / `Distinct` / `Unique` / `Limit` の skip も 1 行ごとに呼ぶ（02 §3.7.4 の規則 2 を直した）。理由: `Materialize` の読み直し・`Values` / `Result` の上のループなど葉に届かない経路で、キャンセルが効かなくなる。影響: 葉だけにすると費用（原子変数の読み出し 1 回）が減るが、上の経路が止まらない。
- **M4-Q150 [R-13] ハッシュ化 SubPlan の NULL は 05 の `HashedSubPlan { set, null_rows, full_rows }` による正確な三値論理**: 仮決め: 02-D17 の「NULL を含むときは `test` を行ごとに評価する経路に落とす」は採らない（05 D5-7、C-26）。理由: C-10 で `SubPlanStates` の型は 05 に決めた。05 は PostgreSQL の `findPartialMatch` と同じ O(n) の走査で 11 件の実機の値と一致している。影響: 02 の方式にすると `SubPlanStates` の型を 02 に戻す。
- **M4-Q151 [R-07・R-08・R-09] シーケンス連携の API と命名・依存関係を 1 つにする**: 仮決め: `ddl::sequence::create_with_oid`（OID は 07 §5.1 の手順 4b で先に採る）と `restart_owned_by_table`、依存関係と DROP は 07 の `catalog/depend.rs` / `CatalogStore`（`plan_drop` / `drop_objects`）、命名は C1 の `catalog/naming.rs` の 1 つ、`analyzer/ddl.rs` は Q1 が持ち主（C-28）。C1 に `update_sequence_params`（+0.2 日）、Q1 に 07 からの依頼（+0.5 日）。理由: 07 と 08 が互いの章に存在しない関数を求めていた。影響: 08 の `ddl/depend.rs` を作る案に戻すと、07 の `plan_drop` と 2 つの依存の実装を持つことになる。
- **M4-Q152 [R-12] INNER の USING / NATURAL の併合列は非キャストの側を選ぶ**: 仮決め: PostgreSQL の `buildMergedJoinVar` と同じ（03 §3.2.2 が正。02 の表を直した）。`t(a int2) JOIN u(a int4) USING (a)` は `Right(0)`（Cast なし）で、`EXPLAIN VERBOSE` の `Output` は `u.a`。理由: 実機の表示を一致させる。影響: 常に `Left(j)`（+ Cast）にすると `Output` の表示が PostgreSQL と変わる。
- **M4-Q153 [R-16] `context` が backend / superuser-backend の 6 つの GUC は SET できない**: 仮決め: `ignore_system_indexes`・`post_auth_delay`・`jit_debugging_support`・`jit_profiling_support`・`log_connections`・`log_disconnections` は `RESTART_ONLY_GUCS`（`55P02 parameter "x" cannot be set after connection start`。`set_config()` も）に入れる。`INERT_GUCS` は 162 件、`RESTART_ONLY_GUCS` は 188 件（10 §9）。理由: 実機（PG17.11）の挙動。影響: 受け付けて保存だけにすると、接続時の `PGOPTIONS` の設定が PostgreSQL と違って通る。
- **M4-Q154 [R-17・R-26] 整数 GUC の小数は `rint` で丸め、enum の同義語は正規名で保存する**: 仮決め: `set default_statistics_target = 5.5` は 6、`2.5` は 2、`statement_timeout = 1.5` は 2ms、`'0.4ms'` は 0。`wal_compression = on` の `SHOW` は `pglz`、`backslash_quote = 'true'` は `on`（10 §9）。理由: 実機（PG17.11）の挙動。影響: 小数を拒否する・入力をそのまま保存すると PostgreSQL と差が出る。
- **M4-Q155 [R-20・R-21] 正規表現の名前つき照合要素と `[\D]` を受け付ける**: 仮決め: `[[.hyphen.]]`・`[[.space.]]` など PG の名前表の名前を受け付け（未定義名は `invalid collating element`）、ブラケット内の `\D` `\S` `\W` は補集合として動く（09 §9.2、§9.4）。理由: 実機の挙動。名前の表は実機から採取して静的な表にする。影響: 拒否すると PostgreSQL が通すパターンを `2201B` にしてしまう。
- **M4-Q156 [R-22・R-23・R-24] 10 章の実機との位置・出力の記述の訂正**: COPY の `format foo` の位置は option の名前、対象不存在の `42P01` に位置なし、EXPLAIN の例 `max(b)`、pgbench のパーティション確認の失敗はログに何も出さず続行（10 §5.1、§3.11、§7.2）。仮決めではなく事実の訂正で、変える選択肢はない。
- **M4-Q157 [R-25] `\ds` と `\d シーケンス` の SQL は 10 §6.1 に採取した全文を正とする**: 仮決め: `\ds` は `relkind IN ('S','')`・`pg_am` の結合なし。`\d シーケンス` は名前の解決・属性・`pg_sequence`・所有列の 4 本（配列を使わない）。担当: カタログは 07、行は 08、`format_type(oid, NULL)` は 09、テストは K3（`psql/ds.slt`、`d_seq.slt`、`tests/compat/psql/{ds,d_seq}.sql`）。理由: 08・07 が「10 章が確かめる」と書いていたのに 10 になかった。
- **M4-Q158 [R-02・R-30] 00 は統合版に直し、残りの読み替えは 11 §7 を正とする。依存の向きの例外は 2 か所**: 仮決め: 00 に C-1〜C-30 の主要な反映（`yuzhu-fuzz-sql` の廃止、`ExplainNode`、`levels_up`、`3h + 1`、D-11・D-12、`Unique`、`copy_in_response`、`Cast.implicit`、`type_env`、担当表）を直接入れ、00 §4.1 に例外（`const_fold` → `executor::eval`、`deparse::stored` → `analyzer`）を明記する。理由: 並列実装者が 00 だけを契約として読んでも誤らないようにする。影響: 00 を初版のままにして 11 の読み替え表だけに頼ると、実装者が古い署名・担当で実装する。

---

### 5. 既存の項目への注記

- M2-Q8（psql の `\dt` は M4）→ M4 で `\dt` `\dn` `\di` `\ds` `\l` と `\d シーケンス` を完了条件に（D-24、M4-Q144）。
- M3 の「`transaction_timeout` は `42704`」（`m3.md` §1.2）→ M4-Q121 で上書き。
- M2-Q9（`pg_get_expr` の正規形）→ D-21 で解決。
- M2-Q22（slt の後始末）→ `slttools lint` と `z_final/no_leftovers.slt` で解決。


## M4 の完了時に追加した確認事項（実装・レビューで決めたもの。2026-10-07 JST）

### A. ディスク形式に関わる ★ 12 件（実装は設計書どおり。変えるなら initdb のやり直しが要る）

承認が要るものとして残す。ID は `spec/design/m4/99-questions.md` と同じ。

| ID | 仮決め（実装した形） | 変えた場合の影響 |
|---|---|---|
| M4-Q52 | B+Tree のピボットは「区切り = 右ページの最初の項目そのもの」（PG と逆向き） | 降下・moveright・検査器を反転。実装後は initdb のやり直し |
| M4-Q53 | 空のインデックスでもメタ + 空の葉の 2 ページを `BTREE_PAGES`（INIT）で作る | 遅延作成にすると REDO の理由が増える（+0.5 日） |
| M4-Q54 | 構造変更は全画像の 1 レコード `BTREE_PAGES`。必要ブロック数は `3h + 1`、32 を超えたときだけ 54000 | PG 方式（INCOMPLETE_SPLIT）は +M |
| M4-Q55 | ピボットは切り詰めなし（葉タプル + 8 バイト、high key の `block = 0`） | suffix truncation は +3 日（ページ形式は変わらない） |
| M4-Q57 | 一括構築は葉を全部作ってから上のレベル（葉 90%・内部 70%、32 枚ずつ） | nbtsort 方式は +1 日 |
| M4-Q60 | `datetime_ops` に型をまたぐ行を入れない（`pg_amop` が PG より 30 行少ない） | 入れるなら `CmpFn` の署名変更を含め +1.5 日 |
| M4-Q72 | 9 カタログを追加（`pg_index` `pg_depend` `pg_sequence` `pg_language` `pg_opfamily` `pg_opclass` `pg_amop` `pg_amproc` `pg_description`） | 依存を足すなら `CATALOG_VERSION_NO` を上げて initdb |
| M4-Q73 | `int2[]` / `int2vector` は `Datum::Int2Vector`（PG の配列ヘッダなし） | M5 で PG 形式にすると `conkey` / `indkey` の読み直しと initdb |
| M4-Q90 | シーケンスのページは PG と同じ構造（special 8 バイト、タプル 57 バイト） | 形を変えると別の実行ノードが要る（+1 日） |
| M4-Q91 | `SEQ_LOG` は 1 ブロック・タプル全体・REDO は無条件上書き | 差分レコードにすると FPW が要る（+1 日） |
| M4-Q92 | SERIAL の DEFAULT は `nextval('<oid>'::regclass)`、`pg_get_expr` が名前に戻す | すべての DEFAULT を正規形にするなら +0.5 日 |
| M4-Q104 | numeric は固定ヘッダ（`ndigits` `weight` `sign` `dscale` + 桁） | PG 形式にすると約 +0.5 日 |

### B. 実装・レビューの途中で決めたこと（承認待ち。変えても initdb は不要）

1. **同じキーの UPDATE を繰り返すとユニーク検査が 2 乗で遅くなる（実在）。** 設計 06 の D6-7 / M4-Q62 は項目を削除しない（LP_DEAD・簡易削除なし）と決めている。直すには `BTREE_DELETE` の WAL とスキャンとの連動が要る。M5 の VACUUM と一緒に入れる想定で M4 では入れなかった。M4 で簡易削除（+2〜3 日）を入れるかの判断が残っている。
2. **`shared_buffers` の下限を 16 から 64 フレーム（512kB）に上げた。** 構築は 33 バッファ、分割は `3h + 1` バッファを同時にピンするため。PG の下限（128kB）より大きい。64 フレームでの CREATE INDEX と大きなキーの INSERT を実サーバで通す確認はしていない。
3. **B+Tree の `descend` と `with_covering_page` と `walk_left` は、`nblocks` の判定に失敗しそうなときに `nblocks` を読み直す。** ファイルの伸長と並行したときの偽の XX001 を防ぐ。
4. **インデックス検査器（`check_structure`）は書き手が止まっているときだけ使う。** 設計 06 §6.2 の「並行してよい」を直した。並行実行できるようにはしていない。
5. **揮発性の select 項目（`nextval` など）は、整列キーでなければ Sort の後の Project で評価する（DISTINCT なしのときだけ）。** `select nextval('sq') from r order by a desc` が PG と同じ 1,2,3 になる。
6. **FROM の括弧つき結合のパース。** 曖昧なときは副問い合わせを先に試し、失敗したら巻き戻して括弧つき結合を試す。
7. **接続スレッドのスタックを 64MB にし、`check_stack_depth` の予算を 16MB にした。** プランナの再帰にも `check_stack_depth` を入れ、`MAX_PLAN_DEPTH` は 100,000 に上げた（実質はスタック予算で制限）。パーサの `MAX_NESTING_DEPTH` は 1000 から 5000 に上げた（6000 項の連鎖は 54001）。PG が受け付ける深さとの完全な一致は目指していない。
8. **`yuzhu-numeric` の `approx_constant` の clippy 指摘は `std::f64::consts::LOG10_E` / `LOG10_2` に置き換えた。** 推定計算に使う定数で、桁がごくわずか変わるだけ。
