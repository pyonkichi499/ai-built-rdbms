# yuzhu M5 基本設計 99: 確認事項（仮決めの一覧）

M5 の設計（00〜10 章）で、ユーザーの不在中に**推奨案で仮決めした点**を 1 か所に集めた一覧です。各行は「仮決め・理由・変えたい場合の影響」の形で、章ごとの `M5-<略号>-Q<n>` と、この章の通し番号 `M5-Q<n>` の対応表を兼ねます。`QUESTIONS.md` には触らない（この一覧から、M5 の完了時に主エージェントが追記する。10 章 §7.3 の 13）。

- **番号の付け方**: 00 §10.1 の契約レベル（M5-Q1〜Q16）、続いてレビュー対応で増えた契約レベルの仮決め（Q17〜Q22）、章 01〜10 の順に章内の番号順で Q23 以降を振る。章の確認事項の本文は各章の「確認事項」の節が正で、この表は要約。
- **★** はディスク形式（永続形式・WAL・制御ファイル・カタログの列の符号化）に関わるもの。実装の前（工程の 5 日目の凍結点。10 章 R-13）に決めるのが望ましい。変えるなら `catalog_version` を上げて initdb をやり直す（M2-Q20 の方針）。
- 以前 10 章 §10.2 にあった通し番号の表は章 01〜09 を読めずに作った暫定で、実際と対応しなかったので廃止した（98 章 R-12）。この表が唯一の通し番号。

## 1. ユーザーの確認が特に要るもの（優先順）

| 優先 | 通し番号 | 内容 | 理由 |
|---|---|---|---|
| 1 | M5-Q1 | SAVEPOINT は M5 に入れない | SQLAlchemy + psycopg 3 は接続時に使う。M5 に入れると +15〜25 日 |
| 2 | M5-Q4 | MultiXact はメモリ上だけ（Q-014）。FK の子の INSERT 中に親の非キー列 UPDATE が待たされ、デッドロックも増える | 実用性に直結（pgbench の FK 付き構成など） |
| 3 | M5-Q17 | Extended Query は解析結果を世代の鍵でキャッシュし、計画は Bind ごと（00 D19 を改訂） | 00 D19 の元の文面と違う。性能のための逸脱 |
| 4 | M5-Q18 | `SELECT srf(args)` だけの最小形を M5 に入れる（00 D25 の例外。+0.5 日） | FK のある表の psql `\d tbl` が動く |
| 5 | M5-Q19 | numeric の `sqrt` などは numeric の引数で `0A000` | float8 を黙って返すより安全だが、ORM の numeric の sqrt が動かない |
| 6 | M5-Q21 | F0 を F0a・F0b-1・F0b-2 に分け（6.5 日）、M4 のファイルへの修正の F1（3 日）を足す。M5 の合計は 172.5 日、クリティカルパス 30 日 | 初版の 158 日の見積もりが各章の依頼で膨らんだため |
| 7 | M5-Q8、Q15 | 配列のディスク形式（PG と同じ）と M4 の `int2[]` の置き換え | ★。`catalog_version` が変わる |
| 8 | M5-Q2（RETURNING・COPY TO） | M4 の欄の続きを M5 で仕上げる（+1.5〜4.5 日） | ORM の INSERT が動くかに直結 |
| 9 | M5-Q95、Q98 | `timetz` なし、`to_char` なし（M6） | 一部のアプリが動かない |
| 10 | M5-Q140 | `ALTER DATABASE` の 3 オプション（+0.5 日） | 契約の範囲に無い追加 |
| 11 | ★の全項目（§4） | ディスク形式の凍結 | 実装前に確定 |

## 2. 契約レベル（00 §10.1 と、レビュー対応で増えたもの）

