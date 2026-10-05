# yuzhu M5 基本設計 98: レビュー対応

M5 の設計書（00〜10 章）に対するレビュー指摘を**指摘ごとに実在確認**し、実在したものを該当の章に反映した記録です。確認は、章の本文の突き合わせ（grep と該当行の読み取り）と、**PostgreSQL 17.11 の実機**（サンドボックスの `sandbox/pg.sh`。生のプロトコルのクライアントで Extended Query のバイナリ値も送った）で行った。却下・一部却下の理由は「却下」の列に書いた。反映した場所には本文中に `R-nn`（または「レビュー対応 R-nn」）の印を付けた。

- 指摘は同じ問題を別の観点から繰り返したものが多いので、**R-nn は問題単位**にまとめた（元の指摘の数は右端）。R-22 は R-08、R-39・R-40 は R-20 に統合した。
- 結論の凡例: **反映** = 実在し、章を直した。**一部反映** = 実在したが指摘の修正案どおりではなく、実機に合わせた別の形にした。**却下** = 実在しなかった（理由を記載）。

## 1. 一覧

| R | 指摘の要約 | 結論 | 反映先 | 数 |
|---|---|---|---|---|
| R-01 | 03 が RW に求める `classify_xmax` / `invalidate_xmax` が 02 に無い。02 の `note_prunable` と 03 の `Page::set_prunable` の名前違い | **反映**。02 §4.1 に `invalidate_xmax(&TupleHeader) -> XmaxWrite` と `with_xmin_frozen` を足し、03 は `decode_xmax` / `lockers_all_finished` を使う形に直した。`set_prunable` に統一 | 02 §4.1・§5.2・§11-12、03 §4.4・§5.1・§5.2・C9 | 2 |
| R-02 | カタログ用スナップショットを登録するかが章ごとに食い違う | **反映**。登録する（短命。`take_snapshot`）に統一。登録しないと VACUUM が走査中のカタログ行を除去する窓が開く | 00 D12・§3.1・§4.3・§5.1、02 §5.8、04 §5.4・§5.5・§5.9・§6.2.5、08（pg_authid の読み）、09 §5 | 1 |
| R-03 | RR の最初のスナップショットを取る時点が 04 と 01・02 で食い違う。04 の独自の `acquire_statement_snapshot`、`txn_snapshot_taken` の M3 規則 | **反映**。RR の最初のスナップショットはロックの前（00 §5.1 d の 0'）。04 は 02 §4.5 の `statement_snapshot` / `note_snapshot_use` / `StatementSnapshot` を使い、独自の関数と `PortalSnapshot` を廃止。`txn_snapshot_taken` は RW-D11 の規則 | 00 §5.1、04 XQ-D3・§5.4・§5.5・§5.9・§6.2.5・C1 | 3 |
| R-04 | 型ごとのバイナリ関数の形が 04・05・06 で三つに割れ、長さ違反の SQLSTATE も食い違う | **反映**。`binary_send(d, ty)` / `binary_recv(&mut RecvBuf, ty)` に統一（04 §4.4 の形）。不足は `RecvBuf` が 08P01、余りは `input_binary` の `finish()` が 22P03。【実機】PG17.11 で `$1::time` に 1 バイトは 08P01 | 00 §4.9、05 §4.6・§11-11、06 §4・TD-D14・§6.7 | 2 |
| R-05 | `executor/dml.rs` の署名が 02 と 07 で別物。`ExecCtx.procs` が 04 の組み立てに無い。`ctx.locks` を使う・使わないの矛盾 | **反映**。`update_with_indexes(ctx, rel, w, snap, tid, old_row, new_row, crosscheck)`・`delete_row(ctx, rel, w, snap, tid, old_row, crosscheck)` に確定（crosscheck は引数）。`ri::after_*` と `ri::finish_statement` を 02 が受け入れた。`ExecCtx` に `procs` を足し、`ctx.locks`・`ctx.procs` は `wait_for_xid` だけが使う | 00 §4.6、02 §4.4・§4.6・§5.9・§11-14、04 §5.9・C15、07 §4.5・R1・§11 冒頭 | 2 |
| R-06 | `MultiXactTable::new` の戻り値が 00・02（`Result`）と 01（`Result` なし）で食い違う | **反映**。`Result` を返さない形に確定（制御ファイルの値を読むだけ） | 00 §4.4、02 §4.3・§11-13、01 §5.12・§11-4 | 2 |
| R-07 | TRUNCATE の FK 拒否で 03 が呼ぶ `fk_constraints_referencing` / `FkRef` が 07 に無い。FK-D18（参照元が同じ TRUNCATE に含まれれば通す）が 00 D42 / 03 VC-D17 と矛盾するのに 07 §11 は「食い違いなし」 | **反映**。03 は 07 の `check_truncate_fks(ctx, rels, cascade)` を呼ぶ。00 D42・03 VC-D17 を FK-D18（PG と同じ）に直し、07 §11 の「食い違いなし」を訂正 | 00 D42、03 VC-D17・§4.4・§5.6.1、07 §11 | 2 |
| R-08（R-22 を含む） | `yz_datxid` の template0 の値・CREATE DATABASE の複製規則・更新方法・API が 03 と 09 で矛盾し、clog の切り詰めで引けない XID が出る | **反映**。03 VC-D13 / C11 に統一: template0 は番兵 `i64::MAX`、CREATE DATABASE は `min(テンプレートの行の値, oldest_xmin())`、更新は HEAP2 INPLACE（VC は XID を持たないので MVCC の更新は不可）、読み書きの関数は VC の `store_vac.rs` のみ。09 の元案（全部 3 でコピー、`set_datfrozenxid`）は廃止。`yz_datxid` の schema / initdb の行の持ち主は F0 + VC-4 | 00 D51・§8.1、03 §4.4、09 DB-D12・§3.2・§3.3・§4.2・§5.3・§6.5・M5-DB-Q8・§11 | 4 |
| R-09 | 10 §6.9 の変異スイッチ名が各章の定義と一致しない。07・08 が定義していない | **反映**。名前は持ち主の章の定義を正とし、10 を合わせた（19 件）。03 は 6 項目に（`vacuum_ignore_horizon` を LK の `vacuum_ignore_registered_snapshots` に統合し、10 が要求する 3 項目を追加）、02 は 7 項目に、07 に `fk_skip_key_share`、08 に `scram_skip_proof_check` を追加 | 03 §7.5・C7、02 §7.6・§11-8、07 §7.3a・R10、08 §7.3a・C13、10 §6.9・TS-D9・CR-5 | 1 |
| R-10 | 仮想リレーション（`pg_locks`・`pg_roles`）への DML の SQLSTATE が三者で違う | **反映**。【実機】PG17.11: INSERT / UPDATE / DELETE は `55000 cannot insert into view ...`、TRUNCATE / DROP は `42809`。`FOR UPDATE` は `pg_locks` で黙って成功、`pg_roles` は PG がエラー（yuzhu は両方黙って無視。02 RW-K4）。00 §4.10 と 08 を 55000 に直し、`pg_roles.slt` の INSERT は共有テストにできる | 00 §4.10、08 §6・§7 | 2 |
| R-11 | `RunningXids` の `impl` を誰が書くかが 03（LK）と 01（VC）で逆 | **反映**。VC-1 が `storage/heap/prune.rs` に書く。LK は固有メソッドだけ（01 LK-D28 に合わせた） | 03 §4.1・§4.4・C5 | 1 |
| R-12 | 10 §10.2 の通し番号の表は章 01〜09 を読めずに作った暫定で、実際の章と対応しない（M5-Q23 の U-14 も【実機】確認済み） | **反映**。表を廃止し、全章の確認事項を 99-questions.md に集約して通し番号を振り直した。U-14・U-15・U-34 を更新 | 10 §10.2・§9、99 | 1 |
| R-13 | interval のフィールド制限の扱いが 10（0A000）と 06（全形式を実装）で逆 | **反映**。06 TD-D3 が正。10 の KD-17 を直した。`timetz` だけ 0A000 | 10 KD-17・99 | 2 |
| R-14 | `scram_iterations` が PG17 の ParameterStatus の報告対象かの検証状況が 04 とそれ以外で矛盾 | **反映**。【実機】PG17.11 の起動時 ParameterStatus は 14 個で `scram_iterations=4096` を含む。00 §3.6・§10.2、08 AU-D12・§9-1・M5-AU-Q11、10 U-15 を「確認済み」に | 00、08、10 | 1 |
| R-15 | 準備済み文の解析結果の扱いが 00 D19 と 04 で食い違う | **反映**。04 の設計（解析結果を `AnalysisKey` でキャッシュ、計画は Bind ごと）を採り、00 D19 を改訂。04 に C14 として逸脱を明記。10 KD-20 を直した。ユーザー確認事項（M5-Q17） | 00 D19、04 XQ-D6・C14、10 KD-20 | 1 |
| R-16 | ロールの DROP との競合を塞ぐ前提（`lock_role_shared`）を 09 が実装していない | **反映**。09 の CREATE DATABASE が所有者ロールに `lock_role_shared` を取る手順を足し、M5-DB-Q12 を「塞ぐ」に改めた | 09 §5.3・M5-DB-Q12 | 1 |
| R-17 | 04 §8 が XQ-1・XQ-4 を M4 の実装中に始めてよいとするが、00 D49 の先行リストに無く、既存ファイルに触れる | **反映**。XQ は先行リストに入れない。先行してよいのは新規ファイルだけの純粋な部分に限る | 04 §8、00 D49・§1.3 | 1 |
| R-18 | `Cluster` の組み立て順（01 §5.12）が実コードおよび 03 と矛盾（Clog の所有者、`StorageStack::new` の引数、REDO の前に作る物） | **反映**。実コード（`engine.rs` の `prepare`、`recovery::startup`、`StorageStack::new`）を読んで確認。`Clog` は `StorageStack::new` が作り、`locks`・`multixact`・`oldest_xid` は `StackConfig` で渡す（`StorageStack::new` の署名は変えない）。`prepare` の手順を直した | 01 §5.12・§11-3、00 §4.5・§6 | 2 |
| R-19 | 01 §11-14 の「準備済み文は `LockSet` を持つ」が古い（01 §4.6 は M6） | **反映** | 01 §11-14 | 1 |
| R-20（R-39・R-40 を含む） | 章の WP 依存と日数が 00 §7・10 §8.2 の工程表に反映されていない（RW-1c → RW-2、VC-3 → RW-5、VC-4 → DB-1、VC-6 → VC-5、DB-2 → LK-3・AU-3、XQ-2・TY-5・DB-3 の増分、合計）。TS-2a が RW-3・RW-5 に依存するのに 14.5〜16 に置かれ成立しない | **反映**。00 §7 と 10 §8.2・§8.3 を引き直した。F0 を F0a・F0b-1・F0b-2 に分け、M4 のファイルの修正を F1 にし、RW-1 を 02 の枝番で LK の裏に出した。合計 **172.5 日**、クリティカルパス **30 日**（初版 158 日・32 日）。TS-2a はハーネスを LK-3 の後に先行、S1（RC）を RW-3・RW-5 の後に置いた | 00 §1.3・§7・§8、10 §8.1〜8.3・CR-3・CR-13 | 5 |
| R-21 | 08 §5.1 手順 9 と AU-D10 の検査順が PG17・09 DB-D1 と食い違う | **反映**。【実機】PG17.11 で、ロールの接続数超過は存在しない DB でも `too many connections for role`、LOGIN できないロールは 28000 が先と確認。08 を 09 の順（ロール → ロールの接続数 → DB）に直した | 08 AU-D10・§5.1 | 1 |
| R-23 | 03 §6.7 の VACUUM のオプション検査の SQLSTATE（INDEX_CLEANUP、列リストと ANALYZE） | **反映**。【実機】PG17.11: `index_cleanup requires a Boolean value` は 42601、`ANALYZE option must be specified when a column list is provided` は 0A000。`PARALLEL` の範囲外は 42601、`ONLY_DATABASE_STATS` と対象表の併用は 0A000 も確認して表を直した | 03 §6.7 | 2 |
| R-24 | 08 §5.8 手順 5-f のブートストラップ超ユーザーの SUPERUSER を外す ALTER ROLE（22023 としている） | **一部反映**。22023・本文に文言、は誤り。ただし指摘の「42501」も違い、【実機】PG17.11 は **`0A000`** `permission denied to alter role` + DETAIL `The bootstrap superuser must have the SUPERUSER attribute.` | 08 §5.8 | 1 |
| R-25 | 10 G1 の `pgbench -i -I dtgvpf` の FK が 3 件 | **反映**。【実機】で 5 件（`pgbench_tellers_bid_fkey`・`accounts_bid`・`history_bid`・`history_tid`・`history_aid`）を確認 | 10 §6.10.3 | 1 |
| R-26 | 10 §10.3 の KD-1・KD-8・KD-9・KD-23 が章の決定と食い違う | **反映**。KD-1（差は更新者と KEY SHARE の 2 セルだけ）、KD-8（FULL だけ 0A000。VERBOSE は INFO）、KD-9（16 列そろえた）、KD-23（全 DB を走査）。KD-11 も直し KD-29〜32 を追加 | 10 §10.3 | 4 |
| R-27 | 06 §6.7 の time の recv 範囲外が 22003（05 は 22008） | **反映**。【実機】PG17.11 は `22008 time out of range`（`7fffffffffffffff`）。22P03 でも 22003 でもない（指摘の「22003 はどちらにも当たらない」は正しい） | 06 §6.7・§9 | 1 |
| R-28 | 07 の完了条件 `\d p` / `\d c` が `pg_partition_ancestors()`（SELECT 句の SRF）を使い、00 D25 / 10 KD-14 と矛盾。引き受ける WP が無い | **反映**。【実機】`psql -E` で、副問い合わせの腕に SELECT 句の SRF が入ることを確認。05 TY-D17（TY-5c。+0.5 日）で「SELECT 句に SRF が 1 つだけ」の最小形を書き換えて対応し、00 D25 に例外を足した | 00 D25、05 TY-D17・§8・§11、07 §6.8・M5-FK-Q11、10 KD-14 | 1 |
| R-29 | 00 の契約本文に後続章が PG17 と違うと指摘済みの記述が残っている（重複ポータルの文言、ブロック内 ANALYZE、D42、RR のスナップショット） | **反映**。00 §3.8・§5.5・D42・§5.1 を直した | 00 | 1 |
| R-30 | 配列の次元数超過のメッセージが 05 内で食い違う | **却下**。【実機】PG17.11: テキスト入力 `'{{{{{{{1}}}}}}}'::int[]`（`array_in`）は `number of array dimensions exceeds the maximum allowed (6)`（次元数なし）、`ARRAY[...]` の構築は `(7)` 入り。05 §5.3.1 の書式は PG のとおりで、`array_recv` だけが `(7)` 入り。章内の食い違いではないので、経路ごとの違いを注記しただけ | 05 §5.3.1（注記） | 1 |
| R-31 | 05 §5.2 の `factorial(int8)` の負数エラーが 22003（PG は 22023 のはず） | **却下**。【実機】PG17.11: `factorial(-1)` は `22003 factorial of a negative number is undefined`（`numeric.c:3654`）。05 は正しい | なし | 1 |
| R-32 | 06 §6.7 の date の記述の誤り（`0000223f` の日数ラベル、下限の日付） | **反映**。【実機】で `date '2024-01-02'` は 8767 日、下限は 4714-11-24 BC（-2451545 = `ffda97a7`）、4713-01-01 BC は -2451507（`ffda97cd`）。`date_recv` の範囲外は `22008 date out of range`（`<日数>` なし） | 06 §6.7・05 §6.4・§9 | 1 |
| R-33 | 08 §3.1 が SCRAM の iterations の符号・空白・`+` を許さないのに「PG と同じ」と書く | **反映**。【実機】PG17.11 は `+4096`・` 4096`・`-4096`・`0` をそのまま保存し、`4096x` だけ再ハッシュ。strtol と同じ読み方に直し、認証で iterations < 1 を 28P01 にする既知の差を M5-AU-Q18 に | 08 §3.1・M5-AU-Q18、10 KD-31 | 1 |
| R-34 | 05 TY-D15（numeric の `sqrt` 系を `0A000` の行にする）が 00 D23 と食い違い、変更依頼になっていない | **反映**。TY-D15 を採り、00 D23 を改訂して 05 §11 に依頼 15 を足した。ユーザー確認事項（M5-Q19） | 00 D23、05 §11、10 KD-15 | 1 |
| R-35 | F0（2.5 日）が各章の依頼で膨らんでいるのに見積もりと範囲が未更新 | **反映**。F0a（機械的な分割）・F0b-1（LK・RW・VC の口）・F0b-2（そのほかの口）に分け合計 6.5 日。範囲を 00 §7 に列挙 | 00 §7 | 1 |
| R-36 | M4 マージ後のファイルへの修正依頼に宛先と工数がない | **反映**。F1（3 日。M4 の担当が不在のため M5 が 1 人で行う）を新設し、00 §8.2 に台帳を置いた。00 §8 の冒頭の規則を整理 | 00 §7・§8 | 1 |
| R-37 | 契約 00 に未反映の変更依頼が多数あり、章ごとに正が分かれている | **反映**。00 の本文を直した（`RegisteredSnapshot`、`Clog::set_status`、`LockStatusRow`、`ConnectGrant`、`DdlCtx`、`commit_and_restart`、`HEAP2 INPLACE`、設定、SQLSTATE ほか）。運用を「各章の §11 に書く」1 つにし、00 §6.3 に取り込み台帳を置いた。01 の `fk_peer_tables` の依頼を 07 が受け入れた | 00 §3〜§6.3、07 §4.2 | 1 |
| R-38 | ファイルの持ち主の重複と宛先違い（`txn_ctl.rs`、`engine.rs` の `connect`、`tuple.rs`、`truncate.rs`、`clog.rs`） | **反映**。00 §8.1 で整理: `txn_ctl.rs` は RW、`Cluster::connect` の本体は DB（AU は検査の関数のみ）、`tuple.rs` の分岐は TY に許可、`truncate.rs` は VC（FK は関数を提供）、`clog.rs` は LK-3b → VC-4 の順 | 00 §8.1、03 C4、08 §8 | 1 |

