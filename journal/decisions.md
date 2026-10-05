# 設計判断と仮決めの台帳

最終更新: 2026-10-04（時刻はすべて UTC。JST は +9 時間）。

- 目的: 「なぜそうなったか」「今どういう状態か」「覆すと何が起きるか」を後から引く。
- 書き方: `<!-- AUTO:名前 BEGIN/END -->` の間は `tools/journal-decisions.sh` が書き換える。それ以外は手書き。手書きの追記は日付見出しで積み、過去の記述は上書きしない。
- 数字の出所は `ファイル:行` で示す。出所が無いものは「不明」または「推定」と明記した。
- 状態列の根拠: メイン会話でユーザーが述べたのは 5 件のみ（2026-10-04 14:26〜15:03Z）。うち 14:27:30Z「良さそうです。ワークフローを最大限使って…」は、アシスタントが 14:26:41Z に「★印のディスク形式の仮決めは、実装前にあなたに確認したい点です」と添えた進め方の提案への返答である。これを個別の仮決め（★を含む）への追認とは扱わず、**明示の追認は 0 件**として「仮決めのまま」と書く。
- 日付列: 設計書・QUESTIONS.md の初出コミット 5e42f81（2026-10-04 04:01:34Z、`git log -S`）。個々の決定の時刻は不明。

## 1. 台帳

種別: 設計 D（spec/design/m2.md 第 0 節の D 表）、仮決め Q（QUESTIONS.md / m2.md 第 9 節）、スコープ、規約例外。
「手戻り規模」は設計書に数字があるものはその出所を付け、無いものは「推定」と書いた。

| ID | 種別 | 日付 (UTC) | 論点 | 採用案 | 捨てた案 | 根拠 | ★ | 状態 | 影響 M | 覆した場合の手戻り規模 |
|---|---|---|---|---|---|---|---|---|---|---|
| D1 | 設計 D | 04:01Z 以前（5e42f81） | ROLLBACK と文の原子性 | xmin/xmax + 永続 clog（`pg_xact/`）で可視性判定 | (a) M1 の undo ログを TID 単位に拡張（m2-page-heap §2.9） | M3 で捨てるコードがほぼ出ない、ダーティリードが M2 で解消、大きな UPDATE でも前イメージをメモリに溜めない、steal を最初から安全に許せる（m2.md:24） | - | 仮決めのまま | M2〜M5 | 大（推定）。可視性判定、clog、チェックポイント、再起動テストの前提。undo 案に戻すと M3 で作り直し（m2-page-heap.md:394） |
| D2 / M2-Q2 | 設計 D + 仮決め Q | 同上 | XID の幅とタプルヘッダ | 64 ビット XID、cmin/cmax 別持ち、35 バイトのヘッダ。1 ページ最大 291 行 → 185 行（m2.md:2128、m2.md:484 `MAX_HEAP_TUPLES_PER_PAGE = 185`） | PostgreSQL 同様 23 バイト・32 ビット XID・combo CID | 周回対策（緊急 VACUUM、xidStopLimit）が不要になる。ただし clog 切り詰め用の「凍結相当」は M5 で必要（m2.md:25） | ★ | 仮決めのまま | M2〜M5 | 大。m2.md:2128 は「変更するなら M2 の実装前」。実装後は形式、WAL ヘッダ（m3.md D3: 32 バイト）に波及し initdb やり直し（D15） |
| D3 / M2-Q3 | 設計 D + 仮決め Q | 同上 | 並行性 | 単一ライターロックをトランザクション終了まで保持（M3 の前倒し） | (a) 文全体を Mutex で直列化、(b) DDL のみ DB 単位ロック | 再評価（EvalPlanQual）もデッドロックも起きない。読み取りは書き込みと並行（m2.md:26）。BEGIN で書いて放置すると他の書き込みは全部待つ（m2.md:2129） | - | 仮決めのまま | M2〜M5 | 中（推定）。複数ライターは M5 で入れる前提の設計 |
| D7 / M2-Q5 | 設計 D + 仮決め Q | 同上 | ページチェックサム | M2 から常に有効（無効化不可。PG17 の initdb 既定は無効） | M3 ではチェックサムなし（m3-wal §2.8、m3-wal.md:422） | WAL の無い M2 でも torn write やビット反転を検出できる（m2.md:30）。m3-recovery §5.4 も M3 での導入を推奨（m3-recovery.md:228）。ヒントビットは立てない（m3.md D7）ため FPI 問題を回避 | ★ | 仮決めのまま | M2〜M3 | 小〜中（推定）。無効化はページ読み書きの検査を外せば済むが、既存ファイルの `pd_checksum` 扱いと全 0 ページ規則（D12）は形式の一部 |
| D8 / M2-Q8 | 設計 D + 仮決め Q | 同上 | psql `\dt` | 前倒ししない。M2 は `\l` まで | LEFT JOIN と正規表現を M2 に前倒し（m2-catalog.md:128、446 の推奨） | JOIN を入れるとアナライザが複数テーブルに広がり M4 と重なる（m2.md:31）。前倒しすると 2〜3 日増（m2.md:2134） | - | 仮決めのまま | M2〜M4 | 小（2〜3 日、m2.md:2134） |
| D15 / M2-Q20 | 設計 D + 仮決め Q | 同上 | マイルストーン間のデータ互換 | M5 まで保証しない。`format_version` / `catalog_version` を上げたら initdb やり直し | M2 データを M3 以降で読む | 変換を作る費用に見合わない（m2.md:38、2146） | ★(関連) | 仮決めのまま | M2〜M5 | 覆すと変換コードが要る。PG 互換のバイナリ形式を狙うなら別問題（D2 参照） |
| D6 | 設計 D | 同上 | ヒントビットの書き方 | M2 は `XMAX_INVALID` 以外を読み書きしない。M3 以降は `PinnedBuffer::try_write()` で排他ラッチが取れたときだけ `page_mut_hint()` | PostgreSQL のように共有ラッチのまま書く | フレームが `RwLock<Box<Page>>` で、`forbid(unsafe_code)` では共有ラッチのままの書き込みが書けない（m2.md:29）。**CLAUDE.md の forbid 規約が設計を決めた例** | - | 仮決めのまま | M2〜M5 | 小（推定）。ヒントを使わず毎回 clog を引く設計なので影響は性能のみ |
| D4 / M2-Q15 | 設計 D + 仮決め Q | 同上 | 制御ファイル | `global/yuzhu_control` の 2 スロット交互上書き | 一時ファイル + rename、`pg_control` 流 | M3 で形式を変えずに済む。ディレクトリ操作を含まず障害モデルが単純。torn write に耐える（m2.md:27、2141） | ★ | 仮決めのまま | M2〜M3 | 小（推定）。ファイル 1 つ。QUESTIONS.md には★が付いておらず食い違い（第 4 節参照） |
| Q-003 → M2-Q1 / D1 | 仮決め Q（改訂） | 04:01Z（5e42f81） | M1 の ROLLBACK | 初案: メモリ上テーブルの undo ログ、ダーティリードあり（M3 で MVCC）。改訂: M2 から xmin/xmax + clog | - | M2-Q1「Q-003 の前倒し」（m2.md:2127）。QUESTIONS.md:12 は未更新のまま（鮮度ずれ） | - | 変更（設計書上。ユーザー未追認） | M1→M2 | 実装済みの M1 undo コードは M2 で撤去（git status で `memory.rs`、`txn.rs` が削除済み） |
| Q-004 → M2-Q7 / D11 | 仮決め Q（改訂） | 同上 | `server_version` | 初案 16.0（psql `\d` の分岐のため）。改訂 17.0 | - | カタログを PG17 の列構成にそろえる。psql の `\d` に 17 専用分岐が無いことは確認済みとされる（m2.md:34、2133）。確認手段の詳細は不明 | - | 変更（設計書上。ユーザー未追認） | M2 | 小（設定値 1 か所とカタログ列構成。m2.md:1353、1993） |
| Q-007 | 仮決め Q | 同上 | PRIMARY KEY / UNIQUE | M1 は構文のみ受理し実行は 0A000 | 黙って無視 | 黙って無視しない（QUESTIONS.md:14）。M4 で対応 | - | 仮決めのまま | M1→M4 | 小（推定） |
| Q-008 | スコープ | 同上 | M1 の型 | 要件の 5 型に smallint、real、varchar(n) を追加 | 5 型のみ | varchar(n) は typmod の経路を M1 で通すため（QUESTIONS.md:15） | - | 仮決めのまま | M1 | 小（実装済み。M1 の 37 slt が通過、journal の wf_fabaf03d integrate 結果） |
| Q-011 | スコープ | 同上 | numeric | M5 から M4 前半へ前倒し | M5 のまま | 小数リテラルや `avg(int)` の結果が numeric。無いと集約の互換が保てない（QUESTIONS.md:25） | - | 仮決めのまま | M4 | 中（推定）。覆すと M4 の集約が PG と非互換 |
| Q-012 | スコープ | 同上 | COPY / char(n) / timestamp | M4 へ前倒し | 後回し | 性能測定の pgbench 初期化に必要（QUESTIONS.md:26） | - | 仮決めのまま | M4 | 中（推定） |
| Q-013 | スコープ | 同上 | M4 冒頭の内部再設計 | 列参照の表現と論理 / 物理プランの分離を M4 冒頭で作り直す（M1〜M3 のコードに手が入る） | M1 の設計を延長 | JOIN とサブクエリに対応するため（QUESTIONS.md:27）。D8 の `\dt` 先送りの根拠にもなる | - | 仮決めのまま | M4 | 大（推定）。許容済みとしてスコープに入れている |
| Q-010 | 仮決め Q | 同上 | XID の幅 | 64 ビット、`xmin` 列は下位 32 ビットを返す | 32 ビット + 凍結 | QUESTIONS.md:24。D2 / M2-Q2 で具体化 | ★(D2 経由) | 仮決めのまま | M2〜 | D2 と同じ |
| Q-006 → M2-Q9 | 仮決め Q | 同上 | DEFAULT / CHECK の保存形式 | SQL テキストのまま保存し再パース | PG 流の内部木の直列化 | M1 の単純さ優先（QUESTIONS.md:13）。M2 でも継続し、`pg_get_expr` が PG の正規形と違う（m2.md:2135）。M4 で正規形へ | - | 仮決めのまま | M2→M4 | 中（推定）。保存済みテキストの移行が要る可能性（D15 により M5 までは initdb で足りる） |
| Q-014 | 仮決め Q | 同上 | 行ロック（M5） | MultiXact はメモリ上だけの簡易版 | 永続化する本格版 | FK のチェックの行ロックが非キー列 UPDATE と衝突しうる（PG 9.2 以前相当）（QUESTIONS.md:28） | - | 仮決めのまま | M5 | M5 まで着手しない。不明 |
| M2-Q6 | 規約例外 | 同上 | 依存の追加 `signal-hook` | yuzhu-server に `signal-hook` 0.3（実際の追加は 610c121（2026-10-04 15:19:58Z）の Cargo.toml。Cargo.lock は 0.3.18 と signal-hook-registry 1.4.8） | 自前で signal 処理（std のみでは unsafe なしに書けない） | CLAUDE.md の「周辺用途」の列挙（暗号、テスト、CLI 引数、ロギング、エラー型）に無い種類。設計書自身が「一覧にはない種類」と認めている（m2.md:2132）。「コア部品は手書き」の対象（パーサ、ストレージ、WAL、トランザクション、ワイヤプロトコル）には当たらない、というのが設計の読み | - | 仮決めのまま（規約の読みはユーザー未確認） | M2 | 小。依存 1 つを外し、停止は kill -9 前提のテストに戻す程度（推定） |
| M2-Q8（regex） | 仮決め Q | 同上 | `!~` の正規表現 | M4 に持ち越し。手書きか `regex` クレートかは M4 で決める | M2 で決める | `\dt` が M4 なので M2 では不要（m2.md:2134）。CLAUDE.md の「周辺用途」に当たるかが論点 | - | 未決（M4 で判断） | M4 | 決めるまで手戻りなし |
| M2-Q11〜14 | 仮決め Q | 同上 | TOAST なし、カタログ DML 禁止、カタログの PG との差、DROP と SELECT | 第 6 節の AUTO:pgdiff と手書き表を参照 | - | m2.md:2137〜2140 | - | 仮決めのまま | M2→M5 | 第 6 節に解消予定の M を書いた |
| 規約例外（lint） | 規約例外 | M1 Stabilize 中（wf_fabaf03d、時刻は workflows.md） | clippy `assert_is_empty` | `impl/rust/Cargo.toml` の `[workspace.lints.clippy]` に `assert_is_empty = "allow"` | テスト側を `assert_eq!` に直す | 型推論の問題が出る、とエージェントが報告（wf_fabaf03d の stabilize result）。pedantic は warn のまま、個別の allow | - | 仮決めのまま | M1〜 | 小。1 行 |