| 通し | ID | ★ | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|---|
| M5-Q1 | 00-Q1 | | SAVEPOINT は M5 に入れない（M6 の先頭候補。D2） | サブトランザクションは XID・可視性・ロック・clog・WAL に触れる。L の工数 | SQLAlchemy + psycopg 3 は M6 まで繋がらない可能性。M5 に入れるなら +15〜25 日。契約は `is_own` / `owns_xid` で拡張点を残した |
| M5-Q2 | 00-Q2 | | RETURNING は M4 が欄を用意し M5 が仕上げる（RW-6、+1.5 日）。COPY TO / CSV は任意（XQ-6、+3 日） | ORM が毎回使う | 入れないと Rails / SQLAlchemy / Prisma の INSERT が動かない |
| M5-Q3 | 00-Q3 | | `INSERT ... ON CONFLICT` は M6 | L。行ロックと一意検査の待ち（M5）が前提 | M5 に入れると +10 日以上 |
| M5-Q4 | 00-Q4 | ★ | MultiXact はメモリ上だけ（Q-014）。xmax の `IS_MULTI` と制御ファイルの `next_multi` が永続形式 | 永続化・WAL・切り詰めを避ける | 永続版（案 C）は +L |
| M5-Q5 | 00-Q5 | | ヒントビットは入れず clog の読みをロックなしにする（D13） | FPI が要る | 入れるなら `XLOG_FPI_FOR_HINT` の運用が増える（+M） |
| M5-Q6 | 00-Q6 | ★ | 64 ビットの凍結境界は別カタログ `yz_relxid` / `yz_datxid`（D14、D51） | 共有テストが `relfrozenxid` の型 `xid` を見ている | `xid8` 型の列にすると `catalog_columns.slt` を直す |
| M5-Q7 | 00-Q7 | | autovacuum は実装するが既定は無効 | 並行試験の再現性 | 有効にすると試験の揺らぎの原因になりうる |
| M5-Q8 | 00-Q8 | ★ | 配列の列はユーザーテーブルで 0A000。ディスク形式は PG の `ArrayType`。bytea・uuid・time・interval の opclass は足す | M5 の範囲。`uuid PRIMARY KEY` は必須 | 配列の列を許すなら比較・ハッシュ・opclass が要る（+M） |
| M5-Q9 | 00-Q9 | | SERIALIZABLE は 0A000 | SSI は L 以上 | SERIALIZABLE を明示するアプリだけが影響 |
| M5-Q10 | 00-Q10 | | 孤児の `base/<oid>/` は消さず WARNING | M3 D15 と同じ | 容量が残る |
| M5-Q11 | 00-Q11 | | DateStyle / IntervalStyle はクレートの全形式を受け付ける | 追加の工数がない | — |
| M5-Q12 | 00-Q12 | | M3 の分離性 spec（書き込みの直列化に依存するもの）を書き直す | M5 で意味が変わる | M3 の期待ファイルは廃止 |
| M5-Q13 | 00-Q13 | | DDL の置き場は M4 の `ddl/`（D41） | 入口を 1 つに | 別の入口だと経路が 2 通り |
| M5-Q14 | 00-Q14 | | M5 の実装は M4 のマージ後（新規ファイルだけの作業は先行。XQ は先行に含めない。R-17） | F0 の分割が M4 の触るファイルを動かす | 並行で始めると手戻りが大きい |
| M5-Q15 | 00-Q15 | ★ | M4 の `int2[]` を `Array` に置き換える（D25） | ディスク形式を PG に一本化 | 置き換えないと配列が 2 系統 |
| M5-Q16 | 00-Q16 | | `IndexStore::insert` の戻り値を `InsertOutcome` にする（D44） | B+Tree が `LockManager` を知らずに済む | B+Tree が内部で待つ案もある |
| M5-Q17 | R-15 | | **Extended Query は解析結果を `AnalysisKey`（世代・search_path・DateStyle・TimeZone）でキャッシュし、計画は Bind ごと（00 D19 を 04 XQ-D6 に合わせて改訂）** | 解析は世代が同じなら結果が同じ。ORM の同じ文の繰り返しを軽くする | 元の D19（Bind ごとに再解析）に戻すと実装は単純だが、再解析のコストが毎回かかる（鍵の判定は +0 日） |
| M5-Q18 | R-28 | | **`SELECT srf(args)`（SRF が 1 つだけ、ほかの出力列も FROM 句もない）を `SELECT * FROM srf(args)` に書き換えて受け付ける（00 D25 の例外。TY-5c +0.5 日）** | psql 17 の `\d tbl`（FK のある表の `Referenced by`）が `pg_partition_ancestors` をこの形で使う | 外すと FK のある表の `\d tbl` が動かない（07 の完了条件 9 を外す） |
| M5-Q19 | R-34 | | **numeric の引数の `sqrt` / `exp` / `ln` / `log` / `power` は `0A000`（00 D23 を 05 TY-D15 に合わせて改訂）** | float8 を黙って返すと PG（numeric を返す）と違う | 行を入れないと `sqrt(2.0)` が float8 を返す。本体の実装は +M |
| M5-Q20 | R-08 | ★ | **`yz_datxid`: template0 は番兵 `i64::MAX`、CREATE DATABASE は `min(テンプレートの行の値, oldest_xmin())`、読み書きは VC の `store_vac.rs` のみ（03 VC-D13 / C11 に統一）** | 番兵をコピーすると clog の切り詰めの境界が実態より大きくなる | 09 の元案（全部 3 でコピー）は template0 の clog が永久に切り詰められない |
| M5-Q21 | R-20、R-35、R-36 | | **F0 を F0a（2.5）・F0b-1（1.5）・F0b-2（2.5）に分け、M4 のファイルへの修正を F1（3 日）で行う。合計 172.5 日、クリティカルパス 30 日** | 各章が F0 に押し付けた 30 項目以上と、M4 のファイルへの修正の担当不在 | F0 を 2.5 日のままにすると全並列作業の出発点が見積もりより遅れる |
| M5-Q22 | R-02 | | **カタログ用スナップショットも `take_snapshot` で登録する（短命。00 D12 を改訂）** | 登録しないと走査中のカタログの行を VACUUM が除去する窓が開く | `Mutex` 2 回分のコスト。外すと VACUUM がカタログ表を対象にしない等の別の保護が要る |