指摘は右端の合計 58 件（重複した指摘を問題単位にまとめた数）。却下は R-30・R-31 の 2 問題、一部反映は R-24。

## 2. 実機で確認した事実（PostgreSQL 17.11）

反映した結論の根拠。`pgx.pl` は自作の最小の v3 プロトコルのクライアント（Perl の標準モジュールだけ。スクラッチ置き場のみで、リポジトリには入れていない）。

| 事実 | 使った章 |
|---|---|
| `time_recv` / `date_recv` / `timestamp_recv` の範囲外は 22008（`time out of range`、`date out of range`、`timestamp out of range`）。不足は 08P01、余りは 22P03 | 05、06 |
| `factorial(-1)` は 22003 | 05（却下の根拠） |
| `array_in` の次元数超過は次元数なしのメッセージ、`ARRAY[]` の構築は `(7)` 入り | 05（却下の根拠） |
| `date '2024-01-02' - date '2000-01-01'` = 8767、`date_send('4714-11-24 BC')` = `ffda97a7` | 06 |
| `VACUUM (index_cleanup foo)` は 42601、列リスト + ANALYZE なしは 0A000、`parallel -1` は 42601、`only_database_stats` + 表は 0A000、`VERBOSE` は INFO | 03 |
| ブートストラップ超ユーザーの `ALTER ROLE ... NOSUPERUSER` は 0A000 | 08 |
| 仮想リレーション（ビュー）への INSERT / UPDATE / DELETE は 55000、TRUNCATE / DROP は 42809 | 00、08 |
| 起動時の ParameterStatus は 14 個で `scram_iterations` を含む | 00、08、10 |
| SCRAM の秘密情報の iterations は `+`・空白・`-`・`0` を許す | 08 |
| `CONNECTION LIMIT 0` のロールは存在しない DB でも `too many connections for role`。DB の接続数超過は DB の検査の後 | 08、09 |
| `pgbench -i -I dtgvpf` の FK は 5 本 | 10 |
| psql の `\d` の `Referenced by` は SELECT 句の `pg_partition_ancestors` を使う | 05、07 |
