# 引き継ぎ: m3.md のレビュー対応（中断時点のメモ）

作成: 2026-10-04。中断時点で **`spec/design/m3.md` はまだ一切編集していない**（読み込みと方針決めまで完了）。

## タスク

- `spec/design/m3.md` を 3 本のレビュー（正しさ・クラッシュ安全性 / PostgreSQL 互換性 / safe Rust 実装可能性と並列契約）に基づいて改訂する。
- 末尾に「レビュー対応」節を足し、各指摘 → 対応（または却下と理由）を表で対応付ける。
- 制約: m3.md 以外（コード、テスト、他の設計書、m3-changes.md、PROGRESS.md）は編集しない。コミットしない。他エージェントが並行してコードを実装中。
- 文体: 日本語、m1.md / m2.md と同じ契約の精度（D 番号の決定表、バイト表、Rust シグネチャ、処理の流れ、担当表は**ファイルが重ならない**こと、工数、確認事項 M3-Qn）。
- 完了時の返答: 日本語 10 行の要約（重要な確認事項を含む）。

## 指摘の識別子

- 正しさレビュー: C1〜C3（重大）、H1〜H4（高）、M1〜M9（中）、低 4 件
- PG 互換レビュー: PA1〜PA7、PB1〜PB8、PC1〜PC5（元の番号 A/B/C に「P」を付けて区別）
- 実装可能性レビュー: XA1〜XA8、XB（derive/lint 表）、XC1〜XC17、XD1〜XD6、XE1〜XE5、XF（元の番号に「X」を付けて区別）

## 対応方針（決定済み。この通りに m3.md を書き換える）

### WAL・リカバリ（正しさ）
- **C1 / XA2**: `run_redo` は `wal.note_read(rec.end)`（`note_replayed` を改名）を **dispatch の前**に呼ぶ。§4.3、§5.5 d-6、§6.3.6、§6.4.1 を修正。§7.4 に「2〜8 フレームのプールで別ページ UPDATE / 2 ブロック FPI を REDO しても Panic しない」を追加。
- **C2 / XA4**: 
  - (a) `decode_record` は「長さ → CRC → prev → 意味検査」の順。`RecordError::is_end_of_wal()` を足し、TooShort/BadLength/BadCrc/BadPrev は終わり、BadRmgr/BadInfo/BadBlockRef/BadCombination は **FATAL XX001 "invalid WAL record at %X/%X: ..."**（finish_recovery を呼ばない）。rmgr 固有の組み合わせ検査は各 rmgr の持ち主に `validate(rec)` を置き、`recovery::dispatch` が REDO 前に呼ぶ（M5 / XC11 の組み合わせ: HEAP_INIT_PAGE と WILL_INIT の一致、INSERT/DELETE は nblocks==1、UPDATE は 1〜2 で SAME_PAGE と一致、DELETE に INIT_PAGE 不可、INIT_PAGE+SAME_PAGE 不可、XLOG FPI は全ブロック HAS_IMAGE）。
  - (b) `finish_recovery` は、end より番号の大きいセグメントについて「ヘッダが不正」または「先頭レコード位置（seg+32）の 32 バイトが 0 でない」なら FATAL XX001 "WAL contains data after end of WAL at %X/%X"、何も変えずに止める。前提として §6.3.3 に W1 契約「セグメント N の sync_data が終わるまで N+1 に 1 バイトも書かない」を明記。
  - 検査を通った後続セグメントは削除ではなく `pg_wal/<name>.discarded` に rename。`remove_segments_before` が segno < REDO 点のセグメントの `.discarded` も消す。D19 を改訂、新決定として記録。