## 3. 章ごとの確認事項

### 01 LK（`M5-LK-Q1`〜`Q14`。通し Q23〜Q36）

| 通し | ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|
| M5-Q23 | LK-Q1 | デッドロック検出は hard + soft の辺を辿り、待ち行列の並べ替えはしない（LK-D1） | hard だけだと soft を含む循環が永久に待つ | PG の並べ替えまで入れると +2〜3 日 |
| M5-Q24 | LK-Q2 | カタログ用スナップショットも登録する（LK-D9。M5-Q22） | VACUUM との窓を作らない | 登録しない場合は別の保護が要る |
| M5-Q25 | LK-Q3 | インデックスにはロックを取らない（LK-D14） | DDL は表のロックを先に取る | PG と同じにするなら +0.5 日 |
| M5-Q26 | LK-Q4 | XID は文の開始時に割り当てる（LK-D24） | `ExecCtx` から `TxnManager` を呼ばずに済む | 遅延割り当ては +1 日 |
| M5-Q27 | LK-Q5 | 同名 CREATE の同時実行は後発が 42P07（PG は 23505） | カタログに一意インデックスがない | PG に合わせるなら +0.5 日 |
| M5-Q28 | LK-Q6 | `pg_locks` は `virtualxid` の行を作らない。16 列は PG17 と同じ | yuzhu に仮想トランザクション ID がない | 行を足すなら +0.5 日 |
| M5-Q29 | LK-Q7 | `lock_timeout` と `statement_timeout` が同じ 20ms 周期で両方切れたら `statement_timeout` を報告する | `InterruptFlag` の口を増やさない | +0.3 日 |
| M5-Q30 | LK-Q8 | RR の最初のスナップショットはリレーションロックの前（RW-D10。R-03） | 実機（PG はそうする） | 後にすると RR の挙動が PG と違う |
| M5-Q31 | LK-Q9 | ロック表は 1 本の `Mutex`（LK-D2） | デッドロック検査が全体を見る | 競合が見えたら 16 分割（+2 日、M6） |
| M5-Q32 | LK-Q10 | `LOCK TABLE` は読み取り専用でも実行でき、仮想リレーションは何もせず成功 | PG17 の実測 | 制限すると PG と差 |
| M5-Q33 | LK-Q11 | `nextval` などは `RowExclusive` を取る。DROP は所有シーケンスも AccessExclusive | ファイルをリレーションロックで守る（D10） | 取らないと DROP が使用中のファイルを消す |
| M5-Q34 | LK-Q12 | 待ちの周期は 20ms | `InterruptFlag` に起こす仕組みを足さずに済む | 即時にするなら +1 日 |
| M5-Q35 | LK-Q13 | `max_locks_per_transaction` は `SHOW` のみ（64 固定）、`SET` は 55P02 | PG17 と同じ | 保存だけにするなら `INERT_GUCS` |
| M5-Q36 | LK-Q14 | `pg_locks` の OID は 9811 | システムビューの OID は版で変わる | PG と同じ 12073 にもできる |

### 02 RW（`M5-RW-Q1`〜`Q15`。通し Q37〜Q51）

