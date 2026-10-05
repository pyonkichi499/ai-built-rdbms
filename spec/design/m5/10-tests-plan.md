# yuzhu M5 基本設計 10: テスト基盤・全体の工程と工数・未検証と確認事項の総覧

M5 は「同時に書く・同時に繋ぐ・掃除する」を足すマイルストーンで、**壊れ方が時間と順序に依存する**。M1〜M4 の「同じ SQL を流して結果を比べる」だけでは、待ちの順序、デッドロック、VACUUM と読み取りの競合、クラッシュの瞬間の状態を確かめられない。この章は、M5 の正しさを確かめるテスト基盤（分離性ランナーの拡張、並行ストレス、クラッシュ試験の拡張、変異テスト、ドライバと互換の CI、SCRAM 構成の CI、性能の確認）と、M5 全体の工程・工数・リスク、全章の確認事項と未検証の点の総覧、既知の差の一覧、契約への変更依頼の集約をまとめる。

- 前提の契約: `spec/design/m5/00-contracts.md`（以下「00」）。**この章は 00 に従う**。従えない点は §11 に書く。章 01〜09 は並行して書かれていて、この章の執筆時点では 00 だけが読めた。**01〜09 の中身に依存する記述は 00 と調査（`spec/research/m5-*.md`）を根拠にし、章が確定したら突き合わせ直す箇所に「（突き合わせ待ち）」と付けた**。
- 前提の設計: `m3.md` §7（テスト）、`m4/00-contracts.md` §17・§18（M4 のテストの置き場所と担当 K、R2、Z）。
- 調査（根拠）: `m5-concurrency.md` §2（実機 24 件）・§15（テスト）、`m5-protocol-auth.md` §3・§8、`m5-types-fk.md` §9.7・§11、`pg-compat-tools.md` §3・§6、`m3-recovery.md` §9（不変条件 I1〜I12）。
- この章の略号は **TS**。決定の ID は `TS-D<n>`、確認事項は `M5-TS-Q<n>`（通し番号は §10.2）、既知の差は `KD-<n>`、未検証は `U-<n>`、リスクは `R-<n>`。
- 根拠の記号は 00 §9 のとおり: 【確認】ソースや実機で確かめた、【記憶】未照合、【提案】yuzhu への推奨。「（未検証）」はソースも実機も見ていない。
- 迷ったら PostgreSQL 17 と同じ。**PostgreSQL 17 が正解**で、`tests/` のテストは PostgreSQL 17 でも通ることを原則にする（CLAUDE.md）。

---

## 0. 決定

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| TS-D1 | 分離性ランナーのブロック判定 | `timeout`（M3 の暫定。`--block-timeout-ms`）／`pg_isolation_test_session_is_blocked`（isolationtester と同じ。m5-concurrency §15.1） | **既定を `pg`（関数）にし、yuzhu の CI も `pg` で流す。`timeout` は診断用に残す**。判定は `LockManager::is_blocked_by`（00 §4.2）に委ねる | M3 は書き込みロックの待ちだけを見ていたので `timeout` で足りた。M5 は行ロック・テーブルロック・XID ロックの待ちが混ざり、時間による判定は誤判定（遅いだけのステップを待ちとみなす）が増える。本家の spec をそのまま流すには関数が要る |
| TS-D2 | 制御接続の問い合わせ | Simple Query に pid を埋め込む（M3 のランナー）／本家と同じ `PREPARE` した文を `Bind` で呼ぶ | **本家と同じく Extended Query（`Parse` を 1 回、判定のたびに `Bind` / `Execute` / `Sync`）。`--blocking-detection pg` のとき** | isolationtester は `PQprepare` / `PQexecPrepared` を使う【記憶。m3 の README は「本家は Extended Query も使う」と記す】。XQ の最初の実地試験になり、ランナーの出力に影響しない |
| TS-D3 | ステップの送信方式 | Simple のみ（M3）／Extended も選べる | **`--protocol simple|extended`（既定 simple）を足す**。extended は、ステップの SQL を文に分け、各文を無名の `Parse` / `Bind` / `Execute` で送り、最後に 1 回 `Sync` する（libpq のパイプラインと同じ形）。PR のジョブは simple（必須）と extended（必須。共有 spec のみ）を流し、移植 spec の extended は夜間 | XQ-2 の暗黙のトランザクション・Sync・エラー後の読み捨てが、ロックの解放と組み合わさって正しいかを確かめる。同じ spec で両経路を比べられる |
| TS-D4 | M3 の分離性 spec の書き直し（D39） | 全部作り直す／期待ファイルを PostgreSQL 17 で再生成し、差分をレビューして直す | **後者。§6.3 の一覧。M3 の期待ファイルは PostgreSQL 17 が出したものなので、同じ行を更新する permutation の期待出力は変わらないはず。変わったら M3 の期待か yuzhu の挙動のどちらかが誤っている** | M3 の spec は「PostgreSQL と同じ結果になる組み合わせだけ」を選んで書いた（M3-Q20）。M5 で消える制限を spec から外し、制限のせいで書けなかった組み合わせを足す |
| TS-D5 | PostgreSQL 本体の spec の移植 | 全 100 本超を流す／選んで移植 | **選んで移植する（§6.4）。置き場は `tests/isolation/ported/`（共有の `specs/` とは分ける）。上流の commit を `UPSTREAM` に固定する。yuzhu の既知の差で期待が変わるものは `<name>.yuzhu-m5.out`（`--variant yuzhu-m5`）に分け、台帳 `tests/known-diffs.toml` の KD に紐づける。SERIALIZABLE・SAVEPOINT などの未対応機能のものは `skip.txt` に理由つきで挙げる** | 共有の `specs/` は「PostgreSQL と yuzhu で完全一致」の規則を保つ。移植分は本家の期待をそのまま使いつつ、差を台帳で管理する |
| TS-D6 | 既知の差をどう管理するか | README の散文／機械可読な台帳 | **`tests/known-diffs.toml`（KD の ID、種別（差 / 未対応）、説明、影響する spec・slt・ドライバのテスト、解消するマイルストーン）。`onlyif yuzhu` / `skipif yuzhu` と variant の `.out` は、台帳の KD を指すコメントを必須にし、CI の `lint-tests` が検査する（§6.6）** | M3 の「既知の差」は m3-tx-semantics §5.6 に散文で書いて、テストを書くたびに人が避けた。M5 は差が増える（KD-1〜KD-28）ので、避け忘れを機械で見つける |
| TS-D7 | 並行ストレスの置き場と対象 | `yuzhu-server/tests` に wire だけ／プロセス内の `Session` と wire の両方 | **両方。ワークロードは `yuzhu-core/src/testing/stress/`（対象に依存しない `StressTarget` トレイトの上）に置き、プロセス内（`TestCluster`）と wire（`postgres` クレート。yuzhu-server と本物の PostgreSQL 17）に同じものを向ける** | プロセス内は速く、失敗時に内部状態（`lock_status()`、`MultiXactTable`）を取れる。wire は PostgreSQL に向けて期待を確かめられる（CLAUDE.md の「PostgreSQL に対しても実行できる」） |
| TS-D8 | クラッシュ試験で複数セッションをどう動かすか | M3 の「1 スレッドで複数 `Session` を順番に操作」のまま／**ステップ駆動のアクタースケジューラ** | **後者（§5.3）。各セッションを専用スレッドに置き、スケジューラが「1 回に 1 ステップ」を投入して、「完了」か「`LockManager::is_blocked_by` で待ち」を観測してから次へ進む。待ちが解けて走り出したセッションが止まる（完了か再び待ち）のを確かめてから次を投入する** | M3 の方式は、待ちが起きた瞬間に唯一のスレッドが止まって自分でデッドロックする。上の方式は、I/O の通し番号（クラッシュ点 N）を決定的に保ったまま待ちを扱える |
| TS-D9 | 変異テストの対象 | M3 の 7 変異に足す | **M5 の「壊すと必ず別の層が検出するはず」の変異を 19 件足す（§6.9。名前は持ち主の章の定義に合わせた）。変異は `DebugKnobs` の隠しスイッチ（M3-Q13 と同じ規則: 既定は無効で、CLI・設定ファイル・SQL から変えられない）。各章が自分のスイッチを `DebugKnobs` に足し、TS が検出テストを書く** | 検出できない変異は、そのテストが無意味だという証拠になる |
| TS-D10 | ドライバと互換のテストの置き場 | `yuzhu-server/tests` に Rust だけ／`tests/compat/`（M4 が作る `psql/`、`pgbench/` の下に足す） | **`tests/compat/drivers/<名前>/`（言語ごとに独立したプロジェクト。`impl/rust` のワークスペースに入れない）、`tests/compat/auth/`、`tests/compat/pgbench/` の拡張。入口は `tests/compat/run.sh --target pg|yuzhu --suite drivers|auth|pgbench|psql`** | M4 の契約（§18）が `tests/compat/{run.sh,psql/,pgbench/}` を置く。言語ごとの依存（Maven・npm・uv）をワークスペースに持ち込まない |
| TS-D11 | 性能の数値目標 | M5 の絶対値の目標を決める／測定して記録し、退行にゲートを掛ける | **後者。測定項目と手順を決め、M4 のベースラインからの退行 10% 以内をゲートにする。絶対値は M5 の完了時に記録して M6 の目標にする** | 要件に M5 の数値目標がなく、AI の実装環境で絶対値を保証できない。退行は M4 の記録と比べられる |
| TS-D12 | 並行ストレス・クラッシュ試験の並行実行の検出 | `loom` などのモデル検査を使う／デバッグアサーションとストレスで見つける | **後者。ロックの順序違反と待つ前の保持（M5 規約 1）は `buffer/track.rs` のデバッグアサーション（00 §2 の規約 1）で検出する。`loom` は入れない** | 依存の追加は慎重に（CLAUDE.md）。順序違反は実行すれば debug ビルドで panic する |
| TS-D13 | 勧告ロック（`pg_advisory_lock`）の扱い | M5 に入れる／M6 | **M6。ただし契約に載っていない需要として §9 の U-21 と確認事項 `M5-TS-Q8` に挙げる**。ドライバの CI は Rails / Prisma / Flyway 系の `migrate` を対象にしない | `pg-compat-tools.md` §3.5・§3.6 が、Rails の `pg_try_advisory_lock`、Prisma の `pg_advisory_lock` を使うと書く（どちらも【未検証】）。00 は勧告ロックを範囲に挙げていない |

---

## 1. 範囲

**この章が作るもの**

| 分類 | 内容 | WP |
|---|---|---|
| 分離性ランナーの拡張 | `pg_isolation_test_session_is_blocked` による判定（既定）、制御接続の Extended Query、`--protocol extended`、`--timeout-scale`、`--variant` の台帳検査、`-- @wait` / `-- @cancel` の維持 | TS-1 |
| spec の書き直しと移植 | M3 の spec の書き直し（D39）、M5 の共有 spec の新規作成、PostgreSQL 本体の spec の移植（`ported/`）、`skip.txt` | TS-1 |
| 共有 SQL テストの規約と基盤 | `tests/slt/m5/` の構成と命名、`known-diffs.toml` と `lint-tests`、`run.sh` の `--protocol extended` と `--password`、`pg.sh` / `yuzhu.sh` の `--auth scram` | TS-5 |
| 並行ストレス | 送金の不変条件（RC / RR）、一意キーの競合、FOREIGN KEY、`FOR UPDATE SKIP LOCKED` のキュー、VACUUM との並行 | TS-2 |
| クラッシュ試験の拡張 | アクタースケジューラ、ワークロード 8〜11（VACUUM、MultiXact、FK の CASCADE、CREATE / DROP DATABASE）、不変条件 I16〜I24、層 2 の拡張、決定性の試験 | TS-3 |
| 変異テスト | M5 の変異 18 件と検出の規則 | TS-3 |
| ドライバと互換の CI | tokio-postgres、psycopg 3、pgJDBC、node-postgres、psql、pgbench（simple / extended / prepared / 同時接続）、SCRAM 構成 | TS-4 |
| 性能の確認 | 測定項目 P1〜P9 と退行ゲート | TS-2、TS-4 |
| 全体の工程 | WP 表の更新、クリティカルパス、並列度、カットライン、リスク登録簿、M4 との統合チェックリスト | この章 §8 |
| 総覧 | 全章の確認事項の通し番号、未検証の点、既知の差、契約への変更依頼 | この章 §9〜§11 |

**この章が作らないもの**: 各機能の単体テストと、その機能の共有 SQL テスト（各章が自分の `tests/slt/m5/<機能>/` に書く。00 §8）。ロックマネージャの性質テスト、B+Tree の構造検査器（`check.rs`。M4）、SCRAM の RFC 7677 のテストベクタ、`yuzhu-numeric` / `yuzhu-datetime` の差分コーパス試験（持ち主の章）。**この章はそれらが「実行される場所」（CI のジョブ）と「共通の部品」を持つ**。

**保証すること**: (1) 共有テスト（`tests/slt/m5`、`tests/isolation/specs`）は PostgreSQL 17 でも通り、yuzhu でも通る。(2) 通らないものは台帳（`known-diffs.toml`）に理由が載る。(3) M5 の完了条件（§7.3）を 1 つのコマンド列で確認できる。

---

## 2. 構成

`★` は M5 で新規、`△` は変更。

```
tests/
├── run.sh                     △ --protocol simple|extended、--password、--list（実行するファイルの一覧だけ出す）
├── pg.sh                      △ --auth trust|scram（POSTGRES_PASSWORD と pg_hba の方式）、PG_IMAGE（既定 postgres:17）
├── yuzhu.sh                   △ --auth trust|scram（yuzhu-initdb の --auth-host / --pwfile）、--hba FILE、--config K=V
├── known-diffs.toml           ★ 既知の差の台帳（KD-n。§10.3）
├── stress-seeds.txt           ★ 失敗したストレスのシード（回帰用。§5.4）
├── tools/
│   ├── isolation/             △ ランナーの拡張（§6.2）
│   │   └── port-pg-specs.sh   ★ 上流（REL_17_STABLE）から spec と期待出力を取得して ported/ に置く
│   ├── lint-tests.sh          ★ onlyif / skipif / variant と台帳の突き合わせ、テーブル名の接頭辞の重複検査（§6.6）
│   └── q-index.sh             ★ 設計書の確認事項（M5-<略号>-Q<n>）を集めて通し番号の表と突き合わせる（§10.2）
├── isolation/
│   ├── specs/                 △ 共有 spec（PostgreSQL と yuzhu で完全一致）。M3 の spec の書き直し（§6.3）と新規
│   ├── expected/              △ 期待出力（PostgreSQL 17 で生成）
│   └── ported/                ★ PostgreSQL 本体から移植した spec
│       ├── UPSTREAM           上流の commit と取得日
│       ├── specs/*.spec
│       ├── expected/*.out     PostgreSQL 17 の期待出力 + <name>.yuzhu-m5.out（差があるもの）
│       └── skip.txt           流さない spec と理由（KD-n）
├── slt/m5/                    ★ 共有 SQL テスト（§6.5）
├── restart/m5/                ★ 再起動・クラッシュをまたぐテスト（§6.5.3）
├── compat/                    M4 が作る（run.sh、psql/、pgbench/）。M5 は次を足す
│   ├── drivers/{tokio-postgres,psycopg,pgjdbc,node-postgres}/   ★ ドライバごとのシナリオ（§6.10）
│   ├── auth/{hba,roles,secret-compat}/                          ★ 認証の互換（§6.10.4）
│   ├── pgbench/                △ extended / prepared / 同時接続 / RR のスクリプトと不変条件
│   └── psql/extended/          ★ \parse \bind \bind_named、FK の \d、\du、\c、\password
└── perf/                      ★ run.sh（P1〜P9 の測定）と baseline.json（M4 の値。M5 の完了時の値）

impl/rust/crates/
├── yuzhu-core/
│   ├── src/testing/           △ testing.rs をディレクトリにする（F0）
│   │   ├── mod.rs                 TestCluster（M3 のもの。複数データベースの操作を足す）
│   │   ├── stress/                ★ StressTarget、ワークロード S1〜S6、チェッカー、ウォッチドッグ（§6.7）
│   │   └── actors.rs              ★ アクタースケジューラ（§5.3）
│   └── tests/
│       ├── crash_sim/             △ main.rs、workload.rs（W8〜W11 を足す）、invariants.rs（I16〜I24）、model.rs、mutation.rs（18 件）、actors を使う
│       │   └── determinism.rs     ★ 同じシードで I/O の列が一致することの試験
│       └── stress/main.rs         ★ プロセス内の並行ストレス（#[test]。PR は短時間、夜間は #[ignore] で長時間）
└── yuzhu-server/tests/
    ├── concurrency.rs         ★ wire 経由のストレス（同じワークロード。YUZHU_TEST_TARGET=yuzhu|pg）
    ├── common/wire_target.rs  ★ StressTarget の wire 実装（postgres クレート）
    └── crash_kill9.rs         △ 層 2（複数ライター、VACUUM、CREATE DATABASE のワークロード）

.github/workflows/ci.yml       △ ジョブの追加（§6.10.6）
```

**依存の方向**: `yuzhu-core/src/testing/` は `yuzhu-core` の公開 API（`Cluster`、`Session`、`LockManager`、`TxnManager`）だけを使う（`pub(crate)` に触らない）。M5 の変異のスイッチは `DebugKnobs`（`yuzhu-core/src/debug_knobs.rs`）に各章が足す（TS-D9）。`tests/tools/*` は `impl/rust` のワークスペースに入れない独立したプロジェクト（M3 のとおり）。

**章の規約（M5 のテストで追加）**:

1. **テストは時間でなく事象を待つ**: 「n ミリ秒後に起きているはず」と書かない。待ちは `is_blocked_by` / `lock_status()` / `pg_locks` の観測か、完了の通知で判定する。時間を使うのは `lock_timeout` / `deadlock_timeout` / `statement_timeout` を検証するテストだけで、その場合も下限と上限に十分な余裕（既定は 5 倍）を持たせ、`--timeout-scale` で伸ばせるようにする。
2. **非決定的なテストは必ずシードと再現の手順を出す**（`YUZHU_STRESS_SEED`、`YUZHU_SIM_SEED`、`YUZHU_CRASH_AT`）。失敗時に出さないテストは作らない。
3. **共有テストは PostgreSQL 17 で先に通す**（`tests/README.md` のとおり）。yuzhu だけの検査は `onlyif yuzhu` + KD のコメント（§6.6）。
4. **共有テストにタイミング依存を入れない**。ブロックする交互実行は isolation spec に、ブロックしない 2 接続は slt に書く（§6.5.1）。
5. **テーブル名・ロール名・データベース名はファイルごとに一意**（§6.5.2）。ランナーは同じ DB に直列に流す。

---

## 3. ディスク上の形式

なし。この章は永続形式を決めない。ディスク形式に関わる確認事項（★）は §10.2 の表で印を付けて集める。テストの側の形式（`known-diffs.toml`、`UPSTREAM`、`skip.txt`、`baseline.json`）は §6 に書く。

---

## 4. 共通の型（契約）

この章の型は**テスト用**（`#[cfg(test)]` か `tests/` の下）で、00 の契約を変えない。00 に頼る点は §11 に挙げる。

### 4.1 StressTarget（`yuzhu-core/src/testing/stress/mod.rs`）

```rust
/// ストレスの対象への接続。プロセス内（Session）、wire（yuzhu-server）、wire（PostgreSQL 17）の 3 つで実装する
pub trait StressTarget: Send + Sync {
    fn connect(&self, database: &str, user: &str) -> Result<Box<dyn StressConn>, StressError>;
    /// 観測用。プロセス内だけが Some（pg_locks の代わりに LockManager::lock_status を直接見る）
    fn internals(&self) -> Option<&dyn StressInternals>;
    fn label(&self) -> &'static str;                     // "inproc" | "yuzhu-wire" | "pg-wire"
}

pub trait StressConn: Send {
    fn simple(&mut self, sql: &str) -> Result<Vec<QueryResult>, StressError>;          // Simple Query（複数文可）
    /// Extended Query（無名の文。パラメータはテキスト形式。型は 0 = 未指定）
    fn extended(&mut self, sql: &str, params: &[Option<&str>]) -> Result<QueryResult, StressError>;
    fn prepared(&mut self, name: &str, sql: &str) -> Result<(), StressError>;
    fn execute_prepared(&mut self, name: &str, params: &[Option<&str>]) -> Result<QueryResult, StressError>;
    fn backend_pid(&self) -> i32;
    fn cancel(&self) -> Result<(), StressError>;                                       // CancelRequest
}

#[derive(Clone, Debug)]
pub struct StressError { pub sqlstate: String, pub message: String, pub detail: Option<String> }
impl StressError {
    pub fn is_retryable(&self) -> bool { self.sqlstate == "40001" || self.sqlstate == "40P01" }
}

#[derive(Clone, Debug, Default)]
pub struct QueryResult { pub rows: Vec<Vec<Option<String>>>, pub affected: u64 }

pub trait StressInternals: Send + Sync {
    fn lock_status(&self) -> Vec<LockStatusRow>;         // 00 §4.2
    fn oldest_xmin(&self) -> Xid;
    fn control(&self) -> ControlSnapshot;                // oldest_xid、next_multi など
}
```

### 4.2 ワークロードの共通部品

```rust
#[derive(Clone, Debug)]
pub struct StressConfig {
    pub seed: u64,                      // YUZHU_STRESS_SEED。省略時は時刻から決めて表示する
    pub threads: usize,                 // YUZHU_STRESS_THREADS
    pub duration: Duration,             // YUZHU_STRESS_SECS（PR は 5 秒、夜間は 600 秒）
    pub isolation: IsolationMix,        // Rc | Rr | Mixed
    pub watchdog: Duration,             // この時間、1 件もコミットがなければ失敗（既定 20 秒）
    pub allowed: &'static [&'static str],   // 許容する SQLSTATE（リトライ対象に加えて、そのワークロードで正常なもの）
}

#[derive(Clone, Copy, Debug)]
pub enum IsolationMix { Rc, Rr, Mixed }

/// ワークロードの結果。0 件のリトライは「競合が起きていない = 試験として空」の疑いがあるので、各ワークロードが下限を主張する
#[derive(Clone, Debug, Default)]
pub struct StressReport {
    pub committed: u64, pub retried_40001: u64, pub retried_40p01: u64,
    pub allowed_errors: BTreeMap<String, u64>,           // SQLSTATE ごとの許容したエラー数
    pub elapsed: Duration,
}

/// 各スレッドの直近 256 操作の記録。失敗時に target/stress-failure-<seed>.log に書く
pub struct OpRing { /* VecDeque<OpRecord> */ }
pub struct OpRecord { pub t_ms: u32, pub thread: u16, pub sql: String, pub outcome: String }

pub trait Invariant {
    fn name(&self) -> &'static str;
    /// 実行中に別スレッドから繰り返し呼ぶ（RC の 1 文のスナップショットで必ず成り立つもの）
    fn check_live(&self, c: &mut dyn StressConn) -> Result<(), String>;
    /// 終了後に 1 回
    fn check_final(&self, c: &mut dyn StressConn) -> Result<(), String>;
}

pub fn run_workload(target: &dyn StressTarget, w: &dyn Workload, cfg: &StressConfig) -> Result<StressReport, StressFailure>;
pub trait Workload: Sync {
    fn name(&self) -> &'static str;
    fn setup(&self, c: &mut dyn StressConn) -> Result<(), StressError>;
    /// 1 スレッドの 1 トランザクション分。リトライはしない（run_workload が is_retryable で繰り返す）
    fn txn(&self, c: &mut dyn StressConn, rng: &mut Xoshiro, tid: usize, iso: IsolationLevel) -> Result<(), StressError>;
    fn invariants(&self) -> Vec<Box<dyn Invariant>>;
    /// 期待する最小のリトライ数（RR のホットスポットなら 1 以上。0 なら主張しない）
    fn expect_min_retries(&self, iso: IsolationLevel) -> u64 { 0 }
    /// RC で 40001 が出てはいけないか（既定 true）
    fn rc_forbids_40001(&self) -> bool { true }
}
```