- **C3**: WAL バッファの不変条件「`buf_base + buf.len() == insert_pos`」を明記。正規化で飛んだら `[end, seg_end)` の 0 と次セグメントのヘッダ領域 32 バイトのプレースホルダを足す（§6.3.2 の疑似コードを書き直す）。§7.4 に「残り 32 未満で終わった直後に flush して読み戻す」。
- **XA1 / M3 の一部**: `Wal::open_at(vfs, cfg, insert_pos, prev, redo: Lsn)`、`finish_recovery(end, last_start, redo: Lsn)`、`open_for_recovery(..) -> Result<Arc<Wal>>`、`WalConfig.sync: bool`（--no-sync 用）。§5.5 c / d-9 で ckpt.redo を渡す。テスト「clean 再起動 → 既存ページを 1 回更新 → torn → 復旧」。
- **H1**: `log_and_truncate` は「呼び出し側が排他ストレージバリアを持つ → コミットゲート共有 → 挿入・flush → `drop_relation_buffers_from`（I/O 中のフレームは完了を待つ）→ truncate・sync_data → ゲート解放」。層の都合でゲートは **`Wal` に置く**（`Wal::gate_shared()` / `gate_exclusive()`。W1）。`TxnManager::commit_gate_exclusive` は wal に委ねるだけにする。§3.8、§4.5、§5.3、§5.6、§5.9、§6.5.2、§6.6.4 を直す。
- **H2 / XC1**: `wal/dump.rs` はヘッダとブロック参照の汎用整形のみ（`&dyn Fn(&DecodedRecord)->String` を受ける）。各 rmgr の持ち主に `pub fn desc(rec) -> String`、振り分けは `recovery::describe`（R）。waldump はそれを使う。§2 図・§8 表を修正。
- **H3**: 制御ファイルのスロット有効性は magic + CRC のみで判定、採用後に format_version を検査（M2 §3.2 の上書き。§3.10・§4.11 に記載し、m3-changes への記録を依頼と書く）。
- **H4**: XACT オフセット 12 を `xinfo u32`（M3 は 0、未知ビット不正、付加部はフラグ順に rels の後ろ）。ブロック参照の画像ヘッダを 8 バイトに（24 hole_offset u16、26 hole_length u16、28 image_len u16、30 bimg_info u8（M3 は 0）、31 予約、32〜画像）。チェックポイントレコードを **64 バイト**（後ろ 24 バイト予約、0 必須）。HEAP DELETE/UPDATE の予約欄は「M3 は 0、0 以外は不正」と明記。**タイムラインは却下**（セグメントヘッダ 32 バイトが埋まっており、SEG_HEADER_SIZE 変更の波及が大きい。M2-D15 により M6 で形式版数を上げて対応。確認事項に記載）。
- **M1 / XC12**: clean = `state == ShutDown && kind == Shutdown && 終わりの理由 ∈ {ZeroLength, MissingSegment}`。それ以外はクラッシュリカバリ。
- **M2**: REDO 最初のレコードは CHECKPOINT_REDO（オンライン）か、redo == checkpoint_lsn のチェックポイントレコード自身。違えば FATAL。
- **M4 / XC8**: `read_buffer_zeroed` はプールにあればディスクを読まず既存フレームを返し 8192 バイト全体を 0 で上書き、I/O 中なら完了を待つ。`init_heap()` はページ全体を 0 にしてから初期化。
- **M6**: D17 の根拠を「番号 < upto の残骸は、コミット / アボートレコードが REDO 点より前」に書き換え。
- **M7**: D15 / M3-Q5 の文言を「中身のあるファイルが孤児として残る場合がある（PG と同じ）」に。
- **M8**: §5.5 の「データディレクトリは変えない」→「REDO は冪等で、失敗しても再実行できる」。
- **M9**: REDO 一致テストは pd_checksum と穴を除いて比較。
- **低**: セグメント先行作成は確認事項へ。`begin_checkpoint_online` 経路でも閾値 flush は Mutex 外と §6.3.4 に明記。再起動直後の「前回チェックポイントの end」を初期化（XC4 と合わせて `StartupOutcome.checkpoint_end`、`CheckpointParts.last: &Mutex<LastCheckpoint>`、`checkpoint::due()` は E）。§7.3 に「排他バリア待ちは is_blocked で報告しない」注記。