| 通し | ID | ★ | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|---|
| M5-Q37 | RW-Q1 | ★ | 更新者の xmax に `EXCL_LOCK` / `LOCK_ONLY` を立てない。新しい版の xmax は常に空（RW-D1、D3）。【実機】で PG も立てないと確認済み | 実機の infomask | 新版にロックを引き継ぐ形（PG と同じ）にすると WAL の値が増える。観測できる差はない |
| M5-Q38 | RW-Q2 | | KEY SHARE がコミット済みの非キー更新に当たったら最新版にロックして旧版を返す（RW-D4） | RR の FK 検査で偽の 40001 を出さない | 他の強度と同じ `Updated` にすると RR の FK 検査が失敗しうる |
| M5-Q39 | RW-Q3 | | RR の最初のスナップショットはロック取得の前（RW-D10。00 §5.1 を改訂） | 【実機】 | 後にすると PG と違う（共有の `rr-first-snapshot` が落ちる） |
| M5-Q40 | RW-Q4 | | 「最初のスナップショットを取った」は `SELECT 1` を含むほぼすべての文で立てる（RW-D11） | 【実機】 | M3 の記述のままだと PG と違う |
| M5-Q41 | RW-Q5 | | 入れ子の `FOR UPDATE`・FROM の副問い合わせへの `FOR` は 0A000 | 黙ってロックを落とすのを避ける | 対応するなら pull-up の前の伝播（+M） |
| M5-Q42 | RW-Q6 | | EPQ で外部結合の ON 条件は再評価しない（RW-K8） | 計画木の部分再実行をしない | ON の式を持たせる（+S〜M） |
| M5-Q43 | RW-Q7 | | RETURNING のシステム列は `ctid` / `tableoid` / `xmin` / `cmin` だけ | `xmax` は DELETE で意味が分かれる | 許すなら +S |
| M5-Q44 | RW-Q8 | | 契約に足したもの（`Ord`、`lockmode`、`table_name`、`ExecCtx.procs`、`InsertOutcome` の non_exhaustive）。00 に反映済み | EPQ・NOWAIT・待ちに要る | 足さない場合は executor が計算 / `RelHandle` に名前 |
| M5-Q45 | RW-Q9 | | MultiXact の表に上限を設けない | 同じメンバー集合は 1 つの ID を共有 | 上限超過で 53200 |
| M5-Q46 | RW-Q10 | | 仮想リレーションへの `FOR UPDATE` は黙って無視（`pg_roles` だけ PG はエラー） | `pg_locks` は PG も同じ | 42809 にする |
| M5-Q47 | RW-Q11 | | B+Tree の分割で不要になったページは孤児として残す（B7） | ラッチの後の確保 | 事前確保（無駄が増える）か M6 の再利用 |
| M5-Q48 | RW-Q12 | | カタログ行の書き込みは `wait: Some`、文言は PG と同じ XX000（M4 の `store.rs` に `&WaitCtx` を足す。F1） | PG の `simple_heap_update` | `wait: None` だと DDL 同士の衝突で稀に失敗 |
| M5-Q49 | RW-Q13 | | SERIALIZABLE の文言は 00 §3.8 に統一 | 契約が固定 | M3 の文言のままなら 00 を直す |
| M5-Q50 | RW-Q14 | | Extended の RR の最初のスナップショットは Bind の直前（Parse は印だけ）。R-03 で 04 と整合 | Parse はロックを取らない | Parse で取る形は XQ-2 の変更 |
| M5-Q51 | RW-Q15 | | `ensure_reserved` の閾値（512 / 1024） | ラッチの外で確保 | 接続数を上げるときは見直す |

### 03 VC（`M5-VC-Q1`〜`Q16`。通し Q52〜Q67）

| 通し | ID | ★ | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|---|
| M5-Q52 | VC-Q1 | | horizon はクラスタ全体（VC-D4）。別 DB の長いトランザクションが VACUUM を止める | 登録簿が 1 つ | データベースごとなら +1〜2 日 |
| M5-Q53 | VC-Q2 | ★ | 統計と凍結境界はその場の上書き（`HEAP2 INPLACE` 0x30）（VC-D3） | ANALYZE がブロック内で動き、VACUUM が XID を持たない | 通常の UPDATE にすると VACUUM が XID を持つ必要 |
| M5-Q54 | VC-Q3 | | `ANALYZE`（VACUUM なし）はブロックの中で動く（00 §5.5 を改訂。R-29） | 【実機】PG17 と同じ | 25001 にすれば初版のまま |
| M5-Q55 | VC-Q4 | | clog は全 DB の全テーブルが VACUUM されるまで切り詰められない。template0 は番兵 | 64 ビット XID なので周回しない | `ALLOW_CONNECTIONS false` の DB が止める |
| M5-Q56 | VC-Q5 | | 起動時に 0 バイトの孤児ファイルだけ消す（VC-D19） | 起動時は安全 | 消さなければ 0 バイトの残骸が残る |
| M5-Q57 | VC-Q6 | ★ | FSM は 2 段、約 508 GiB まで | 実装が小さい | 3 段なら +1 日（形式変更） |
| M5-Q58 | VC-Q7 | | autovacuum の統計はメモリ上のみ | 並行試験の再現性 | 永続化は +1〜2 日 |
| M5-Q59 | VC-Q8 | | VACUUM はテーブルごとにコミット | PG と同じ | — |
| M5-Q60 | VC-Q9 | | `VACUUM FULL`・VM・HOT などは M6（FULL は 0A000） | 範囲 | FULL は +3〜5 日 |
| M5-Q61 | VC-Q10 | | 凍結は常に horizon まで | clog の切り詰めが進む | `vacuum_freeze_min_age` 相当にすると書き込みが減る |
| M5-Q62 | VC-Q11 | ★ | 行ポインタ（TID）の再利用が M5 で始まる | 領域回収に必要。安全性は 5.3.5 | — |
| M5-Q63 | VC-Q12 | | VERBOSE の出力は INFO で PG に似せるが細部は違う | 共有テストで比較しない | — |
| M5-Q64 | VC-Q13 | | 権限の検査をしない（GRANT は M6） | M5 の範囲 | M6 で所有者・MAINTAIN の検査 |
| M5-Q65 | VC-Q14 | | cleanup lock を作らない（VC-D1） | 待ちの仕組みが要らない | 参照を持ち出す実装を入れるなら必要 |
| M5-Q66 | VC-Q15 | | `TRUNCATE ... CASCADE` は参照元が文に含まれない場合だけ 0A000（00 D42 を改訂。R-07） | FK-D18 と PG | PG と同じ再帰（+0.5 日） |
| M5-Q67 | VC-Q16 | | VACUUM 用のリングバッファなし | M5 の範囲 | M6 |