- 乱数は `Xoshiro`（`yuzhu-core/src/testing/` の自前。`tests/tools/difftest` と同じ xoshiro256**。外部クレートの版に左右されない）。
- `StressFailure` は、違反した不変条件の名前、観測値、シード、スレッドごとの `OpRing`、（プロセス内なら）`lock_status()` の全行を持ち、`target/stress-failure-<seed>.log` に書く。

### 4.3 アクタースケジューラ（`yuzhu-core/src/testing/actors.rs`）

```rust
pub struct ActorId(pub usize);

pub enum StepOutcome {
    /// 文が完了した（成功も失敗も）
    Done(Result<Vec<Row>, Error>),
    /// is_blocked_by が真になった（待ちに入った）。後で settle が結果を返す
    Blocked,
}

pub struct Scheduler<'a> { /* tc: &'a TestCluster、actors: Vec<Actor>、poll: Duration（既定 1ms）、hang: Duration（既定 10 秒） */ }

impl<'a> Scheduler<'a> {
    pub fn new(tc: &'a TestCluster) -> Self;
    /// 専用スレッドに Session を作る。database / user を指定できる
    pub fn spawn(&mut self, name: &str, database: &str) -> Result<ActorId>;
    /// 1 ステップを投入し、完了か待ちになるまで返らない。すでに待っているアクターには投入できない（panic）
    pub fn submit(&mut self, a: ActorId, sql: &str) -> StepOutcome;
    /// 動いているアクターがなくなる（全員が「完了」か「待ち」）まで待ち、待っていたステップの完了を返す
    pub fn settle(&mut self) -> Vec<(ActorId, Result<Vec<Row>, Error>)>;
    pub fn blocked(&self) -> Vec<ActorId>;
    /// 全アクターに停止を要求し（InterruptFlag の shutdown）、スレッドを join する。クラッシュ後の後始末にも使う
    pub fn abandon(self);
    /// 実行したステップの列（シードの再現とログ用）
    pub fn trace(&self) -> &[TraceEntry];
}
```

- `hang` を超えても動き続けるか、動いていないのに `is_blocked_by` が偽のとき（デッドロックの検出前など）は `Err(SchedulerHang)` で失敗し、`lock_status()` の全行と各アクターの最後のステップを出す。
- 待ちの判定は `LockManager::is_blocked_by(backend_id, &all_backend_ids)` を使う（00 §4.2。`Session::backend_id()` が要る。§11 の CR-4）。

### 4.4 クラッシュ試験の拡張（`yuzhu-core/tests/crash_sim/`）

```rust
// invariants.rs に足す（M3 の I1〜I12、M4 の I13〜I15 に続く）
pub fn i16_index_tids_not_unused(db: &DbView) -> Result<(), Violation>;      // VACUUM: インデックスが LP_UNUSED の行ポインタを指さない
pub fn i17_no_resurrection(model: &Model, db: &DbView) -> Result<(), Violation>;
pub fn i18_fsm_is_derived(tc: &TestCluster) -> Result<(), Violation>;        // FSM を壊す/消しても内容が変わらない
pub fn i19_xid_references_valid(db: &DbView) -> Result<(), Violation>;       // 全タプルの xmin / xmax が oldest_xid 以上、または凍結 / 無効
pub fn i20_relfrozenxid_is_lower_bound(db: &DbView) -> Result<(), Violation>;// yz_relxid / yz_datxid が下限になっている
pub fn i21_multixact_ids_below_next(db: &DbView) -> Result<(), Violation>;   // IS_MULTI の xmax の ID が next_multi 未満
pub fn i22_locks_gone_after_restart(tc: &TestCluster) -> Result<(), Violation>; // 全行に FOR UPDATE NOWAIT が成功する
pub fn i23_ri_holds(db: &DbView) -> Result<(), Violation>;                   // FK の孤児がない
pub fn i24_databases_consistent(tc: &TestCluster) -> Result<(), Violation>;  // pg_database ⇔ yz_datxid、コピー先 = テンプレート、接続可能
```

詳細は §5.5。`DbView` は M3 のハーネスがヒープを走査する部品（`heap_scan_all(rel)`、`read_control()`、`list_relation_files()`）で、M5 は FSM のフォークと `yz_relxid` を読む関数を足す。

---
## 5. 処理の流れ

### 5.1 共有テストを足す手順（各章の担当が従う）

```
1. tests/slt/m5/<機能>/<内容>.slt（または tests/isolation/specs/<name>.spec）を書く
2. PostgreSQL 17 に対して期待値を作る
     tests/pg.sh start
     SLT_EXTRA_ARGS=--override tests/run.sh --target pg tests/slt/m5/<機能>/<内容>.slt
     （isolation は yuzhu-isolation --accept。yuzhu に対して --override / --accept を使ってはいけない）
   git diff を読んで期待値をレビューする（statement error の `db error: ...` は SQLSTATE の形に直す）
3. PostgreSQL に対して 2 回続けて通ることを確かめる（後始末の漏れを見つける。§6.5.2 の規則 3）
4. yuzhu に対して通す（simple と extended の両方）
     tests/run.sh --target yuzhu --protocol simple|extended tests/slt/m5/<機能>/<内容>.slt
5. 食い違ったら: yuzhu のバグか、既知の差か
     バグ        → 直す（持ち主の章へ）
     既知の差    → KD が台帳にあるか確かめる。なければ台帳に足す（TS が受け付ける）。
                   差が出るケースは共有テストから外す。yuzhu の挙動を確かめたいなら
                   `onlyif yuzhu` + `# KD-n` のコメントで別のレコードにする
6. tests/tools/lint-tests.sh が通る
```

### 5.2 分離性ランナーの 1 つの permutation（M3 §7.3 から変わる部分）

```
run_permutation(spec, perm):
  制御接続: setup（spec の setup ブロック）
  各セッション: 接続（application_name = isolation/<spec>/<session>）、session setup
  制御接続: PREPARE 相当の Parse を 1 回だけ（PREP_WAITING = 本家と同じ名前）:
      SELECT pg_catalog.pg_isolation_test_session_is_blocked($1, '{<全セッションの pid>}')
  for step in perm:
     1. step を送る
          simple:   Query（SQL 全体）
          extended: split_statements(sql) の各文を Parse(無名) / Bind / Execute とし、最後に Sync を 1 回
     2. 10ms ごとに:
          応答が揃った                → 結果を出力して次の step へ（「step s1: <SQL>」と PQprint 形式。ERROR は主メッセージだけ）
          揃っていない かつ blocking-detection=pg:
              制御接続で Bind($1 = そのセッションの pid) / Execute / Sync
              真なら「<waiting ...>」を出力して次の step へ（このセッションは完了待ち）
          timeout が max_step_wait を超えた → CancelRequest。2 倍を超えたら FAILED で打ち切る
     3. 待っていたセッションがあれば、各 step の後に完了を確かめ、完了していたら「step s1: <... completed>」を出力
  teardown（セッション → spec）
```

- 判定は**関数だけに頼る**。`pg_isolation_test_session_is_blocked` は、そのセッションの待ちのロックを、interesting な pid のどれかが保持している（hard）か、interesting な pid のどれかが先に並んでいて衝突する待ちを出している（soft）ときに真を返す【記憶: waitfuncs.c。契約 CR-1 で yuzhu 側の意味を固定する】。
- `-- @cancel <セッション>`（M3 の拡張）は、extended のときも CancelRequest を別の接続で送るだけで変わらない。

### 5.3 アクタースケジューラ（クラッシュ試験で複数セッションを決定的に動かす）

```
submit(a, sql):
   actor[a].tx.send(Cmd::Run(sql));   state[a] = Running
   loop {
      match state[a] {
         Finished(r)  => return Done(r),
         Running if locks.is_blocked_by(backend_id(a), all_ids) => { state[a] = Waiting; return Blocked }
         Running      => sleep(poll)
      }
      if elapsed > hang { fail(SchedulerHang) }
   }

settle():
   // 他のアクターの完了で待ちが解けたものを含め、動いているアクターがなくなるまで
   loop {
      for a in Waiting: if state[a] is Finished(r) → 完了として記録、Idle にする
      if no actor is Running → break
      if all Running actors are now blocked (is_blocked_by) → それらを Waiting にする
      sleep(poll); hang を検査
   }
```

- **決定性の鍵**: 次のステップを投入する前に必ず `settle()` する。待ちが解けたアクターが走っている間に別のアクターの I/O が入ると、I/O の通し番号 N が揺れる。
- `Waiting` のアクターには投入しない。ワークロードのスクリプトは「待っているアクター以外から選ぶ」。全員が待っているなら（デッドロック）、`deadlock_timeout` が最も短いアクター（スクリプトが `SET deadlock_timeout` で決める。§6.8.2 の W3 以降）が被害者になるのを `settle()` が待つ。
- クラッシュ（`CrashFreeze` 後の Panic）が起きたら、`abandon()` が全アクターに shutdown を要求し、`LockManager` の待ち（57P01 で戻る。00 §4.2）を起こしてからスレッドを join する。
- 決定性の検査（`crash_sim/determinism.rs`）: 同じシードのワークロードを 3 回流し、`SimVfs` の操作列のハッシュ（種別・パス・オフセット・長さ）が一致する。**これが壊れたらクラッシュ点 N の再現ができないので、CI で必ず先に走らせる**。

### 5.4 並行ストレスの流れ

```
run_workload(target, w, cfg):
  1. 管理接続で w.setup。invariants を取得
  2. スレッド threads 本: 各自 connect → loop until stop:
        iso = cfg.isolation に従って決める（Mixed は tid の偶奇）
        BEGIN ISOLATION LEVEL <iso>; w.txn(); COMMIT
        失敗: is_retryable → ROLLBACK してカウントし、同じ txn をやり直す（最大 1000 回。超えたら失敗）
              cfg.allowed に含まれる → ROLLBACK してカウントし、次の txn へ
              それ以外 → 失敗（XX000 `tuple concurrently updated` など内部エラーはバグ）
  3. チェッカー 1 本: 5ms ごとに check_live（RC の 1 文。RR はトランザクション内で 2 回読んで一致も確かめる）
  4. ウォッチドッグ 1 本: watchdog の間、committed が増えなければ失敗。internals() があれば lock_status() を出す
  5. duration 後に stop → join → check_final → StressReport
  6. 主張: rc_forbids_40001 なら RC のスレッドで 40001 が 0。expect_min_retries を満たす
```

- 失敗時の出力: シード、違反した不変条件、観測値、各スレッドの `OpRing`、`lock_status()`。ファイルは `target/stress-failure-<seed>.log`。再現: `YUZHU_STRESS_SEED=<seed> YUZHU_STRESS_THREADS=<n> cargo test -p yuzhu-core --test stress <name>`。**同じシードでもスレッドの順序は再現しない**（再現するのは各スレッドの操作列だけ）。それでも壊れの再現率を上げるため、失敗したシードは `tests/stress-seeds.txt` に登録して毎回流す（回帰用）。

### 5.5 クラッシュ試験の 1 周（M3 §7.5 の拡張）

```
for (workload, mode, N) in plan(seed):
   tc = TestCluster（SimVfs。shared_buffers = 16、wal_segment_size = 2 MiB、background_checkpointer = false、deadlock_timeout = 10ms）
   sched = Scheduler(tc); model = Model::new()
   vfs.arm(FaultRule { op: Any, nth: Some(N), effect: CrashFreeze })
   script = workload.script(seed)     // Vec<Step { actor, sql, checkpoint_after: bool }>
   for step in script:
        if step.actor is waiting: スキップ（スクリプトは待ち行列を考慮して作る）
        match sched.submit(...) { Done(r) => model.apply(r, step), Blocked => model.pending(...) }
        for (a, r) in sched.settle(): model.apply_deferred(a, r)
        if step.checkpoint_after: tc.checkpoint()
        if vfs.frozen(): break
   sched.abandon(); vfs.crash(mode); tc = TestCluster::open(vfs)          // リカバリ
   invariants: I1〜I12（M3）、I13〜I15（M4。インデックス・シーケンスのワークロード）、I16〜I24（ワークロードごと。§6.8.3）
   繰り返し（I10）: リカバリ後に同じワークロードの続きをもう 1 周流して、もう一度クラッシュ → 不変条件
```

- **確定 / 不明の記録**: COMMIT が `Ok` なら確定、COMMIT を投入して Panic で戻ったら不明（M3 §7.5）。M5 で増える種類: `Blocked` のまま abandon したステップは「効果なし」（待ちの中の文は何も書いていない）。
- **クラッシュ点の網羅**: 小さいワークロード（I/O が数百回）は N を 0 から最後まですべて。大きいものはシードで N を選ぶ（W8 と W11 は 400 点）。`CrashCoverage`（§6.8.4）が「その点が何の直後か」を分類し、必要な分類がすべて踏まれたことを主張する。
- **失敗時**: `YUZHU_SIM_SEED`、`YUZHU_CRASH_AT`、モード、ワークロード名、`sched.trace()`（投入したステップの列）を出す。

### 5.6 期待値の生成と台帳

```
PostgreSQL（PG_IMAGE。既定 postgres:17。SHOW server_version をログの先頭に出す）
   → --override / --accept で期待値 → レビュー → コミット
yuzhu → 同じ spec / slt を流す → 差分
   差分 = バグ   → 持ち主の章に報告
   差分 = KD     → known-diffs.toml の KD を指して variant / onlyif
```

- 期待値は PostgreSQL 17 の**どのマイナー版でも**成り立つものだけにする。差が出たものは台帳に `[[kd]] kind = "pg-minor"` で挙げ、そのテストを共有から外す（既知: 17.x 間の差は `m5-concurrency` の実機 17.11 で観測した範囲では見つかっていない。未検証 U-1）。

---

## 6. モジュールごとの仕様

### 6.1 `run.sh`・`pg.sh`・`yuzhu.sh` の変更（TS-5）

| スクリプト | 変更 | 仕様 |
|---|---|---|
| `tests/run.sh` | `--protocol simple|extended`（既定 simple） | extended は `--engine postgres-extended` で流す。`--label` は `pg` / `yuzhu` のまま（エンジン名のラベルは sqllogictest 側が付ける。**sqllogictest-bin 0.29.1 の `postgres-extended` が使えること、浮動小数点の整形が simple と同じ出力になるかは未検証（U-2）**。差が出る行は `skipif postgres-extended` に `# KD-` なしで書いてよい（クライアント側の整形の差。ランナーの都合）） |
| 同 | `--password PW` | sqllogictest の `--pass` に渡す。環境変数 `PGPASSWORD` でも指定できる |
| 同 | `--list` | 実行するファイルの一覧だけ出して終わる（`lint-tests.sh` と CI のジョブ分割が使う） |
| 同 | `--restart` のフェーズ | フェーズのファイル `NN-<名前>.slt` と同じ場所に `NN-<名前>.db`（1 行、データベース名）があれば、そのフェーズだけ `--db` に使う（CREATE DATABASE をまたぐテスト用） |
| `tests/pg.sh` | `start --auth scram` | コンテナを `POSTGRES_PASSWORD=yuzhu-test-pw`、`POSTGRES_HOST_AUTH_METHOD=scram-sha-256`、`POSTGRES_INITDB_ARGS="--auth-host=scram-sha-256 --locale=C --encoding=UTF8"` で起動する。`sandbox/pg.sh` にも同じオプション（`initdb --auth-host=scram-sha-256 --pwfile`） |
| `tests/yuzhu.sh` | `start --auth scram` | `yuzhu-initdb -U postgres --auth-host=scram-sha-256 --pwfile <(printf yuzhu-test-pw)`（AU-2 のオプション。00 §6 の `InitdbOptions`）で作る。`--hba FILE`（`yuzhu_hba.conf` を差し替えて起動）、`--config K=V`（`autovacuum=on`、`max_connections` など。起動オプションに渡す） |

- **`tests/yuzhu.sh crash` と `restart` は SCRAM の設定を引き継ぐ**（前回のオプションを保存する既存の仕組みに `--auth` と `--hba` を含める）。

### 6.2 分離性ランナー（`tests/tools/isolation`。TS-1）

M3 の README の「本家との違い」を次のように変える。

| 項目 | M3 | M5 |
|---|---|---|
| ブロック判定の既定 | `pg` だが yuzhu では `timeout` を使っていた | `pg`。yuzhu の CI も `pg`（TS-D1） |
| 制御接続の問い合わせ | Simple Query（pid を SQL に埋める） | Extended Query（Parse を 1 回、判定ごとに Bind / Execute / Sync。TS-D2） |
| ステップの送信 | Simple のみ | `--protocol simple|extended`（TS-D3） |
| 待ちの上限 | `--max-step-wait` 30 秒 | 同じ。`--timeout-scale F`（既定 1.0。`--max-step-wait` と `--block-timeout-ms` に掛ける。遅い CI 用）。環境変数 `YUZHU_TIMEOUT_SCALE` でも指定できる |
| 期待ファイルの variant | `--variant NAME` で `<name>.<NAME>.out` を正解の候補にする | 同じ。**さらに `--variant` を使うとき、`known-diffs.toml` に `affects` として載っていない variant ファイルがあれば失敗する**（`tests/known-diffs.toml` を読む。`--no-kd-check` で外せる） |
| 実行する spec の指定 | ファイルまたはディレクトリ | 同じ。`--skip-list FILE`（`ported/skip.txt`）を読んで、理由つきで飛ばしたことを出力に出す（`SKIP <name>: KD-3`） |
| 出力形式 | 本家と同じ | 同じ。`--format junit FILE` を足す（CI の集計用。任意） |

**ステートメント分割 `split_statements(sql) -> Vec<&str>`**（extended のときだけ使う。`spec.rs` とは別に `split.rs`）:

- 文は `;` で区切る。次の中の `;` は区切りでない: 単一引用符の文字列（`''` のエスケープ。`E'...'` は `\` のエスケープも）、二重引用符の識別子（`""`）、ドル引用符（`$tag$ ... $tag$`、タグは空でもよい）、`--` から行末、`/* ... */`（入れ子を許す）。
- 空の文（空白とコメントだけ）は捨てる。末尾の `;` の後の空白は捨てる。戻り値の各文は元の SQL のスライス（ステップの出力で元の SQL をそのまま出すため）。
- 閉じていない引用符・コメントがあれば、全体を 1 つの文として返し、そのステップは Simple で送って警告を出す（本家の spec にも存在しうる。サーバが構文エラーにする）。
- 単体テスト: 上の各規則、`BEGIN; UPDATE t SET s = 'a;b'; COMMIT;` の 3 文、`SELECT $$;$$;`、`-- ;` の行、入れ子コメント、空の入力、末尾に空白だけ。

**`-- @cancel` と extended**: 変わらない。**spec の文法に足すものはない**（本家との互換を保つ）。

### 6.3 M3 の分離性 spec の書き直し（D39。TS-D4）

M3 の spec は「同じ行を更新する組み合わせ」だけを書いた（別の行への書き込みは M3 のライターロックが待つが PostgreSQL は待たないため）。M5 でその制限が消える。

| spec | 書き直し | 期待ファイル |
|---|---|---|
| `writer-waits-writer` | (1) コメントの「別の行への更新は PostgreSQL では待たない（yuzhu の M3 は待つ）」を削除。(2) permutation を足す: `a_b a_inc b_b b_upd2other a_c b_c c_sel`（A が id=1、B が id=2 を更新。**B は待たない**。`<waiting>` が出ないことが期待）、DELETE と INSERT の組み合わせ。(3) 既存 7 permutation は変えない | PostgreSQL 17 で再生成し、既存 permutation の出力が M3 のものと**同一**であることを確かめる（差が出たらどちらかが誤り） |
| `writer-queue` | (1) 同上のコメントの整理。(2) `permutation a_b a_upd b_b b_upd2 c_b c_upd a_c b_c c_c c_sel`（A と B は別の行、C は A と同じ行。C だけが待つ）を足す。(3) 既存 2 permutation の「A の終了で B だけが再開し C は待ち続ける」は PostgreSQL ではタプルロックの順序の結果で、そのままで一致する | 再生成。同一を確かめる |
| `lock-timeout` | M3 のコメント「PostgreSQL では行ロック待ち、yuzhu ではライターロック待ち」を削除。**テーブルロックの `lock_timeout`**（`LOCK TABLE ... IN ACCESS EXCLUSIVE MODE` を持つ A に対して B の `SELECT` が 55P03）と **NOWAIT**（`LOCK TABLE ... NOWAIT` と `SELECT ... FOR UPDATE NOWAIT`）の permutation を足す | 再生成 |
| `reader-not-blocked` | 変更なし（M3 でも PostgreSQL と同じ）。ただし**テーブルロック**を追加の permutation に足す: A が `LOCK TABLE ... IN ROW EXCLUSIVE MODE` を持つあいだ、B の SELECT は進む。`ACCESS EXCLUSIVE` なら B の SELECT は待つ（`<waiting>`） | 再生成 |
| `statement-timeout-wait`、`cancel-wait`、`idle-in-tx-timeout` | 変更なし。**M5 で再実行して同一の出力になることを確かめる**（`idle-in-tx-timeout` は FATAL でセッションが閉じたときに XID ロックとリレーションロックが解放され、待っていた B が進むことの最初の試験になる） | そのまま |
| `lost-update` | RR の permutation 2 本は、M3 では `lost-update.yuzhu-m3.out` が必要になる想定だった（README）。**M5 では variant を使わない**（PostgreSQL と完全一致） | そのまま。M3 の variant ファイルがあれば削除 |
| `write-skew-rr`、`rc-visibility` | 変更なし。`write-skew-rr` は M5 で初めて yuzhu で通る | そのまま |

併せて直す共有 slt（実装の担当が自分のマイルストーンの作業として行う。TS-5 が漏れを確かめる）:

| ファイル | 直し | 担当 |
|---|---|---|
| `tests/slt/m2/dml/update_errors.slt` | `UPDATE ... RETURNING` と `DELETE ... RETURNING` の `onlyif yuzhu` / `statement error (0A000)` を、PostgreSQL と同じ成功する形に変える（RW-6）。`UPDATE ... FROM` / `DELETE ... USING` の 2 件は M4 が直す | RW（M4） |
| `tests/slt/m3/session/lock_timeout_2conn.slt` | コメントの「PostgreSQL では行ロック待ち、yuzhu ではライターロック待ち」を削除（どちらも行ロック待ち） | TS |
| `tests/slt/m3/txn/isolation_level.slt` | `BEGIN ISOLATION LEVEL REPEATABLE READ` と `SHOW transaction_isolation` が `repeatable read` の項を足す。SERIALIZABLE は `onlyif yuzhu` + `# KD-3` で `m5/rr/serializable_unsupported.slt` に置く（M3 のこのファイルには足さない） | RW |
| `tests/slt/m3/functions/backend.slt` | 変更なし。ただし `pg_isolation_test_session_is_blocked(pg_backend_pid(), '{1}')` が `Datum::Array` の引数で動くことの回帰になる（D25） | — |
| `tests/README.md`、`tests/tools/isolation/README.md` | M3 の記述（ブロック判定が `timeout`、M3 の制約）の更新、M5 の実行方法の追加 | TS |
| `tests/restart/m3/*` | 変更なし。**M5 のサーバで全シナリオが通ること**（`01-committed-dml`〜`10-checksum-corrupt`）。シナリオの前提コメント「M3 は書き込みが 1 本ずつ」は更新する | TS |

