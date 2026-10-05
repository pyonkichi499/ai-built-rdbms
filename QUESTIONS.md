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