### 04 XQ（`M5-XQ-Q1`〜`Q14`。通し Q68〜Q81）

| 通し | ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|
| M5-Q68 | XQ-Q1 | `pg_prepared_statements` は作らない（M6） | `VirtualCtx` にセッション情報がない | +1.5 日 |
| M5-Q69 | XQ-Q2 | 汎用プランなし。解析結果は鍵でキャッシュし計画は Bind ごと（M5-Q17） | M4 の planner は軽い | 計画キャッシュは +2〜3 日 |
| M5-Q70 | XQ-Q3 | 結果の形式コードを Bind で検査 | 早く失敗する方が単純 | PG と同じは +0.3 日 |
| M5-Q71 | XQ-Q4 | 定数畳み込みのエラー（22001 等）は Execute で出る | `ExternParam` を実行時に評価 | Bind で畳み込むなら +2 日 |
| M5-Q72 | XQ-Q5 | `statement_timeout` は 1 メッセージごと | M3 の延長 | バッチ全体なら +0.5 日 |
| M5-Q73 | XQ-Q6 | Describe（文）の再解析 0A000 は ParameterDescription なしでエラーだけ | `Result` を返す契約 | +0.3 日 |
| M5-Q74 | XQ-Q7 | Parse で未知の型 OID は `XX000 cache lookup failed for type N` | 【実機】 | 親切にするなら 42704 |
| M5-Q75 | XQ-Q8 | XQ-6（COPY TO・CSV・Extended COPY）は任意で見積もりに含める | pg_dump・`\copy` | 外すと M6 |
| M5-Q76 | XQ-Q9 | `DISCARD TEMP` は何もしない | 一時テーブルがない | — |
| M5-Q77 | XQ-Q10 | `F`（FunctionCall）は 0A000 | ドライバは使わない | +1 日 |
| M5-Q78 | XQ-Q11 | Parse の宣言の個数が多くても受ける | 【実機】 | — |
| M5-Q79 | XQ-Q12 | Extended の SELECT は行をストリーミング。Simple は溜めてから送る | PG と同じ | Simple も変えるなら +1 日 |
| M5-Q80 | XQ-Q13 | `P B E` の後の `Q` は暗黙のトランザクションを引き継ぐ | 【実機】 | — |
| M5-Q81 | XQ-Q14 | 準備済み文・ポータルの数に上限を置かない | PG も置かない | 上限は +0.2 日 |

### 05 TY（`M5-TY-Q1`〜`Q13`。通し Q82〜Q94）

| 通し | ID | ★ | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|---|
| M5-Q82 | TY-Q1 | | `ARRAY(SELECT)` と添字をカットラインから外す（TY-D14） | psql の `\d tbl` が送る | 落とすと `\d tbl` を完了条件から外す |
| M5-Q83 | TY-Q2 | | `\d tbl` の `string_agg` と SELECT 句の SRF の最小対応を M5 に足す（TY-D17、M5-Q18） | FK の `\d` | 外すと FK のある表の `\d` が動かない |
| M5-Q84 | TY-Q3 | ★ | 多次元配列は 0A000（入口で `M5_MAX_NDIM = 1`） | 00 D25 | 6 にするなら +1 日 |
| M5-Q85 | TY-Q4 | | ユーザーテーブルの配列列は 0A000 | 比較・ハッシュ・opclass が要る | +3 日（比較系だけなら +1.5 日） |
| M5-Q86 | TY-Q5 | | 配列の ORDER BY / GROUP BY / DISTINCT は 42883 | Q85 と同じ | Q85 に含まれる |
| M5-Q87 | TY-Q6 | | opclass の OID は実機の値。`pg_amproc` の support 2 / 4 は入れない | genbki の連番 | 実装時に読み直す（0.2 日） |
| M5-Q88 | TY-Q7 | | `md5` / `sha*` は任意項目 | ORM が使うことがある | 落とすと 42883 |
| M5-Q89 | TY-Q8 | | `convert_to` / `convert_from` は UTF8 だけ | 符号化は UTF8 のみ | LATIN1 は +0.2 日 |
| M5-Q90 | TY-Q9 | | `FnKind::Env` を足す（F0b-2） | `array_to_string` が TypeEnv を使う | 足さないと DateStyle を無視 |
| M5-Q91 | TY-Q10 | | `int2vector` / `oidvector` は yuzhu 独自の符号化のまま | M4 のディスク形式を変えない | `ArrayType` にすると +1.5 日 |
| M5-Q92 | TY-Q11 | | numeric の関数は `yuzhu-numeric` の公開 API だけで書く | クレートを変えない | 境界が出れば +0.5 日 |
| M5-Q93 | TY-Q12 | | 差分コーパスは式の一覧から生成した sqllogictest | サンドボックスに Python がない | TSV 形式に揃えると解析の差を見逃す |
| M5-Q94 | TY-Q13 | | numeric の `sqrt` 系は行だけ入れて `0A000`（TY-D15。M5-Q19） | float8 を黙って返さない | 本体は +M |