### 6.4 PostgreSQL 本体の spec の移植（`tests/isolation/ported/`）

**取得**: `tests/tools/isolation/port-pg-specs.sh` が `REL_17_STABLE` の `src/test/isolation/specs/<name>.spec` と `expected/<name>.out`（および `_1.out` など）を取得し、`UPSTREAM` に commit SHA と取得日を書く。spec のヘッダにライセンス（PostgreSQL License）のコメントを残す。**期待出力は上流のものをそのまま使わず、`--accept` で PostgreSQL 17（`PG_IMAGE`）に流し直した出力を `expected/` に置く**（上流の期待との差は diff を見てレビュー。`validate-pg-suite.sh` が 120 / 123 本の一致を確かめ済みなので、差は少ないはず）。

**分類**:

- **完全一致**: yuzhu でも PostgreSQL の期待と同じ。
- **variant**: yuzhu の既知の差（KD）で出力が変わる。`<name>.yuzhu-m5.out` を持つ。差のある permutation が一部だけでも、ファイル全体を variant にする。
- **skip**: 未対応の機能（SERIALIZABLE、SAVEPOINT など）を使う。`skip.txt` に `<name> <KD-n> <理由>` の 1 行。

名前は m5-concurrency §15.2 の表と、上流に存在する記憶のある名前（【記憶】の印）。**実装前に上流で存在と中身を確かめる（U-3）**。「見込み」は PostgreSQL の README と調査からの予想で、実際に流して確定する。

| spec | 内容 | 分類（見込み） | 理由 |
|---|---|---|---|
| `deadlock-simple` | 2 者のデッドロック | 完全一致 | 被害者は先に `deadlock_timeout` が切れた側（D9）。spec が `SET deadlock_timeout` でどちらが先かを決める。**誰でも SET できる（KD-10）ので動く** |
| `deadlock-hard` | 多者の複雑なデッドロック | 完全一致 | 同上 |
| `deadlock-soft`、`deadlock-soft-2` | 待ち行列の並べ替えで解消されるデッドロック | variant | KD-5。PostgreSQL は救い、yuzhu は 40P01 |
| `deadlock-parallel` | 並列クエリのデッドロック | skip | 並列クエリなし（M6 以降の候補。KD-28 の周辺） |
| `eval-plan-qual` | EPQ の全般（結合、サブクエリ、CTE を含む） | variant | KD-6。単一テーブルの permutation は一致するので、**共有の `epq-single` に抜き出して完全一致を保つ**（§6.4.1） |
| `eval-plan-qual-trigger` | EPQ とトリガ | skip | トリガなし（KD-2 の周辺。未対応） |
| `tuplelock-conflict` | 4 強度の衝突表 | variant | KD-1（KEY SHARE × NO KEY UPDATE が衝突する） |
| `tuplelock-update` | ロックと UPDATE | variant（見込み） | KD-1 |
| `tuplelock-upgrade-no-deadlock` | ロックの昇格 | 流して判定 | m5-concurrency §15.2 が「一部差の可能性」とする |
| `skip-locked`、`skip-locked-2`、`-3`、`-4` | `SKIP LOCKED` | 完全一致（見込み） | `-2` 以降は【記憶】 |
| `nowait`、`nowait-2`〜`nowait-5` | `NOWAIT` | 完全一致（見込み） | 同上 |
| `lock-update-delete`、`lock-committed-update` | ロック後の UPDATE / DELETE | 完全一致（見込み） | 更新者がロック保持者と衝突する簡易版でも、同じ行を更新する組み合わせは変わらない |
| `lock-committed-keyupdate`、`lock-update-traversal` | キー更新と更新連鎖のたどり | variant（見込み） | KD-1。【記憶】 |
| `update-locked-tuple` | ロック済みタプルの更新 | 流して判定 | 【記憶】 |
| `read-write-unique`、`read-write-unique-2`、`-3`、`-4` | 一意検査の待ち（RR を含む） | 完全一致（見込み） | RW-5。`-3`、`-4` は【記憶】。SAVEPOINT を使うものは skip |
| `fk-contention`、`fk-deadlock`、`fk-deadlock2` | FOREIGN KEY とロック | variant | KD-1（子の INSERT と親の非キー列 UPDATE が衝突する） |
| `fk-snapshot` | FK の RR のスナップショット | 完全一致（見込み） | 【記憶】。FK-3 の crosscheck の検証 |
| `multixact-no-deadlock`、`multixact-no-forget` | MultiXact | 流して判定 | m5-concurrency §15.2 が「案 B で一致するか要確認」とする |
| `vacuum-concurrent-drop` | VACUUM と DROP TABLE の競合 | 完全一致（見込み） | VC |
| `timeouts` | `lock_timeout`・`statement_timeout` | 流して判定 | 【記憶】。`pg_cancel_backend` を使う部分は skip（KD-18） |
| `read-only-anomaly`、`simple-write-skew`、`receipt-report`、`project-manager`、`classroom-scheduling`、`total-cash`、`temporal-range-integrity`、`referential-integrity`、`ri-trigger`、`two-ids`、`multiple-row-versions`、`predicate-*`、`serializable-parallel*` | SERIALIZABLE（SSI）が前提 | skip | KD-3 |
| `delete-abort-savept`、`delete-abort-savept-2`、`savepoint-*` | SAVEPOINT が前提 | skip | KD-4 |
| `insert-conflict-*`、`merge-*` | `ON CONFLICT`、`MERGE` | skip | KD-25 |
| `alter-table-*`、`drop-index-concurrently-*`、`partition-*`、`fk-partitioned-*` | ALTER TABLE の大半、パーティション | skip | KD-26、KD-28 |
| `async-notify`、`prepared-transactions*` | LISTEN / NOTIFY、2PC | skip | KD-24 |
| `index-only-scan`、`vacuum-reltuples`、`freeze-the-dead`、`vacuum-conflict` | VM、統計、FREEZE オプション | skip | KD-8 |

#### 6.4.1 共有 spec の新規作成（`tests/isolation/specs/`。完全一致）

PostgreSQL の `expected` を元に、**yuzhu の既知の差を避けて**書く（M3 の方針と同じ）。実機の観測（m5-concurrency §2）の番号を `#` で示す。

| spec | 内容（観測） |
|---|---|
| `epq-single` | #1（待った後に SET を再計算）、#2（WHERE を再評価して外れた）、#3（相手が削除した行は黙って飛ばす）、#12（`FOR UPDATE` も再評価）。単一テーブルだけ |
| `rr-update-conflict` | #4（RR でスナップショット後のコミット済み更新に当たる）、#4b（相手が中断なら続行）、#4c（待った後で相手がコミットしたら 40001）、#4d（削除に当たると `concurrent delete`）、#4e（スナップショットは最初の文）、#4f（`FOR UPDATE` も 40001）、#4g と #13（挿入だけ・読むだけの RR はエラーにならない） |
| `for-update-nowait-skip` | #6（NOWAIT の 55P03 の文言、SKIP LOCKED、`lock_timeout`、`FOR SHARE NOWAIT`、通常の SELECT は止まらない） |
| `lock-strength-matrix` | 4 強度のうち、PostgreSQL と yuzhu で同じになる組み合わせだけ（FOR UPDATE × すべて、SHARE × SHARE、SHARE × UPDATE、KEY SHARE × KEY SHARE、KEY SHARE × FOR UPDATE）。**KEY SHARE × NO KEY UPDATE と、更新者 × KEY SHARE は入れない（KD-1）** |
| `deadlock-2way-row` | #5（2 者の行ロックのデッドロック。被害者は先に待った側。`SET deadlock_timeout` で順序を決める）、#7b（共有ロックの昇格） |
| `deadlock-table` | #8d（テーブルロックのデッドロック） |
| `lock-table-queue` | #8、#8b（DROP の待ち行列、後続の SELECT は FIFO で待つ、待ち明けは 42P01）、#8c、#8e（25P01）、#8f |
| `drop-while-reading` | 読み取り中のテーブルの DROP は待つ（M3-Q20 の差が消えることの確認） |
| `unique-wait` | #10（挿入者の終了を待つ。COMMIT で 23505、ROLLBACK で成功） |
| `vacuum-concurrent-dml` | VACUUM と INSERT / UPDATE / DELETE の並行。VACUUM は待たせず（ShareUpdateExclusive）、結果のデータが変わらない。**VERBOSE の文言は出力に出さない（NOTICE の内容は KD-8）** |
| `vacuum-line-pointer-reuse` | **要検討（U-4）**: 古い RR が horizon を止めている間は VACUUM が行を消さず（新しい INSERT の `ctid` は進む）、止めていないと消えて `ctid` が再利用される（#11、#11b）。`ctid` で観測する。PostgreSQL が最も小さい未使用の行ポインタを再利用する規則を yuzhu が揃えない（CR-12）なら variant にする |
| `fk-parent-delete-race` | FK の「親の削除と子の挿入の競合」（m5-types-fk §9.5 の 3 手順）。KD-1 を避けるため、子の INSERT と親の **DELETE** だけ（親の非キー列 UPDATE は使わない）。子の INSERT が先なら親の DELETE が待ち、COMMIT なら 23503、ROLLBACK なら成功 |
| `idle-session-lock-release` | 接続の切断（FATAL）でリレーションロックと XID ロックが解放され、待っていた側が進む |

#### 6.4.2 台帳との対応

`ported/` の variant は `known-diffs.toml` の `affects` に `isolation/ported/<name>` を載せる。台帳に載っていない variant が `ported/expected/` にあれば、ランナーも `lint-tests.sh` も失敗する。

### 6.5 共有 SQL テスト（`tests/slt/m5/`）と再起動テスト

#### 6.5.1 ディレクトリ構成（機能ごと。1 ファイル 1 機能）

| ディレクトリ | 内容 | 持ち主 |
|---|---|---|
| `lock/` | `lock_table_modes`、`lock_table_errors`（25P01、42P01、`NOWAIT`）、`pg_locks_basic`（自分の `relation` で絞る。KD-9）、`pg_blocking_pids`、`deadlock_timeout_set`、`lock_timeout_table`（2 接続・`lock_timeout` で必ず終わる形） | LK |
| `rowlock/` | `for_update_basic`、`for_share`、`for_key_share`、`of_clause`、`nowait_skiplocked`（2 接続。待たない形のみ）、`for_update_limit_order`、`for_update_errors`（`DISTINCT` / `GROUP BY` / 集合演算 / 集約の 0A000 の文言を PostgreSQL で固定） | RW |
| `rr/` | `isolation_level_rr`（`SHOW`、`SET TRANSACTION` の 25001）、`rr_snapshot_single`、`rr_two_conn`（相手がコミット済みの状態で 40001 を確かめる。待たない）、`serializable_unsupported`（`onlyif yuzhu` + `# KD-3`） | RW |
| `returning/` | INSERT / UPDATE / DELETE の `RETURNING`（`FROM` / `USING`、式、`*`、シーケンス、複数行） | RW |
| `vacuum/` | `vacuum_syntax`、`vacuum_in_block`（25001）、`vacuum_data_unchanged`、`analyze`、`vacuum_errors`（42P01）、`truncate_basic`（ロールバック可能）、`truncate_referenced`（0A000） | VC |
| `extended/` | `prepare_execute`、`deallocate_discard`、`prepare_errors`（26000 / 42P05 / 42P18）、`prepare_param_types`、`plan_invalidation`（PREPARE の後に DDL し、`cached plan must not change result type` と再解析） | XQ |
| `types/` | `numeric_gap`、`bpchar_gap`、`bytea_*`、`uuid_*`、`array_*`（`ARRAY[]`、`ANY`、`::T[]`、`array_length`、`cardinality`、`array_to_string`）、`interval_*`、`time_*`、`datestyle`、`intervalstyle`、`timezone_*`（先頭で `SET TIME ZONE 'UTC'`、`SET DateStyle = 'ISO, MDY'`。m5-types-fk §11） | TY、TD |
| `fk/` | `create_column`、`create_table`、`alter_add`、`self_ref`、`composite`、`names`、`violation_child`、`violation_parent`、`action_*`（5 つの参照アクション）、`match_*`、`statement_end`（文の終わりの検査）、`drop_truncate`、`catalog_pg_constraint`、`rr_two_conn` | FK |
| `auth/` | `role_create`、`role_alter`、`role_drop`、`pg_roles`、`role_errors`（パスワードの比較は書かない。SCRAM は §6.10.4） | AU |
| `database/` | `create_drop_errors`（42P04、55006、25001）、`pg_database_rows`、`template_in_use`（`connection` で別接続を保持し、`CREATE DATABASE` が 55006）。**別のデータベースへの接続は slt では書けない**（sqllogictest の `connection` は同じ `--db`）。接続を伴う確認は `restart/m5` の `.db` と `compat/` | DB |
| `catalog/` | `catalog_columns_m5`（M2 の `catalog_columns.slt` の回帰。`relfrozenxid` の型が `xid` のまま。D14）、`yz_relxid`（`onlyif yuzhu`） | VC |

**ブロックの有無の規則**: slt に書いてよい 2 接続は、**待ちが起きない**形（相手がコミット済み・`NOWAIT`・`SKIP LOCKED`）か、`lock_timeout` / `statement_timeout` で**必ず終わる**形だけ。待つことを確かめるテストは isolation spec に書く（sqllogictest はレコードを直列に流すので、待ちに入ると全体が止まる）。

#### 6.5.2 命名と後始末の規則

1. **接頭辞**: ファイルのパス `tests/slt/m5/<dir>/<stem>.slt` に対し、すべてのテーブル・インデックス・シーケンス・ロール・データベースの名前の先頭を `<dir>_<stem>_`（例 `fk_action_cascade_p`）にする。63 バイトに収まること。`lint-tests.sh` が `CREATE` の対象名を検査する。
2. **後始末**: ファイルの最後に作ったものをすべて DROP する（M1 と同じ）。**さらにファイルの先頭で `DROP ... IF EXISTS` を同じ名前に対して実行する**（M2-Q22 のとおり、途中で失敗したファイルが残骸を残して次の実行を壊すため）。ロールとデータベースはクラスタ全体で共有されるので、この 2 つは必須。
3. **冪等性**: PostgreSQL に対して同じファイルを 2 回続けて流して通ること（手順 5.1 の 3）。
4. **SET したものは RESET**（M1 と同じ）。`SET ROLE` / `SET SESSION AUTHORIZATION` は使わない（GRANT がない。M6）。
5. **システム列・OID の値は比べない**（M2 と同じ）。`pg_locks` は `relation = '<名前>'::regclass`（regclass は M4）と `locktype` で絞り、行数だけでなくモードと granted を比べる（KD-9）。
6. **日時**: `now()` などは値を比べない。タイムゾーンと DateStyle は先頭で明示する（m5-types-fk §11）。
7. **エラー**は SQLSTATE で照合し、メッセージは短い部分一致に留める（M1 と同じ）。

#### 6.5.3 再起動・クラッシュをまたぐテスト（`tests/restart/m5/`。`--crash`）

M3 と同じ仕組み（`yuzhu.args`、`yuzhu.only`、`NN-*.after.sh`）に、**`NN-*.db`**（§6.1）を足す。テーブル名の接頭辞は `crm5_<NN>_`。

| シナリオ | 内容 |
|---|---|
| `01-multiwriter-commit` | 2 接続（本体と `other`）が別の行を同時に更新。本体がコミットし、`other` は未コミットのままクラッシュ → 本体の変更だけが残り、`other` の行は元の値で、ロックされていない（M3 の「本体を先にコミットしてから other が書き始める」制限が消える） |
| `02-vacuum-crash` | 多数の行を DELETE / UPDATE → `VACUUM` → クラッシュ → 内容が同じで、`INSERT` も `UPDATE` も通る。`CHECKPOINT` の前後の 2 通り |
| `03-fk-cascade-crash` | ON DELETE CASCADE（コミット → クラッシュ → 子の行も消える。未コミット → クラッシュ → 親も子も残る）、`SET NULL`、`RESTRICT` の違反は何も変えない |
| `04-multixact-locks` | 2 接続が同じ行を `FOR SHARE` / `FOR KEY SHARE` で保持 → クラッシュ → すべての行に `FOR UPDATE NOWAIT` が成功する（ロックは消える） |
| `05-create-database` | `CREATE DATABASE` をコミット → クラッシュ → `pg_database` に行があり、フェーズ 2（`.db` で接続先を切り替え）でコピー先の内容がテンプレートと同じ。さらに書いてもう一度クラッシュ |
| `06-drop-database` | `DROP DATABASE` をコミット → クラッシュ → 行がなく、接続は 3D000、同名で作り直せる |
| `07-role-persist` | `CREATE ROLE` / `ALTER ROLE ... NOLOGIN` / `DROP ROLE` をコミット → クラッシュ → `pg_roles` に反映 |
| `08-truncate-vacuum` | `TRUNCATE` → `INSERT` → `VACUUM` → クラッシュ。ロールバックした `TRUNCATE` は元の行が残る |
| `09-large-vacuum` | **yuzhu.only**（`yuzhu.args`: `--shared-buffers 1MB`）。プールより大きい表で `VACUUM`（ページが追い出される）→ クラッシュ |

### 6.6 PostgreSQL 17 でも通る原則と、既知の差を入れない規則

**原則**: `tests/slt/` と `tests/isolation/specs/` のすべてのテストは、PostgreSQL 17 に対して通る。通らないテストは期待が誤っているとみなす（`tests/README.md`）。

**規則**:

1. 既知の差（KD。§10.3）が出るケースは共有テストに入れない。入れるなら `onlyif yuzhu` を付けた**別のレコード**にし、直前の行に `# KD-<n>` のコメントを書く。
2. `skipif yuzhu` は「未実装の一時回避」にだけ使い、`# 未実装: <章>-<WP>` のコメントを書く。M5 の完了時に `skipif yuzhu` は 0 件にする（§7.3 の 10）。
3. isolation の variant は `known-diffs.toml` の `affects` に載せる（§6.2、§6.4.2）。
4. `ported/skip.txt` の各行は KD の ID を持つ。

**台帳 `tests/known-diffs.toml`**:

```toml
[[kd]]
id = "KD-1"
kind = "diff"                   # "diff": 実装した機能の挙動の差 / "unsupported": 未対応（0A000 などを返す）
title = "MultiXact は簡易版。更新者とロック保持者は常に衝突する"
chapter = "02-row-lock-rr"      # 持ち主の章
since = "M5"
until = "M6"                    # 解消の予定。なければ省略
affects = ["isolation/ported/tuplelock-conflict", "isolation/ported/fk-deadlock", "slt/m5/fk/*"]   # 影響するテスト
note = "FK の子の INSERT 中に親の非キー列の UPDATE が待たされる。共有テストにはこの差が出るケースを入れない"
```

**`tests/tools/lint-tests.sh`（CI の `lint-tests` ジョブ。数秒）**:

| 検査 | 内容 |
|---|---|
| L1 | すべての `onlyif yuzhu` の直前の行（または同じレコードの直前のコメント）が `# KD-<n>` を含み、その KD が台帳にある |
| L2 | すべての `skipif yuzhu` が `# 未実装: <章>-<WP>` を含む。M5 の完了判定（`--release`）では 0 件であること |
| L3 | `tests/isolation/ported/expected/*.yuzhu-m5.out` が台帳の `affects` に載っている |
| L4 | `skip.txt` の各行の KD が台帳にある |
| L5 | `tests/slt/m5` のファイルの `CREATE` 対象名が接頭辞の規則（§6.5.2 の 1）に従う。ファイル先頭に同名の `DROP ... IF EXISTS` がある |
| L6 | 台帳の `affects` のパスが実在する（glob は 1 件以上に一致） |
| L7 | `statement error` に SQLSTATE がある（`db error:` の接頭辞が残っていない。M2 の `--override` 後の直し忘れ） |
| L8 | 台帳の `kind = "unsupported"` の KD の `pattern`（正規表現。例 KD-3 は `ISOLATION LEVEL SERIALIZABLE`）に一致する行が、共有テスト（`tests/slt`、`tests/isolation/specs`）に `onlyif yuzhu` なしで現れたら失敗（未対応の機能を共有テストが使うと yuzhu が必ず落ちるため） |

### 6.7 並行ストレス（TS-2。`yuzhu-core/src/testing/stress/`）

共通: テーブル・シーケンス名に接頭辞 `st_`。PR のジョブは各 5 秒、スレッド 8 本。夜間は各 600 秒、スレッド {2, 8, 16}。シードは PR で固定、夜間でランダム。

| ID | ワークロード | スキーマ・トランザクション | 不変条件 | 主張 |
|---|---|---|---|---|
| **S1** | 送金（RC と RR、`Mixed`） | `acct(id int PRIMARY KEY, bal int NOT NULL CHECK (bal >= 0))` と `xfer_log(id bigserial, src, dst, amt)`。`BEGIN; UPDATE acct SET bal = bal - $1 WHERE id = $2; UPDATE acct SET bal = bal + $1 WHERE id = $3; INSERT INTO xfer_log ...; COMMIT`。src / dst はランダム（順序を揃えない → デッドロックが自然に起きる）。残高不足は 23514 を許容して中断 | **(a) 全口座の合計が一定**（`SELECT sum(bal)` を 5ms ごとに。RC の 1 文でもスナップショットが一貫しているので必ず一定）、(b) 最終的に `bal[i] = 初期値 + Σin − Σout`（`xfer_log` から再計算。更新の消失がない）、(c) RR のチェッカーはトランザクション内で `sum` を 2 回読み同じ値、(d) `count(xfer_log)` は単調非減少 | RC で 40001 が 0。RR のホットスポット（口座 2 つ）で 40001 が 1 以上。デッドロック（40P01）は両方で起きうる |
| S1' | 送金の変種 | (i) 口座 2 / 10 / 1000、(ii) PK なし（全件走査）と PK あり（B+Tree の点検索）、(iii) 手前に `SELECT ... WHERE id IN (a, b) ORDER BY id FOR UPDATE` で順序を揃える（**デッドロックが 0**であることを主張）、(iv) `FOR NO KEY UPDATE`、(v) Extended Query と準備済み文（`prepared`）、(vi) RC と RR を混ぜる | 同上 | (iii) で 40P01 が 0 |
| **S2** | 一意キーの競合 | `uq(k int PRIMARY KEY, who int)`。k は 0〜99 の乱数。`INSERT INTO uq VALUES ($1, $2)`。一部のスレッドは INSERT 後に 50% で ROLLBACK | **重複キーがない**（`GROUP BY k HAVING count(*) > 1` が空）、**インデックス経由と全件走査の結果が一致**（`SET enable_indexscan = off` の有無で 全行を `ORDER BY k` で取った結果を比べる）、23505 は許容 | 23505 が 1 以上、RW-5 の待ち（挿入者の終了待ち）が少なくとも 1 回は起きた（`internals()` があれば `lock_status()` に `transactionid` の待ちが現れたことを記録） |
| **S3** | FOREIGN KEY | `parent(id PK, v)`、`child(id, pid REFERENCES parent(id) ON DELETE (RESTRICT / CASCADE / SET NULL の 3 変種))`。スレッド: 子の INSERT、親の DELETE、親のキーの UPDATE（`ON UPDATE CASCADE` の変種）、親の INSERT | **孤児がない**（`child LEFT JOIN parent` で `pid IS NOT NULL AND parent.id IS NULL` が 0。RC の 1 文で常に 0）、`CASCADE` の変種で子の件数が親の件数と整合 | 23503 が 1 以上（`allowed`）。**親の非キー列 UPDATE は含めない**（KD-1。含めると待ち・デッドロックが増える。含める変種は夜間で、成立だけを確かめる） |
| **S4** | VACUUM との並行 | S1 に加えて、(a) VACUUM スレッド（`VACUUM acct`、`VACUUM xfer_log` を繰り返す）、(b) 長い RR の読み手（`BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT sum(bal); pg_sleep(2); SELECT sum(bal); COMMIT` — **登録済みスナップショットが horizon を止めるので同じ値**）、(c) SELECT だけのスレッド（機会的 pruning を走らせる）、(d) 夜間: `CHECKPOINT` スレッド | S1 の (a)〜(d) に加え、(e) **RR の読み手の 2 回の `sum` が一致**、(f) 実行後に読み手を閉じて `VACUUM` を 2 回流したあと、`pg_relation_size('acct')` が初期の 8 倍以下（FSM による再利用。膨張しない。`xfer_log` は増える一方なので対象外）、(g) インデックス経由と全件走査の結果が一致（`acct_pkey` が LP_UNUSED の行を指さない）、(h) `yz_relxid.relfrozenxid8` が単調非減少 | VACUUM が少なくとも 20 回完了。(f) の上限は暫定（U-5） |
| S4' | VACUUM と DDL | `VACUUM` と `DROP TABLE` / `TRUNCATE` / `CREATE INDEX` の並行（`vacuum-concurrent-drop` の並行版） | DROP 後に VACUUM は 42P01 か成功（どちらでもよいが、内部エラー XX000 は不可）。TRUNCATE と VACUUM は排他（ShareUpdateExclusive と AccessExclusive） | — |
| **S5** | ジョブキュー | `jobs(id serial, state text, worker int)`。ワーカー: `BEGIN; SELECT id FROM jobs WHERE state = 'new' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED; UPDATE jobs SET state = 'done', worker = $1 WHERE id = $2; COMMIT`。供給スレッドが INSERT し続ける | **各ジョブがちょうど 1 回処理される**（`done` の件数 = 供給数 − `new` の残り、`worker` が NULL の `done` がない）。ワーカー間で同じ ID を処理しない（ログ表で検査） | 40001 / 40P01 は RC では 0（SKIP LOCKED は待たない。デッドロックもない） |
| **S6** | 接続と DATABASE | 接続・切断を繰り返すスレッドと、`CREATE DATABASE c<n> TEMPLATE s6t` / `DROP DATABASE c<n>` を繰り返すスレッド（同時に接続するスレッドが 3 つ） | 接続は 3D000（作成前 / 削除後）、55006（テンプレートが使用中の `CREATE`）、成功のいずれか。**作成に成功したら、必ず接続でき、テンプレートと同じ内容**。内部エラーなし。`pg_database` の行数 = 作成成功 − 削除成功 + 初期 | 55006 が 1 以上（`allowed`）。夜間のみ（PR は 1 サイクルだけの煙試験） |
| S7 | 混在（夜間のみ） | S1 + S2 + S3 + S4 + S5 を別テーブルで同時に。接続の数 = 32。`max_connections` 付近まで | 各ワークロードの不変条件。ウォッチドッグ | 24 時間ではなく 600 秒。RC / RR / VACUUM / FK / ジョブが干渉しない |

