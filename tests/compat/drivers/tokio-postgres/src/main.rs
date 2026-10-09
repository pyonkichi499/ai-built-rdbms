//! yuzhu 互換テスト: tokio-postgres のシナリオ C1〜C10（10-tests §6.10.2）。
//!
//! 接続先は環境変数（PGHOST / PGPORT / PGUSER / PGDATABASE / PGPASSWORD）。TARGET=pg|yuzhu、AUTH=trust|scram。
//! 1 シナリオ 1 行 `RESULT <id> <ok|FAIL|KD> <詳細>` を標準出力に出す（run.sh が表にする）。
//! アサーションは言語側（値の往復、SQLSTATE、行数）。KD に当たるシナリオは yuzhu では指定の SQLSTATE で
//! 失敗することを期待し、PostgreSQL では成功することを期待する。

use std::error::Error as StdError;
use std::fmt::Debug;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc};
use futures_util::future::try_join_all;
use futures_util::SinkExt;
use rust_decimal::Decimal;
use tokio_postgres::types::{to_sql_checked, FromSql, IsNull, ToSql, Type};
use tokio_postgres::{Client, Config, NoTls};
use uuid::Uuid;

type BoxErr = Box<dyn StdError + Send + Sync>;
type ErrCase<'a> = (String, &'a str, Option<(&'a str, &'a str)>);
type Res<T = String> = Result<T, BoxErr>;

static FAILED: AtomicBool = AtomicBool::new(false);
const TABLE: &str = "compat_drv_tokio";

fn target() -> String {
    std::env::var("TARGET").unwrap_or_else(|_| "pg".into())
}
fn auth() -> String {
    std::env::var("AUTH").unwrap_or_else(|_| "trust".into())
}
fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.into())
}

fn report(id: &str, status: &str, detail: &str) {
    if status == "FAIL" {
        FAILED.store(true, Ordering::SeqCst);
    }
    let detail = detail.replace('\n', " ");
    println!("RESULT {id} {status} {detail}");
}

fn config() -> Config {
    let mut c = Config::new();
    c.host(env("PGHOST", "127.0.0.1"))
        .port(env("PGPORT", "55432").parse().expect("PGPORT"))
        .user(env("PGUSER", "postgres"))
        .dbname(env("PGDATABASE", "postgres"))
        .connect_timeout(Duration::from_secs(10))
        .channel_binding(tokio_postgres::config::ChannelBinding::Prefer);
    if let Ok(p) = std::env::var("PGPASSWORD") {
        if !p.is_empty() {
            c.password(&p);
        }
    }
    c
}

async fn connect_with(cfg: &Config) -> Res<Client> {
    let (client, conn) = cfg.connect(NoTls).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client.batch_execute("set timezone = 'UTC'").await?;
    Ok(client)
}
async fn connect() -> Res<Client> {
    connect_with(&config()).await
}

fn sqlstate(e: &BoxErr) -> Option<String> {
    e.downcast_ref::<tokio_postgres::Error>()
        .and_then(|e| e.code())
        .map(|c| c.code().to_string())
}

async fn scenario<F: Future<Output = Res>>(id: &str, f: F) {
    match f.await {
        Ok(d) => report(id, "ok", &d),
        Err(e) => report(
            id,
            "FAIL",
            &format!("{e} [{}]", sqlstate(&e).unwrap_or_default()),
        ),
    }
}

/// KD: yuzhu は states のどれかで失敗する（期待）。pg は成功する（期待）。
async fn known_difference<F: Future<Output = Res<()>>>(id: &str, kd: &str, states: &[&str], f: F) {
    match f.await {
        Ok(()) if target() == "yuzhu" => report(
            id,
            "FAIL",
            &format!("{kd}: 失敗するはずが成功した（期待を反転する）"),
        ),
        Ok(()) => report(id, "ok", &format!("{kd}: PostgreSQL では成功")),
        Err(e) => match sqlstate(&e) {
            Some(s) if target() == "yuzhu" && states.contains(&s.as_str()) => {
                report(id, "KD", &format!("{kd}: 期待どおり {s} で失敗"))
            }
            s => report(
                id,
                "FAIL",
                &format!("想定外のエラー {}: {e}", s.unwrap_or_default()),
            ),
        },
    }
}

macro_rules! ensure {
    ($c:expr, $($a:tt)+) => { if !$c { return Err(format!($($a)+).into()); } };
}