### 06 TD（`M5-TD-Q1`〜`Q12`。通し Q95〜Q106）

| 通し | ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|
| M5-Q95 | TD-Q1 | `timetz` と `current_time` は 0A000 | 非推奨。00 の範囲 | 足すなら `Datum::TimeTz` と 12 バイト形式 |
| M5-Q96 | TD-Q2 | 既定の `TimeZone` は `UTC`（実機は `Etc/UTC`） | M1 のまま | 1 行 |
| M5-Q97 | TD-Q3 | `DEFAULT 'now'` は CREATE TABLE の時点で畳み込む（M4 の呼び出し 1 か所。F1） | PG と同じ | M4 が受け入れなければ既知の差 |
| M5-Q98 | TD-Q4 | `to_char` / `to_date` / `to_timestamp(text, text)` は M6（0A000） | 書式言語が大きい | +5〜8 日 |
| M5-Q99 | TD-Q5 | `AT TIME ZONE` を `time` に使うのは 42883 | `timetz` がない | `timetz` で消える |
| M5-Q100 | TD-Q6 | `OVERLAPS` は M6 | 使用頻度が低い | +1 日 |
| M5-Q101 | TD-Q7 | `generate_series(timestamp, …)` は M4 の登録方式に乗れれば M5 | M4 の章が未確定 | 日付の連番をアプリで作る |
| M5-Q102 | TD-Q8 | `date_bin` などは任意 | 安い（各 0.2 日） | 落とすと 42883 |
| M5-Q103 | TD-Q9 | startup パラメータの不正値は無視（PG は接続拒否） | M1 のまま | `PGTZ=Foo/Bar` の差 |
| M5-Q104 | TD-Q10 | `timezone_abbreviations` は `Default` 固定 | 00 §3.6 | 設定ファイルは +1 日 |
| M5-Q105 | TD-Q11 | `now` はトランザクションの開始時刻 | PG と同じ | — |
| M5-Q106 | TD-Q12 | `Datum::Time(i64)` のまま | 00 の契約 | 新型にしてもディスク形式は同じ |

interval のフィールド指定（`year to month` など）と `interval(p)` は **PG と同じ符号化で対応**する（TD-D3。10 の KD-17 を R-13 で直した）。

### 07 FK（`M5-FK-Q1`〜`Q12`。通し Q107〜Q118）

| 通し | ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|
| M5-Q107 | FK-Q1 | `INITIALLY IMMEDIATE` / `NOT DEFERRABLE` は受け付け、`DEFERRABLE` / `INITIALLY DEFERRED` は 0A000（FK-D7） | `INITIALLY IMMEDIATE` は既定と同じ | 依頼文どおり全部 0A000 でもよい |
| M5-Q108 | FK-Q2 | TRUNCATE は参照元が同じ文に全部含まれれば通す（FK-D18。00 D42 を改訂） | PG と同じ | 常に拒否に戻すと PG と差 |
| M5-Q109 | FK-Q3 | `DROP CONSTRAINT` は FK だけ（FK-D16） | ALTER の大半は M6 | M4 の `ddl/constraint.rs` を直す |
| M5-Q110 | FK-Q4 | 連鎖の処理は幅優先（FK-D3） | 再帰するとスタックが尽きる | 深さ優先は `check_stack_depth` |
| M5-Q111 | FK-Q5 | D6 の簡易版の帰結（親の非キー UPDATE が待つ。デッドロック増） | Q-014 | 永続 MultiXact で消える |
| M5-Q112 | FK-Q6 | `CONTEXT` 行は出さない | ORM は使わない | +S |
| M5-Q113 | FK-Q7 | `relhastriggers` は FK の表でも `f`、`pg_trigger` に行なし | D26 | `pg_dump` は M6 |
| M5-Q114 | FK-Q8 | 型の互換は簡約（クロスタイプ演算子の組は 42804） | 子と親の索引を同じ比較で引く | 組ごとの比較関数 |
| M5-Q115 | FK-Q9 | `pg_depend` の行は PG17 の実機と同じ | M4 の簡約形と違う | M4 の依存解決が `pg_depend` を引くなら動く |
| M5-Q116 | FK-Q10 | 早期の処理（`CheckParent`）は任意 | 大きい COPY のメモリ | 入れないと数百万行で 53200 の恐れ |
| M5-Q117 | FK-Q11 | `\d` の `pg_partition_ancestors` は TY-5c で対応（M5-Q18） | psql 17 の問い合わせ | TY-5c を外すと `\d` が動かない |
| M5-Q118 | FK-Q12 | エラーの位置情報は構文エラーのみ | DDL の意味エラーには付けない | +S |