- **ウォッチドッグの意味**: デッドロック検出（LK-2）が壊れると S1 が止まる。`deadlock_timeout` は 1 秒（既定）なので、ウォッチドッグは 20 秒とする（2 者のデッドロックは 1 秒強で解けるはず。多者でも `deadlock_timeout` × 参加者数以内）。
- **PostgreSQL への適用**: wire 版は `YUZHU_TEST_TARGET=pg` で PostgreSQL 17 に向けられる（S1〜S5。S6 の CREATE DATABASE は可。S4 の `yz_relxid` の検査は yuzhu のみ）。**PostgreSQL に向けても通ること**が期待値の確認になる（特に S1 の 40001 の挙動、S5 の SKIP LOCKED）。
- 実行方法: `cargo test --release -p yuzhu-core --test stress`（プロセス内。`YUZHU_STRESS_SECS`、`YUZHU_STRESS_SEED`、`YUZHU_STRESS_THREADS`）、`YUZHU_TEST_TARGET=yuzhu|pg YUZHU_TEST_DSN='host=... port=...' cargo test --release -p yuzhu-server --test concurrency`。夜間は `-- --ignored`。

### 6.8 クラッシュ試験の拡張（TS-3。`yuzhu-core/tests/crash_sim/`）

#### 6.8.1 アクター

M3 の `workload.rs` の 1 スレッド・複数 `Session` を、`Scheduler`（§4.3、§5.3）に置き換える（M3 のワークロード 1〜5 は 1 アクターのまま動かし、M4 の 6〜7 も同じ）。`TestClusterOptions` に `deadlock_timeout`（既定 10ms）と `autovacuum = false` を足す。

#### 6.8.2 ワークロード（M3 の 1〜5、M4 の 6〜7 に続く）

| ID | ワークロード | 中身 | クラッシュ点 |
|---|---|---|---|
| **W8** | VACUUM | 表 `v(id int PRIMARY KEY, val text)` に 200 行。3 アクター: 書き手 A（UPDATE / DELETE / INSERT。トランザクションは 1〜3 文、一部 ROLLBACK）、書き手 B（A とは別の行。同じページ）、VACUUM 担当 V（`VACUUM v` を k 文ごとに）。k 文ごとに `CHECKPOINT`。プールは 16 フレーム。**行の再利用が起きるまで**（DELETE → VACUUM → INSERT）を何周かする。凍結を起こすため、`TestClusterOptions.freeze_all = true`（VC の `vacuum_freeze_min_age` 相当をテスト用に 0 にする。CR-6） | N = 0..400 の 400 点（M3 の `DropUnsynced` / `KeepAll`、`TornSectors` をシードで） |
| **W9** | MultiXact と行ロック | 表 `m(id, v)`。アクター 4: `SELECT ... FOR SHARE`、`FOR KEY SHARE`、`FOR UPDATE`、UPDATE。共有ロックの保持者が複数いる状態（MultiXact）を作ったまま、一部をコミット、一部を ROLLBACK、一部を実行中のままにする。待ちも作る（`FOR UPDATE` が `Blocked`）。再起動後に新しい MultiXact を作る（I10 の 2 周目） | 全 N（小さいワークロード） |
| **W10** | FK の CASCADE | `p1 ← c1 ← g1`（3 階層。`ON DELETE CASCADE`）、`p2 ← c2`（`SET NULL`）、`p3 ← c3`（`RESTRICT`）。アクター 3: 親の DELETE（連鎖で子と孫が消える）、子の INSERT / UPDATE、親のキーの UPDATE（`ON UPDATE CASCADE`）。連鎖の途中で I/O が落ちる | N = 0..300 の 300 点 |
| **W11** | CREATE / DROP DATABASE | テンプレート `t1`（`CREATE DATABASE t1 TEMPLATE template1` の後、表を 3 つと各 100 行）。アクター: 本体、他データベースで動き続ける書き手（コピーの最中のテンプレートでなく、`postgres` データベースの別の表）。`CREATE DATABASE c1 TEMPLATE t1` → 書き込み → `DROP DATABASE c1` → `CREATE DATABASE c1`（同名。OID が変わる） | **全 N**（`CHECKPOINT` → コピー → WAL → 行の挿入 → コミット → チェックポイントのすべての I/O を網羅） |

- **W3 のデッドロック**: M3 のワークロード 3（少数の行の UPDATE の繰り返し）は、アクターを 2 つに分け、互いの行を逆順に更新するスクリプトを 1 つ含める。1 つ目のアクターは `SET deadlock_timeout = '10ms'`、2 つ目は `'60s'`（被害者を決定的にする。§5.3）。**被害者の ROLLBACK 後にもう一方が進み、コミットし、クラッシュ後も合計が一定**（銀行振込の不変条件の確認）。

#### 6.8.3 不変条件（M3 の I1〜I12、M4 の I13〜I15 に続く）

| ID | 内容 | 検査 | 適用 |
|---|---|---|---|
| **I16** | インデックスの項目は、行ポインタが `LP_UNUSED` のタプルを指さない。さらに、`LP_NORMAL` を指す項目のキーは、その行のキーと一致する（再利用された行ポインタを古いキーが指していない） | 全インデックスの全 TID をヒープで引く（M4 の I13 の拡張） | W8 |
| **I17** | **復活がない**: モデルで削除済み・VACUUM 済みの行がクラッシュ後に見えない。ROLLBACK された INSERT / UPDATE の行が見えない | モデルとの比較（I4 に加える） | W8、W10 |
| **I18** | **FSM は導出データ**: クラッシュ後に FSM のフォークを (a) 削除する、(b) 先頭 1 バイトを壊す、(c) そのままにする、の 3 通りで開き直し、`SELECT` の結果が同じ。その後の `INSERT` が成功する（FSM が見つからなければ拡張する。規約 4） | FSM ファイルの操作 | W8 |
| **I19** | **XID の参照が有効**: 全ヒープの全タプルの `xmin` と（`LOCK_ONLY` でない）`xmax` は、`oldest_xid` 以上、または `XMIN_FROZEN`（両ビット）/ `XMAX_INVALID`。**clog の切り詰めで引けなくなる XID がない** | ヒープ全走査 + 制御ファイルの `oldest_xid` | W8（`freeze_all` で clog を切り詰める） |
| **I20** | `yz_relxid.relfrozenxid8` が各リレーションの**下限**（そのリレーションに残る凍結されていない `xmin` の最小値以下）。`yz_datxid` はそのデータベースの `yz_relxid` の最小値以下。`pg_class.relfrozenxid` は `relfrozenxid8` の下位 32 ビット | カタログとヒープの突き合わせ | W8、W11 |
| **I21** | `IS_MULTI` の `xmax` の MultiXactId は、すべて制御ファイルの `next_multi` 未満（再起動後に古い ID と新しい ID が衝突しない。D36） | ヒープ全走査 | W9 |
| **I22** | **再起動後にロックが残らない**: クラッシュ時に実行中だったトランザクションが持っていた行ロックが、リカバリ後に全行で `SELECT ... FOR UPDATE NOWAIT` が成功する（`LockManager` も MultiXact の表も空。xmax のロックビットは無効扱い。§5.4） | 新しいアクターで全行に NOWAIT | W9 |
| **I23** | **参照整合性**: FK の孤児がない。確定したトランザクションの CASCADE は子孫まですべて反映され、不明なトランザクションは全部か 0（文の原子性） | 孤児検査 + モデル | W10 |
| **I24** | **データベースの整合**: (a) `pg_database` の各行に `base/<oid>/` があり、`yz_datxid` の行がある（逆も）、(b) コミットされた `CREATE DATABASE` のコピー先は接続でき、内容がテンプレートの（コピー時点の）内容と同じ、(c) コミットされていなければ `pg_database` に行がない（`base/<oid>/` が残っていてよい。WARNING が 1 件出る。D31）。同名で `CREATE DATABASE` を再実行して成功する、(d) `DROP DATABASE` がコミットされていれば接続は 3D000、同名で作り直せる、(e) 他のデータベースの確定したトランザクションがすべて見える（共有の clog） | `pg_database` と `base/` の列挙 | W11 |

#### 6.8.4 クラッシュ点の網羅（`CrashCoverage`）

スイープ（N の全部、または 400 点）の**各 N について、直前に起きた「意味のある出来事」を分類**し、分類の集合が必須の集合を含むことを主張する。分類は `SimVfs` の操作列と `WalReader` で作る。

| 必須の分類 | 判定 |
|---|---|
| `HEAP2/PRUNE_FREEZE` の挿入の直後 / flush の直前・直後 | WAL の末尾のレコード種別と flush 済みの位置 |
| `BTREE/VACUUM` の挿入の直後 | 同上（W8） |
| `HEAP2/VACUUM_UNUSED` の挿入の直後 | 同上（W8） |
| `HEAP/LOCK` の挿入の直後 | 同上（W9） |
| `DBASE/CREATE_FILE_COPY` の前・後（ディレクトリのコピーの途中を含む） | W11 |
| `DBASE/DROP` の前・後 | W11 |
| `SMGR/TRUNCATE`（VACUUM の末尾の切り詰め） | W8 |
| 制御ファイルの書き込みで `oldest_xid` が変わった直後 / `pg_xact` のセグメントの unlink の前・後 | W8（`freeze_all`） |
| FSM のフォークの書き込みの途中 | W8 |
| CASCADE の途中（親の削除の後・子の削除の前） | W10 |

必須の分類が 1 つでも踏まれなかったら、テスト失敗（「このスイープでは検査したい場所が検査されていない」）。

#### 6.8.5 層 2（`yuzhu-server/tests/crash_kill9.rs`）

M3 の構成（4〜8 本のクライアントスレッド、`postgres` クレート、確定 / 不明の記録、ランダムな時刻に SIGKILL → 再起動 → 不変条件）に足す:

- **ワークロード**: 送金（RC、8 スレッド。M3 の銀行振込を複数ライターにする）、VACUUM を繰り返す 1 スレッド、FK のワークロード 1 スレッド（W10 の簡略版）、データベースの作成・削除を繰り返す 1 スレッド（`kk_<n>`）。
- **不変条件**: M3 の I1〜I4、I10 に加えて、I17（モデルは送金の合計とログで代用）、I19（SQL では見えないので、`yuzhu-dumpheap` 相当のオフライン検査ツールがなければ省く。U-6）、I22（全行に `FOR UPDATE NOWAIT`）、I23、I24。
- **回数**: 既定 20 回、夜間 200 回。`#[ignore]`（M3 のとおり）。
- **SCRAM 構成でも 1 回**: 認証付きの再接続（クラッシュ後に `pg_authid` から読めて接続できる）が通ること。

### 6.9 変異テスト（TS-D9。`crash_sim/mutation.rs`、`stress/mutation.rs`、`yuzhu-server/tests/auth.rs`）

**規則**（M3-Q13 のとおり）: 変異は `DebugKnobs` の隠しスイッチ。既定は無効で、コマンドライン・設定ファイル・SQL から変えられない。各章の担当が、自分の機能を壊すスイッチを `DebugKnobs` に足す（CR-5）。TS が検出テストを書く。**各変異について「固定シードの集合のうち少なくとも 1 つで、指定の検出器が失敗する」ことをテストにする**。検出されなければテスト失敗。

| # | スイッチ（持ち主） | 壊し方 | 検出するもの |
|---|---|---|---|
| 1 | `vacuum_skip_index_pass`（VC。03 C7。旧名 `vacuum_unused_before_index`） | 第 2 段（インデックスの削除）を省いて第 3 段（`LP_UNUSED`）へ進む（`LP_UNUSED` がインデックスの削除より先になる） | I16、S4 (g) |
| 2 | `vacuum_ignore_registered_snapshots`（LK。01 §6.7。VC と共有。03 の `vacuum_ignore_horizon` は統合） | horizon を実行中の XID だけで計算し、登録済みスナップショットを無視 | S4 (e)（RR の読み手の `sum` が変わる） |
| 3 | `freeze_skip_wal`（VC。03 C7。旧名 `skip_freeze_wal`） | 凍結を WAL に書かない | I19（クラッシュ後に凍結が消え、clog の切り詰めで引けない XID が残る） |
| 4 | `truncate_clog_before_control_fsync`（VC。03 C7 に足した） | 制御ファイルの `oldest_xid` を fsync する前に `pg_xact` を消す | I19、または起動の失敗 |
| 5 | `relfrozenxid_too_high`（VC。03 C7 に足した） | `yz_relxid` を実際より進める | I20 |
| 6 | `fsm_trust_corrupt`（VC。03 C7 に足した） | FSM のチェックサム不正を 0 のページとして扱わず、そのまま信じる（または panic） | I18 |
| 7 | `skip_update_wait`（RW。02 §7.6・§11-8 に足した） | 更新・削除が `BeingModified` で待たずに続行する | S1（RC の合計・`bal[i]` の再計算）、isolation の `lost-update` |
| 8 | `rr_skip_serialization_check`（RW。02 に足した） | RR で `Updated` / `Deleted` を 40001 にせず最新版を更新する | isolation の `rr-update-conflict`、S1 の RR |
| 9 | `epq_skip_recheck`（RW。02 に足した） | 待った後に WHERE を再評価しない | isolation の `epq-single`（#2） |
| 10 | `multixact_ignore_boundary`（RW。02 §7.6。旧名 `multixact_dead_as_live`） | 再起動後の古い MultiXactId を「メンバーが実行中」と扱う | I22 |
| 11 | `unique_skip_wait`（RW。02 に足した） | 一意検査が `WaitFor` で待たずに挿入する | S2（重複キー、インデックスと全件走査の不一致）、isolation の `unique-wait` |
| 12 | `fk_skip_key_share`（FK。07 §7.3a・R10 に足した） | 子の検査で親の行に `FOR KEY SHARE` を取らない | S3、I23、isolation の `fk-parent-delete-race` |
| 13 | `release_locks_before_commit_record`（LK） | D38 の順序を破り、ロックの解放を WAL の flush と clog より前にする | S1、isolation（待っていた側がコミット前の値を見る） |
| 14 | `lock_ignore_access_exclusive`（LK） | AccessExclusive が他のモードと衝突しない | isolation の `drop-while-reading`、`lock-table-queue` |
| 15 | `deadlock_detection_off`（LK） | デッドロックを検出しない | isolation の `deadlock-2way-row`（ランナーの `max_step_wait` で FAILED）、S1 のウォッチドッグ |
| 16 | `skip_pre_copy_checkpoint`（DB。09 §4.7。旧名 `createdb_skip_checkpoint`） | コピーの前のチェックポイントを省く | I24 (b)（ダーティなページがコピーに入らず、内容が違う） |
| 17 | `skip_dbase_wal`（DB。09 §4.7。旧名 `createdb_skip_wal`。検出には `skip_copy_fsync` と `DropUnsynced` を併用） | `DBASE/CREATE_FILE_COPY` を WAL に書かない | I24 (a)(b)（`DropUnsynced` で作りかけのコピーが残る / 欠ける） |
| 18 | `scram_skip_proof_check`（AU。08 §7.3a・C13 に足した） | クライアントの `ClientProof` を検証しない | `yuzhu-server/tests/auth.rs` の「誤ったパスワードで接続できてしまう」の検出、`tests/compat/auth` |

| 19 | `clog_truncate_ignore_floor`（VC。03 C7） | clog の切り詰めの境界 `floor` を `next_xid` にして切り詰める | V3（`XX001`） |

**スイッチ名の台帳（レビュー対応 R-09）**: スイッチ名は**持ち主の章の定義が正**で、この表はそれに合わせた（以前の初版は 03・02・09 の名前と食い違っていた）。上の 19 件のほかに、章が自分のテストのために持つ補助スイッチがある（10 章が検出テストを書くかは章の判断）: LK の `lock_ignore_conflicts`・`lock_disable_fifo`・`lock_disable_breakin`（01 §6.7）、RW の `skip_heap_lock_wal`・`skip_tuple_lock`（02 §7.6）、DB の `skip_copy_fsync`・`drop_dir_before_commit`（09 §4.7）。**スイッチを足す持ち主（F0 が `DebugKnobs` に項目を置く）**: LK（01 §6.7）、RW（02 §11-8）、VC（03 C7）、FK（07 R10）、AU（08 C13）、DB（09 §11-10）。
- **失敗の出し方**: 検出テストは、検出された不変条件の名前とシード、`N` を出して成功する。検出されなければ「変異 `<名前>` が検出されなかった（試した <k> 個のシード）」で失敗する。
- 既存の M3 の変異 7 件（FPW なし、コミットで flush しない、WAL-before-data、clog を書かない、制御ファイル 1 スロット、REDO の LSN 判定、ディレクトリの fsync）は、M5 のワークロード（W8〜W11）でも検出されること（モデルの拡張で検出が弱くならない）を、`mutation.rs` の再実行で確かめる。

### 6.10 ドライバと互換の CI（TS-4。`tests/compat/`）

#### 6.10.1 共通

- 入口: `tests/compat/run.sh --target pg|yuzhu --suite drivers|auth|pgbench|psql [--driver NAME] [--auth trust|scram] [--port N]`。各ドライバの結果を「ドライバ名・シナリオ ID・成功 / 失敗 / 既知の差（KD）」の表で出力し、失敗があれば終了コード 1。
- **同じシナリオを PostgreSQL 17 に流して通ることが先**（期待値の確認）。シナリオは「結果を PostgreSQL の結果と比べる」のではなく、**言語側のアサーション**（値の往復が一致、SQLSTATE が一致、行数が一致）で書く。
- 依存は**固定した版**で入れる（Cargo.lock、uv.lock、Maven の版、package-lock.json）。ネットワークが要るので、**claude-sandbox（オフライン）では実行しない**（CI だけ。ローカルは `--driver` で選んで実行。U-7）。
- **既知の未対応（KD）に当たるシナリオは書かない**。ドライバが内部でそれを使う場合（名前つきカーソルの `DECLARE`、`DatabaseMetaData` など）は、「0A000 / 42P01 で失敗するのが期待」というシナリオとして書き、失敗の SQLSTATE を主張する（M6 で解消したら期待を反転）。

#### 6.10.2 ドライバごとのシナリオ

各ドライバで共通のシナリオ ID: **C1** 接続（trust と SCRAM）、**C2** パラメータ付き SELECT の型の往復（下の表）、**C3** 準備済み文の再利用、**C4** トランザクション、**C5** エラーの SQLSTATE とフィールド、**C6** 行数制限（PortalSuspended）、**C7** パイプライン / バッチ、**C8** 同時接続の送金（S1 のドライバ版。8 並列）、**C9** キャンセル、**C10** 失敗後の復帰（Sync 後に次のクエリが通る）。

**C2 の型の往復**: `int2`、`int4`、`int8`、`float4`、`float8`、`bool`、`text`、`varchar`、`bpchar`（`char(5)`）、`numeric`（`12345.678900` と NaN）、`date`、`timestamp`、`timestamptz`（UTC と `+09`）、`interval`（`1 mon 2 days 03:04:05.678`）、`time`、`bytea`（空と非 ASCII）、`uuid`、`int4[]`（NULL 要素を含む）、`text[]`、NULL。パラメータ（クライアント → サーバ）と結果（サーバ → クライアント）の両方向を、テキストとバイナリの両方で（ドライバが対応する範囲）。**値は PostgreSQL に対して同じコードで往復して通ること**。