/// 任意の型の生のバイナリ値（numeric の NaN、interval の月など、既存の型に載らない値用）。
#[derive(Debug, PartialEq, Clone)]
struct Raw(Vec<u8>);
impl ToSql for Raw {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxErr> {
        out.extend_from_slice(&self.0);
        Ok(IsNull::No)
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    to_sql_checked!();
}
impl<'a> FromSql<'a> for Raw {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, BoxErr> {
        Ok(Raw(raw.to_vec()))
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

// ------------------------------------------------------------ C1
async fn c1() -> Res {
    let c = connect().await?;
    let row = c.query_one("select 1::int4", &[]).await?;
    ensure!(row.get::<_, i32>(0) == 1, "select 1");
    if auth() == "scram" {
        let mut bad = config();
        bad.password("wrong-password");
        match connect_with(&bad).await {
            Ok(_) => return Err("誤ったパスワードで接続できた".into()),
            Err(e) => ensure!(
                sqlstate(&e).as_deref() == Some("28P01"),
                "SQLSTATE {:?}",
                sqlstate(&e)
            ),
        }
        return Ok("scram（channel_binding=prefer）+ 28P01".into());
    }
    Ok("trust".into())
}

// ------------------------------------------------------------ C2
async fn rt<T>(c: &Client, ty: &str, v: T) -> Res<()>
where
    T: ToSql + for<'a> FromSql<'a> + PartialEq + Debug + Sync,
{
    let st = c.prepare(&format!("select $1::{ty}")).await?;
    let row = c.query_one(&st, &[&v]).await?;
    let got: T = row.try_get(0)?;
    ensure!(got == v, "{ty}: {got:?} != {v:?}");
    ensure!(
        row.columns()[0].type_() == &st.params()[0],
        "{ty}: 列の型 {:?} != パラメータの型 {:?}",
        row.columns()[0].type_(),
        st.params()[0]
    );
    Ok(())
}

async fn c2() -> Res {
    let c = connect().await?;
    let ts = NaiveDate::from_ymd_opt(2024, 2, 29)
        .unwrap()
        .and_hms_micro_opt(12, 34, 56, 789_012)
        .unwrap();
    let jst = FixedOffset::east_opt(9 * 3600).unwrap();
    let mut n = 0;
    macro_rules! t {
        ($ty:expr, $v:expr) => {{
            rt(&c, $ty, $v).await.map_err(|e| format!("{}: {e}", $ty))?;
            n += 1;
        }};
    }
    t!("int2", i16::MIN);
    t!("int4", i32::MAX);
    t!("int8", i64::MIN);
    t!("float4", 1.5f32);
    t!("float8", 0.1f64);
    t!("bool", true);
    t!("text", "héllo 日本語".to_string());
    t!("varchar", "abc".to_string());
    t!("numeric", Decimal::from_str("12345.678900")?);
    t!("date", ts.date());
    t!("timestamp", ts);
    t!("timestamptz", Utc.from_utc_datetime(&ts));
    t!(
        "timestamptz",
        jst.from_utc_datetime(&ts).with_timezone(&Utc)
    );
    t!(
        "time",
        NaiveTime::from_hms_micro_opt(3, 4, 5, 678_000).unwrap()
    );
    t!("bytea", Vec::<u8>::new());
    t!("bytea", vec![0u8, 1, 0xFF, 0x80, 0xE3]);
    t!(
        "uuid",
        Uuid::parse_str("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")?
    );
    t!("int4[]", vec![Some(1i32), None, Some(3)]);
    t!(
        "text[]",
        vec![Some("a".to_string()), Some("b c".to_string()), None]
    );
    t!("int4", Option::<i32>::None);
    // bpchar: char(5) は右を空白で埋めて返す
    let st = c.prepare("select $1::char(5)").await?;
    let got: String = c.query_one(&st, &[&"ab"]).await?.get(0);
    ensure!(got == "ab   ", "bpchar: {got:?}");
    n += 1;
    // numeric NaN（バイナリ: ndigits=0, weight=0, sign=0xC000, dscale=0）
    let nan = Raw(vec![0, 0, 0, 0, 0xC0, 0, 0, 0]);
    let st = c.prepare("select $1::numeric").await?;
    let got: Raw = c.query_one(&st, &[&nan]).await?.get(0);
    ensure!(got == nan, "NaN: {got:?}");
    let txt: String = c
        .query_one("select $1::numeric::text", &[&nan])
        .await?
        .get(0);
    ensure!(txt == "NaN", "NaN text: {txt}");
    n += 1;
    // interval '1 mon 2 days 03:04:05.678'（バイナリ: i64 マイクロ秒, i32 日, i32 月）
    let iv = (3i64 * 3600 + 4 * 60 + 5) * 1_000_000 + 678_000;
    let mut bytes = iv.to_be_bytes().to_vec();
    bytes.extend_from_slice(&2i32.to_be_bytes());
    bytes.extend_from_slice(&1i32.to_be_bytes());
    let iv = Raw(bytes);
    let st = c.prepare("select $1::interval").await?;
    let got: Raw = c.query_one(&st, &[&iv]).await?.get(0);
    ensure!(got == iv, "interval: {got:?}");
    let txt: String = c
        .query_one("select $1::interval::text", &[&iv])
        .await?
        .get(0);
    ensure!(txt == "1 mon 2 days 03:04:05.678", "interval text: {txt}");
    n += 1;
    // timestamptz の入力 +09 と UTC の等価
    let a: DateTime<Utc> = c
        .query_one("select '2024-02-29 21:34:56.789012+09'::timestamptz", &[])
        .await?
        .get(0);
    ensure!(a == Utc.from_utc_datetime(&ts), "timestamptz +09: {a}");
    Ok(format!("{n} 往復（バイナリ）"))
}

async fn c2_infer() -> Res {
    let c = connect().await?;
    c.batch_execute(&format!("drop table if exists {TABLE}; create table {TABLE} (id int4 primary key, v text, n numeric, t timestamptz)")).await?;
    let cases: Vec<(String, Vec<Type>, Vec<Type>)> = vec![
        (
            "select $1::int4 + 1".into(),
            vec![Type::INT4],
            vec![Type::INT4],
        ),
        ("select $1::int8".into(), vec![Type::INT8], vec![Type::INT8]),
        ("select $1".into(), vec![Type::TEXT], vec![Type::TEXT]),
        ("select $1 + 1".into(), vec![Type::INT4], vec![Type::INT4]),
        (
            format!("select * from {TABLE} where id = $1"),
            vec![Type::INT4],
            vec![Type::INT4, Type::TEXT, Type::NUMERIC, Type::TIMESTAMPTZ],
        ),
        (
            format!("insert into {TABLE} (id, v, n, t) values ($1, $2, $3, $4)"),
            vec![Type::INT4, Type::TEXT, Type::NUMERIC, Type::TIMESTAMPTZ],
            vec![],
        ),
        (
            format!("update {TABLE} set v = $2 where id = $1 returning id"),
            vec![Type::INT4, Type::TEXT],
            vec![Type::INT4],
        ),
        (
            "select $1::int4[]".into(),
            vec![Type::INT4_ARRAY],
            vec![Type::INT4_ARRAY],
        ),
    ];
    for (sql, params, cols) in &cases {
        let st = c.prepare(sql).await?;
        ensure!(
            st.params() == params.as_slice(),
            "{sql}: パラメータ {:?} != {:?}",
            st.params(),
            params
        );
        let got: Vec<Type> = st.columns().iter().map(|c| c.type_().clone()).collect();
        ensure!(&got == cols, "{sql}: 結果列 {got:?} != {cols:?}");
    }
    c.batch_execute(&format!("drop table {TABLE}")).await?;
    Ok(format!(
        "{} 件の Describe（パラメータと結果列の OID）",
        cases.len()
    ))
}

// ------------------------------------------------------------ C3
async fn c3() -> Res {
    let c = connect().await?;
    c.batch_execute(&format!(
        "drop table if exists {TABLE}; create table {TABLE} (id int4, v text)"
    ))
    .await?;
    let sel = c.prepare("select $1::int4 * 2").await?;
    for i in 0..50i32 {
        let r: i32 = c.query_one(&sel, &[&i]).await?.get(0);
        ensure!(r == i * 2, "再利用 {i}: {r}");
    }
    let ins = c
        .prepare(&format!("insert into {TABLE} (id, v) values ($1, $2)"))
        .await?;
    for i in 0..20i32 {
        let n = c.execute(&ins, &[&i, &format!("v{i}")]).await?;
        ensure!(n == 1, "execute の行数 {n}");
    }
    let cnt: i64 = c
        .query_one(&format!("select count(*) from {TABLE}"), &[])
        .await?
        .get(0);
    ensure!(cnt == 20, "件数 {cnt}");
    let named: i64 = c
        .query_one(
            "select count(*) from pg_prepared_statements where name like 's%'",
            &[],
        )
        .await?
        .get(0);
    ensure!(named >= 2, "準備済み文が {named} 件");
    drop(sel);
    drop(ins);
    c.batch_execute(&format!("drop table {TABLE}")).await?;
    Ok("1 つの準備済み文を 50 回再利用".into())
}

// ------------------------------------------------------------ C4
async fn c4() -> Res {
    let mut c = connect().await?;
    let other = connect().await?;
    c.batch_execute(&format!(
        "drop table if exists {TABLE}; create table {TABLE} (id int4 primary key, v text)"
    ))
    .await?;
    let q = format!("select count(*) from {TABLE}");
    let n = |r: tokio_postgres::Row| r.get::<_, i64>(0);
    let tx = c.transaction().await?;
    tx.execute(&format!("insert into {TABLE} values (1, 'a')"), &[])
        .await?;
    ensure!(
        n(other.query_one(&q, &[]).await?) == 0,
        "未コミットが見えた"
    );
    tx.commit().await?;
    ensure!(
        n(other.query_one(&q, &[]).await?) == 1,
        "コミット後に見えない"
    );
    let tx = c.transaction().await?;
    tx.execute(&format!("insert into {TABLE} values (2, 'b')"), &[])
        .await?;
    tx.rollback().await?;
    ensure!(
        n(other.query_one(&q, &[]).await?) == 1,
        "rollback が効いていない"
    );
    // Drop（commit なし）は rollback
    {
        let tx = c.transaction().await?;
        tx.execute(&format!("insert into {TABLE} values (3, 'c')"), &[])
            .await?;
    }
    ensure!(
        n(other.query_one(&q, &[]).await?) == 1,
        "Drop で rollback されていない"
    );
    c.batch_execute(&format!("drop table {TABLE}")).await?;
    Ok("commit / rollback / Drop".into())
}

async fn c4_nested() -> Res<()> {
    let mut c = connect().await?;
    let mut tx = c.transaction().await?;
    let sp = tx.transaction().await?; // SAVEPOINT
    sp.query("select 1", &[]).await?;
    sp.commit().await?;
    tx.rollback().await?;
    Ok(())
}

// ------------------------------------------------------------ C5
async fn c5() -> Res {
    let c = connect().await?;
    c.batch_execute(&format!("drop table if exists {TABLE}; create table {TABLE} (id int4 primary key, v text not null); insert into {TABLE} values (1, 'a')")).await?;
    let cases: Vec<ErrCase> = vec![
        ("select 1/0".into(), "22012", None),
        ("select * from compat_drv_tokio_nope".into(), "42P01", None),
        (
            format!("insert into {TABLE} values (1, 'x')"),
            "23505",
            Some(("constraint", "compat_drv_tokio_pkey")),
        ),
        (
            format!("insert into {TABLE} values (2, null)"),
            "23502",
            Some(("column", "v")),
        ),
        ("select 'abc'::int4".into(), "22P02", None),
        ("selec 1".into(), "42601", None),
    ];
    for (sql, state, extra) in &cases {
        let e = match c.query(sql.as_str(), &[]).await {
            Ok(_) => return Err(format!("{sql}: エラーにならなかった").into()),
            Err(e) => e,
        };
        let db = e
            .as_db_error()
            .ok_or_else(|| format!("{sql}: DbError ではない: {e}"))?;
        ensure!(
            db.code().code() == *state,
            "{sql}: {} != {state}",
            db.code().code()
        );
        ensure!(
            db.severity() == "ERROR" && !db.message().is_empty(),
            "{sql}: severity / message"
        );
        if let Some((k, v)) = extra {
            let got = match *k {
                "constraint" => db.constraint(),
                _ => db.column(),
            };
            ensure!(got == Some(*v), "{sql}: {k} = {got:?}");
            ensure!(db.table() == Some(TABLE), "{sql}: table = {:?}", db.table());
        }
        if *state == "42601" {
            ensure!(db.position().is_some(), "42601 に position がない");
        }
    }
    c.batch_execute(&format!("drop table {TABLE}")).await?;
    Ok(format!("{} 件の SQLSTATE とフィールド", cases.len()))
}

// ------------------------------------------------------------ C6
async fn c6() -> Res {
    let mut c = connect().await?;
    let tx = c.transaction().await?;
    let st = tx
        .prepare("select g::int4 from generate_series(1, 35) g")
        .await?;
    let portal = tx.bind(&st, &[]).await?;
    let mut sizes = vec![];
    let mut all: Vec<i32> = vec![];
    loop {
        let rows = tx.query_portal(&portal, 10).await?;
        if rows.is_empty() {
            break;
        }
        sizes.push(rows.len());
        all.extend(rows.iter().map(|r| r.get::<_, i32>(0)));
    }
    ensure!(
        sizes == vec![10, 10, 10, 5],
        "PortalSuspended の分割 {sizes:?}"
    );
    ensure!(all == (1..=35).collect::<Vec<_>>(), "行の内容");
    tx.rollback().await?;
    Ok("max_rows=10 で 10,10,10,5".into())
}

// ------------------------------------------------------------ C7
async fn c7() -> Res {
    let c = connect().await?;
    let st = c.prepare("select $1::int4 + 1000").await?;
    let futs = (0..100i32).map(|i| {
        let c = &c;
        let st = &st;
        async move { c.query_one(st, &[&i]).await.map(|r| r.get::<_, i32>(0)) }
    });
    let got = try_join_all(futs).await?;
    ensure!(
        got == (0..100).map(|i| i + 1000).collect::<Vec<_>>(),
        "パイプラインの結果の順序"
    );
    // パイプライン中のエラーは他のクエリに波及しない（それぞれ Sync で閉じる）
    let bad = c.prepare("select 1/($1::int4)").await?;
    let futs = (0..10i32).map(|i| {
        let c = &c;
        let bad = &bad;
        async move { c.query(bad, &[&(i % 2)]).await.map(|r| r.len()) }
    });
    let results = futures_util::future::join_all(futs).await;
    for (i, r) in results.iter().enumerate() {
        if i % 2 == 0 {
            let e = r.as_ref().err().ok_or("0 で割ってエラーにならなかった")?;
            ensure!(
                e.code().map(|c| c.code()) == Some("22012"),
                "SQLSTATE {:?}",
                e.code()
            );
        } else {
            ensure!(r.is_ok(), "{i} 番が失敗した: {r:?}");
        }
    }
    // batch_execute（Simple Query、複数文）
    c.batch_execute(&format!("drop table if exists {TABLE}; create table {TABLE} (id int4); insert into {TABLE} values (1),(2),(3)")).await?;
    let msgs = c
        .simple_query(&format!("select id from {TABLE} order by id"))
        .await?;
    let rows = msgs
        .iter()
        .filter(|m| matches!(m, tokio_postgres::SimpleQueryMessage::Row(_)))
        .count();
    ensure!(rows == 3, "simple_query の行数 {rows}");
    c.batch_execute(&format!("drop table {TABLE}")).await?;
    Ok("100 本のパイプライン + エラー混在 + batch_execute".into())
}

// ------------------------------------------------------------ C8
async fn c8() -> Res {
    const ACC: i32 = 10;
    const WORKERS: u64 = 8;
    const XFERS: u32 = 40;
    let c = connect().await?;
    c.batch_execute(&format!("drop table if exists {TABLE}; create table {TABLE} (id int4 primary key, balance int4 not null); insert into {TABLE} select g, 1000 from generate_series(1, {ACC}) g")).await?;
    let mut handles = vec![];
    for w in 0..WORKERS {
        handles.push(tokio::spawn(async move {
            let mut cl = connect().await.map_err(|e| e.to_string())?;
            let mut seed = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(w + 1);
            let mut next = move |m: u64| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 33) % m
            };
            for _ in 0..XFERS {
                let a = next(ACC as u64) as i32 + 1;
                let mut b = next(ACC as u64 - 1) as i32 + 1;
                if b >= a {
                    b += 1;
                }
                let amt = next(10) as i32 + 1;
                let mut legs = [(a, -amt), (b, amt)];
                legs.sort(); // 行ロックの順序を固定
                let tx = cl.transaction().await.map_err(|e| e.to_string())?;
                for (acc, d) in legs {
                    tx.execute(
                        &format!("update {TABLE} set balance = balance + $1 where id = $2"),
                        &[&d, &acc],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                }
                tx.commit().await.map_err(|e| e.to_string())?;
            }
            Ok::<(), String>(())
        }));
    }
    for h in handles {
        h.await.map_err(|e| e.to_string())??;
    }
    let row = c
        .query_one(
            &format!("select sum(balance)::int8, count(*) from {TABLE}"),
            &[],
        )
        .await?;
    let (sum, cnt): (i64, i64) = (row.get(0), row.get(1));
    ensure!(
        sum == 1000 * ACC as i64 && cnt == ACC as i64,
        "合計 {sum} 件数 {cnt}"
    );
    c.batch_execute(&format!("drop table {TABLE}")).await?;
    Ok(format!("{WORKERS} 並列 x {XFERS} 送金で合計が不変"))
}

// ------------------------------------------------------------ C9
async fn c9() -> Res {
    let c = connect().await?;
    let token = c.cancel_token();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        token.cancel_query(NoTls).await
    });
    let t0 = Instant::now();
    match c.query("select pg_sleep(30)", &[]).await {
        Ok(_) => return Err("キャンセルされなかった".into()),
        Err(e) => ensure!(
            e.code().map(|c| c.code()) == Some("57014"),
            "SQLSTATE {:?}",
            e.code()
        ),
    }
    canceller.await.map_err(|e| e.to_string())??;
    ensure!(t0.elapsed() < Duration::from_secs(10), "キャンセルが遅い");
    let r: i32 = c.query_one("select 1::int4", &[]).await?.get(0);
    ensure!(r == 1, "キャンセル後のクエリ");
    Ok("57014 の後に接続が使える".into())
}