### PostgreSQL 互換
- **PA1**: Failed での AND CHAIN は**既定の特性**で新ブロック、タグ ROLLBACK。（未検証）と §9 項目を削除、chain.slt に追加。
- **PA2**: SET TRANSACTION ISOLATION の 25001 は**現在値と異なる場合のみ**。READ WRITE への変更のみ 25001。slt は READ UNCOMMITTED で 25001、READ COMMITTED 再設定は成功。
- **PA3**: DML はアナライズ・型付け後、ライターロック前に 25006。CREATE/DROP TABLE はパース木段階。読み取り専用のアナライズはロック不要と §5.2 に。read_only.slt に `INSERT INTO nosuch` → 42P01。
- **PA4**: 独自ランナーは Simple Query で pid をリテラル埋め込み。
- **PA5**: `pg_cancel_backend(int4) → bool` を追加（OID 2171、strict、volatile、parallel s）。見つからなければ f と WARNING 01000 "PID %d is not a PostgreSQL backend process"。実装は Cluster に pid → Weak<InterruptFlag> の登録表（R）、Session が登録（S）、RuntimeInfo に `cancel_backend` と警告出力。J の CancelRequest も秘密鍵照合後これを使う。spec は setup で pids 表に pid を書く。
- **PA6**: `idle-in-tx-timeout.spec` は削除し J の Rust 結合テストへ。
- **PA7**: `transaction_timeout` を受け付け・SHOW（既定 0）。0 以外は同じ期限の仕組みで実装（FATAL 25P04 "terminating connection due to transaction timeout"、文言は未検証）。§1.2 の 42704 記述を削除。
- **PB1**: pg_backend_pid は stable / parallel r。pg_sleep、is_blocked は v / s / strict。OID 確認済み、§9 から削除。
- **PB2**: _int4 行（typname _int4、typlen -1、typbyval f、typtype b、typcategory A、typalign i、typstorage x、typelem 23、array_in/array_out、typsubscript 0）。format_type → integer[]。
- **PB3**: 構造不正 `22P02 malformed array literal: "{1,2"` DETAIL `Unexpected end of input.`、要素不正 `22P02 invalid input syntax for type integer: "a"`。引用符要素 `{"3"}` は受け付ける。
- **PB4**: void の入力は任意の文字列を受け付け Datum::Void。
- **PB5**: SHOW のみ設定への SET は 55P02、文言 3 種（sighup / internal / postmaster）。
- **PB6**: synchronous_commit は正規化して表示（true → on）、不正値 22023 + HINT `Available values: local, remote_write, remote_apply, on, off.`。
- **PB7**: 暗黙ブロック内の SET TRANSACTION はその暗黙トランザクションに適用（WARNING なし）。
- **PB8**: PREPARE TRANSACTION は max_prepared_transactions=0 と同じ挙動（ブロック内 55000 + HINT で終了、外は WARNING 25P01 + タグ ROLLBACK、Failed はアボートしてタグ ROLLBACK）。§5.2 b の許可リストに明記。
- **PC1**: D4・§3.3・Q3 の根拠文言を「PG（33 = XLR_MAX_BLOCK_ID + 1）より 1 少ない」に。
- **PC2**: 割り込みの優先順位を「停止 → 期限 → キャンセル」に。
- **PC3**: ログ文言を PG17 に（`could not locate a valid checkpoint record at %X/%X`、`redo done at %X/%X system usage: ...`、クラッシュ時の 2 行）。`checkpoint record is inconsistent with control file` は yuzhu 独自と注記。
- **PC4**: WAL 書き込み失敗の SQLSTATE は M2 の `Error::from_io`（ENOSPC 53100、他 58030）で写す。
- **PC5**: statement_timeout.slt の void 空文字列は `(empty)`。
- 実機確認済み（PD 節）の点は §9 から外す。