| ドライバ（版の固定） | 形式 | 固有のシナリオ | 備考 |
|---|---|---|---|
| **tokio-postgres**（`tests/compat/drivers/tokio-postgres/`。独立した Cargo プロジェクト。Rust） | パラメータも結果もバイナリ | `query` / `prepare` / `execute`（型推論の結果と `Describe` の型 OID が PostgreSQL と同じ。`stmt.params()` の OID 列を比べる）、`query_portal` と `max_rows`、`futures::join!` で 100 本のクエリを 1 接続に流すパイプライン（C7）、`batch_execute`（Simple）、`cancel_token`（C9）、`copy_in` / `copy_out`（XQ-6 が入ったら。それまで `0A000` を期待） | 型の静的表にある OID ならカタログ問い合わせは起きない。`channel_binding=prefer`（TLS なしでは `n,,`） |
| **psycopg 3**（`psycopg[binary]`、`tests/compat/drivers/psycopg/`。Python、uv） | 既定テキスト。`binary=True` の接続とカーソルも | `str` パラメータ（OID 0 = unknown の推論）、`prepare_threshold` を超える反復（名前つきの `_pg3_<n>` と DEALLOCATE）、`cursor.executemany`（パイプライン）、`conn.transaction()`（**入れ子の `with conn.transaction()` は SAVEPOINT で 0A000 になる（KD-4）ことを期待**）、名前つきカーソル（`DECLARE`。KD-22 で失敗を期待）、`cursor.copy`（XQ-6 後） | libpq 17 が SCRAM を処理 |
| **pgJDBC**（`tests/compat/drivers/pgjdbc/`。Java、`java -cp postgresql-42.7.x.jar Main.java`） | 5 回目以降の一部の型がバイナリ（`prepareThreshold=5`、`binaryTransfer=true`） | **同じ文を 10 回以上ループ**（バイナリへの切り替えを踏む）、`Statement` と `PreparedStatement`、`setFetchSize(n)` + `autocommit=false`（PortalSuspended）、`addBatch` / `executeBatch`、`getGeneratedKeys`（`RETURNING`）、`setString` の varchar と int 列の比較が PostgreSQL と同じ 42883（C2 の注意）、`setTimestamp` / `getTimestamp`（バイナリの timestamptz と JVM のタイムゾーン）、`setObject(UUID)`、`setBytes`、`createArrayOf("int4")`、`Connection.cancel()`、`preferQueryMode=simple` / `extendedForPrepared` | `DatabaseMetaData.getTables` は KD-27 で失敗を期待（M6 で解消） |
| **node-postgres**（`pg` 8.x、`tests/compat/drivers/node-postgres/`） | パラメータはテキスト、型 OID はすべて 0（推論が必須） | `client.query(text, values)`、名前つき文（2 回目以降は Parse を省く）、`rowMode: 'array'`、`pg-cursor` の `read(n)`（PortalSuspended）、`Pool`（10 クライアント、C8）、型の読み取り（`int8` は文字列、`numeric`・`timestamptz`・`bytea`・`uuid`・配列） | pg_type は問い合わせない |
| **psql 17**（`tests/compat/psql/extended/`） | テキスト | `\parse` / `\bind_named` / `\bind` / `\close_stmt`（名前は PG17 の psql で確かめる。U-8）、`\d` の FK 表示（`Foreign-key constraints:` と `Referenced by:`。`pg_get_constraintdef` の出力が一致）、`\du`（`pg_roles`）、`\l`（作成した DB）、`\c <db>`、`\password`（`ALTER ROLE ... PASSWORD`。SCRAM のジョブ）、`\copy`（XQ-6 後） | **psql の出力を PostgreSQL 17 の出力と diff する**（OID と環境依存の値は置換。M4 の `tests/compat/psql` と同じ） |
| **pgbench**（`tests/compat/pgbench/`） | §6.10.3 | | |

#### 6.10.3 pgbench（extended / prepared / 同時接続）

| ID | コマンド | 確認 |
|---|---|---|
| G1 | `pgbench -i -s 1 -I dtgvpf`（f = FK。M4 の `dtgvp` に FK が加わる） | 完走。FK が張られる。`pg_constraint` の contype `f` が **5 件**（`pgbench_tellers_bid_fkey`・`pgbench_accounts_bid_fkey`・`pgbench_history_bid_fkey`・`pgbench_history_tid_fkey`・`pgbench_history_aid_fkey`。【実機】PG17.11 の `pgbench -i -I dtgvpf` で確認。07 §6.8 と同じ。レビュー対応 R-25。以前の 3 件は PostgreSQL に流した時点で落ちた） |
| G2 | `pgbench -n -M simple -c 8 -j 4 -T 20`（tpcb-like。スケール 1 で `bid = 1` の行ロックが最大に競合） | 完走。**不変条件**: `sum(abalance) = sum(tbalance) = sum(bbalance) = sum(delta)`（pgbench_history） |
| G3 | `-M extended -c 8 -j 4 -T 20` | G2 と同じ。`$1` の型推論（`abalance + $1`） |
| G4 | `-M prepared -c 32 -j 8 -T 30`（同時接続 32） | G2 と同じ。準備済み文が 32 接続で作られる。`-M prepared` と `-M simple` の TPS の比（P7） |
| G5 | `PGOPTIONS='-c default_transaction_isolation=repeatable\ read' pgbench -n -M prepared -c 8 -T 20 --max-tries=20 --failures-detailed` | RR で完走。40001 が `serialization failures` として数えられ、再試行で収束。不変条件 |
| G6 | `pgbench -n -S -M prepared -c 16 -j 8 -T 20`（select-only） | 完走。読み取りのスケール（P2） |
| G7 | `pgbench -n -C -c 8 -T 10`（トランザクションごとに接続） | 接続の確立の負荷（SCRAM のときも）。P8 |
| G8 | G2 を `autovacuum = on` の yuzhu で 300 秒（夜間） | 完走。**膨張しない**: `pgbench_branches` と `pgbench_tellers` のサイズが一定の上限以下（P9） |
| G9 | `pgbench -n -f custom.sql -c 8`（`FOR UPDATE` と `INSERT ... RETURNING` を使うスクリプト） | 完走。RETURNING と LockRows を負荷の下で |

`PSQL` / `PGBENCH` は環境変数で差し替えられる（docker の postgres:17 イメージの `psql` / `pgbench` を使う場合も同じ入口）。

#### 6.10.4 SCRAM 構成と認証の互換（`tests/compat/auth/`、`yuzhu-server/tests/auth.rs`）

| ID | 内容 |
|---|---|
| A1 | `yuzhu.sh start --auth scram` で、**slt（m1〜m5）、isolation、ドライバ、pgbench を SCRAM で流す**（ジョブ `scram` のマトリクス。§6.10.6） |
| A2 | **誤ったパスワード**: SQLSTATE 28P01、メッセージ `password authentication failed for user "alice"`（FATAL）。**存在しないユーザーでも同じメッセージ**（偽のソルトで最後まで進む。00 §4.11）。`NOLOGIN` のロールは 28000 `role "alice" is not permitted to log in` |
| A3 | **hba の互換**: `tests/compat/auth/hba/*.conf`（`trust`、`reject`、`password`、`scram-sha-256` の組み合わせ。ユーザー・データベース・アドレスの照合）を yuzhu の `yuzhu_hba.conf` と PostgreSQL の `pg_hba.conf` の**同じ内容**として使い、(ユーザー, DB, アドレス) → 結果（接続できる / 28000 / 28P01）の表が一致する。`md5` の行は PostgreSQL では MD5 を使うため比べない（KD-12） |
| A4 | **秘密の相互互換**（`tests/compat/auth/secret-compat/`）: yuzhu で `CREATE ROLE x PASSWORD 'pw'` → `SELECT rolpassword FROM pg_authid`（スーパーユーザー）の値を PostgreSQL の `ALTER ROLE x PASSWORD '<値>'` に設定して `pw` で PostgreSQL に接続できる。逆（PostgreSQL の値を yuzhu に設定）も。ソルトと反復回数は取り出して比べる |
| A5 | `VALID UNTIL`（期限切れは 28P01）、`CONNECTION LIMIT`（53300）、`ALTER ROLE ... PASSWORD NULL`（SCRAM が使えない → 28P01）、`password` 方式（平文要求 `R 3`）、`scram_iterations` の変更（既存の秘密は影響を受けない） |
| A6 | `\password`（psql）で変更 → 再接続 |
| A7 | **`authentication_timeout`**: 認証の途中で止まったクライアントが、設定の時間で切断される（短い値で起動して確かめる。生のソケット） |
| A8 | **SCRAM のクライアント間の互換**: tokio-postgres、libpq（psql）、pgJDBC（`com.ongres.scram`）、node-postgres（`lib/crypto/sasl.js`）、psycopg 3 の 5 つのクライアントすべてで接続できる |
| A9 | チャネルバインディング: TLS なしでは `channel_binding=require` の接続は失敗する（クライアント側の検査。KD-12） |

#### 6.10.5 PostgreSQL 17 に対する期待値の確認

すべての `tests/compat/` のシナリオを、`--target pg`（PostgreSQL 17）で先に通す。yuzhu だけで通るシナリオは作らない（KD を主張するシナリオ — 例: 入れ子の `conn.transaction()` の 0A000 — は「yuzhu では 0A000、PostgreSQL では成功」を `if target == yuzhu` で書き分け、台帳の KD を `# KD-4` で指す）。

#### 6.10.6 CI のジョブ（`.github/workflows/ci.yml` に足す）

| ジョブ | 内容 | 起動 | 失敗の扱い |
|---|---|---|---|
| `lint-tests` | `tests/tools/lint-tests.sh`、`tests/tools/q-index.sh --check` | PR | 必須 |
| `slt-pg`（既存の拡張） | PostgreSQL 17 に m1〜m5。**simple と extended の 2 通り** | PR | 必須 |
| `slt-yuzhu`（既存の拡張） | yuzhu に m1〜m5。simple と extended | PR | 必須（M5 の実装が揃った時点で `continue-on-error` を外す） |
| `isolation`（既存の拡張） | `specs/` を pg / yuzhu × simple / extended で。`ported/` を pg（期待確認）と yuzhu（variant 込み）で | PR（共有 specs は simple・extended とも。ported は simple のみ）／夜間（ported の extended） | 共有は必須。ported は必須（variant 込みで通る） |
| `restart-yuzhu` / `restart-pg`（既存の拡張） | `--restart` と `--crash tests/restart/m3 tests/restart/m5` | PR | 必須 |
| `crash-sim`（既存の拡張） | 層 1 を W1〜W11 で。決定性の試験を先に。合計 180 秒以内（PR）。変異テスト | PR | 必須 |
| `stress` | プロセス内 S1〜S5（各 5 秒）。wire 版を yuzhu と PostgreSQL に（S1、S2、S5） | PR | 必須 |
| `drivers` | `compat/run.sh --suite drivers`（trust）。**PR は tokio-postgres と psql と pgbench の G1・G2・G3（各 20 秒）**、残りのドライバと G4〜G9 は夜間 | PR（最小）／夜間（全部） | PR 分は必須。夜間は通知 |
| `scram` | 上のマトリクスを `--auth scram` で。PR は slt（m5 の一部）と tokio-postgres と A2・A3・A4、夜間は全部 | PR（最小）／夜間 | PR 分は必須 |
| `nightly-crash` | 層 1 の長時間ランダム（全 N）、層 2（200 回）、変異の全シード | 夜間 | 通知 |
| `nightly-stress` | S1〜S7 を 600 秒 × スレッド {2, 8, 16}、PostgreSQL にも | 夜間 | 通知 |
| `perf` | `tests/perf/run.sh`（P1〜P9）。前回の夜間の結果と比べて 15% を超える退行を失敗にする | 夜間 | 通知（M5 の完了判定は §7.3） |

- **PR のジョブの合計時間**は 15 分以内を目標にする（`drivers` と `scram` は最小のシナリオだけ、`crash-sim` は固定シード）。超えたら夜間へ移す。
- キャッシュ: `sqllogictest-bin`、`tests/tools/*/target`、Cargo、uv、Maven、npm のキャッシュ。ドライバの版は lock ファイルで固定する。
- PostgreSQL 17 の版は `PG_IMAGE`（既定 `postgres:17`）。ログの先頭に `SHOW server_version` を出す。**期待値を生成した版を `tests/PG_VERSION` に記録する**（変わったら再生成して差分をレビュー）。

### 6.11 性能の確認項目（TS-D11。`tests/perf/`）

測定は `tests/perf/run.sh --target yuzhu`（pgbench を中心）が JSON を出す。環境はコア数・ディスク（tmpfs か実ディスク）・`fsync` を記録し、**同じ環境で M4 のバイナリ（M4 の完了タグ）と M5 のバイナリを比べる**。`baseline.json` は M4 の完了時の値（M4 の K が作る。無ければ M4 の完了コミットで最初に測る）。

| ID | 項目 | 測定 | ゲート |
|---|---|---|---|
| P1 | 単一接続の書き込み | `pgbench -n -M prepared -c 1 -T 30`（tpcb-like、スケール 10） | M4 比で TPS が 90% 以上（ロックマネージャと XID ロックのオーバーヘッドが 10% 以内） |
| P2 | 読み取りのスケール | `pgbench -n -S -M prepared -c {1, 4, 8} -T 20`（スケール 10） | c=8 の TPS が c=1 の 3 倍以上（**コアが 8 以上のとき**。グローバルロックがない。clog のロックなし読み） |
| P3 | 書き込みのスケール（競合が少ない） | `pgbench -n -M prepared -c {1, 8} -T 30`（スケール 100） | c=8 の TPS が c=1 の 1.5 倍以上（fsync が支配的でも素朴なグループコミットで伸びる。暫定。U-9） |
| P4 | 書き込み（競合が最大） | G2（スケール 1、c=8） | 完走し、TPS を記録（ゲートなし）。40001 / 40P01 が 0（RC） |
| P5 | ロックマネージャ単体 | `LockManager` の取得・解放 100 万回（競合なし、1 スレッド、`#[ignore]` のテスト） | 1 組あたり 2 マイクロ秒以下（暫定） |
| P6 | デッドロック検出の遅延 | 2 者のデッドロック（`deadlock_timeout = 1s`）、100 者のサイクルの検出 | 40P01 までが 1.0〜1.5 秒。100 者の DFS が 10 ミリ秒以下 |
| P7 | Extended のオーバーヘッド | G3 / G4 と G2 の TPS の比 | prepared が simple の 95% 以上（Bind ごとの再解析のコスト。D19） |
| P8 | 接続の確立 | G7（`-C`）、trust と SCRAM | SCRAM が trust の 70% 以上の接続数 / 秒（サーバ側は HMAC だけ） |
| P9 | VACUUM と膨張 | (a) 100 MB の表（インデックス 1 本）の 50% を DELETE して VACUUM、(b) G8（autovacuum on、300 秒） | (a) 30 秒以内、メモリが `maintenance_work_mem` 以内。(b) `pgbench_branches` / `pgbench_tellers` のサイズが初期の 10 倍以下 |
| P10 | `CREATE DATABASE` | `template1`（空）からの作成時間と、100 MB のテンプレートからの作成時間 | 空は 1 秒以内。100 MB は記録だけ |

- **ゲートの意味**: 退行ゲートは M4 比。絶対値の目標は M6 で立てる（TS-D11）。**暫定**の印のあるものは M5 の完了時に実測して上限を決め直す（`M5-TS-Q3`）。
- 未達の扱い: ゲート未達は M5 の完了を止める（原因を調べて直すか、数値を見直す理由を `QUESTIONS.md` に書いて承認を得る）。

---
## 7. テスト

この章のテストは「テストのためのテスト」と、M5 全体の完了条件。各機能のテストは各章が書く。

### 7.1 テスト基盤自体のテスト

| 対象 | 内容 |
|---|---|
| 分離性ランナー（`tests/tools/isolation`、`cargo test`） | (1) `split_statements` の単体テスト（§6.2 の列挙）、(2) extended 送信の出力が simple と同じになることを、**内蔵の疑似サーバ**（Parse / Bind / Execute / Sync を受けて固定の応答を返す小さな TCP サーバ）で確かめる、(3) 制御接続が Parse を 1 回だけ送り、判定のたびに Bind / Execute / Sync を送ること、(4) `--variant` の台帳検査（台帳にない variant で失敗）、(5) `--timeout-scale`、(6) `validate-pg-suite.sh` を PostgreSQL 17 に対して流し、**M3 のときの 120 / 123 から後退しない**（拡張でランナーが壊れていない確認。extended でも同じ 120 以上を目標にし、差の原因を記録する。U-10） |
| `lint-tests.sh` | 意図的に壊した fixture（台帳にない KD、`# KD-` のない `onlyif yuzhu`、接頭辞違反、`db error:` の残り）のそれぞれで失敗すること。正しい fixture で成功すること |
| `q-index.sh` | 章のファイルから `M5-<略号>-Q<n>` の見出しを集め、§10.2 の表と一致するか（欠けている ID、表にない ID を報告） |
| アクタースケジューラ | (1) 2 アクターの待ち（A が UPDATE、B が同じ行を UPDATE → `Blocked`、A の COMMIT で `settle()` が B の完了を返す）、(2) 3 者の待ち行列、(3) デッドロック（`deadlock_timeout` の違いで被害者が決まる）、(4) `hang` の検出（わざと止める）、(5) `abandon()` で待ちのアクターが解放される、(6) **決定性**: 同じスクリプトを 100 回流して `SimVfs` の操作列のハッシュが一致 |
| ストレスの部品 | (1) `Invariant` の `check_live` が、わざと壊したワークロード（合計を変える UPDATE を混ぜる）で失敗する、(2) ウォッチドッグが、わざと止めたワークロードで失敗し `lock_status()` を出す、(3) `OpRing` と失敗ログの出力、(4) 予期しない SQLSTATE（XX000）で失敗する |
| `CrashCoverage` | 既知の WAL 列に対して分類が期待どおり。必須の分類が欠けたら失敗 |
| 変異テストの枠 | 検出されない変異を人工的に作り（何も壊さないスイッチ）、テストが「検出されなかった」で失敗すること |
| `run.sh` | `--list`、`--protocol`、`--password`、`.db` の読み取り（疑似の `sqllogictest` を `SLT_BIN` で差し替える） |

### 7.2 実行コマンド（まとめ）

```sh
# 1. 静的検査と単体（CLAUDE.md と同じ）
(cd impl/rust && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test)
tests/tools/lint-tests.sh

# 2. 共有 SQL テスト（m1〜m5）。PostgreSQL 17 と yuzhu、simple と extended
tests/pg.sh start
tests/run.sh --target pg --protocol simple
tests/run.sh --target pg --protocol extended
tests/yuzhu.sh start --port 5433
tests/run.sh --target yuzhu --port 5433 --protocol simple
tests/run.sh --target yuzhu --port 5433 --protocol extended

# 3. 再起動・クラッシュ
tests/run.sh --target yuzhu --restart --crash tests/restart/m3 tests/restart/m5

# 4. 分離性
ISO=${CARGO_TARGET_DIR:-tests/tools/isolation/target}/release/yuzhu-isolation
$ISO --port 55432 tests/isolation/specs                                   # PostgreSQL: 期待の確認
$ISO --port 5433 tests/isolation/specs                                    # yuzhu（pg 判定）
$ISO --port 5433 --protocol extended tests/isolation/specs
$ISO --port 5433 --variant yuzhu-m5 --skip-list tests/isolation/ported/skip.txt tests/isolation/ported/specs

# 5. クラッシュ試験とストレス
(cd impl/rust && cargo test --release -p yuzhu-core --test crash_sim && cargo test --release -p yuzhu-core --test stress)
(cd impl/rust && cargo test --release -p yuzhu-server --test crash_kill9 --test concurrency -- --ignored)    # 夜間

# 6. ドライバと互換、SCRAM
tests/compat/run.sh --target yuzhu --suite drivers
tests/yuzhu.sh clean && tests/yuzhu.sh start --port 5433 --auth scram && tests/compat/run.sh --target yuzhu --suite auth --auth scram

# 7. 性能
tests/perf/run.sh --target yuzhu --compare tests/perf/baseline.json
```

### 7.3 M5 の完了条件（Done の定義）

次のすべてが満たされたとき、M5 は完了とする。各項目は「どのジョブ / コマンドで確認するか」を持つ（§6.10.6、§7.2）。

| # | 条件 | 確認 |
|---|---|---|
| 1 | `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` がすべて通る。`#![forbid(unsafe_code)]` を全クレートで維持。**`yuzhu-core` が crates.io の外部クレートに直接依存していない**（D29。`cargo tree -p yuzhu-core --depth 1 --edges normal` が path 依存だけ）。M3 / M4 のデータディレクトリが明確なエラー（`format_version` の不一致）で起動を拒否される（D35） | `rust` ジョブ |
| 2 | `tests/slt/m1`〜`m5` が PostgreSQL 17 と yuzhu の両方で、simple と extended の両方で全通過（失敗 0、スキップは `onlyif yuzhu` と `skipif postgres-extended` のみ） | `slt-pg`、`slt-yuzhu` |
| 3 | `tests/isolation/specs`（共有）が PostgreSQL と yuzhu で完全一致（simple と extended）。M3 の spec の書き直し（§6.3）が済み、M5 で新規の共有 spec（§6.4.1）がすべて通る | `isolation` |
| 4 | `tests/isolation/ported`: 完全一致のものは一致、variant のものは variant と一致、`skip.txt` 以外の spec がすべて流れている。**すべての variant と skip が台帳の KD に紐づく** | `isolation` |
| 5 | `tests/restart` 直下、`m3`、`m5` の全シナリオが `--crash` で通る（pg と yuzhu） | `restart-*` |
| 6 | クラッシュ試験 層 1: M3 のワークロード 1〜5、M4 の 6〜7、M5 の W8〜W11 が、決まったスイープで I1〜I24 を満たす。`CrashCoverage` の必須分類がすべて踏まれる。決定性の試験が通る。**変異テスト（M3 の 7 件 + M5 の 18 件 = 25 件）がすべて検出される** | `crash-sim`、`nightly-crash` |
| 7 | 層 2（`crash_kill9`）: 夜間 200 回で不変条件違反 0（SCRAM 構成の 1 回を含む） | `nightly-crash` |
| 8 | 並行ストレス S1〜S7: 夜間 600 秒 × スレッド {2, 8, 16} × 3 シードで違反 0、ウォッチドッグの発動 0、RC の 40001 が 0。同じ wire 版が PostgreSQL 17 でも通る（S1、S2、S5） | `nightly-stress` |
| 9 | ドライバと互換: tokio-postgres、psycopg 3、pgJDBC、node-postgres、psql の C1〜C10（既知の未対応の「失敗を期待」を含む）が trust と SCRAM の両方で通る。pgbench G1〜G9 が完走し不変条件を満たす（G8 の膨張の上限を含む）。認証 A1〜A9 が通る | `drivers`、`scram`（夜間の全部） |
| 10 | 台帳と lint: `lint-tests.sh --release` が通る。**`skipif yuzhu` が 0 件**。すべての `onlyif yuzhu`・variant・skip が台帳の KD を指し、台帳の KD はすべて 1 件以上のテストか章の確認事項に紐づく | `lint-tests` |
| 11 | 性能ゲート P1〜P10（§6.11）を満たす。数値を `tests/perf/baseline.json`（M5 の値）に記録する | `perf` |
| 12 | M4 との統合チェックリスト（§8.6 の A1〜A12）がすべて済み、M4 の章（01〜11）が確定したあとの突き合わせ（§1.3 の注意）を各章が済ませた | 各章の担当 |
| 13 | ドキュメント: `tests/README.md` に M5 の実行方法と規則（§6.5.2、§6.6）、既知の差の一覧（台帳から生成）を書く。`QUESTIONS.md` に M5 の確認事項（99-questions.md。M5-Q1〜）と既知の差の要約を追記し、`PROGRESS.md` を M5 完了に更新する（**この 2 つは M5 の完了時に主エージェントが更新する。この章の担当は編集しない**）。`spec/design/m5-changes.md` に §11.2 の内容を反映する | レビュー |

- 13 の `README.md`・`QUESTIONS.md`・`PROGRESS.md` の更新は、この設計書の作成時には行わない（この章の担当は `spec/design/m5/` の Markdown だけを編集する）。

---

## 8. 実装の分担と工数

### 8.1 TS の WP（00 §7 を詳しくしたもの。ID は変えない。枝番を付ける）