// ------------------------------------------------------------ C10
async fn c10() -> Res {
    let c = connect().await?;
    for _ in 0..3 {
        ensure!(
            c.query("select 1/0", &[]).await.is_err(),
            "エラーにならなかった"
        );
        let r: i32 = c.query_one("select 2::int4", &[]).await?.get(0);
        ensure!(r == 2, "復帰後のクエリ");
    }
    // 準備済み文の実行エラーの後も同じ文が使える
    let st = c.prepare("select 10/$1::int4").await?;
    ensure!(c.query(&st, &[&0i32]).await.is_err(), "0 除算");
    let r: i32 = c.query_one(&st, &[&5i32]).await?.get(0);
    ensure!(r == 2, "準備済み文の再実行");
    // トランザクション内のエラーは 25P02 になり rollback で復帰
    c.batch_execute("begin").await?;
    ensure!(
        c.query("select 1/0", &[]).await.is_err(),
        "エラーにならなかった"
    );
    match c.query("select 1", &[]).await {
        Ok(_) => return Err("25P02 にならなかった".into()),
        Err(e) => ensure!(
            e.code().map(|c| c.code()) == Some("25P02"),
            "SQLSTATE {:?}",
            e.code()
        ),
    }
    c.batch_execute("rollback").await?;
    let r: i32 = c.query_one("select 3::int4", &[]).await?.get(0);
    ensure!(r == 3, "rollback 後");
    Ok("Sync 後に次のクエリが通る、25P02 から rollback で復帰".into())
}