### 実装可能性・契約
- **XA3**: `find_target(&self, rel, len, exclude: BlockNumber)`。exclude に当たれば extend。§5.1 の「N == O にならない」の根拠をこれにする。
- **XA5**: §2 規約に「I/O を発行する走査はキー順（BTreeMap かソート）」。smgr の pending_sync、clog dirty、sync_data_directory の列挙、pending_unlink。system_identifier と時刻はシード・注入可能な時計から（層 1）。
- **XA6**: 層 1 ワークロード制約: 書き込み中のセッションは同時に 1 つ、実行中のまま残すのはクラッシュ直前の最後のもののみ。
- **XA7**: set_lsn なし Drop の検査は `std::thread::panicking()` またはプール / クラスタの poison 時は行わない。M2 の既存テストの修正は C・D。
- **XA8**: poison フラグを一本化: `Arc<PoisonFlag>` を Wal・CriticalSection・Cluster で共有、`Cluster::is_poisoned()` はそれを見る。
- **XB**: RedoStats は Default を derive しない（run_redo が初期化）。RecordError に Clone。Page に Clone と手書き Debug（C）。RedoBuffer/WaitCtl/HeapDeleteMain/HeapUpdateMain/XactRecord/CheckpointParts/ClusterOptions/InitdbOptions に Debug。DebugKnobs に `#[allow(clippy::struct_excessive_bools)]`。cast lint の許可を `wal/*` 全体に広げる（他は usize::try_from）。
- **XC2**: 穴の計算は `wal/record.rs` の非公開関数（pd_lower/pd_upper アクセサのみ使用）。page.rs の hole_range は削除。
- **XC3**: `RedoCtx.ext` を廃止。`recovery::dispatch` は Arc<Clog> を捕捉したクロージャ、`txn::xact_wal::redo(ctx, rec, clog: &Clog)`。
- **XC5**: E に `checkpoint::bootstrap_shutdown(pool, smgr, clog, wal, next_xid, next_oid) -> Result<Inserted>`。§5.8 手順 4 はこれを呼ぶ。
- **XC6**: `ExecCtx.runtime` を足し、EvalCtx を組み立てる全箇所（executor/build.rs、nodes/*、DEFAULT/CHECK 評価）を A の範囲に。「FnKind::Runtime は定数畳み込みしない」。
- **XC7**: ctid は `max_offset()+1` で予測したバイト列を add_item し、戻り値一致を debug_assert。DELETE の blk0、UPDATE の blk1 は STANDARD。
- **XC9**: 規約 3 を「proc の Mutex を持ったまま insert も flush も呼ばない」に。
- **XC10**: リカバリモードの smgr.unlink は NotFound を Ok（C の契約）。
- **XC13**: M2 の決定参照に接頭辞（D17 行、§3.5、§6.4.4 の 2 か所、§6.10 の「D13」「D15」→ M2-D13 / M2-D15）。§4.4 コメント「e-3」→「d-6」。
- **XC14**: 不変条件 I1〜I12 の表を本文（§7.5）に転記（spec/research/m3-recovery.md 507〜518 行）。
- **XC15**: SimVfs: `nth` は `set_faults` 後の条件に合う操作の 1 始まり番号。`is_frozen()` 追加。ReadOnly ハンドルの sync_all もそのファイルの未 sync 書き込みを永続化（状態はファイル単位）。§7.5 の「N を 0 から」を 1 始まりに。
- **XC16**: アイドルタイムアウトは BufReader が空のときだけ設定、先頭 1 バイト後に外す。部分読み後の WouldBlock を区別。
- **XC17**: ライターロック待ちは `min(50ms, 期限までの残り)`。
- **XD1**: `yuzhu-core/src/testing.rs` は R、クラッシュ補助は T の `tests/crash_sim/*` のみ。
- **XD2 / XD3**: analyzer/planner/executor/{build,nodes}/catalog/{schema,store,cache,reader}/util は A の範囲（Datum 追加の網羅 match、EvalCtx）。A のスタブ作成で他担当ファイルに触るのは「最初の 1 回だけの例外」と明文化。
- **XD4**: pg_proc 行を追加: void_in 2298、void_out 2299、array_in 750、array_out 751、recv/send（void_recv 3120、void_send 3121、array_recv 2400、array_send 2401。未検証）、pg_cancel_backend 2171。
- **XD5**: 54000 の検査は S。
- **XD6**: 最長経路を A(1) → W1 本実装(3) → C(2.5) → D(2.5) → R(3) と書き直し、「W1 の空実装は型の雛形だけ」と注記。工数表も更新（S に transaction_timeout・cancel_backend で +0.5 など）。
- **XE1**: 既存 `write-skew-rr.spec` / `lost-update.spec` は target ごとのスキップ一覧（K）。ランナー既定の判定は関数（`--blocking-detection pg`、README 上は既定 pg）。
- **XE2**: DebugKnobs に `skip_commit_gate`、`dirty_on_drop`、`skip_sync_data_directory`。D13 は二段クラッシュ（KeepAll → リカバリ中 CrashFreeze → DropUnsynced → I1）で検証。
- **XE3**: グループコミットのテストは `FaultEffect::Delay` で sync を遅らせ決定的に。
- **XE4**: 追加テスト（XA1、C1、C2(b)、XA3、D31 の文ごと statement_timeout、XC16、info の不正組み合わせ）。
- **XE5**: isolation spec のタイムアウトは 1〜2 秒。

### 決定表に足す新しい D（D32 以降の案）
D32 意味不正レコードは FATAL / D33 transaction_timeout / D34 pg_cancel_backend / D35 PREPARE TRANSACTION / D36 READ ONLY 判定の段階 / D37 層 1 の決定性 / D38 log_and_truncate の排他とゲートを Wal に置く / D39 制御ファイルのスロット有効性 / D40 poison の一本化 / D41 waldump の desc の置き場所 / D42 形式の予約（xinfo、画像ヘッダ 8 バイト、チェックポイント 64 バイト、タイムラインは見送り）。D19 は改訂（後続データで FATAL、.discarded に rename）。

### 確認事項に足す案
- M3-Q23 後続セグメントにデータがあればリカバリを FATAL で止める（.discarded への rename）
- M3-Q24 層 1 の決定性のため I/O 順序に関わる走査はキー順
- M3-Q25 transaction_timeout を受け付けて実装
- M3-Q26 pg_cancel_backend を M3 で追加
- M3-Q27 PREPARE TRANSACTION を max_prepared_transactions=0 と同じ挙動に
- M3-Q28 READ ONLY の判定を DML はアナライズ後に
- M3-Q29 WAL 形式の予約（チェックポイント 64 バイト、画像ヘッダ 8 バイト、xinfo）とタイムライン見送り
- M3-Q30 次の WAL セグメントの先行作成（チェックポインタが作る案。M3 では入れない）
- M3-Q5 の文言修正

## 参考にした事実（M2 側）
- m2.md 302 行: スロット有効性に format_version を含む（H3 の根拠）
- m2.md 669 行: FaultRule.nth は 1 始まり
- m2.md 523 行: `Error::from_io`（NotFound→58P01、StorageFull→53100、他→58030）
- m2.md 776 行: CriticalSection は `Arc<PoisonFlag>` を持つ。1265 行: Cluster は `poisoned: AtomicBool`
- m2.md 1001/1003 行: ストレージバリアは `TxnManager::statement_barrier()` / `exclusive_barrier()`
- Xid（m2.md 933 行）は Default を derive していない。Page（1650 行）は derive なし
- tests/isolation/specs: lost-update.spec、rc-visibility.spec、write-skew-rr.spec
- tests/tools/isolation/README.md: `--blocking-detection pg|timeout`（既定 pg）

## 再開の手順
1. この文書と `spec/design/m3.md`（1775 行）を読む。
2. 上の方針どおりに m3.md を節ごとに Edit（§0.2 決定表 → §1 → §2 → §3 → §4 → §5 → §6 → §7 → §8 → §9 → §10）。
3. 末尾に「## 11. レビュー対応」節を追加し、上記の全識別子を「対応箇所 / 却下理由」の表で網羅する。
4. 終わったらこの引き継ぎファイルを削除してよい（m3.md 以外を残さない約束のため）。
