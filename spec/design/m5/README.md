# yuzhu M5 基本設計（実用化）

M5 のゴールは**実用化**です。複数の接続が同時に書き込み（行ロック・テーブルロック・デッドロック検出・Repeatable Read）、ドライバが既定のモード（Extended Query）で繋がり、パスワード認証（SCRAM-SHA-256）で守られ、複数のデータベースを持ち（CREATE / DROP DATABASE）、VACUUM で膨らみ続ける状態から抜けます。あわせて、型（numeric・日付時刻・char(n)・bytea・uuid・配列）と FOREIGN KEY を足します。PostgreSQL 17 が正解です。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 前提: `spec/design/m1.md`・`m2.md`・`m3.md`・`m4/`、調査 `spec/research/m5-concurrency.md`・`m5-protocol-auth.md`・`m5-types-fk.md`・`m3-mvcc.md`・`pg-compat-tools.md`
- 章の書式は M3 の設計書に倣う（決定、範囲、構成、ディスク形式、契約、処理の流れ、モジュール仕様、テスト、実装の分担と工数、未検証の点、確認事項、契約への変更依頼）。

## 章の一覧

| 章 | ファイル | 略号 | 主題 | 工数（日） |
|---|---|---|---|---|
| 00 | `00-contracts.md` | — | 契約（共通の型・ID・モジュール構成・処理の流れ・WP・ファイルの持ち主・取り込み台帳）。F0 / F1 | F0 6.5、F1 3 |
| 01 | `01-lock-txn.md` | LK | トランザクション基盤の再構成、ロックマネージャ、テーブルロック、デッドロック、観測 | 13.5 |
| 02 | `02-row-lock-rr.md` | RW | 行ロック、更新競合、MultiXact（簡易版）、EvalPlanQual、Repeatable Read、B+Tree の複数ライター、RETURNING | 19 |
| 03 | `03-vacuum.md` | VC | VACUUM、pruning、FSM、凍結、clog の切り詰め、TRUNCATE、autovacuum | 20 |
| 04 | `04-extended-query.md` | XQ | Extended Query、パラメータ型推論、準備済み文、バイナリ形式の仕組み、COPY（任意） | 21.5 |
| 05 | `05-types-core.md` | TY | 型の枠組み、numeric・bpchar の穴埋め、bytea、uuid、配列、全型のバイナリ形式 | 17.5 |
| 06 | `06-types-datetime.md` | TD | interval、time、TimeZone / DateStyle / IntervalStyle、日時の関数とバイナリ | 13.5 |
| 07 | `07-foreign-key.md` | FK | FOREIGN KEY（実行器に組み込んだ RI） | 17 |
| 08 | `08-auth-roles.md` | AU | SCRAM-SHA-256、`yuzhu_hba.conf`、ロールの DDL | 9 |
| 09 | `09-database-ddl.md` | DB | CREATE / DROP / ALTER DATABASE、接続の検査 | 10.5 |
| 10 | `10-tests-plan.md` | TS | テスト基盤、全体の工程、リスク、既知の差の台帳、未検証の総覧 | 21.5 |
| 98 | `98-review-response.md` | — | レビュー対応（指摘の実在確認、反映先、却下の理由、実機で確認した事実） | — |
| 99 | `99-questions.md` | — | 確認事項（仮決めの一覧。通し番号 M5-Q1〜Q161） | — |

## 読む順

1. **00 §1**（M5 の範囲、食い違いの決定 D1〜D51、工程）→ **00 §4〜§5**（契約と処理の流れ）→ **00 §6.3・§7・§8**（取り込み台帳、WP、持ち主）。
2. 基盤: **01（LK）** → **02（RW）** → **03（VC）**。クリティカルパスの上流。
3. 利用側: **07（FK）**（02 の行ロックと EPQ、03 の TRUNCATE 連携の上に載る）、**04（XQ）**（01・02 のスナップショットとロックの順序に従う）。
4. 型: **05（TY）** → **06（TD）**（バイナリ形式は 04 §4.4 と 00 §4.9 の形に統一）。
5. 認証・データベース: **08（AU）** → **09（DB）**（接続の検査順は 08 §5.1 と 09 §5.1 が同じ）。
6. **10（TS）**（テスト、工程、リスク、既知の差）→ **99**（確認事項）→ **98**（レビュー対応）。