> 補足: ここまでの台帳の「状態」は、ユーザーの明示の追認が無いため、変更に分類したもの以外は全件「仮決めのまま」。ユーザーが追認や変更をしたら、日付と発言を `## 追認ログ` に追記してから状態を更新する。

### 追認ログ

- 2026-10-04: 該当なし（上記のとおり。次にユーザーが QUESTIONS.md や★に触れた発言をしたらここに書く）。

### 改訂・前倒しの注記行（自動抽出）

<!-- AUTO:supersede BEGIN -->
_生成: 2026-10-05 07:32:11 UTC。出所: `grep -n '改訂\|前倒し'` を spec/ と QUESTIONS.md に対して実行。判断が後から変わった箇所の候補で、真の改訂かどうかは手書き欄で判断する。_

| 場所 | 該当行（先頭 100 字） |
|---|---|
| `spec/design/m2.md:26` | \| D3 \| 並行性の制御 \| (a) M1 と同じく、文の実行全体を 1 本の Mutex で直列化する<br>(b) DDL だけデータベース単位の書き込みロックにする（catalog §7.2）… |
| `spec/design/m2.md:31` | \| D8 \| psql の `\dt` \| LEFT JOIN と正規表現を M2 に前倒しする（catalog §2.4） \| **前倒ししない。M2 で動かすのは `\l` まで** \| JOIN… |
| `spec/design/m2.md:166` | │   │   ├── mod.rs             △ TableStore（全面改訂）、RelHandle、TupleDesc、TmResult、WriteCtx |
| `spec/design/m2.md:1353` | \| 設定 \| `server_version = 17.0`、`server_version_num = 170000`（Q-004 の改訂） \| |
| `spec/design/m2.md:1993` | \| `server_version` \| `17.0` \| Q-004 の改訂（D11） \| |
| `spec/design/m2.md:2127` | - **M2-Q1 ROLLBACK の方式**: M1 の undo ログ方式をやめ、M2 から xmin/xmax と永続化したコミットログ（`pg_xact/`）で ROLLBACK と文の原子… |
| `spec/design/m2.md:2129` | - **M2-Q3 単一ライターロック**: 書き込むトランザクションは同時に 1 つだけにします（D3。M3 の方針の前倒し）。BEGIN の中で書き込んだまま放置すると、ほかのセッションの書き込み… |
| `spec/design/m2.md:2133` | - **M2-Q7 server_version を 17.0 に上げる（Q-004 の改訂）**: カタログを PG17 の列構成にそろえるので、名乗る版も 17.0 にします。psql の `\d… |
| `spec/design/m2.md:2134` | - **M2-Q8 psql の対応範囲**: M2 で動かすのは `\l` までにします（psql 17 で確かめます。§1.1）。`\dt` には LEFT JOIN と正規表現が要るので、JOI… |
| `spec/design/m5/99-questions.md:15` | \| 3 \| M5-Q17 \| Extended Query は解析結果を世代の鍵でキャッシュし、計画は Bind ごと（00 D19 を改訂） \| 00 D19 の元の文面と違う。性能のための逸脱 \| |
| `spec/design/m5/99-questions.md:45` | \| M5-Q17 \| R-15 \| \| **Extended Query は解析結果を `AnalysisKey`（世代・search_path・DateStyle・TimeZone）でキャッシュし、… |
| `spec/design/m5/99-questions.md:47` | \| M5-Q19 \| R-34 \| \| **numeric の引数の `sqrt` / `exp` / `ln` / `log` / `power` は `0A000`（00 D23 を 05 TY-… |
| `spec/design/m5/99-questions.md:50` | \| M5-Q22 \| R-02 \| \| **カタログ用スナップショットも `take_snapshot` で登録する（短命。00 D12 を改訂）** \| 登録しないと走査中のカタログの行を VACU… |
| `spec/design/m5/99-questions.md:79` | \| M5-Q39 \| RW-Q3 \| \| RR の最初のスナップショットはロック取得の前（RW-D10。00 §5.1 を改訂） \| 【実機】 \| 後にすると PG と違う（共有の `rr-first… |
| `spec/design/m5/99-questions.md:99` | \| M5-Q54 \| VC-Q3 \| \| `ANALYZE`（VACUUM なし）はブロックの中で動く（00 §5.5 を改訂。R-29） \| 【実機】PG17 と同じ \| 25001 にすれば初版の… |
| `spec/design/m5/99-questions.md:111` | \| M5-Q66 \| VC-Q15 \| \| `TRUNCATE ... CASCADE` は参照元が文に含まれない場合だけ 0A000（00 D42 を改訂。R-07） \| FK-D18 と PG \|… |
| `spec/design/m5/99-questions.md:175` | \| M5-Q108 \| FK-Q2 \| TRUNCATE は参照元が同じ文に全部含まれれば通す（FK-D18。00 D42 を改訂） \| PG と同じ \| 常に拒否に戻すと PG と差 \| |
| `spec/design/m5/04-extended-query.md:1336` | **M4 が済むまでの動き方**（D49。レビュー対応 R-17 で改訂）: **XQ の WP は F0（M4 のマージ後）に依存し、00 D49 / §1.3 の先行可能リスト（AU-1、TD-4… |
| `spec/design/m5/98-review-response.md:26` | \| R-15 \| 準備済み文の解析結果の扱いが 00 D19 と 04 で食い違う \| **反映**。04 の設計（解析結果を `AnalysisKey` でキャッシュ、計画は Bind ごと）を採り… |
| `spec/design/m5/98-review-response.md:44` | \| R-34 \| 05 TY-D15（numeric の `sqrt` 系を `0A000` の行にする）が 00 D23 と食い違い、変更依頼になっていない \| **反映**。TY-D15 を採り、… |
| `spec/design/m5/09-database-ddl.md:954` | \| M5-DB-Q3 \| 最後のチェックポイントはコミットの後（D30 のまま）。失敗時は、コミット済みの DB が残ったままエラーを返し、REDO の窓（最後のチェックポイントの完了まで。その間にテ… |
| `spec/design/m5/09-database-ddl.md:986` | 8. **ファイルの持ち主**（レビュー対応 R-08 で改訂）: `yz_datxid`（OID 9802）の `catalog/schema.rs` の定義と `catalog/rows.rs` … |
| `spec/design/m5/01-lock-txn.md:1465` | 3. **§4.5 / §6 `StorageStack::new`**（レビュー対応 R-18 で改訂）: **署名は 00 §4.5 のまま `StorageStack::new(vfs, cfg… |
| `spec/design/m5/01-lock-txn.md:1476` | 14. **04 章（準備済み文）への依頼（レビュー対応 R-19 で改訂）**: Bind が使う口は `Session::lock_statement_relations(&Statement) … |
| `spec/design/m5/10-tests-plan.md:1150` | レビュー対応 R-12 で、この節の通し番号の表（章 01〜09 を読めずに作った暫定。章ごとの件数と内容が実際と違った）を廃止した。**全章の確認事項の一覧と通し番号は `99-questions.… |
| `spec/design/m3.md:22` | M2 は M3 の調査の推奨を大きく前倒しした。M3 の作業はそれを前提にする。 |
| `spec/design/m3.md:40` | \| D1 \| M3 の範囲 \| 要件の M3（MVCC・clog・単一ライター・WAL）をそのまま作る／M2 の前倒しを前提に WAL とリカバリに絞る \| **後者** \| MVCC・clog・単一… |
| `spec/design/m3.md:1755` | - **M3-Q1 M3 の範囲**: M2 で MVCC・コミットログ・単一ライターを前倒ししたので、M3 は WAL・チェックポイント・クラッシュリカバリ・クラッシュ試験と、残りのトランザクション… |
| `spec/design/m4/00-contracts.md:81` | \| D-18 \| インデックス選択はヒューリスティクス（一意で全列等値 > 等値の列数 > 先頭列の範囲）。`IN (...)`・`OR`・Index Only Scan・Bitmap Scan は … |
| `spec/design/m4/02-pipeline-refactor.md:1145` | **段階が解放する担当**（00 §17 の「P0 のマージ後に並列」を段階ごとに前倒しする）: |
| `spec/design/m4/02-pipeline-refactor.md:1400` | - **段階ごとの解放**（§5.1）により、00 §17 の「P0 のマージ後に N1〜N3・L1・L2・X1〜X3・E1 を並列に」を前倒しできる。P0 の開始を 0 日目とすると、X1〜X3 は… |
| `spec/design/m4/02-pipeline-refactor.md:1484` | \| 12 \| §17 P0 の行 \| 「P0 が全員の足場を置く」を、段階ごと（P0-b: 依存のない ★、P0-c: executor、P0-d: analyzer、P0-e: planner）に置… |
| `spec/research/m5-types-fk.md:51` | \| **numeric の前倒し** \| 小数リテラル（`1.5`）が numeric 型であること、`avg(int)` や `sum(int8)` の結果が numeric であることから、**M… |
| `spec/research/m5-types-fk.md:67` | ### 1.1 M1〜M4 の現状と、前倒しが必要なもの |
| `spec/research/m5-types-fk.md:693` | \| numeric-core（I/O、typmod、四則、比較、キャスト、sum/avg） \| M \| **M4 の前半に前倒し** \| |
| `spec/research/m5-types-fk.md:713` | - **C-1 numeric の前倒し**: 小数リテラル（`1.5`）と `avg(int)` の結果が numeric であるため、numeric の基本部分（入出力・四則・比較・キャスト・su… |
| `spec/research/pg-compat-tools.md:4` | - 前提: `CLAUDE.md`（マイルストーン）、`spec/design/m1.md`、`spec/research/m2-catalog.md`（psql の `\dt` `\l` と LEF… |
| `spec/research/pg-compat-tools.md:46` | 3. **COPY プロトコルは M4 に前倒しすることを推奨する**（FROM STDIN、テキスト形式のみ。工数 M）。pgbench、pg_dump の出力のリストア、psql の `\copy… |
| `spec/research/pg-compat-tools.md:65` | \| P1 \| COPY FROM STDIN（CopyInResponse `G`、CopyData `d`、CopyDone `c`、CopyFail `f`）、テキスト形式 \| pgbench `… |
| `spec/research/pg-compat-tools.md:76` | \| Q1 \| LEFT JOIN、カンマ区切りの FROM（暗黙の内部結合） \| psql の全メタコマンド、すべての ORM \| M2 で前倒し（`m2-catalog.md` §2.1）、M4 で… |
| `spec/research/pg-compat-tools.md:124` | \| T1 \| `char(n)`（bpchar、OID 1042。空白の詰め物と比較時の末尾空白の無視） \| **pgbench**、pg_dump の出力 \| **M4（M5 から前倒し）** \| … |
| `spec/research/pg-compat-tools.md:125` | \| T2 \| `timestamp`（1114）、`timestamptz`（1184）、`CURRENT_TIMESTAMP`、timestamptz → timestamp の代入キャスト \| *… |
| `spec/research/pg-compat-tools.md:439` | \| A \| `char(n)`、`timestamp`/`timestamptz`、`CURRENT_TIMESTAMP` を M5 から前倒し \| pgbench \| T1、T2 \| |
| `spec/research/pg-compat-tools.md:526` | 1. **COPY FROM STDIN を M4 に前倒しするか**（推奨: する）。pgbench の初期化、pg_dump の出力のリストア、psql の `\copy` で必要。代わりに `p… |
| `spec/research/pg-compat-tools.md:527` | 2. **`char(n)` と `timestamp`/`timestamptz` を M5 から M4 に前倒しするか**（推奨: する）。pgbench の表定義に含まれる。前倒ししない場合は、… |
| `spec/research/m4-query.md:18` | 6. **集約の結果型は PostgreSQL と完全に一致させる**（`sum(int4)` = int8、`sum(int8)` = numeric、`avg(int*)` = numeric、`… |
| `spec/research/m4-query.md:395` | \| A: numeric の最小核を M4 に前倒し \| `Datum::Numeric`（PG の `NumericVar` と同じ base-10000 の桁配列 + weight + sign … |
| `spec/research/m4-query.md:737` | - **M4Q-2 numeric の前倒し**: `sum(bigint)` と `avg(整数)` の結果型が numeric のため、numeric の最小核（演算・入出力・キャスト・小数リテラ… |
| `spec/research/m2-catalog.md:96` | - SQL 機能: `LEFT JOIN`、`CASE`、`IN`、`ORDER BY 1,2`。**LEFT JOIN は M4 の範囲なので、M2 で `\dt` を動かすなら JOIN（少なくと… |
| `spec/research/m2-catalog.md:122` | - **推奨: M2 でカタログを PG17 の列構成に合わせ、`server_version` を `17.0` に上げる。** QUESTIONS.md に Q-004 の改訂として記録する。 |
| `spec/research/m2-catalog.md:128` | \| `\dt`, `\dt+` \| 動かす \| 上記。LEFT JOIN の前倒しと、正規表現（`~`, `!~`）の最小実装 \| |
| `spec/research/m2-catalog.md:431` | \| psql 対応 \| `pg_get_userbyid` などの関数、`E'...'`、`OPERATOR(...)`、`COLLATE`、正規表現の最小実装、LEFT JOIN の前倒し \| 4〜… |
| `spec/research/m2-catalog.md:439` | 1. **Q-004 の改訂**: M2 で server_version を 17.0 に上げ、カタログを PG17 の列構成にする（§2.3）。 |
| `spec/research/m2-catalog.md:446` | 8. `\dt` のために LEFT JOIN を M4 から M2 に前倒しするか、`\dt` を M4 まで待つか（推奨は前倒し。Nested Loop だけなら 1〜2 日）。 |
| `spec/research/m5-protocol-auth.md:648` | - **C-1 Extended Query の優先度**: M5 の中で Extended Query を最初に実装する（ドライバの既定モードがこれに依存するため）。M4 完了直後に前倒しする案もあ… |
| `spec/research/m2-dml-exec.md:15` | 3. **書き込みは単一ライターロック**で直列化する（M3 の「単一ライター + 複数リーダー」を前倒しする）。ロックは DML/DDL 文の開始時、**スナップショットを取る前に**取得し、トラン… |
| `QUESTIONS.md:25` | - **Q-011 numeric の前倒し**: `1.5` のような小数リテラルや `avg(int)` の結果は、PostgreSQL では numeric 型になります。numeric がない… |
| `QUESTIONS.md:26` | - **Q-012 COPY・char(n)・timestamp の前倒し**: 性能目標の測定に使う pgbench の初期化に、COPY FROM STDIN、char(n)、timestamp … |
| `QUESTIONS.md:40` | - **M2-Q7 server_version**: カタログを PostgreSQL 17 にそろえるので、名乗る版を 17.0 に上げます（Q-004 の改訂）。 |

合計 59 行。
<!-- AUTO:supersede END -->

### 契約からの逸脱ログ（M1）

`spec/design/m1-changes.md` が正本。台帳には転記しない。件数の推移だけ記す。

| 日付 (UTC) | C 番号の範囲 | 件数 | 出所 |
|---|---|---|---|
| 2026-10-04 | C-01〜C-16 | 16 件（C-14 まで契約逸脱、C-15・C-16 は調査で見つかった設計書の誤りの修正） | `spec/design/m1-changes.md`（`grep -c '^| C-'` で 14、C-15・16 は箇条書き） |

## 2. QUESTIONS.md の索引

QUESTIONS.md に載っているのは M2-Q のうち一部のみ（全 21 件のうち 10 件）。残りは `spec/design/m2.md` 第 9 節にしか無い。

<!-- AUTO:qindex BEGIN -->
_生成: 2026-10-05 07:32:11 UTC。出所: QUESTIONS.md（行番号は現在のファイル）、初出コミットは `git log -S` の最古の結果（日時は UTC）。_

| Q | 見出し | ★ | 行 | 初出コミット (UTC) |
|---|---|---|---|---|
| Q-001 | コミットについて |  | 7 | 5e42f81 2026-10-04 04:01 |
| Q-002 | 基本設計書の置き場所 |  | 8 | 5e42f81 2026-10-04 04:01 |
| Q-003 | M1 の ROLLBACK |  | 12 | 5e42f81 2026-10-04 04:01 |
| Q-004 | server_version の値 |  | 13 | 5e42f81 2026-10-04 04:01 |
| Q-005 | 既定のデータベース名 |  | 14 | 5e42f81 2026-10-04 04:01 |
| Q-006 | DEFAULT・CHECK 式の保存形式 |  | 15 | 5e42f81 2026-10-04 04:01 |
| Q-007 | PRIMARY KEY / UNIQUE |  | 16 | 5e42f81 2026-10-04 04:01 |
| Q-008 | M1 の型 |  | 17 | 5e42f81 2026-10-04 04:01 |
| Q-009 | 既定のポート |  | 18 | 5e42f81 2026-10-04 04:01 |
| Q-010 | トランザクション ID の幅 |  | 24 | 5e42f81 2026-10-04 04:01 |
| Q-011 | numeric の前倒し |  | 25 | 5e42f81 2026-10-04 04:01 |
| Q-012 | COPY・char(n)・timestamp の前倒し |  | 26 | 5e42f81 2026-10-04 04:01 |
| Q-013 | M4 での内部構造の大きな変更 |  | 27 | 5e42f81 2026-10-04 04:01 |
| Q-014 | 行ロックの方式（M5） |  | 28 | 5e42f81 2026-10-04 04:01 |
| M2-Q1 | ROLLBACK の方式 |  | 34 | 5e42f81 2026-10-04 04:01 |
| M2-Q2 | タプルヘッダ | ★ | 35 | 5e42f81 2026-10-04 04:01 |
| M2-Q3 | 単一ライター |  | 36 | 5e42f81 2026-10-04 04:01 |
| M2-Q4 | クラッシュ時の保証 |  | 37 | 5e42f81 2026-10-04 04:01 |
| M2-Q5 | ページチェックサム | ★ | 38 | 5e42f81 2026-10-04 04:01 |
| M2-Q6 | 依存の追加 |  | 39 | 5e42f81 2026-10-04 04:01 |
| M2-Q7 | server_version |  | 40 | 5e42f81 2026-10-04 04:01 |
| M2-Q8 | psql の対応範囲 |  | 41 | 5e42f81 2026-10-04 04:01 |
| M2-Q11 | TOAST なし |  | 42 | 5e42f81 2026-10-04 04:01 |
| M2-Q20 | データディレクトリの互換性 |  | 43 | 5e42f81 2026-10-04 04:01 |
| M2-Q22 | slt の後始末 |  | 47 | ab02b68 2026-10-04 21:57 |
| M2-Q23 | CHECK 制約の重複エラー文言 |  | 48 | ab02b68 2026-10-04 21:57 |
| M2-Q24 | `finish_pending_unlinks` の残骸 |  | 49 | ab02b68 2026-10-04 21:57 |

合計 27 件、★ 2 件（QUESTIONS.md の見出し行から数えた値。★ の追認状況は手書き欄で管理する）。
<!-- AUTO:qindex END -->

## 3. 設計書の D 表

<!-- AUTO:dtable BEGIN -->
_生成: 2026-10-05 07:32:11 UTC。出所: `grep -n '^| D[0-9][0-9]* ' spec/design/*.md`。列は 行番号 / ID / 題 / 採用（4 列目。列が無い表は 不明）。_

- `spec/design/m1-changes.md`: D 表なし（0 行）

- `spec/design/m1.md`: D 表なし（0 行）

- `spec/design/m2-changes.md`: D 表なし（0 行）

#### `spec/design/m2.md`（15 行）

| 行 | ID | 題 | 採用 |
|---|---|---|---|
| 24 | D1 | ROLLBACK と文の原子性 | **(b)** |
| 25 | D2 | XID の幅とタプルヘッダ | **(b)** |
| 26 | D3 | 並行性の制御 | **(c)** |
| 27 | D4 | 制御ファイル | **(c)** |
| 28 | D5 | ページヘッダの版数 | **yuzhu 独自の 1** |
| 29 | D6 | ヒントビット | **M2 は `XMAX_INVALID` 以外を読みも書きもしない** |
| 30 | D7 | チェックサム | **M2 から常に有効** |
| 31 | D8 | psql の `\dt` | **前倒ししない。M2 で動かすのは `\l` まで** |
| 32 | D9 | relfilenode の型 | **`RelFileNumber(u32)`** |
| 33 | D10 | スキャンカーソル | **後者** |
| 34 | D11 | `server_version` | **17.0** |
| 35 | D12 | 新しいページの判定 | **`pd_upper == 0` なら全 8192 バイトが 0 であることを検査し、違えば `XX001`。全 0 のページにはチェックサムを入れずに書く** |
| 36 | D13 | リレーションのファイルの削除 | **後者** |
| 37 | D14 | 失敗しうるページの変更 | **`page_mut()` の後に失敗しうる処理を書かない。`CriticalSection` の中のエラーと panic はクラスタの Panic に昇格する** |
| 38 | D15 | マイルストーン間のデータディレクトリ互換 | **M5 まではマイルストーンをまたぐ互換を保証しない**。`format_version` か `catalog_version` を上げたら initdb のやり直し |

#### `spec/design/m3.md`（31 行）

| 行 | ID | 題 | 採用 |
|---|---|---|---|
| 40 | D1 | M3 の範囲 | **後者** |
| 41 | D2 | WAL の物理形式 | **後者** |
| 42 | D3 | レコードヘッダの XID の幅 | **u64。ヘッダは 32 バイト** |
| 43 | D4 | `MAX_BLOCK_REFS` | **32**。あわせて `MAX_RECORD_LEN = 1 MiB`、WAL セグメントの最小を 2 MiB にする |
| 44 | D5 | ヒープレコードのタプル | **タプル全体** |
| 45 | D6 | clog の WAL | **書かない。clog の rmgr は作らない** |
| 46 | D7 | ヒントビット | **M3 でも立てない** |
| 47 | D8 | データページのチェックサム | **M2 のまま常に有効** |
| 48 | D9 | チェックポイントの方式 | **ファジー。オンラインでは `CHECKPOINT_REDO` レコードを置いてその開始位置を REDO 点にする** |
| 49 | D10 | コミットとチェックポイントの競合 | **コミットゲート（`RwLock`）を置く**。コミットは WAL 挿入から clog 更新まで共有、チェックポイントは REDO 点の決定の間だけ排他 |
| 50 | D11 | ページを dirty にする時機 | **`page_mut()` の時点（M2 §6.3 の 5 を変更）** |
| 51 | D12 | XID と OID の永続化 | **M2 の先取りを続ける。加えて REDO で見た XID の最大値 + 1 まで `next_xid` を進める** |
| 52 | D13 | リカバリ開始時のデータディレクトリの fsync | **全ファイルとディレクトリを fsync してから REDO する** |
| 53 | D14 | REDO でファイルやブロックがない | **追跡する**。後の unlink / truncate の REDO で消し、REDO の終わりに残っていれば起動を拒否する |
| 54 | D15 | 孤児ファイルの掃除 | **M3 ではしない** |
| 55 | D16 | DDL と読み取りの競合 | **ロックなし（M2 のストレージバリアのまま）** |
| 56 | D17 | D13 の残骸の削除時機 | **後者（unlink サイクル）** |
| 57 | D18 | WAL セグメントの再利用 | **しない。新しいセグメントは 0 埋めしてから rename で置く** |
| 58 | D19 | リカバリ後の WAL の末尾 | **後者** |
| 59 | D20 | FPW の判定の場所 | **挿入 Mutex の中** |
| 60 | D21 | UPDATE の WAL | **後者（M2 §6.5.3 の予告どおり）** |
| 61 | D22 | セーブポイント | **後者** |
| 62 | D23 | 分離性テストのブロック判定 | **関数を実装する**。そのために `int4[]` の最小限（テキスト入出力だけ）を足す |
| 63 | D24 | タイムアウトとキャンセル | **M3** |
| 64 | D25 | M2 のデータディレクトリ | **制御ファイルの `format_version` を 2 に上げ、1 は起動を拒否する** |
| 65 | D26 | initdb の書き込み | **WAL を通す** |
| 66 | D27 | `synchronous_commit` | **SET は受け付けるが、どの値でも同期コミットとして動く** |
| 67 | D28 | `kill -9` ハーネスのクライアント | **`postgres` クレート**（M2 で dev-dependency に追加済み） |
| 68 | D29 | 変異テストのためのスイッチ | **`DebugKnobs` を置く**（既定はすべて無効。CLI・設定ファイルからは触れない。テストが `ClusterOptions` で渡す） |
| 69 | D30 | WAL の書き込み・fsync の失敗 | **Panic**（M2 の fsync 失敗と同じ）。クラスタを poison し、サーバは終了コード 3 で終わる。次の起動でリカバリする |
| 70 | D31 | `statement_timeout` の単位 | **文ごと** |
<!-- AUTO:dtable END -->

注意: D の番号は設計書ごとに別の連番。m2.md の D1〜D15 と m3.md の D1〜D（別物）が同じ ID で衝突するので、台帳では `m3.md D7` のようにファイル名を付ける。上の台帳の D 番号は、特記が無ければ m2.md。

## 4. ディスク形式（★）

M2 でディスク形式を固定する。以後のマイルストーンでは変えない前提（m2.md 冒頭、依頼の条件）。ただし D15 により「M5 までは互換を保証せず、変えるなら initdb のやり直し」。

### 4.1 ★ 一覧

| ★ | 前提 | 決め手 | 覆す条件 | 覆した場合のコスト |
|---|---|---|---|---|
| タプルヘッダ 35 バイト（M2-Q2、D2） | XID は 64 ビット。cmin/cmax を別々に持つ | 周回対策の緊急処理を作らない。combo CID の表が不要（m2.md:25） | PostgreSQL とバイナリ互換のデータディレクトリが必要になった場合。32 ビット XID に戻す場合 | initdb やり直し。`MAX_HEAP_TUPLES_PER_PAGE`、タプルの形式化 / 復元、可視性判定、WAL のタプル全体記録（m3.md D5）、WAL ヘッダの XID 幅（m3.md D3）、テスト。範囲の大きさは不明（推定: 全面改修に近い） |
| ページチェックサム常時有効（M2-Q5、D7） | CRC は CRC32C、全 0 ページには入れない（D12） | torn write / ビット反転の検出（m2.md:30）。m3-recovery §5.4 の推奨とも整合 | 性能問題。PG 既定（無効）との互換が必要になった場合 | 読み書きの検査を外す変更。ヒントビットの扱い（M3 の D7）が変わる。既存ファイルを読むだけなら互換が保てる可能性があるが未確認（不明） |
| 制御ファイル 2 スロット（M2-Q15、D4） | `global/yuzhu_control`、スロット交互上書き、CRC32C（m2.md §3.2） | M3 で形式を変えない。torn write 耐性 | `pg_control` 互換が必要になった場合 | 小（推定）。ファイル 1 種類。ただし LSN の予約値などが M3 と結びつく（m2.md の形式レビュー #16） |
| 新しいページの判定（D12） | `pd_upper == 0` なら全 8192 バイトが 0。違えば `XX001` | PG の `PageIsVerifiedExtended` と同じ（m2.md:35） | 不要になる条件は無い | 既存ページが不正扱いになる（m2.md:35 の自己記述） |
| リレーションファイルの遅延削除（D13） | 最初のセグメントを 0 バイトに切り詰めて残し、次のチェックポイントで削除 | M3 のリカバリで同じ relfilenode の再利用による誤適用を防ぐ | M3 で WAL を入れない場合 | 小（推定）。ファイル消去の手順のみ |
| ページヘッダ版数 1（D5） | yuzhu 独自の 1 | 35 バイトのタプルで PG と形式が異なるため（m2.md:28） | PG 互換バイナリを狙う場合 | ヘッダの版数のみ（D2 が覆れば同時に覆る） |

### 4.2 未承認の★の件数（日付ごとに 1 行追記）

| 日付 (UTC) | 未承認 ★ 件数 | 数え方 |
|---|---|---|
| 2026-10-04 15:30 | 3 件（M2-Q2、M2-Q5、M2-Q15） | m2.md:2123 は M2-Q2、Q5、Q15 を「ディスク形式に関わるもの」とする。QUESTIONS.md の★は M2-Q2 と M2-Q5 の 2 件（Q15 は QUESTIONS.md に無い）。ユーザーの明示の追認は 0 件 |

## 5. 調査レポートの推奨を設計で覆した事例

m2.md 第 0 節は、4 本の調査（と M3 の調査）の推奨が食い違った点を決めたもの。

1. **チェックサム（m3-wal の「M3 ではチェックサムなし」に対して D7 は常時有効）**
   - m3-wal §2.8 は、ヒントビットのみの変更による torn page を無害にするため `pd_checksum = 0` を提案（m3-wal.md:422）。一方 m3-recovery §5.4 は M3 での導入を推奨しており、調査同士が食い違っていた（m3-recovery.md:228）。
   - 決め手: WAL が無い M2 でも壊れたページを黙って読まない（m2.md:30）。ヒントビットを立てなければ FPI の問題は出ない（m3.md D7、D8）。
   - 代償: M3 でヒントを入れるなら「dirty にしない」か FPI を選ぶ必要がある（M2-Q5）。

2. **ROLLBACK（m2-page-heap の undo ログ案に対して D1 は MVCC を前倒し）**
   - page-heap は M2 を「M1 と同じ undo ログ」とし、M3 で undo を撤去すれば済むと考えた（m2-page-heap.md:347、394）。dml-exec は案 B を推奨し、案 A は M3 でほぼ全部捨てると指摘（m2-dml-exec.md:13）。
   - 決め手: 捨てるコードが出ない、ダーティリードが M2 で消える、大きな UPDATE で前イメージをメモリに溜めない（m2.md:24）。これは Q-003 の仮決めを覆す。
   - 代償: 古い版と中断行が M5 の VACUUM まで領域を占める（m2.md:2127）。

3. **`\dt`（m2-catalog の前倒し案に対して D8 は先送り）**
   - catalog 調査は LEFT JOIN（Nested Loop だけなら 1〜2 日）と正規表現の前倒しを推奨（m2-catalog.md:128、446）。
   - 決め手: JOIN でアナライザの対象が複数テーブルに広がり、Q-013 の M4 冒頭の再設計と重なる（m2.md:31）。前倒しすれば 2〜3 日増（m2.md:2134）。
   - 代償: M2 の psql は `\l` まで。`\dt` は M4 まで動かない。

## 6. PostgreSQL との意図的な差分

自動抽出の行（ファイル:行）と、手書きの解消予定を併記する。転記はしない。

<!-- AUTO:pgdiff BEGIN -->
_生成: 2026-10-05 07:32:11 UTC。出所: `grep -n 'PG との差\|PostgreSQL と違'` を spec/ と QUESTIONS.md に対して実行。転記はせず、該当位置だけを示す。_

| 場所 | 該当行（先頭 100 字） |
|---|---|
| `spec/design/m2.md:91` | - 配列型の行（`typcategory = 'A'`）は、上の 5 つと `oidvector` だけを `pg_type` に入れる。ほかの型の `typarray` は 0 にする（PG との差… |
| `spec/design/m2.md:119` | - M2 では、`pg_type` の `typreceive`・`typsend`・`typmodin`・`typmodout`・`typanalyze`・`typsubscript` をすべて 0… |
| `spec/design/m2.md:2139` | - **M2-Q13 カタログの PostgreSQL との差**: 次の点が PostgreSQL と違います。(1) ユーザーテーブルと一部のカタログの行型（`reltype`）を作らない（6 つ… |
| `spec/design/m5/00-contracts.md:1337` | - 「PostgreSQL と違う」点は、その章の確認事項と、`10-tests-plan.md` が集める「既知の差」に挙げる。共有テストには差が出るケースを入れない。 |
| `spec/design/m5/07-foreign-key.md:1198` | - **M5-FK-Q5 D6 の簡易版の帰結**: 子の INSERT 済みのトランザクションがあると、親の非キー UPDATE が待たされ、デッドロックも増える（§5.8、§1.4）。PG との差… |
| `spec/design/m5/02-row-lock-rr.md:1430` | \| M5-RW-Q3 \| RR の最初のスナップショットはロック取得の**前**（RW-D10）。00 §5.1 d・D5 の「ロックはスナップショットより先」は RC の文のスナップショットに限る … |
| `spec/design/m5/02-row-lock-rr.md:1431` | \| M5-RW-Q4 \| 「最初のスナップショットを取った」は、`SELECT 1` を含むほぼすべての文で立てる（RW-D11）。M3 §5.2 e・§6.11.1 の「FROM のない SELEC… |
| `spec/design/m5/01-lock-txn.md:41` | \| LK-D15 \| CREATE の名前の衝突 \| PostgreSQL: カタログの一意インデックスで待ち、後発は `23505`（`pg_type_typname_nsp_index`。実機 E… |
| `spec/design/m5/10-tests-plan.md:1179` | \| KD-20 \| 差 \| **汎用プランを作らない**（Bind ごとに計画をやり直す。解析結果は `AnalysisKey`（世代・search_path・DateStyle・TimeZone）が… |
| `spec/design/m4/07-catalog-ddl.md:1861` | - **[07-Q12] `relhasindex` を `DROP INDEX` で下ろさない**（D07-9）。M4 の `VACUUM` は何もしないので、下ろされる機会がない。**変えたい場合… |
| `spec/design/m4/05-executor.md:55` | \| D5-12 \| `avg(float)` \| 入力順に `f64` で足し、`n` で割る（`float8_accum` の `Sx` と同じ）。結果は float8。`Sxx`（分散用）は持たな… |
| `spec/design/m4/08-sequence-serial.md:92` | \| `ALTER SEQUENCE` の後の `log_cnt` \| PostgreSQL は `START` だけを変えたとき `log_cnt` を保ったまま新しいファイルに WAL を書く（`P… |
| `spec/design/m4/08-sequence-serial.md:1357` |   - 仮決め: `SeqRun.wal_lsn` は呼び出し後のページの LSN。D8-7 の順序も PostgreSQL と違う。どちらも観測できる SQL の挙動は変えず、クラッシュ後の重複だけ… |
| `spec/design/m4/11-tests-plan.md:278` | 各章が「PostgreSQL と違う」と明記したもの。ID は固定する（ファイルのコメントが参照する）。 |
| `spec/design/m4/10-explain-copy-compat.md:2079` | \| `explain/deparse_plan.slt` \| §4.9 の `Plan` の表: `EXPLAIN (COSTS OFF) SELECT * FROM t WHERE <式>` の `… |
| `spec/design/m4/10-explain-copy-compat.md:2325` | - **[10-Q9] 混合幅の整数演算子の有無で、式の表示が変わる**。仮決め: 章 09 が `int2`/`int4`/`int8` の混合幅の比較・算術演算子を持つ前提で、`bi > 5` は… |
| `spec/research/m2-page-heap.md:458` | 2. データチェックサムは常に有効（PostgreSQL と違い無効化できない）。アルゴリズムは PostgreSQL と同じ FNV-1a 派生。 |
| `spec/research/m5-types-fk.md:720` | - **C-8 FK のトリガー**: PG は FK を内部トリガーで実装しますが、yuzhu はトリガーを作らず実行器に組み込みます。`pg_trigger` に行が出ない点が PG との差分にな… |
| `spec/research/pg-compat-tools.md:176` | **要点**: 6 と 8 は中身が空でも**解析を通る**必要がある。PostgreSQL は、テーブルが空でも式の型検査をするので、yuzhu でも配列型の列と演算子が解析できないと失敗する。抜け… |
| `spec/research/m4-query.md:737` | - **M4Q-2 numeric の前倒し**: `sum(bigint)` と `avg(整数)` の結果型が numeric のため、numeric の最小核（演算・入出力・キャスト・小数リテラ… |
| `spec/research/m3-tx-semantics.md:54` | 9. **SET はトランザクショナル**（ROLLBACK や暗黙トランザクションの失敗で元に戻る）。SET LOCAL はトランザクション終了で戻る。**ParameterStatus は Rea… |

合計 21 行。
<!-- AUTO:pgdiff END -->

| 差分 | 症状 | 出所 | 解消予定 M |
|---|---|---|---|
| TOAST なし | 1 行が約 8160 バイトを超えると `54000 row is too big` | M2-Q11（m2.md:2137） | 不明（設計書に解消予定の記載なし） |
| カタログへの DML 禁止 | `pg_class` などへの INSERT/UPDATE/DELETE は `42501 permission denied for table pg_class` | M2-Q12（m2.md:2138） | 不明（解消予定の記載なし） |
| `pg_get_expr` が非正規形 | DEFAULT / CHECK がユーザー入力のまま（`(a > 0)` にならない） | M2-Q9（m2.md:2135） | M4 |
| DROP が SELECT を待たせない | 他セッションは古い定義でテーブルを読み続ける。DROP のコミットは実行中の文の終了を待つ | M2-Q14（m2.md:2140） | M5（リレーション単位のロック導入時） |
| ACL 列が常に NULL | `datacl` など。`\l` の Access privileges が空 | M2-Q13（m2.md:2139） | GRANT/REVOKE は M6 以降（CLAUDE.md） |
| `pg_authid` を全員が読める | PG では一般ユーザーは読めない | M2-Q13 (6)（m2.md:2139） | M5 |
| `pg_proc.prolang = 12` が dangling | `pg_language` を作らない | M2-Q13 (5)、R2 | M4 で `pg_language` を作る方針（m2.md の R2 の記述） |
| システム列が PG と一致しない | `ctid`、`xmin` を slt で使わない。`xmin`/`xmax` は下位 32 ビット | M2-Q19（m2.md:2145） | 解消しない（D2 の帰結） |
| FSM なし | 削除領域は VACUUM まで再利用されない | M2-Q17（m2.md:2143） | M5（VACUUM） |

## 7. 追加依存と規約の整合

<!-- AUTO:deps BEGIN -->
_生成: 2026-10-05 07:32:11 UTC。出所: `git log -p -- '*Cargo.toml' '*Cargo.lock'`（コミット済み）と `git diff HEAD`（未コミット）。追加行（+）のみ。内部クレート yuzhu-* と package メタデータは除く。_

| 状態 | コミット | ファイル | 追加された行 |
|---|---|---|---|
| コミット済み | 5e42f81 | impl/rust/Cargo.toml | `clap = { version = "4", features = ["derive"] }` |
| コミット済み | 5e42f81 | impl/rust/Cargo.toml | `serde = { version = "1", features = ["derive"] }` |
| コミット済み | 5e42f81 | impl/rust/Cargo.toml | `toml = "0.9"` |
| コミット済み | 5e42f81 | impl/rust/Cargo.toml | `tracing = "0.1"` |
| コミット済み | 5e42f81 | impl/rust/Cargo.toml | `tracing-subscriber = "0.3"` |
| コミット済み | 5e42f81 | impl/rust/Cargo.toml | `postgres = "0.19"` |
| コミット済み | 5e42f81 | impl/rust/crates/yuzhu-server/Cargo.toml | `clap.workspace = true` |
| コミット済み | 5e42f81 | impl/rust/crates/yuzhu-server/Cargo.toml | `serde.workspace = true` |
| コミット済み | 5e42f81 | impl/rust/crates/yuzhu-server/Cargo.toml | `toml.workspace = true` |
| コミット済み | 5e42f81 | impl/rust/crates/yuzhu-server/Cargo.toml | `tracing.workspace = true` |
| コミット済み | 5e42f81 | impl/rust/crates/yuzhu-server/Cargo.toml | `tracing-subscriber.workspace = true` |
| コミット済み | 5e42f81 | impl/rust/crates/yuzhu-server/Cargo.toml | `postgres.workspace = true` |
| コミット済み | 5e42f81 | tests/tools/difftest/Cargo.toml | `clap = { version = "4", features = ["derive", "env"] }` |
| コミット済み | 5e42f81 | tests/tools/difftest/Cargo.toml | `postgres = "0.19"` |
| コミット済み | 5e42f81 | tests/tools/isolation/Cargo.toml | `postgres-protocol = "0.6"` |
| コミット済み | 5e42f81 | tests/tools/isolation/Cargo.toml | `bytes = "1"` |
| コミット済み | 5e42f81 | tests/tools/isolation/Cargo.toml | `fallible-iterator = "0.2"` |
| コミット済み | 5e42f81 | tests/tools/isolation/Cargo.toml | `clap = { version = "4", features = ["derive", "env"] }` |
| コミット済み | 5e42f81 | tests/tools/isolation/Cargo.toml | `similar = "2"` |
| コミット済み | 610c121 | impl/rust/Cargo.toml | `proptest = "1"` |
| コミット済み | 610c121 | impl/rust/Cargo.toml | `signal-hook = "0.3"` |
| コミット済み | 610c121 | impl/rust/crates/yuzhu-core/Cargo.toml | `proptest.workspace = true` |
| コミット済み | 610c121 | impl/rust/crates/yuzhu-server/Cargo.toml | `signal-hook.workspace = true` |
| コミット済み | 5e42f81 | Cargo.lock | 272 件の新規パッケージ: anstream,anstyle,anstyle-parse,anstyle-query,anstyle-wincon,async-trait,base64,bitflags,block-buffer,bumpalo,byteorder,bytes,cfg-if,chacha20,clap,clap_builder,clap_derive,clap_lex,cmov,colorchoice,const-oid,cpufeatures,crypto-common,ctutils,digest,equivalent,fallible-iterator,futures-channel,futures… |
| コミット済み | 610c121 | Cargo.lock | 27 件の新規パッケージ: autocfg,bit-set,bit-vec,errno,fastrand,fnv,getrandom,linux-raw-sys,num-traits,ppv-lite86,proptest,quick-error,r-efi,rand,rand_chacha,rand_core,rand_xorshift,regex-syntax,rustix,rusty-fork,signal-hook,signal-hook-registry,tempfile,unarray,wait-timeout,zerocopy,zerocopy-derive |

注: 依存の承認状況（CLAUDE.md「依存追加は慎重に」との関係、M2-Q6 など）は手書き欄で扱う。Cargo.lock の行は推移的依存を含む。
<!-- AUTO:deps END -->

手書きの整合確認:

| 依存 | 対象 | 規約との整合 | 状態 |
|---|---|---|---|
| `signal-hook` 0.3 | yuzhu-server（本番） | 「周辺用途のみ」の列挙に無い。「コア部品は手書き」の対象（パーサ、ストレージ、WAL、トランザクション、ワイヤプロトコル）ではない。ワークスペースの `forbid(unsafe_code)` は自分のクレートだけにかかり、依存先の内部実装には及ばない（一般知識。signal-hook 内部が unsafe を使うかは未確認） | M2-Q6 で明示。ユーザー未追認 |
| `proptest` 1 | yuzhu-core の dev-dependencies | テスト用途で規約に合う。m2.md:223 で明示的に許可 | 整合 |
| yuzhu-core の本番依存 | - | 外部依存なしを維持（m2.md:221） | 整合（610c121 時点で yuzhu-core の本番依存の追加なし） |

## 8. 「未検証」の追跡

<!-- AUTO:unverified BEGIN -->
_生成: 2026-10-05 07:32:11 UTC。出所: `grep -c 未検証`（行数ベース）。前回値は本ブロック内のコメント行に保存している。_

| ファイル | 現在 | 前回 | 差分 |
|---|---|---|---|
| spec/design/m2.md | 35 | 不明 | 不明 |
| spec/design/m3.md | 10 | 不明 | 不明 |
| spec/research/m2-buffer-io.md | 9 | 不明 | 不明 |
| spec/research/m2-dml-exec.md | 5 | 不明 | 不明 |
| spec/research/m2-page-heap.md | 6 | 不明 | 不明 |
| spec/research/m3-mvcc.md | 2 | 不明 | 不明 |
| spec/research/m3-tx-semantics.md | 8 | 不明 | 不明 |
| spec/research/m4-btree.md | 2 | 不明 | 不明 |
| spec/research/m4-query.md | 6 | 不明 | 不明 |
| spec/research/m5-concurrency.md | 21 | 不明 | 不明 |
| spec/research/m5-types-fk.md | 2 | 不明 | 不明 |
| spec/research/pg-compat-tools.md | 25 | 不明 | 不明 |
| spec/research/research-slt.md | 1 | 不明 | 不明 |

合計 132 行（0 件のファイルは、過去に記録がある場合を除き省く）。

<!-- unv spec/design/m2.md 35 不明 -->
<!-- unv spec/design/m3.md 10 不明 -->
<!-- unv spec/research/m2-buffer-io.md 9 不明 -->
<!-- unv spec/research/m2-dml-exec.md 5 不明 -->
<!-- unv spec/research/m2-page-heap.md 6 不明 -->
<!-- unv spec/research/m3-mvcc.md 2 不明 -->
<!-- unv spec/research/m3-tx-semantics.md 8 不明 -->
<!-- unv spec/research/m4-btree.md 2 不明 -->
<!-- unv spec/research/m4-query.md 6 不明 -->
<!-- unv spec/research/m5-concurrency.md 21 不明 -->
<!-- unv spec/research/m5-types-fk.md 2 不明 -->
<!-- unv spec/research/pg-compat-tools.md 25 不明 -->
<!-- unv spec/research/research-slt.md 1 不明 -->
<!-- AUTO:unverified END -->

突き合わせ欄（後で判明した結果を書く。外れた前提は setbacks.md にも転記する）。

| 項目 | 出所 | 判明した結果 | 判明日 (UTC) | 前提は外れたか |
|---|---|---|---|---|
| `xid8` / `pg_snapshot` の型 OID（5038、5069） | m3-mvcc.md:533（§12 未検証） | 未突合。2026-10-04 15:30Z 時点で sandbox の PG17（127.0.0.1:55432）は未起動のため確認していない | - | 不明 |
| SnapshotAny 相当の可視性（`get_new_oid`） | m2.md:1119、1939 | 未突合。M2 の A-foundation 以降の実装 / Review で判明次第追記 | - | 不明 |
| `pg_get_userbyid` の存在しない OID の文言（`unknown (OID=N)`） | m2.md:1973 | 未突合。slt に入れる前に PG で確かめる、と設計書にある | - | 不明 |
| DML 拒否の文言（`aclcheck_error` 由来） | m2.md:1915（R13） | 未突合。`aclcheck_error` の語は設計書内に見つからず、文言は `42501 permission denied for table pg_class` として記載 | - | 不明 |
| pg_type の OID と分類値（記憶に基づく） | m2.md:93、118 | 未突合。tests/slt/m2/catalog の `pg_type.slt` は PG17 で 24/24 通過（K-m2-tests の報告）。ただしこの結果が pg_type の値を網羅するかは未確認 | 2026-10-04（K-m2-tests 報告） | 外れていない（PG 側の slt が通った範囲に限る） |
| `File::try_lock` の版数（Rust 1.89 で安定化） | m2.md:221 | 未突合。`rust-version = "1.96"` なので版数の問題は無い（Cargo.toml の履歴より） | 2026-10-04 | 外れていない（実質無関係） |

## 9. M2 実装中の決定ログ

設計書に無い決定や追加の仮決めを、Workflow の result と issues から拾って、Workflow 終了のたびに日付見出しで追記する。

### 2026-10-04

- 状況 (15:16Z 時点): M2 Workflow wf_0cc0a7be は A-foundation の 1 担当だけが起動済み（開始 14:50:14Z）で、**進行中。result は無い**。したがって実装中に生じた決定はまだ拾えない。
- 設計書に無かった事実（実装の先行分。610c121 は 2026-10-04 15:19:58Z のコミット）:
  - ワークスペースに `proptest = "1"` と `signal-hook = "0.3"` が追加された（610c121 `feat(core): M2 の基盤…`、`git log -p impl/rust/Cargo.toml`）。どちらも m2.md が許容 / 明示した依存であり、設計外の追加依存は無い。
  - `catalog/memory.rs`、`storage/memory.rs`、`txn.rs` が削除された（作業ツリーの状態。コミット有無は未確認）。M1 の undo 方式の撤去で、D1 の実装に当たる。
- M1 Workflow（wf_fabaf03d）の result から拾った決定:
  - `assert_is_empty = "allow"` の追加（第 1 節に記載）。
  - 統合担当は「コミットは b8c7ea3 と、その後の PROGRESS.md 修正 1 件」と報告した。`git cat-file -t b8c7ea3` は `commit` を返すが `git log`（現ブランチ）には出ない。報告と実状態の食い違いの可能性は setbacks.md で扱う。
  - K-m2-tests は `sandbox/pg.sh` に `restart` を追加した（担当外の変更、最小限）。コンテナ内で docker が使えないため、と報告。
- M1 契約逸脱ログ（C-01〜C-16）の増減: 今日の時点で変化なし。