### 08 AU（`M5-AU-Q1`〜`Q18`。通し Q119〜Q136）

| 通し | ID | ★ | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|---|
| M5-Q119 | AU-Q1 | | initdb の既定は `trust`（警告つき）。`--auth-host` で変更 | 既存のテスト・CI が動き続ける | 既定を scram にすると CI の接続にパスワードが要る |
| M5-Q120 | AU-Q2 | | `max_connections` の検査は認証の後 | M1 の設計 | PG は認証の前 |
| M5-Q121 | AU-Q3 | ★ | 制御ファイルに `mock_auth_nonce`（32 バイト、オフセット 128） | 偽のソルトでロールの存在を隠す | プロセスごとの乱数だと再起動で変わる |
| M5-Q122 | AU-Q4 | | 認証エラーの文言は `pg_hba.conf` のまま | ドライバが PG の文言を照合 | DETAIL で補う |
| M5-Q123 | AU-Q5 | | hba は PG の部分集合（`local` などは読み捨て、ホスト名などは起動エラー） | 黙って緩めない | 再読み込みは M6 |
| M5-Q124 | AU-Q6 | | DROP ROLE は全データベースの所有物を走査する（R-26 で KD-23 を直した） | 他の DB の所有物を見逃さない | `pg_shdepend` は M6 |
| M5-Q125 | AU-Q7 | | M5 にはデータのアクセス制御がない | GRANT / REVOKE は M6 | 最小の追加案は 08 に |
| M5-Q126 | AU-Q8 | | ロールの権限モデルは PG15 以前の意味（CREATEROLE が全ロールに効く） | ADMIN OPTION は M6 | PG16 の規則は +M |
| M5-Q127 | AU-Q9 | | `pg_authid` の SELECT はスーパーユーザーだけ。`pg_roles` は誰でも（`********`） | M2-Q13 の解消 | 全員に読ませるとパスワードが漏れる |
| M5-Q128 | AU-Q10 | | 平文 `password` 方式の存在判定が時間差で分かる | PG も同じ | scram は偽のソルトで消す |
| M5-Q129 | AU-Q11 | | `scram_iterations` を ParameterStatus で報告（【実機】で報告対象と確認済み。R-14） | libpq の `\password` が使う | 報告しないと PG と差 |
| M5-Q130 | AU-Q12 | | 接続の検査順は PG 17（ロール → ロールの接続数 → DB。R-21） | 【実機】 | M2 の順に戻すと PG と違う |
| M5-Q131 | AU-Q13 | | `\du` のために `pg_auth_members` を 0 行の仮想リレーションで置く | psql | 置かないと `\du` が 42P01 |
| M5-Q132 | AU-Q14 | | 認証の期限は全体の壁時計 | PG と同じ | M1 の読み書きごとの期限に戻す |
| M5-Q133 | AU-Q15 | | `pg_roles` の OID は 9810 | 00 §3.2 の申請 | — |
| M5-Q134 | AU-Q16 | | `yuzhu-initdb` の既定の超ユーザー名は `postgres` | M2 のまま | OS のユーザー名にする |
| M5-Q135 | AU-Q17 | | 認証失敗の遅延は入れない | M5 の範囲外 | 数行で足せる |
| M5-Q136 | AU-Q18 | ★ | SCRAM の秘密情報の iterations は PG と同じ読み方（strtol。空白・符号・0・負を許す）。認証では iterations < 1 を 28P01（R-33） | 他クラスタの秘密情報を再ハッシュしない | 厳しくすると PG が SCRAM と見なす値を平文として再ハッシュする差 |

### 09 DB（`M5-DB-Q1`〜`Q13`。通し Q137〜Q149）