// ------------------------------------------------------------ KD: COPY（XQ-6）
async fn copy_kd() -> Res<()> {
    let c = connect().await?;
    c.batch_execute(&format!(
        "drop table if exists {TABLE}; create table {TABLE} (id int4, v text)"
    ))
    .await?;
    let result: Res<()> = async {
        let sink = c.copy_in(&format!("copy {TABLE} from stdin")).await?;
        futures_util::pin_mut!(sink);
        sink.send(bytes::Bytes::from_static(b"1\ta\n2\tb\n"))
            .await?;
        let n = sink.finish().await?;
        ensure!(n == 2, "copy_in の行数 {n}");
        let stream = c.copy_out(&format!("copy {TABLE} to stdout")).await?;
        let chunks: Vec<bytes::Bytes> = futures_util::TryStreamExt::try_collect(stream).await?;
        let total: Vec<u8> = chunks.iter().flat_map(|b| b.to_vec()).collect();
        ensure!(total == b"1\ta\n2\tb\n", "copy_out の内容 {total:?}");
        Ok(())
    }
    .await;
    let _ = c
        .batch_execute(&format!("drop table if exists {TABLE}"))
        .await;
    result
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    scenario("C1", c1()).await;
    scenario("C2", c2()).await;
    scenario("C2-infer", c2_infer()).await;
    scenario("C3", c3()).await;
    scenario("C4", c4()).await;
    scenario("C5", c5()).await;
    scenario("C6", c6()).await;
    scenario("C7", c7()).await;
    scenario("C8", c8()).await;
    scenario("C9", c9()).await;
    scenario("C10", c10()).await;
    known_difference("C4-nested", "KD-4", &["0A000"], c4_nested()).await;
    known_difference("C-copy", "XQ-6", &["0A000"], copy_kd()).await;
    std::process::exit(if FAILED.load(Ordering::SeqCst) { 1 } else { 0 });
}