| ID | 内容 | 依存 | 日数 | 00 との差 |
|---|---|---|---|---|
| TS-1a | ランナーの拡張（`pg` 判定を既定に、制御接続の Extended、`--protocol extended`、`split.rs`、`--timeout-scale`、`--variant` の台帳検査、`--skip-list`、疑似サーバの単体テスト）、`port-pg-specs.sh`、移植 spec の取得と PostgreSQL 17 の期待の生成、`skip.txt` の第 1 版。**PostgreSQL だけで完結する** | なし（M4 の実装中に始めてよい。D49 の「新規ファイルだけ」の範囲） | 2.5 | 依存を緩める（00 は LK-5、RW-3） |
| TS-1b | yuzhu での通し: M3 の spec の書き直し（§6.3）、共有 spec の新規（§6.4.1）、variant の確定、`ported/` の分類の確定（流して判定するものを決める）、extended での差の調査 | LK-5（`pg_isolation_test_session_is_blocked`）、RW-3 | 2.5 | 00 の TS-1 の後半 |
| TS-2a | ストレスのハーネス（`StressTarget`、`Workload`、チェッカー、ウォッチドッグ、`OpRing`）、プロセス内の `TestCluster` 実装（1.0 日。**LK-3 の後に先行**）、行ロック・XID ロックだけの S0。S1（RC）（0.5 日。**RW-3（EPQ）と RW-5（複数ライターの B+Tree）の後**）。**最初の複数ライターの不具合を見つける** | ハーネス・S0: LK-3。S1（RC）: RW-3、RW-5 | 1.5 | 00 は RW-4 以降。以前の 10 章は RW-1・LK-3 だけに依存して S1 を 14.5〜16 に置いたが、S1 は PRIMARY KEY 付きの表への並行 UPDATE を検証し、RC の最新版での再評価（RW-3）と複数ライターの B+Tree（RW-5）が要るので成立しなかった。ハーネスを先に作る形に直した（R-20） |
| TS-2b | S1（RR、変種）、S2〜S7、wire 版（`concurrency.rs`）、PostgreSQL 17 への適用、P5・P6 | RW-4、VC-3、RW-5、DB-2 | 2.0 | |
| TS-3a | アクタースケジューラ、決定性の試験、`TestClusterOptions` の拡張、M3 のワークロード 3 のデッドロック版 | LK-5 | 1.0 | |
| TS-3b | W8（VACUUM）、W9（MultiXact）、I16〜I22、`CrashCoverage` | VC-3、RW-2 | 1.0 | |
| TS-3c | W11（CREATE / DROP DATABASE）、I24、層 2 のデータベースのワークロード | DB-2、DB-3 | 0.5 | |
| TS-3d | W10（FK の CASCADE）、I23、層 2 の FK のワークロード | FK-2 | 0.75 | |
| TS-3e | 変異テストの枠と 18 件の検出テスト、層 2 の拡張 | 各章のスイッチ（CR-5） | 0.75 | |
| TS-4a | `tests/compat/drivers/*`、`auth/`、`psql/extended/`、`pgbench/` のシナリオとランナー（**PostgreSQL に対して先に通す**） | なし（PostgreSQL だけ） | 2.5 | 00 は XQ-7 の後。シナリオを先に作って XQ-7 が使う |
| TS-4b | CI ジョブ（`drivers`、`scram`、`stress`、`perf` ほか）、SCRAM 構成の通し、`tests/perf/run.sh` と P1〜P10 の測定、`tests/yuzhu.sh --auth` | XQ-7、AU-2 | 1.5 | |
| TS-5a | slt の規約と基盤: `lint-tests.sh`、`known-diffs.toml` の初版（KD-1〜KD-28）、`run.sh` の `--protocol` / `--password` / `--list` / `.db`、`pg.sh` / `yuzhu.sh` の `--auth`、`q-index.sh`、`tests/README.md` の M5 の節の下書き。**各章が `tests/slt/m5` に書き始める前に済ませる** | なし | 1.5 | |
| TS-5b | 全章の slt と restart の統合確認（PostgreSQL で 2 回通る、yuzhu で simple / extended、接頭辞の衝突、`skipif` の撲滅）、既知の差の一覧の生成、`QUESTIONS.md` への追記案 | 各章 | 3.5 | |
| 合計 | | | **21.5** | 00 の TS 合計 17 から +4.5 |

**見直しの理由**（00 の TS-1〜TS-5 の合計 17 日 → 21.5 日）: (1) Extended Query での isolation 実行（TS-D3）と制御接続の Extended 化（TS-D2）を足した（TS-1 +1）。(2) クラッシュ試験のアクタースケジューラ（TS-D8）が M3 のハーネスにない新しい部品で、決定性の試験と 4 つのワークロード・9 つの不変条件・18 の変異を足す（TS-3 +1）。(3) ドライバのシナリオを XQ-7 の前に作る分、TS-4 を分けた（TS-4 +1）。(4) 台帳・lint・規約の基盤（TS-5 +1）。(5) 早期に不具合を見つけるため TS-2 を 2 つに分けた（TS-2 +0.5）。

### 8.2 M5 全体の WP 表（00 §7 を最新にしたもの）

開始・終了は M4 のマージ（F0a の開始）を 0 日とした稼働日。**日数・依存は 00 §7 の最新の表と同じ**（レビュー対応 R-20 で、章 01〜09 の確定した見積もり・依存を反映して引き直した。F0 は F0a・F0b-1・F0b-2 に分け、M4 のファイルへの修正の F1 を足した。RW-2 は F0b-1 だけに依存し、RW-1c が RW-2 を待つ。VC-3 は RW-5 の後に終わり、VC-4 は DB-1 に、VC-6 は VC-5 に依存する。DB-2 は LK-3 と AU-3 に依存する。XQ-2 +1、TY-5 +1.5、DB-3 +0.5）。レーンは §8.3。

| ID | 日数 | 開始 | 終了 | 依存 | レーン | ID | 日数 | 開始 | 終了 | 依存 | レーン |
|---|---|---|---|---|---|---|---|---|---|---|---|
| F0a | 2.5 | 0 | 2.5 | M4 | A | XQ-5 | 3 | 14.5 | 17.5 | XQ-2、LK-4（RR の結合 0.5 だけ RW-4 の後） | I |
| F0b-1 | 1.5 | 2.5 | 4 | F0a | A | XQ-6 | 3 | 14 | 17 | XQ-2、M4 | L |
| F0b-2 | 2.5 | 4 | 6.5 | F0b-1 | A | XQ-7 | 3 | 18 | 21 | XQ-5、RW-4 | L |
| F1 | 3 | 4 | 7 | F0b-1 | A2 | TY-1 | 3 | 6.5 | 9.5 | F0b-2 | M |
| LK-1 | 3 | 4 | 7 | F0b-1 | B | TY-2 | 1.5 | 9.5 | 11 | TY-1、M4 | N |
| LK-2 | 1.5 | 7 | 8.5 | LK-1 | CC | TY-3 | 0.5 | 11 | 11.5 | TY-1、M4 | N |
| LK-3 | 4 | 7 | 11 | LK-1 | B | TY-4 | 2 | 11.5 | 13.5 | TY-1 | N |
| LK-4 | 3.5 | 11 | 14.5 | LK-3、M4 | B | TY-5（5c を含む） | 6.5 | 9.5 | 16 | TY-1 | M |
| LK-5 | 1.5 | 7 | 8.5 | LK-1、F0b-1、M4 | C | TY-6 | 2 | 16 | 18 | TY-1、XQ-4、TY-4、TY-5 | N |
| RW-1a・1d | 2 | 4 | 6 | F0b-1 | D | TY-7 | 2 | 16 | 18 | TY-2〜5 | M |
| RW-2 | 2.5 | 4 | 6.5 | F0b-1 | E | TD-1 | 2.5 | 7 | 9.5 | F0b-2、M4、F1 | O |
| RW-3a・3b | 3 | 6.5 | 9.5 | F0b-2、M4 | D | TD-2 | 2.5 | 9.5 | 12 | TD-1 | O |
| RW-1b・1c | 3 | 11 | 14 | RW-1a、LK-3、RW-2、F1 | D | TD-3 | 4 | 9.5 | 13.5 | TD-1 | P |
| RW-3c | 2 | 14 | 16 | RW-1c、RW-3b | D | TD-4 | 2.5 | 12 | 14.5 | TD-1 | O |
| RW-4 | 2 | 16 | 18 | RW-3、LK-3、LK-4 | D | TD-5 | 2 | 14.5 | 16.5 | TD-1、XQ-4 | O |
| RW-5 | 3 | 14 | 17 | RW-1c、M4 | F | FK-1 | 4 | 12 | 16 | TY-5（`ArrayValue` を先に）、F0b-2、M4 | Q |
| RW-6 | 1.5 | 16 | 17.5 | M4、RW-3 | F | FK-2 | 6 | 16 | 22 | FK-1、RW-3 | Q |
| VC-1 | 4 | 4 | 8 | F0b-1 | G | FK-3 | 2 | 22 | 24 | FK-2、RW-4 | Q |
| VC-2 | 3 | 8 | 11 | VC-1 | G | FK-4 | 3 | 22 | 25 | FK-2、VC-5、F1 | R |
| VC-3 | 5 | 14.5 | 19.5 | VC-1、VC-2、LK-4、RW-5、M4 | G | FK-5 | 2 | 22 | 24 | FK-2 | R |
| VC-4 | 4 | 19.5 | 23.5 | VC-3、LK-3、DB-1 | G | AU-1 | 2.5 | 0 | 2.5 | なし（D49 で先行） | S |
| VC-5 | 2 | 19.5 | 21.5 | LK-4、VC-3 | H | AU-2 | 3 | 6.5 | 9.5 | AU-1、F0b-2 | S |
| VC-6 | 2 | 21.5 | 23.5 | VC-3、VC-5 | H | AU-3 | 3.5 | 8.5 | 12 | AU-1、LK-5、F0b-2 | C |
| XQ-1 | 2.5 | 6.5 | 9 | F0b-2 | I | DB-1 | 2 | 8.5 | 10.5 | LK-1、LK-3 の `BackendRegistry`、F0b-1 | T |
| XQ-2 | 5 | 9 | 14 | XQ-1、M4 | I | DB-2 | 4 | 12 | 16 | DB-1、LK-3、AU-3、F0b-2 | T |
| XQ-3 | 3.5 | 7 | 10.5 | M4、F1 | J | DB-3 | 2.5 | 16 | 18.5 | DB-1、DB-2 | T |
| XQ-4 | 1.5 | 6.5 | 8 | F0b-2 | K | DB-4 | 2 | 18.5 | 20.5 | DB-2、DB-3、TS-3a | T |
| **TS 合計** | 21.5 | | | §8.1 | U、V、W | | | | | | |

（TS の WP の開始・終了は §8.1 の表と §8.3 の「早期に入れる試験」。TS-2a のハーネスは LK-3 の後の 11〜12 日、S1（RC）は RW-3c・RW-5 の後の 17〜17.5 日。）

**合計**: **172.5 日**（00 §7 と同じ。初版の 158 日 + F0 の拡大 4 + F1 3 + XQ-2 1 + TY-5 1.5 + DB-3 0.5 + TS の見直し 4.5）。章の見積もりがさらに動いたら、この表の「日数」を更新する。

### 8.3 クリティカルパス・並列度・レーン

**クリティカルパス**（開始 0 日から）:

```
F0a (2.5) → F0b-1 (1.5) → LK-1 (3) → LK-3 (4) → RW-1b/1c (3) → RW-3c (2) → FK-2 (6) → FK-4 (3) → 結合・安定化 (5)
  2.5          4            7          11          14             16          22          25          30 日
```

- **クリティカルパスは 25 日 + 結合・安定化 5 日 = 30 日（6 週間。稼働日 5 日 / 週）**。00 §1.3 と同じ。00 の初版（FK-3 終端で約 32 日）と、この章の初版（33.5 日）は、RW-1 を 1 つの 5 日の WP として LK-3 の後ろに置いていたが、02 §8 の枝番（RW-1a・1d・RW-2 は F0 だけに依存して LK の裏で済む）に合わせて引き直したら 30 日になった（FK-4 が FK-3 より長いことは同じ。CR-13）。**第 2 の経路**: F0b-2（6.5）→ TY-1（9.5）→ TY-5 の最初の 2.5 日（12）→ FK-1（16）→ FK-2 の開始。RW-3c と同じ日（16）に合流するので、TY-5 の `ArrayValue` / `encode_array` を先に FK-1 に渡せないと FK が遅れる。
- **結合・安定化の 5 日**（25〜30）: 全ジョブ（§6.10.6）を通し、夜間のストレスとクラッシュ試験を 1 回、性能ゲート（P1〜P10）、ドキュメントの更新（§7.3 の 13）、見つかった不具合の修正。TS-2a / TS-3 が早期に見つけた分が効いて縮むことを期待するが、**縮める前提では計画しない**。
- **余裕（スラック）**: クリティカルパスの外で余裕が大きいのは、TD 全体（終了 16.5。余裕 約 13 日）、TY-1〜TY-4（13.5 まで。約 16 日）、AU（12。約 18 日）、DB（20.5。約 9.5 日）、XQ-1〜XQ-4（14 まで。約 16 日）、XQ-5〜XQ-7（21。約 9 日）。**余裕が最も小さい非クリティカルの塊は VC-3 → VC-4 → VC-6（終了 23.5。約 1.5 日）、XQ-7（21。約 4 日）、TS-5b（25.5）**。VC-3 は RW-5（17）の後に終わるので、**RW-5 か LK-4 が 2 日遅れると VC の系列がクリティカルパスに乗る**（リスク R-3、R-6）。DB-2 は LK-3（11）と AU-3（12）の後に始まる。
- 総量 172.5 日 ÷ 30 日 = **平均約 5.8 本**。ピークは 9.5〜12 日の約 13 本（LK-3、LK-4 前の待ち、VC-2、XQ-2、XQ-3、TY-5、TY-2、TD-1〜TD-3、AU-2、AU-3、DB-1、F1、TS-3a、TS-4a）。00 の「並列度 8〜10」はこの時期のピークに相当する。**上限が 10 本なら、DB-3 と TD-3 と TY-4 を後ろにずらしても完了時期は変わらない**（いずれも余裕が 9 日以上）。

**レーン（担当エージェントの割り当て案）**: 1 つのレーンは 1 本のエージェントが順に WP を進める（途中の空きは他のレーンの完了待ち）。

| レーン | WP（順） | レーン | WP（順） |
|---|---|---|---|
| A | F0a → F0b-1 → F0b-2 → （結合・不具合修正） | M | TY-1 → TY-5 → TY-7 |
| A2 | F1 | N | TY-2 → TY-3 → TY-4 → TY-6 |
| B | LK-1 → LK-3 → LK-4 | O | TD-1 → TD-2 → TD-4 → TD-5 |
| C | LK-5 → AU-3 | P | TD-3 |
| CC | LK-2 | Q | FK-1 → FK-2 → FK-3 |
| D | RW-1a・1d → RW-3a・3b → RW-1b・1c → RW-3c → RW-4 | R | FK-4 → FK-5 |
| E | RW-2 | S | AU-1 → AU-2 |
| F | RW-5 → RW-6 | T | DB-1 → DB-2 → DB-3 → DB-4 |
| G | VC-1 → VC-2 → VC-3 → VC-4 | U | TS-5a（0〜1.5）→ TS-1a（1.5〜4）→ TS-1b（16〜18.5）→ TS-5b（21.5〜25.5）（テスト基盤・共有テスト） |
| H | VC-5 → VC-6 | V | TS-3a（8.5〜9.5）→ TS-2a（ハーネス 11〜12、S1（RC）17〜17.5）→ TS-3c（18.5〜19）→ TS-3b（19.5〜20.5）→ TS-2b（19.5〜21.5）→ TS-3d（22〜22.75）→ TS-3e（22.75〜23.5）（ストレスとクラッシュ） |
| I | XQ-1 → XQ-2 → XQ-5 | W | TS-4a（3〜5.5）→ TS-4b（21〜22.5）（ドライバ・互換・性能） |
| J | XQ-3 | | |
| K | XQ-4 | | |
| L | XQ-6 → XQ-7 | | |

**工程表の引き直し**（章の見積もりが確定したとき）: (1) §8.2 の日数を更新、(2) 各 WP の開始 = 依存する WP の終了の最大値（同じレーンの前の WP の終了より前にしない）、(3) 終了 = 開始 + 日数、(4) クリティカルパスは「終了が最大」の WP から依存をさかのぼって求める。TS-1b・TS-2b・TS-3b〜3e・TS-5b は各章の完了に追従する（TS の終了は章の最終 WP の終了 + 0.5〜1 日）。

**早期に入れる試験**（工程の中で TS が早く不具合を見つけるための順序）:

| 時期（日） | 入れるもの | 効果 |
|---|---|---|
| 1.5〜4 | TS-1a: ランナーと移植 spec が PostgreSQL 17 で通る | yuzhu に向ける前に、期待値の誤りと spec の問題を先に潰す |
| 8.5〜9.5 | TS-3a: アクタースケジューラ | LK-5 の `is_blocked_by` の意味（CR-1）の検証 |
| 11〜12 | TS-2a のハーネス（`StressTarget`・`Workload`・チェッカー・ウォッチドッグ・`OpRing`・プロセス内の `TestCluster`）と、行ロック・XID ロックだけで書ける S0（INSERT と `SELECT ... FOR UPDATE` の並行。LK-3 だけが前提） | LK-3 の XID ロックと `ProcArray` の最初の統合試験 |
| 17〜17.5 | **TS-2a の S1（RC）**: PRIMARY KEY 付きの口座表への並行 UPDATE（RC で同じ行を奪い合う）。**RC の最新版での再評価（EPQ。RW-3c）と複数ライターの B+Tree（RW-5）が前提**なので、RW-1 だけでは不変条件（b）（更新の消失なし）を検証できない。以前の計画は RW-1・LK-3 だけに依存して 14.5〜16 に置いていたが、その時点では S1 が検証できない（レビュー対応 R-20） | RW-3 と RW-5 の最初の統合試験。S1 の早期検出の狙いは保つが、ハーネスを先に作り、S0 で LK-3・RW-1 の部分を先に確かめる |
| 18.5〜19 | TS-3c: W11（CREATE DATABASE のクラッシュ） | DB-2 / DB-3 のクラッシュ安全の確認 |
| 19.5〜20.5 | TS-3b: W8、W9 | VC-3 の最も危険な部分（A7）を VC-4 が始まる前に（VC-3 の終了直後） |
| 19.5〜21.5 | TS-2b: S1（RR）、S2〜S7 | RW-5（B+Tree の複数ライター）と VC-3 の統合 |

### 8.4 カットライン

00 §1.3 のカットライン（VC-6 → XQ-6 → TD-4 → VC-4 の clog の切り詰め → VC-5 の末尾の切り詰め → TY-5 の `ARRAY(SELECT)` と添字）に、**テスト側で落とす順**を足す。ほかの章の契約を変えずに落とせるものだけを挙げる。

| 順 | 落とすもの | 影響 |
|---|---|---|
| 1 | 夜間の長時間（S7、層 2 の 200 回、G8 の 300 秒）を M6 の soak に回す。PR のジョブは変えない | M5 の完了条件 6〜8 を「PR の分 + 夜間 1 回」に弱める |
| 2 | pgJDBC と node-postgres のシナリオを C1〜C5 だけにする（C6〜C10 を M6） | 準備済み文のバイナリ切り替えと PortalSuspended の確認が手動になる |
| 3 | `ported/` の「流して判定」の spec（約 8 本）を M6 に回す | KD-1 の影響範囲の確定が M6 に延びる |
| 4 | 変異テストを 18 件から主要な 10 件（#1、#2、#7、#8、#11、#13、#15、#16、#17、#18）に絞る | 検出能力の確認が部分的になる |
| 5 | W10（FK の CASCADE）と W11（CREATE DATABASE）の全 N 網羅を 100 点の標本に | 網羅の保証が弱まる |
| 6 | `isolation` の extended 実行（共有 specs）を夜間に回す | XQ とロックの組み合わせの確認が PR から消える |

**落とさないもの**: 共有 specs の simple 実行、S1・S2・S5、W8 と W9、台帳と lint、M3 の spec の書き直し、ドライバの C1〜C5、SCRAM の A2・A3・A4。

### 8.5 リスク登録簿

確率と影響は 低 / 中 / 高。

| ID | リスク | 確率 | 影響 | 早期の兆候 | 対策 | 担当 |
|---|---|---|---|---|---|---|
| R-1 | M4 のマージが遅れ、F0 が始まらない（D49）。M4 の章（04、06、07、09）が確定して M5 の契約と食い違う | 中 | 高 | M4 の P0 が完了していない | 新規ファイル・新規クレートの作業（AU-1、TD-4 の `Time`、LK-1、LK-2、TS-1a、TS-4a、TS-5a）を先行。§8.6 の突き合わせを M4 の章の確定のたびに行う | 全員 |
| R-2 | `ProcArray` の切り出しで M2 / M3 の `TxnManager` の単体テストが大量に壊れ、LK-3 が延びる（00 §10.2） | 高 | 中 | LK-3 の最初の 1 日でテストの修正量が見えない | LK-3 の見積もりに含めてある（4 日）。延びるとクリティカルパスが延びるので、LK-3 の最初に壊れるテストの数を報告させる | LK |
| R-3 | B+Tree の複数ライターの不具合が遅れて見つかる（M4 の前提の洗い出しの漏れ。RW-5） | 中 | 高 | S2 の重複キー・インデックスと全件走査の不一致 | **TS-2a の早い段階で S2 の簡易版（インデックス付きの INSERT の並行）を足す**。M4 の木の検査器（`check.rs`、I14）を並行ストレスの後に必ず流す | RW、TS |
| R-4 | EPQ の簡易版が PostgreSQL と食い違う範囲が想定より広い（結合・サブクエリ。KD-6） | 中 | 中 | `eval-plan-qual` の variant の差が大きい | 単一テーブルを共有の `epq-single` で守る。差は variant で台帳に載せ、M6 で計画木の部分再実行を検討 | RW |
| R-5 | テストの揺らぎ（待ちのタイミング、CI の遅さ）で偽陽性が増え、信頼を失う | 高 | 中 | 同じコミットで赤と緑が入れ替わる | 規約 1（事象を待つ）、`--timeout-scale`、ウォッチドッグの猶予、失敗ログ（`OpRing`）で原因を特定、`stress-seeds.txt` で再現、揺らぐテストは直すまで夜間へ隔離（無効にはしない） | TS |
| R-6 | VACUUM とインデックススキャン・行ポインタの再利用（A7）の正しさ。最も危険な変更 | 中 | 高 | I16 / S4 (g) の失敗、インデックスと全件走査の不一致 | 3 段階の順序の証明を VC が書く。変異 #1・#2 で検出能力を確認。S4 と W8 を VC-3 の完了直後に | VC、TS |
| R-7 | clog の切り詰めと凍結（`oldest_xid`、`yz_relxid`）の誤りが、データの引けない XID（実質のデータ破損）になる | 低 | 高 | I19 / I20 の失敗、起動時の XID の引けないエラー | 変異 #3〜#5。カットラインで clog の切り詰めだけを落とせる（凍結の記録は残す）。`freeze_all` を W8 に入れて頻繁に起こす | VC |
| R-8 | ドライバの互換の長い尾（各ドライバが接続時や型の読み込みで想定外のカタログを問い合わせる） | 高 | 中 | `drivers` ジョブの未知の 42P01 / 42883 | XQ-7 に 3 日、TS-4 に余裕を持つ。「失敗を期待」のシナリオで未対応を明示し（KD-22、KD-27）、必須の範囲（C1〜C10）を守る。未知のものは台帳に足して M6 へ | XQ、TS |
| R-9 | 分離性ランナーの extended 経路が本家の出力と食い違う（出力の形式、パイプラインでの NOTICE の順序など） | 中 | 中 | `validate-pg-suite.sh` の extended で一致が 120 本を下回る | extended は共有 specs と PostgreSQL の spec の一部（simple で通るもの）に限り、不一致の spec は理由を記録して simple に戻す。TS-D3 の分割（`split_statements`）が不安定なら `--protocol extended` を診断用に格下げ | TS |
| R-10 | PostgreSQL 17 のマイナー版の差やコンテナの不調で期待値が揺れる | 低 | 中 | CI の `slt-pg` が突然落ちる | `PG_IMAGE` を固定、`tests/PG_VERSION` を記録、差が出たら台帳の `pg-minor` に | TS |
| R-11 | PR のジョブの時間が増えて開発が遅くなる（スイープ、ストレス、ドライバ） | 高 | 低 | PR の CI が 15 分を超える | 固定シード・最小シナリオ、超えたら夜間へ（§6.10.6）。キャッシュ | TS |
| R-12 | MultiXact の簡易版（KD-1）が、PostgreSQL 本体の spec で想定より多くの差を出し、FK の実用に支障（デッドロックが多発） | 中 | 中 | `fk-deadlock`、`tuplelock-*` の variant の数、pgbench の G1（FK あり）の G2 での 40P01 | variant で台帳に載せる。pgbench の FK なしの構成（`dtgvp`）を既定にし、`f` は G1 だけ。実用で問題なら案 C（永続 MultiXact）を M6 の最初に | RW、FK |
| R-13 | ディスク形式（FSM、`yz_relxid`、配列、WAL の新しい種別、制御ファイル）の決定が実装の途中で変わる | 中 | 高 | ★ の確認事項が回答待ちのまま実装が進む | **ディスク形式の凍結点を 5 日目に置く**（★の確認事項に回答し、`format_version` 3 の内容を固定）。変えるなら `catalog_version` を上げて initdb をやり直す（M2-Q20 の方針）。凍結後の変更は 00 の変更依頼 | 全章 |
| R-14 | 10 本以上の並列作業での衝突（`session/`、`catalog/builtin/`、`datum.rs`、`ast.rs`、`recovery.rs`） | 中 | 中 | マージの衝突が毎日起きる | F0 の分割（D34）、1 変種・1 行ずつ足す規則（00 §8）、レーン単位での頻繁なマージ | 全員 |
| R-15 | 時間依存のテスト（`deadlock_timeout`、`lock_timeout`、`authentication_timeout`、`statement_timeout`）が CI の遅さで失敗 | 中 | 低 | 夜間の特定のテストだけが落ちる | 下限・上限に 5 倍の余裕、`--timeout-scale`、設定を短い値にして起動（`--config`） | TS |
| R-16 | ドライバ CI が外部のパッケージ（crates.io / PyPI / Maven / npm）の取得に依存し、ネットワーク障害で赤になる | 中 | 低 | `drivers` が依存の取得で落ちる | 版の固定とキャッシュ。取得の失敗は再試行 2 回。sandbox（オフライン）では実行しない（U-7） | TS |
| R-17 | 勧告ロック（U-21）がないため、Rails / Prisma / Flyway などの `migrate` が M5 で動かず、「実用化」の期待とずれる | 中 | 中 | ドライバの調査で `pg_advisory_lock` の呼び出しが見つかる | 確認事項 `M5-TS-Q8`。実装は `LockTag::Object` を使えば約 1 日（S）。M6 の先頭、または M5 の余裕で | LK |