## 実装の分担と工数の要約

**M5 の合計は約 172.5 日**（AI の実装エージェント 1 本の稼働日。初版の 158 日に、F0 の拡大 +4、M4 のファイルへの修正の F1 +3、XQ-2 +1、TY-5 +1.5、DB-3 +0.5、TS の見直し +4.5 を足した）。**クリティカルパスは 30 日（6 週間）**: F0a → F0b-1 → LK-1 → LK-3 → RW-1b/1c → RW-3c → FK-2 → FK-4 → 結合・安定化。平均の並列度は約 6、ピークは 10〜13 本（10 章 §8.3）。

| 段階 | WP | 備考 |
|---|---|---|
| 基盤 | F0a（2.5）→ F0b-1（1.5）→ F0b-2（2.5）、F1（3） | F0 は M4 のマージ後。F1 は M4 のファイルへの M5 の修正を 1 人で行う（00 §8.2） |
| 先行できる | AU-1、TD-4 のクレート側、LK-1・LK-2、TS-1a・TS-4a・TS-5a | 新規ファイル・新規クレートだけ（D49）。XQ は含めない |
| ロックと行 | LK-1〜LK-5、RW-1〜RW-6 | RW-1a・1d・RW-2 は LK の裏で進める |
| VACUUM | VC-1〜VC-6 | VC-3 は RW-5 の後に終わる。VC-6（autovacuum）はカットライン上位 |
| プロトコルと型 | XQ-1〜XQ-7、TY-1〜TY-7、TD-1〜TD-5 | TY-5 は SELECT 句の SRF の最小対応（TY-5c）を含む |
| 制約・認証・DB | FK-1〜FK-5、AU-1〜AU-3、DB-1〜DB-4 | |
| テスト | TS-1〜TS-5（枝番 a・b） | ストレス（S1 の RC）は RW-3・RW-5 の後 |

**カットライン**（遅れたら M6 に回す順）: VC-6 → XQ-6（COPY TO）→ TD-4（time）→ VC-4 の clog の切り詰め → VC-5 の末尾の切り詰め。TY-5 の `ARRAY(SELECT)` と添字、TY-5c は psql の `\d tbl` に要るので落とさない。

## 主な設計判断（要約）

- 複数ライター: グローバル書き込みロックを廃止し、8 モードのリレーションロック・XID ロック・行ロック（xmax の 4 強度）。MultiXact はメモリ上だけ（Q-014）。
- スナップショット: 登録して horizon を決める。カタログ用も登録（短命）。RR の最初のスナップショットはロックの前。
- VACUUM: 3 段階、機会的 pruning、FSM、常に horizon まで凍結、clog の切り詰め（`yz_relxid` / `yz_datxid`。template0 は番兵）、行ポインタの再利用。
- Extended Query: `msg_*` を 1 メッセージ 1 メソッド、解析結果は鍵でキャッシュし計画は Bind ごと、バイナリは `binary_send` / `binary_recv`。
- FK: 実行器に組み込んだ RI、親の行に `FOR KEY SHARE`、文の終わりに検査。
- 認証: SCRAM-SHA-256（`yuzhu-auth`）、hba は部分集合、`CREATE ROLE` の属性とパスワード。
- データベース: FILE_COPY 相当、`DBASE` の WAL、`Database(oid)` ロックで接続と排他。

## ユーザーの確認が要る主な仮決め

99-questions.md の §1 に優先順で挙げた。SAVEPOINT を入れない（Q1）、MultiXact の簡易版（Q4）、Extended Query の解析キャッシュ（Q17）、SELECT 句の SRF の最小対応（Q18）、numeric の `sqrt` が `0A000`（Q19）、工数と F0 / F1 の分割（Q21）、配列のディスク形式と `int2[]` の置き換え（Q8・Q15）、`ALTER DATABASE`（Q140）。