| 通し | ID | ★ | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|---|
| M5-Q137 | DB-Q1 | | 接続の検査順は PG17（DB-D1） | 【実機】 | M2 の順だと複数条件のエラーが違う |
| M5-Q138 | DB-Q2 | | CREATE / DROP DATABASE は他セッションの退出を最長 5 秒ポーリング（DB-D5。D40 を拡張） | ドライバのテストランナーが接続を閉じてすぐ DROP する | 即失敗にすると PG と違う |
| M5-Q139 | DB-Q3 | | 最後のチェックポイントはコミットの後。失敗時はコミット済みの DB が残る | D30 | REDO の窓がある |
| M5-Q140 | DB-Q4 | | `ALTER DATABASE` は 3 つのフラグだけ（+0.5 日）。`RENAME` / `OWNER` / `SET` は 0A000 | `IS_TEMPLATE true` の DB を消す手段 | 契約の範囲に無い追加 |
| M5-Q141 | DB-Q5 | ★ | `DBASE_CREATE` の REDO で `src` が無ければ WARNING で飛ばす | 起動不能を避ける | PANIC にすると PG と同じだが起動不能の恐れ |
| M5-Q142 | DB-Q6 | ★ | `DBASE_DROP` の `xid` は 0。コミットの後の独立したレコード | コミット前に置くと生きた DB を消す | PG のようにトランザクション内 |
| M5-Q143 | DB-Q7 | | `ENCODING` は UTF8 系だけ、ロケールは `C` / `POSIX` / 空だけ | M1〜M5 が UTF8 / C | 他の符号化は変換表が要る |
| M5-Q144 | DB-Q8 | ★ | **解決済み（R-08）**: template0 の `yz_datxid` は番兵、CREATE DATABASE は `min(.., oldest_xmin())` | 番兵を継承しない | — |
| M5-Q145 | DB-Q9 | | 孤児ディレクトリは WARNING のみ | M3 D15 | 消すなら起動時 `remove_dir_all` |
| M5-Q146 | DB-Q10 | | オプションの検証をアナライザでまとめて行う | 構造が単純 | PG の順序に完全に合わせる |
| M5-Q147 | DB-Q11 | | 新しい DB の `Database` ロックも最後のチェックポイントの後まで保持 | 「返れば使えて永続」を単純な規則に | PG はコミット後すぐ接続可 |
| M5-Q148 | DB-Q12 | | **所有ロールの同時削除は `lock_role_shared` で塞ぐ（R-16）** | 08 AU-D13・C9 の前提 | 取らないと所有者のいない DB の競合 |
| M5-Q149 | DB-Q13 | | autovacuum ワーカーは `Database(oid)` の AccessShare を取り接続数に数えない | CREATE / DROP との排他 | 取らないと DROP 中に VACUUM が動く |

### 10 TS（`M5-TS-Q1`〜`Q12`。通し Q150〜Q161）

| 通し | ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|---|
| M5-Q150 | TS-Q1 | 共有 specs の extended は PR で必須、`ported/` は夜間 | XQ との組み合わせを守る | PR の CI が数分延びる |
| M5-Q151 | TS-Q2 | PostgreSQL の spec を `ported/` にコピー | 上流の変更に左右されない | 自作なら +3 日 |
| M5-Q152 | TS-Q3 | 性能は退行ゲートと暫定の下限 | 要件に数値がない | 絶対値は +1 日（基準環境） |
| M5-Q153 | TS-Q4 | ドライバ CI の PR は最小、残りは夜間 | PR を 15 分に | 全部 PR だと 30 分超 |
| M5-Q154 | TS-Q5 | 変異スイッチ 19 件を各章が `DebugKnobs` に足す（名前は R-09 で章の定義に統一） | 検出能力の確認 | 入れないと M3 の 7 件だけ |
| M5-Q155 | TS-Q6 | 並行ストレスを PG17 にも向ける | 期待値の確認 | yuzhu だけだと保証がない |
| M5-Q156 | TS-Q7 | 既知の差の台帳 `tests/known-diffs.toml` を唯一の記録に | lint で検査 | 散文だと KD が追えない |
| M5-Q157 | TS-Q8 | 勧告ロックは M6 | 00 の範囲にない | `migrate` が M5 で動かない。+1 日 |
| M5-Q158 | TS-Q9 | ドライバ CI は外部取得が要り、オフラインでは実行しない | 版を固定してキャッシュ | 同梱は容量と更新の手間 |
| M5-Q159 | TS-Q10 | PR のジョブは 15 分以内 | 開発の速度 | 厳しくすると夜間が増える |
| M5-Q160 | TS-Q11 | extended のクライアント側の整形の差は `skipif postgres-extended` | sqllogictest 側の都合 | 別ファイルにするとファイルが倍 |
| M5-Q161 | TS-Q12 | 結合・安定化に 5 日（クリティカルパスに含める） | 全ジョブを通した最初の不具合 | 縮めると安定性が下がる |

## 4. ★ の集計（実装前に決める。5 日目の凍結点）

M5-Q4（MultiXact）、Q6（`yz_relxid` / `yz_datxid`）、Q8（配列）、Q15（`int2[]`）、Q20（`yz_datxid` の値の規則）、Q37（更新者の xmax）、Q53（`HEAP2 INPLACE`）、Q57（FSM。★は VC-Q6）、Q62（TID の再利用）、Q84（多次元配列）、Q121（`mock_auth_nonce`）、Q136（SCRAM の iterations の読み方）、Q141・Q142（DBASE の WAL）。TD の `time`（i64 LE）と `interval`（16 バイト）のディスク形式は 06 TD-D の符号化と 00 §3.7（確認事項としては番号を持たない）。**00 §3.7 の表とこの集計がずれたら、章の記述を正とする。**