### 8.6 M4 との統合チェックリスト（00 §1.3 の A1〜A12）

M4 のマージ（F0 の開始前）と、M4 の章（01〜11）が確定するたびに、次を確かめる。**確認の担当は各項目の「使う章」**。結果は各章の「契約への変更依頼」に書く。

| # | M4 の前提（00 §1.3） | 確認する内容 | 確認の方法 | 担当 |
|---|---|---|---|---|
| A1 | `Executor`、`ExecCtx`、`PhysicalPlan`、`RelHandle.indexes`、`rewind` | `ExecCtx` の項目の追加（`locks`、`wait`、`bind_params`、`xact_snapshot`、`ri`）が M4 の構築コードを壊さない。`PhysicalPlan::{Update, Delete}` の `recheck`、`LockRows`、`VirtualScan` の追加で M4 の `plan_golden` のスナップショットが変わらない（変わるのは新しい変種を使う SQL だけ） | `cargo test -p yuzhu-core`（`plan_golden`）、`tests/slt/m4` | RW、LK |
| A2 | `Expr<C, Q>` の単一定義、`PhysCol::Param` | `ExprKind::ExternParam(u16)` が既存の `match` で網羅エラーにならない。`PhysCol::Param` と取り違えていない | コンパイル、`slt/m5/extended` | XQ |
| A3 | `IndexStore`、`UniqueCheck`、`DirtyResult::WaitFor`、`BTREE_INSERT_LEAF = 0x00`、`BTREE_PAGES = 0x10` | `insert` の戻り値が `InsertOutcome`（D44）に変わっても M4 の呼び出し側（`insert_with_indexes`、`build`、`CREATE INDEX`）が正しく扱う。`BTREE_VACUUM = 0x20` が M4 の `recovery` の振り分けと衝突しない。M4 の B+Tree の「単一ライター前提」の一覧（RW-5 が洗い出す）を M4 の 06 章と照合 | `slt/m4/index`、S2、W8 の I16、M4 の I14（木の検査器） | RW、VC |
| A4 | numeric、char(n)、date、timestamp、timestamptz は M4。`int2vector` と `int2[]`（`Int2Vector`） | `Datum::Array` への置き換え（D25）後も、カタログ列（`indkey`、`conkey` など）の読み書きが同じ値を返す。`yuzhu-numeric` / `yuzhu-datetime` の差分コーパスが M4 の統合後に通っている | `catalog_columns.slt`、コーパス、`slt/m5/types` | TY |
| A5 | `COPY FROM STDIN`（Simple のみ）、`generate_series`、`UPDATE ... FROM`、シーケンス、`VACUUM`（何もしない）、`TRUNCATE` | `VACUUM` の本物への置き換えで、pgbench の `-i`（`v` の手順）が通り続ける。`TRUNCATE` の D42 の 4 点が M4 の実装に載る。COPY を Extended 経由で使う口が M4 の `copy/` にある | `tests/compat/pgbench`（G1）、`slt/m5/vacuum` | VC、XQ |
| A6 | シーケンスは MVCC を使わずその場上書き | 複数ライターでも `nextval` が重複しない。コミットで `wal_flush_upto` まで flush（D48） | S1（`xfer_log` の `bigserial`）、S5（`jobs.id`） | RW、LK |
| A7 | `IndexScan` は葉ごとに TID をコピーし、ピンもラッチも持ち越さない。ヒープの行ポインタを再利用しない前提 | VACUUM の 3 段階の順序、登録済みスナップショット、一意検査の `fetch_dirty` の葉ラッチの 3 つで安全（00 A7）。**証明が VC の章にある** | I16、S4 (g)、変異 #1・#2、`vacuum-line-pointer-reuse`（U-4） | VC |
| A8 | `RETURNING` は M4 の任意項目。欄は M4 が用意 | RW-6 の範囲が M4 の完了度で決まる。`BoundReturning`・`PhysicalPlan::*.returning` の欄が実在する | `slt/m5/returning`、`update_errors.slt` の `onlyif yuzhu` の撤去 | RW |
| A9 | `ddl::execute(&mut DdlCtx, BoundDdl)`、`BoundStatement::Ddl` | `DdlCtx` の項目の追加（`backend`、`wait`、`session`、`role`、`outside_block`）と `execute_standalone`、`TxnControl` が M4 の呼び出しを壊さない。M4 の `analyzer/ddl.rs` の振り分けが `ddl_ext` を呼ぶ口を持つ | コンパイル、`slt/m4/*` の回帰 | LK、VC、AU、DB、FK |
| A10 | `Transaction.wal_flush_upto`、`started_at`、`finish_without_xid` | `Transaction.writer` → `guard` の置き換えで、`wal_flush_upto` が失われない。XID のないシーケンスだけのトランザクションがコミットでロックを解放する | `slt/m4/seq`、`restart/m4`、S1 | LK |
| A11 | `RmgrId = { Xlog 0, Xact 1, Smgr 2, Heap 3, Btree 4, Seq 5 }` | `Heap2 = 6`、`Dbase = 7` が M4 の REDO の振り分け表と衝突しない。`waldump` が新しい種別を表示できる | `yuzhu-waldump`、W8〜W11 | F0 |
| A12 | `CATALOG_VERSION_NO`、`INERT_GUCS`、`maintenance_work_mem`、`DateStyle` / `TimeZone` → `TypeEnv.datetime` | `catalog_version` / `format_version` の再度の更新で M4 のクラスタを明確に拒否。各章の設定が `INERT_GUCS` に重複して載らない。`IntervalStyle` の ParameterStatus | 起動拒否の試験、`slt/m5/types/intervalstyle` | F0、TD |

M4 の章が確定したあとの突き合わせ（00 §10.2 の 4 章）:

| M4 の章 | 突き合わせる M5 の章 | 見る点 |
|---|---|---|
| `06-btree.md` | RW（RW-5）、VC（VC-3） | 複数ライターの前提の洗い出しの結果、`UniqueCheck`、`_bt_getstackbuf` 相当の有無、`BTREE_VACUUM = 0x20` の予約 |
| `04-planner-optimizer.md` | RW（RW-3）、LK | `LockRows` を置ける位置、`Update` の入力の形（`recheck` の列の渡し方）、`VirtualScan` |
| `07-catalog-ddl.md` | FK、AU、DB、VC | `ddl/` の構成、`pg_constraint` の列（`conkey` など）、`bootstrap` の口、`CatalogStore` を拡張する形 |
| `09-types-functions.md` | TY、TD | numeric・日時の範囲、`TypeEnv`、関数・演算子の OID の割り当て |

---

## 9. 未検証の点の総覧

実装前に確かめることを、章をまたいで集める。「出典」は 00・調査・この章。章 01〜09 が確定したら、各章の「未検証の点」をこの表の続きに足す（`q-index.sh` が欠けを報告する）。

| ID | 未検証の点 | 出典 | 確かめる担当・時期 |
|---|---|---|---|
| U-1 | PostgreSQL 17 のマイナー版（17.x）間で、共有テストの期待値が変わらないこと | この章 §5.6 | TS-1a、TS-5b（`PG_IMAGE` を変えて 1 回流す） |
| U-2 | sqllogictest-bin 0.29.1 の `--engine postgres-extended` が使えるか、ラベル、浮動小数点の整形が simple と同じか | この章 §6.1 | TS-5a |
| U-3 | `ported/` の spec の名前と中身（【記憶】の印のもの）が REL_17_STABLE に存在し、使っている構文が M5 の範囲か | この章 §6.4。m5-concurrency §17 の末尾 | TS-1a |
| U-4 | PostgreSQL が最も小さい未使用の行ポインタを再利用する規則と、`vacuum-line-pointer-reuse` を共有 spec にできるか | この章 §6.4.1 | TS-1a（PostgreSQL で実測）、VC |
| U-5 | S4 (f) と G8 の膨張の上限（8 倍、10 倍）が適切か | この章 §6.7、§6.10.3 | TS-2b、TS-4b（実測して決め直す） |
| U-6 | 層 2 で I19（XID の参照の有効性）を SQL だけで確かめられるか。オフラインのヒープ検査ツール（`yuzhu-dumpheap` 相当）が要るか | この章 §6.8.5 | TS-3e |
| U-7 | claude-sandbox（オフライン）でドライバ CI を実行できない。CI の実行環境でのみ確認 | この章 §6.10.1 | TS-4b |
| U-8 | psql 17 の `\parse` / `\bind_named` / `\close_stmt` の名前と挙動（PG17 で `\close` から改名された記憶） | この章 §6.10.2。pg-compat-tools §3 | TS-4a |
| U-9 | P3（c=8 が c=1 の 1.5 倍以上）が、素朴なグループコミット（M3）でも成り立つか | この章 §6.11 | TS-4b（実測） |
| U-10 | isolationtester が制御接続で `PQprepare` / `PQexecPrepared` を使うか（M3 README は「本家は Extended Query も使う」。記憶は一致するが未確認）。extended の `validate-pg-suite.sh` の一致数 | この章 TS-D2、§7.1 | TS-1a |
| U-11 | `pg_isolation_test_session_is_blocked` の正確な意味（hard と soft、safe snapshot の待ち、「interesting」でない pid を介した推移的な待ち） | waitfuncs.c（【記憶】） | LK-5（CR-1）、TS-1b |
| U-12 | 本家の deadlock 系の spec が `SET deadlock_timeout`（セッションごと）で被害者を決めているか、サーバの設定で決めているか。yuzhu で誰でも SET できる（KD-10）ので足りるか | 【記憶】 | TS-1a |
| U-13 | テストで clog の切り詰めを起こす方法: M3 の clog のセグメントの大きさを小さくできるか、できなければ XID を大量に消費する `DebugKnobs`（`advance_next_xid(n)`）が要る | 00 §3.3、この章 §6.8.2 | VC（CR-6） |
| U-14 | ~~更新者の xmax に PostgreSQL が `EXCL_LOCK` ビットを立てるか~~ **確認済み: 立てない**（【実機】`pageinspect`。02 RW-D1。00 §3.7 に反映済み。レビュー対応 R-12） | 00 §10.2 | RW-1（確認済み） |
| U-15 | ~~`scram_iterations` が PG17 の ParameterStatus の報告対象か~~ **確認済み: 報告対象**（【実機】PG17.11 の起動時の ParameterStatus 14 個に含まれる。00 §3.6・§10.2、08 AU-D12 に反映済み。R-14） | 00 §10.2 | AU-2（確認済み） |
| U-16 | `HeapTupleSatisfiesUpdate` / `HeapTupleSatisfiesVacuum` の全分岐 | m5-concurrency §17 | RW-1、VC-1 |
| U-17 | `heap_update` で旧版にロック保持者がいるときの新しい版へのロックの引き継ぎ（`xmax_new_tuple`）の規則 | m5-concurrency §17 | RW-1（案 B では更新者がロック保持者と衝突するので、引き継ぎは不要か確認） |
| U-18 | EPQ がサブプラン・InitPlan をどう扱うか | m5-concurrency §17 | RW-3 |
| U-19 | `LockRows` の計画上の位置（Limit との上下関係）と、`FOR UPDATE` が許されない構文の一覧・文言 | m5-concurrency §17、00 §4.6 | RW-3 |
| U-20 | `RangeVarGetRelidExtended` の再解決ループの正確な手順、`GetCatalogSnapshot` の無効化の契機 | m5-concurrency §17 | LK-4 |
| U-21 | **勧告ロック（`pg_advisory_lock`、`pg_try_advisory_lock`、`pg_advisory_xact_lock`）が 00 の範囲にない。Rails の `pg_try_advisory_lock`、Prisma の `pg_advisory_lock`、Flyway が使う（いずれも【記憶・未検証】）** | pg-compat-tools §3.5・§3.6 | LK（`M5-TS-Q8`） |
| U-22 | `deadlock_timeout` 後の検査が 1 回だけか、再検査があるか | m5-concurrency §17 | LK-2 |
| U-23 | 「キー列」の正確な定義（部分インデックス・式インデックス・`NULLS NOT DISTINCT`） | m5-concurrency §17 | RW-1 |
| U-24 | PostgreSQL 17 の pruning / freeze の WAL レコード構成（17 で統合された記憶） | m5-concurrency §17 | VC-1 |
| U-25 | `ALTER TABLE ... ADD FOREIGN KEY` のロックモード（00 §3.4 は ShareRowExclusive）。`CREATE TABLE ... REFERENCES` が親に取るロック | m5-concurrency §17 | FK-1、LK-4 |
| U-26 | 型・関数・演算子の OID（`pg_type.dat` などで確認。00 §3.2） | 00 §3.2 | TY、TD |
| U-27 | 文の実行が `statement_barrier` を持たなくなることで、`assert_no_pins` やチェックポイントのバッファ書き出しの前提が崩れないか | 00 §10.2 | LK-3 |
| U-28 | カタログ用スナップショットを RR でも最新にする（D12）と、`StatementCatalog` のキャッシュの世代の前提が崩れないか | 00 §10.2 | LK-4、RW-4 |
| U-29 | `ProcArray` の切り出しで壊れる M2 / M3 の単体テストの量 | 00 §10.2 | LK-3（R-2） |
| U-30 | 同名の `CREATE TABLE` の同時実行で PostgreSQL が返すエラー（`23505` `pg_type_typname_nsp_index` の記憶）と、yuzhu の名前予約ロック（42P07）との差 | m3-tx-semantics §5.6。KD-7 | TS-1b（PostgreSQL で実測） |
| U-31 | ドライバの挙動: psycopg 3 + SQLAlchemy で SAVEPOINT が失敗したときに接続自体が失敗するか、pgJDBC の `TypeInfoCache` の問い合わせ、Prisma / Rails の接続時の問い合わせ（Extended のバイナリ要求） | pg-compat-tools §9 | XQ-7、TS-4a |
| U-32 | tokio-postgres の型 OID の静的表に載っている型の範囲（`bpchar`、`interval`、`uuid`、`bytea`、配列）、pgJDBC がバイナリで要求する型 | m5-protocol-auth §3 | TS-4a |
| U-33 | pgbench の `--max-tries` の既定、`COPY ... FREEZE` の失敗時の SQLSTATE | pg-compat-tools §9 | TS-4a、M4 |
| U-34 | PostgreSQL の `pg_locks` の `locktype` のうち、yuzhu が出さない行の一覧（KD-9。**列は 16 列そろえた**。01 LK-D19）と、psql / ツールがそれに依存するか | この章 | LK-5 |
| U-35 | FK の `pg_constraint` の列（`confdelsetcols` ほか）、`DROP TABLE ... CASCADE` の NOTICE の文言 | m5-types-fk §13 | FK-1 |
| U-36 | 日時: 2 桁年、タイムゾーン略称、POSIX 形式のタイムゾーン、`interval` の細部 | m5-types-fk §13 | TD-1、TD-2 |
| U-37 | M4 の章（01〜11）の確定後の、00 の A1〜A12 の突き合わせ（§8.6） | 00 §10.2 | 各章（R-1） |

---

## 10. 確認事項

仮決めのままで進める。ディスク形式に関わるもの（★）は、実装の前（工程の 5 日目の凍結点。R-13）に決めるのが望ましい。

### 10.1 この章の確認事項

| ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|
| M5-TS-Q1 | 分離性の**共有 specs は simple と extended の両方を PR で必須**にし、`ported/` の extended は夜間（TS-D3、§6.10.6） | ロックと XQ の組み合わせを PR で守る。移植分の extended は spec の数が多く時間がかかる | `ported/` の extended も PR にすると PR の CI が数分延びる。共有 specs の extended を夜間にすると XQ の不具合の発見が遅れる |
| M5-TS-Q2 | PostgreSQL 本体の spec を `tests/isolation/ported/` にコピーして持つ（PostgreSQL License のヘッダを残し、`UPSTREAM` に commit を固定） | 期待出力を yuzhu のランナーで再現できる。上流の変更に左右されない | コピーを持たず取得のたびに上流を使うと、上流の変更で CI が揺れる。コピーを避けて自作するなら +3 日 |
| M5-TS-Q3 | 性能は**退行ゲート（M4 比 10%）と暫定の下限（P2、P3、P5〜P10）**で、絶対値の目標は M5 の完了時に記録して M6 の目標にする（TS-D11） | 要件に M5 の数値がなく、環境で絶対値が変わる | 絶対値を M5 で決めるなら、基準環境を固定する作業（+1 日）が要る |
| M5-TS-Q4 | ドライバ CI は **PR は最小（tokio-postgres、psql、pgbench の G1〜G3）、残りは夜間** | PR の時間（15 分）を守る | 全部を PR にすると PR が 30 分を超える見込み。夜間だけだと、ドライバの退行の発見が最長 1 日遅れる |
| M5-TS-Q5 | 変異テストのスイッチ（18 件）を各章が `DebugKnobs` に足す（既定無効、外から変えられない。M3-Q13 と同じ） | 検出能力の確認はテストの価値の根拠 | 入れないなら、変異テストは M3 の 7 件だけになり、M5 の新しい不変条件（I16〜I24）の検出能力が確認できない |
| M5-TS-Q6 | 並行ストレスを yuzhu だけでなく **PostgreSQL 17 にも向ける**（wire 版。TS-D7） | 期待値の確認（CLAUDE.md の原則） | yuzhu だけにすると、ストレスの不変条件が PostgreSQL で成り立つ保証がない |
| M5-TS-Q7 | 既知の差の**台帳 `tests/known-diffs.toml` を唯一の記録**にし、CI の `lint-tests` で検査する（TS-D6） | テストの避け忘れを機械で見つける | README の散文だけにすると、KD が増えたとき（28 件）に追えない |
| M5-TS-Q8 | **勧告ロック**は M5 に入れず M6（TS-D13、U-21）。ドライバ CI は `migrate` を対象にしない | 00 の範囲にない。`LockTag::Object` で約 1 日で足せる | Rails / Prisma / Flyway の `migrate` が M5 で動かない。入れるなら LK に +1 日（クリティカルパス外） |
| M5-TS-Q9 | ドライバ CI は**外部パッケージの取得が要る**ので、claude-sandbox（オフライン）では実行せず、CI だけで通す（R-16、U-7） | 版を固定してキャッシュするが、初回の取得はネットワーク | オフラインで通したいなら、依存を `tests/compat/vendor/` に同梱する（容量と更新の手間） |
| M5-TS-Q10 | PR のジョブの合計は **15 分以内**を目標にし、超えたら夜間へ移す | 開発の速度 | 厳しくすると夜間に移るテストが増え、PR で検出できない不具合が増える |
| M5-TS-Q11 | `tests/slt/m5` の extended 実行で、クライアント側の整形の差（浮動小数点など）は `skipif postgres-extended`（理由コメントのみ。KD 不要）で逃がす | sqllogictest 側の都合で、yuzhu の差ではない | 逃がさず期待を別ファイルにすると、ファイル数が倍になる |
| M5-TS-Q12 | 結合・安定化に **5 日**を置き、クリティカルパスに含める（§8.3） | 全ジョブを通した最初の実行で見つかる不具合の修正 | 縮めると M5 の完了時の安定性が下がる。00 の「結合 約 4 日」から +1 日 |

### 10.2 全章の確認事項（通し番号は 99-questions.md が正）

レビュー対応 R-12 で、この節の通し番号の表（章 01〜09 を読めずに作った暫定。章ごとの件数と内容が実際と違った）を廃止した。**全章の確認事項の一覧と通し番号は `99-questions.md` に集約した**（章の ID `M5-<略号>-Q<n>` は章ごとの連番で、99 が通し番号 `M5-Q<n>` との対応表を持つ）。00 §10.1 の契約レベル（M5-Q1〜M5-Q16。Q17 に D19 の改訂、Q18 に SELECT 句の SRF の例外などを追記）、章 01〜10 の順に振る。「★」はディスク形式に関わるもの（実装前、工程の 5 日目の凍結点までに決める。R-13）。

誤参照を避けるため、以前の暫定の番号（例: 「LK-Q1 = `deadlock_timeout`」。実際の M5-LK-Q1 は deadlock 検出の辺の扱い）、「M5-Q23 = 更新者の xmax に `EXCL_LOCK` を立てるかは U-14 未検証」（RW-D1 で【実機】確認済み: 立てない）は使わない。この章（TS）の確認事項は §10.1 の `M5-TS-Q1`〜`M5-TS-Q12` で、99 の通し番号は 99 の TS の節を見る。

### 10.3 既知の差の一覧（台帳 `tests/known-diffs.toml` の初版）

「差」は実装した機能の挙動が PostgreSQL 17 と違う（結果や挙動が変わる）もの、「未対応」は 0A000 などを返すもの。**共有テストにはこれらに依存するケースを入れない**（§6.6）。

| KD | 種別 | 内容 | 影響するテスト | 解消 |
|---|---|---|---|---|
| KD-1 | 差 | **MultiXact は簡易版（メモリ上、Q-014）**。更新者は MultiXact に入れず、更新者とロック保持者は常に衝突する。**差が出る衝突は 2 セルだけ**: 実際の非キー列の UPDATE（更新者）と `FOR KEY SHARE`（02 RW-K1。明示的な `FOR NO KEY UPDATE` ロックと `FOR KEY SHARE` は PostgreSQL の README.tuplock どおり**両立する**）。FK の子の INSERT 中に親の非キー列の UPDATE が待たされ、デッドロックも PostgreSQL より増えうる（レビュー対応 R-26。以前の記述は `FOR NO KEY UPDATE` ロックも衝突すると読め、影響範囲が過大だった） | `ported/`: `tuplelock-conflict`、`tuplelock-update`、`lock-committed-keyupdate`、`lock-update-traversal`、`fk-contention`、`fk-deadlock`、`fk-deadlock2`、`multixact-*`。共有 slt の `fk/` と S3 は親の非キー列の UPDATE を入れない | M6（永続 MultiXact。案 C） |
| KD-2 | 差 | **`pg_trigger` に FK の内部トリガーの行がない**（FK は実行器に組み込み。D26）。`SELECT ... FROM pg_trigger WHERE tgisinternal` の結果が違う。`ALTER TABLE ... DISABLE TRIGGER` は未対応 | `eval-plan-qual-trigger`（skip）、`fk/catalog_pg_constraint`（`pg_trigger` を見ない） | M6 以降（トリガー） |
| KD-3 | 未対応 | **SERIALIZABLE は 0A000**（`SERIALIZABLE isolation level is not supported yet`）。PostgreSQL は SSI で受け付ける | SSI の spec（`ported/skip.txt`）、`slt/m5/rr/serializable_unsupported`（`onlyif yuzhu`） | M6 以降（SSI） |
| KD-4 | 未対応 | **SAVEPOINT** はブロック内で 0A000、`RELEASE` / `ROLLBACK TO` は 3B001（M3 のまま）。入れ子のトランザクション（psycopg 3 の入れ子の `transaction()`、SQLAlchemy の `begin_nested`、Rails の入れ子）が使えない | `delete-abort-savept*`（skip）、`slt/m3/txn/savepoint_unsupported`、ドライバの C4 の一部 | M6 の先頭候補 |
| KD-5 | 差 | **soft deadlock の並べ替えをしない**。待ち行列の並べ替えで解消されるデッドロックを yuzhu は 40P01 にする | `ported/`: `deadlock-soft`、`deadlock-soft-2` | M6 以降 |
| KD-6 | 差 | **EPQ は簡易版**。計画木の部分再実行をせず、対象行の最新版で WHERE と SET を再評価する。結合・相関サブクエリ・CTE・複数の `FOR ... OF` を含む場合に結果が違いうる | `ported/eval-plan-qual`（variant）。共有は単一テーブルの `epq-single` | M6 以降 |
| KD-7 | 差 | 同名の `CREATE TABLE` の同時実行で、yuzhu は名前予約ロックの後に 42P07、PostgreSQL は `23505`（`pg_type_typname_nsp_index`。【記憶】U-30）。M3-Q20 の 5 つの差のうち、これだけが残る | 共有テストには入れない | M6 以降 |
| KD-8 | 差 / 未対応 | **VACUUM**: `VERBOSE` は **INFO**（PG の `vacuumlazy.c` も INFO）で出すが、統計の行の文面と数値が違う（03 VC-D24）。VM・HOT がない（常に全ページを走査）。**`VACUUM FULL` だけが 0A000**。`FREEZE`（常に horizon まで凍結するので、有無で挙動が変わらない）・`INDEX_CLEANUP`・`SKIP_LOCKED`・`TRUNCATE`・`ONLY_DATABASE_STATS`・`SKIP_DATABASE_STATS` は受け付け、`PARALLEL`・`DISABLE_PAGE_SKIPPING`・`PROCESS_MAIN`・`PROCESS_TOAST`・`BUFFER_USAGE_LIMIT` は受け付けて無視する（03 §1.1。レビュー対応 R-26。以前の「FREEZE・INDEX_CLEANUP・SKIP_LOCKED・PARALLEL などは 0A000」は誤り）。`VACUUM ANALYZE` は `reltuples` / `relpages` だけ（`pg_statistic` は空）。autovacuum は既定で無効 | `ported/`: `index-only-scan`、`vacuum-reltuples`、`freeze-the-dead`、`vacuum-conflict`（skip）。共有 spec / slt は VACUUM の NOTICE の内容を比べない | M6 |
| KD-9 | 差 | **`pg_locks`** は locktype が `relation` / `tuple` / `transactionid` / `database` / `object` だけ（`virtualxid`、`page`、`advisory` などの行がない）。**列は PostgreSQL 17 と同じ 16 列を同じ名前・型・順序で持つ**（01 LK-D19・§4.7。`virtualxid`・`virtualtransaction` は常に NULL、`fastpath` は常に false、`waitstart` は待ち手だけが待ち始めの時刻）。差は行だけ: `virtualxid` の行がない、インデックスのロックの行がない（01 LK-D14）、`select * from pg_locks` の文自身の `AccessShare` の行が違う（レビュー対応 R-26。以前の「`fastpath`、`waitstart` などがない」は誤り）。PostgreSQL は各バックエンドが `virtualxid` を持つので行数が違う | 共有 slt は `relation = '<名前>'::regclass` と locktype で絞る | M6 |
| KD-10 | 差 | **`deadlock_timeout` を誰でも SET できる**（PostgreSQL は superuser のみ。GRANT / REVOKE がないため） | ロール属性の slt には入れない | M6（GRANT） |
| KD-11 | 差 | `max_locks_per_transaction` は `SHOW` だけ（64 固定。`SET` は PG17 と同じ `55P02`）で、`53200`（`out of shared memory`）が起きない（01 M5-LK-Q13。R-12） | — | M6 |
| KD-12 | 未対応 | **認証**: MD5 なし（hba の `md5` は `scram-sha-256` として扱う。`ALTER ROLE ... PASSWORD 'md5...'` は 0A000）、Unix ソケットなし（hba の `local` は不可）、TLS なし（`SSLRequest` には `N`）、チャネルバインディングなし、GSS / peer / ident / LDAP なし | `compat/auth` の A3（`md5` の行を比べない）、A9 | M6（TLS） |
| KD-13 | 差 / 未対応 | **CREATE / DROP DATABASE**: `STRATEGY = WAL_LOG` は FILE_COPY で実行、ENCODING / LOCALE は UTF8 / C のみ、`DROP DATABASE ... WITH (FORCE)` は 0A000、孤児の `base/<oid>/` は消さず WARNING | `slt/m5/database`（オプションを使わない）、W11 | M6（`pg_terminate_backend` と FORCE） |
| KD-14 | 未対応 | **配列**: ユーザーテーブルの配列列は 0A000。`unnest`・`array_agg`・スライス・代入・SELECT 句の SRF（**ただし `SELECT srf(args)` だけの最小形は FROM 句の形に書き換えて対応する。05 TY-D17、TY-5c。psql の `\d tbl` 用。R-28**）・多次元の生成・順序比較（`<`）・ハッシュは M6。`= ANY`・`<> ALL`・`ARRAY[...]`・`ARRAY(SELECT ...)`・`a[i]`・`::T[]`・`array_length`・`cardinality`・`array_to_string` は対応 | `slt/m5/types/array_*`、ドライバの C2（配列の往復は結果・パラメータだけで、列には使わない） | M6 |
| KD-15 | 未対応 | **numeric の `sqrt` / `exp` / `ln` / `log` / `power`**（numeric の引数）は M6。**numeric の引数（`sqrt(2.0)`）は `0A000 numeric sqrt is not supported yet`**（05 TY-D15。R-34。PG は numeric を返すので float8 を黙って返さない）。整数・float8 の引数は float8 版で動く | `slt/m5/types/numeric_gap` | M6 |
| KD-16 | 未対応 | **FOREIGN KEY の `DEFERRABLE` / `INITIALLY DEFERRED`、`SET CONSTRAINTS`** は 0A000。検査は文の終わり | `slt/m5/fk` | M6 以降 |
| KD-17 | 未対応 | **`timetz` は 0A000**。`interval` のフィールド指定（`YEAR`・`MONTH`・`DAY`・`HOUR`・`MINUTE`・`SECOND`、`... TO ...`、`SECOND(p)`）と `interval(p)` は **PostgreSQL と同じ符号化で対応する**（06 TD-D3。調査 C-6 の 0A000 案は採らなかった。レビュー対応 R-13。以前の記述は「フィールド制限は 0A000」で、テストの期待が逆になっていた） | `slt/m5/types/interval_*` | M6 |
| KD-18 | 未対応 | `pg_stat_*`、`pg_prepared_statements`、`pg_cursors`、`pg_stat_activity`、`pg_terminate_backend`、`pg_cancel_backend` がない（42P01 / 42883）。キャンセルは CancelRequest で行い、分離性ランナーの `-- @cancel` を使う | `ported/timeouts`（`pg_cancel_backend` を使う部分） | M6 |
| KD-19 | 未対応 | **勧告ロック**（`pg_advisory_lock` など）がない（42883）。Rails / Prisma / Flyway の `migrate` が動かない可能性（U-21） | ドライバ CI は `migrate` を対象にしない | M6（`M5-TS-Q8`） |
| KD-20 | 差 | **汎用プランを作らない**（Bind ごとに計画をやり直す。解析結果は `AnalysisKey`（世代・search_path・DateStyle・TimeZone）が変わらなければ再利用する。D19 を 04 XQ-D6 に合わせて改めた。R-15）。`plan_cache_mode` は保存のみ。`EXPLAIN EXECUTE` の出力が PostgreSQL と違いうる | — | M6 以降 |
| KD-21 | 未対応 | **COPY**: バイナリ形式は M6。`COPY ... TO STDOUT` と CSV は XQ-6 が入るまで 0A000（カットライン上位） | ドライバの C7 の `copy_in` / `copy_out`（XQ-6 が入るまで「失敗を期待」） | M6（または XQ-6 が入れば解消） |
| KD-22 | 未対応 | **`DECLARE CURSOR` / `FETCH` / `MOVE` / `CLOSE`** がない（M1〜M5 のどの章にも入っていない）。psycopg 3 の名前つきカーソル、pgJDBC の（Extended を使わない）`setFetchSize` 以外のカーソルが使えない | ドライバの C6 の psycopg 側（「失敗を期待」） | M6 |
| KD-23 | 差 | **ロール**: GRANT / REVOKE・権限の検査がない（スーパーユーザー以外も多くを実行できる）。`DROP ROLE` の依存検査は `pg_shdepend` を作らず、**全データベースの `pg_class.relowner`・`pg_namespace.nspowner`・`pg_proc.proowner`・`pg_type.typowner` と `pg_database.datdba` を走査する**（08 AU-D14。他のデータベースの所有物も 2BP01 で拒否するので PostgreSQL と同じ形。レビュー対応 R-26。差は DETAIL の並びと `DROP OWNED`・`REASSIGN OWNED` がないこと） | `slt/m5/auth` は属性と DDL だけ | M6（GRANT） |
| KD-24 | 未対応 | **LISTEN / NOTIFY**、**`PREPARE TRANSACTION`（2PC）** | `ported/skip.txt`: `async-notify`、`prepared-transactions*` | M6 以降 |
| KD-25 | 未対応 | **`INSERT ... ON CONFLICT`、`MERGE`** | `ported/skip.txt`: `insert-conflict-*`、`merge-*` | M6 |
| KD-26 | 未対応 | **`ALTER TABLE`** の大半（FK の ADD / DROP CONSTRAINT と、M4 の ADD PRIMARY KEY / UNIQUE を除く） | `ported/skip.txt`: `alter-table-*` | M6 |
| KD-27 | 未対応 | **`CREATE VIEW` / `CREATE SCHEMA` / information_schema**。pgJDBC の `DatabaseMetaData`、Prisma の introspection などが失敗する | ドライバの C1〜C10 に含めない。「失敗を期待」のシナリオだけ | M6 |
| KD-29 | 差 | **XID を文の開始時に割り当てる**（書く文と `FOR` 句つき SELECT。01 LK-D24、M5-LK-Q4）。何も書かない `UPDATE ... WHERE false` が XID を持ち、horizon を止め、`pg_locks` に `transactionid` の行が出る | 共有テストは `pg_locks` の `transactionid` の行数を比べない | M6 |
| KD-30 | 差 | **`pg_database.datfrozenxid` の値**: PG は全 DB が同じ値のコピー。yuzhu は `min(テンプレートの行の値, oldest_xmin())` の下位 32 ビット（template0 から作った DB は `oldest_xmin()` の下位 32 ビット。09 §3.2・R-08） | 値そのものを比べる共有テストを入れない | — |
| KD-31 | 差 | **SCRAM の秘密情報の iterations が 1 未満**のロールは認証が 28P01（PG は PBKDF2 をそのまま計算する。08 M5-AU-Q18・R-33） | 共有テストに入れない | — |
| KD-32 | 差 | **numeric の引数の `sqrt` など**は `0A000`（KD-15。R-34）。 `FOR UPDATE` を仮想リレーション（`pg_roles`）に付けると PG はエラー、yuzhu は黙って無視（02 RW-K4） | 共有テストに入れない | M6 |
| KD-28 | 未対応 | **パーティションと継承**（`ported/skip.txt`: `partition-*`、`fk-partitioned-*`、`deadlock-parallel`）。pgbench のパーティション確認の問い合わせは失敗しても続行する（pg-compat §3.2.2） | — | M6 以降 |

**M3-Q20 の 5 つの既知の差の行方**: (1) 別の行への同時の INSERT / UPDATE も待つ → **消える**（行ロック。`writer-waits-writer` の新しい permutation で確認）。(2) website の例で DELETE の件数が違う → **消える**（待った後の EPQ）。(3) 読み取り中のテーブルの DROP を待たない → **消える**（AccessExclusive。`drop-while-reading`）。(4) 同名の `CREATE TABLE` の同時実行のエラーが 42P07 → **残る**（KD-7）。(5) デッドロックが起きない → **消える**（`deadlock-2way-row`、`deadlock-table`）。

---

## 11. 契約への変更依頼

### 11.1 変更依頼の一覧

| # | 宛先 | 依頼 | 理由 |
|---|---|---|---|
| CR-1 | 00 §4.2、LK-5 | **`LockManager::is_blocked_by(id, among)` の意味を固定する**: 「`id` が待ちの状態で、(a) 待っているロックを `among` のどれかが保持している（hard）、または (b) `among` のどれかが `id` より先に待ち行列に並び、要求のモードが衝突する（soft）」とする。`among` 以外のバックエンドを介した推移的な待ちは含めない（PostgreSQL の `pg_isolation_test_session_is_blocked` と同じ。未検証 U-11）。SQL の関数は pid（`BackendKeyData`）を受けるので、`BackendRegistry::by_pid` で `BackendId` に変換する | 分離性ランナーの判定の正しさがこの意味に依存する（deadlock-soft 系、tuplelock 系）。00 は署名だけで意味が書いていない |
| CR-2 | 00 §8 | **ファイルの持ち主に TS を追加する**: `yuzhu-core/src/testing/`（`testing.rs` を `testing/` にする。CR-7）、`yuzhu-core/tests/{crash_sim,stress}/*`、`yuzhu-server/tests/{concurrency,auth,crash_kill9}.rs` と `common/`、`tests/{compat/drivers,compat/auth,compat/psql/extended,perf,known-diffs.toml,tools/*}`、`.github/workflows/ci.yml`。00 §8 は「`tests/*` の共通部品」とだけ書いている。M3 の「T」と「K」、M4 の「R2」「K」「Z」の後継 | M3 は `yuzhu-core/src/testing.rs` と `crash_sim/*` を T が持っていた。M5 では TS が引き継ぐ |
| CR-3 | 00 §7 | TS の WP を分ける（TS-1a / 1b、TS-2a / 2b、TS-3a〜3e、TS-4a / 4b、TS-5a / 5b）。日数を 17 → 21.5。依存を緩める（TS-1a は PostgreSQL だけで完結するので LK-5・RW-3 に依存しない。TS-4a は XQ-7 の前に作り、XQ-7 がそれを使う）。**合計を 158 → 162.5 日にする** | §8.1 |
| CR-4 | 00 §4.7・§4.8、LK、F0 | **`Session::backend_id() -> BackendId` と `Session::backend_pid() -> i32`** を公開する（アクタースケジューラが `is_blocked_by` を呼ぶのに要る）。`Cluster::backends().by_pid()` で足りれば不要 | §4.3 |
| CR-5 | 各章 | **変異用のスイッチ 19 件を `DebugKnobs`（M3 の `debug_knobs.rs`）に足す**（§6.9 の #1〜#19。持ち主が自分の機能の分。名前は各章の定義と一致済み）。既定は無効、外から変えられない（M3-Q13） | TS-D9 |
| CR-6 | VC | **テストで凍結と clog の切り詰めを頻繁に起こす仕組み**: (a) `TestClusterOptions.freeze_all`（`vacuum_freeze_min_age` 相当を 0 に）、(b) clog のセグメントを小さくする、またはテスト用に `next_xid` を進める `DebugKnobs`（`advance_next_xid(n)`）。**VACUUM の主な出来事（`PRUNE_FREEZE` の挿入、`oldest_xid` の更新、`pg_xact` の unlink、FSM の書き込み）が `SimVfs` の操作列と `WalReader` から分類できる**こと（`CrashCoverage`） | I19、I20、§6.8.4。U-13 |
| CR-7 | F0 | `yuzhu-core/src/testing.rs` を `testing/mod.rs` にし、`stress/` と `actors.rs` を置けるようにする（機械的） | §2 |
| CR-8 | 00 §3.6、DB | **`max_connections` 設定が 00 の設定項目の表にない**が、`Cluster::connect` は 53300（`TOO_MANY_CONNECTIONS`）を返す（00 §3.5、§4.8）。DB-1 が定義するはずなので、表に足す（既定 100。`ALTER DATABASE ... CONNECTION LIMIT` とロールの `CONNECTION LIMIT` は別）。接続数の上限のテスト（S7、A5）が使う | 00 の抜け |
| CR-9 | 00 §10、LK | **勧告ロック（U-21、KD-19）を範囲外と明記する**か、LK に `pg_advisory_lock` 系を足す（約 1 日。`LockTag::Object { class = 0, obj }`）。仮決めは M6（`M5-TS-Q8`） | 00 の範囲に挙がっていない |
| CR-10 | DB、F0 | `TestCluster::session(database)`（既存）に加え、`DbView` が `base/<oid>/` を列挙し、`pg_database` と突き合わせる関数を持てるよう、`Cluster::database_dir(oid)` を公開する | I24 |
| CR-11 | AU | **`yuzhu-server` に `--hba FILE` と `--config K=V` 相当の起動オプション**（`authentication_timeout`、`scram_iterations`、`max_connections`、`autovacuum`）と、`yuzhu-initdb` の `--auth-host`・`--pwfile`（00 §6 に既出）を、`tests/yuzhu.sh` が使えるようにする | §6.1 |
| CR-12 | VC、RW | **ヒープの行ポインタの再利用の規則を PostgreSQL と同じにする**（`LP_UNUSED` のうち最も小さいものを `INSERT` が再利用する）。揃えられなければ `vacuum-line-pointer-reuse` は variant（KD）にする（U-4） | 共有 spec にできるか |
| CR-13 | 00 §1.3 | クリティカルパスを **25 日 + 結合・安定化 5 日 = 30 日（6 週間）**に直す（FK-4 が FK-3 より長いこと、RW-1a・1d・RW-2 を LK の裏に出せること）。並列度は「ピーク約 13、平均約 5.8」。**00 §1.3・§7 に反映済み**（初版の 33.5 日は 00 の WP 表の依存を引いた値で、章 02 の枝番を使っていなかった。R-20） | §8.3 |
| CR-14 | XQ | 分離性ランナーの制御接続が `SELECT pg_catalog.pg_isolation_test_session_is_blocked($1, '{...}')` を Extended Query（パラメータ `$1` の型を関数の引数 `int4` から推論）で呼ぶ。**XQ-3 の型推論が、関数呼び出しの引数の `$n` を解決できること**（および `'{...}'` が `int4[]` のリテラルとして扱われること）を、XQ-3 の受け入れ条件に足す | TS-D2 |

### 11.2 `spec/design/m5-changes.md` への反映案

M1〜M3 と同じ運用（`m2-changes.md` の形式）で、`spec/design/m5-changes.md` を M5 の実装中に作り、契約から変えた点・足した点を記録する。**この章の担当は `m5-changes.md` を作らない**（`spec/design/m5/` の Markdown だけを編集する）。初版の案を次に示す。

```markdown
# M5 契約からの変更記録

`m5/00-contracts.md` の契約から変えた点・足した点。

## 工程・見積もり（10 章）

- 00 §7 の TS の WP を TS-1a/1b、TS-2a/2b、TS-3a〜3e、TS-4a/4b、TS-5a/5b に分け、合計を 17 日から 21.5 日にした。M5 全体は 158 日から 172.5 日（CR-3 の TS +4.5 のほか、F0 +4、F1 +3、XQ-2 +1、TY-5 +1.5、DB-3 +0.5。R-20）。
- クリティカルパスを 30 日に直した（RW-1 の枝番、FK-4 が FK-3 より長い、結合・安定化 5 日。CR-13、R-20）。
- TS-1a と TS-4a は PostgreSQL だけで完結するので、M4 の実装中に始めてよい（D49 の範囲）。

## 持ち主（00 §8）

- `yuzhu-core/src/testing/`、`yuzhu-core/tests/{crash_sim,stress}/*`、`yuzhu-server/tests/{concurrency,auth,crash_kill9}.rs`、`tests/{compat/drivers,compat/auth,perf,known-diffs.toml,tools/*}`、`.github/workflows/ci.yml` を TS の持ち主に追加（CR-2）。

## 契約（LK、F0、各章）

- `LockManager::is_blocked_by` の意味を固定（hard と soft。CR-1）。
- `Session::backend_id()` / `backend_pid()`（CR-4）。
- `DebugKnobs` に変異用のスイッチ 18 件（CR-5）。`TestClusterOptions` に `deadlock_timeout`・`freeze_all`（CR-6）。
- `testing.rs` → `testing/`（CR-7）。
- `max_connections` を 00 §3.6 の設定項目に追加（CR-8）。
- 勧告ロックは範囲外と明記（M6。CR-9）。
- 行ポインタの再利用の規則（CR-12）。XQ-3 の受け入れ条件（CR-14）。

## テストの変更

- 分離性ランナー: `pg` 判定を既定、制御接続の Extended、`--protocol extended`、`--timeout-scale`、`--skip-list`、`--variant` の台帳検査。
- M3 の spec（`writer-waits-writer`、`writer-queue`、`lock-timeout`、`reader-not-blocked`）の書き直し（D39）。
- `tests/known-diffs.toml`（KD-1〜KD-28）と `lint-tests.sh`。
- `tests/run.sh` の `--protocol`、`--password`、`--list`、`.db`。`tests/pg.sh` / `tests/yuzhu.sh` の `--auth`。
```

### 11.3 この章が 00 に従えなかった点

なし（00 に反する決定はしていない）。ただし **00 の記述で曖昧だった次の点は、この章の仮決めを 00 の変更依頼にした**: `is_blocked_by` の意味（CR-1）、TS の持ち主（CR-2）、`max_connections`（CR-8）、勧告ロック（CR-9）、クリティカルパスの数値（CR-13）。
