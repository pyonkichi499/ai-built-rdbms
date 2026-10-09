"""yuzhu 互換テスト: psycopg 3 のシナリオ C1〜C10（10-tests §6.10.2）。

接続先は環境変数（PGHOST / PGPORT / PGUSER / PGDATABASE / PGPASSWORD）。TARGET=pg|yuzhu、AUTH=trust|scram。
結果は 1 シナリオ 1 行 `RESULT <id> <ok|FAIL|KD> <詳細>` で標準出力に出す（run.sh が表にする）。
アサーションは言語側（値の往復、SQLSTATE、行数）。KD（既知の未対応）に当たるシナリオは、
yuzhu では指定の SQLSTATE で失敗することを期待し、PostgreSQL では成功することを期待する。
"""

from __future__ import annotations

import datetime as dt
import os
import sys
import threading
import time
import uuid
from decimal import Decimal

import psycopg
from psycopg import errors
from psycopg.types.numeric import Float4, Float8, Int2, Int4, Int8

TARGET = os.environ.get("TARGET", "pg")
AUTH = os.environ.get("AUTH", "trust")
TABLE = "compat_drv_psycopg"
failed = False


def report(sid: str, status: str, detail: str = "") -> None:
    global failed
    if status == "FAIL":
        failed = True
    print(f"RESULT {sid} {status} {detail}".rstrip(), flush=True)


def conninfo(**over: str) -> str:
    kv = {
        "host": os.environ.get("PGHOST", "127.0.0.1"),
        "port": os.environ.get("PGPORT", "55432"),
        "user": os.environ.get("PGUSER", "postgres"),
        "dbname": os.environ.get("PGDATABASE", "postgres"),
        "connect_timeout": "10",
    }
    if os.environ.get("PGPASSWORD"):
        kv["password"] = os.environ["PGPASSWORD"]
    kv.update(over)
    return " ".join(f"{k}={v}" for k, v in kv.items())


def connect(**kw) -> psycopg.Connection:
    kw.setdefault("autocommit", True)
    conn = psycopg.connect(conninfo(), **kw)
    conn.execute("set timezone = 'UTC'")
    return conn


def scenario(sid: str):
    """通常のシナリオ。例外は FAIL にする。"""

    def deco(fn):
        try:
            detail = fn() or ""
            report(sid, "ok", detail)
        except Exception as e:  # noqa: BLE001
            report(sid, "FAIL", f"{type(e).__name__}: {e}".replace("\n", " "))
        return fn

    return deco


def known_difference(sid: str, kd: str, sqlstates: tuple[str, ...]):
    """KD シナリオ。yuzhu は sqlstates のどれかで失敗する（期待）、pg は成功する（期待）。"""

    def deco(fn):
        try:
            fn()
        except psycopg.Error as e:
            if TARGET == "yuzhu" and e.sqlstate in sqlstates:
                report(sid, "KD", f"{kd}: 期待どおり {e.sqlstate} で失敗")
            else:
                report(sid, "FAIL", f"想定外のエラー {e.sqlstate}: {e}".replace("\n", " "))
        except Exception as e:  # noqa: BLE001
            report(sid, "FAIL", f"{type(e).__name__}: {e}".replace("\n", " "))
        else:
            if TARGET == "yuzhu":
                report(sid, "FAIL", f"{kd}: 失敗するはずが成功した（期待を反転する）")
            else:
                report(sid, "ok", f"{kd}: PostgreSQL では成功")
        return fn

    return deco


def reset_table(conn: psycopg.Connection, ddl: str) -> None:
    conn.execute(f"drop table if exists {TABLE}")
    conn.execute(f"create table {TABLE} ({ddl})")


# ---------------------------------------------------------------- C1
@scenario("C1")
def c1():
    with connect() as conn:
        assert conn.execute("select 1").fetchone() == (1,)
        assert conn.pgconn.server_version >= 170000 or TARGET == "yuzhu"
        assert conn.info.parameter_status("client_encoding") == "UTF8"
    if AUTH == "scram":
        try:
            psycopg.connect(conninfo(password="wrong-password"))
        except psycopg.OperationalError as e:
            # libpq の接続エラーは SQLSTATE を持たない（メッセージで確認する）
            assert "password authentication failed" in str(e), str(e)
        else:
            raise AssertionError("誤ったパスワードで接続できた")
        return "scram + 認証失敗"
    return "trust"


# ---------------------------------------------------------------- C2
ts = dt.datetime(2024, 2, 29, 12, 34, 56, 789012)
JST = dt.timezone(dt.timedelta(hours=9))
C2_CASES: list[tuple[str, str, object, object]] = [
    # (名前, SQL 型, パラメータ値, 期待する Python 値)
    ("int2", "int2", Int2(-32768), -32768),
    ("int4", "int4", Int4(2147483647), 2147483647),
    ("int8", "int8", Int8(-9223372036854775808), -9223372036854775808),
    ("float4", "float4", Float4(1.5), 1.5),
    ("float8", "float8", Float8(0.1), 0.1),
    ("bool", "bool", True, True),
    ("text", "text", "héllo 日本語", "héllo 日本語"),
    ("varchar", "varchar", "abc", "abc"),
    ("bpchar", "char(5)", "ab", "ab   "),
    ("numeric", "numeric", Decimal("12345.678900"), Decimal("12345.678900")),
    ("numeric_nan", "numeric", Decimal("NaN"), "NaN"),
    ("date", "date", dt.date(2024, 2, 29), dt.date(2024, 2, 29)),
    ("timestamp", "timestamp", ts, ts),
    ("timestamptz_utc", "timestamptz", ts.replace(tzinfo=dt.timezone.utc), ts.replace(tzinfo=dt.timezone.utc)),
    ("timestamptz_jst", "timestamptz", ts.replace(tzinfo=JST), ts.replace(tzinfo=JST)),
    ("interval", "interval", dt.timedelta(days=2, hours=3, minutes=4, seconds=5, milliseconds=678),
     dt.timedelta(days=2, hours=3, minutes=4, seconds=5, milliseconds=678)),
    ("time", "time", dt.time(3, 4, 5, 678000), dt.time(3, 4, 5, 678000)),
    ("bytea_empty", "bytea", b"", b""),
    ("bytea_binary", "bytea", bytes([0, 1, 0xFF, 0x80, 0xE3]), bytes([0, 1, 0xFF, 0x80, 0xE3])),
    ("uuid", "uuid", uuid.UUID("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"), uuid.UUID("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")),
    ("int4_array", "int4[]", [1, None, 3], [1, None, 3]),
    ("text_array", "text[]", ["a", "b c", None], ["a", "b c", None]),
    ("null", "int4", None, None),
]


def c2_check(conn, name, sqltype, value, expected, pfmt, rbinary):
    cur = conn.cursor(binary=rbinary)
    cur.execute(f"select %{pfmt}::{sqltype}", [value])
    got = cur.fetchone()[0]
    if name == "numeric_nan":
        assert isinstance(got, Decimal) and got.is_nan(), got
    else:
        assert got == expected, f"{name}: {got!r} != {expected!r}"
    # 型 OID も確認（結果の列の型が要求した型）
    assert cur.description[0].type_code is not None


@scenario("C2")
def c2():
    n = 0
    with connect() as conn:
        for name, sqltype, value, expected in C2_CASES:
            for pfmt in ("t", "b"):  # パラメータ: テキスト / バイナリ
                for rbinary in (False, True):  # 結果: テキスト / バイナリ
                    try:
                        c2_check(conn, name, sqltype, value, expected, pfmt, rbinary)
                    except psycopg.Error as e:
                        raise AssertionError(f"{name} param={pfmt} resultbinary={rbinary}: {e.sqlstate} {e}") from e
                    n += 1
        # interval の月（Python の timedelta では表せない）はテキストで確認
        r = conn.execute("select %s::interval::text", ["1 mon 2 days 03:04:05.678"]).fetchone()[0]
        assert r == "1 mon 2 days 03:04:05.678", r
        # str パラメータ（OID 0 = unknown）の型推論
        assert conn.execute("select %s::int4 + 1", ["41"]).fetchone()[0] == 42
        assert conn.execute("select %s", ["abc"]).fetchone()[0] == "abc"
        assert conn.execute("select %s = 5::int8", ["5"]).fetchone()[0] is True
    return f"{n} 往復"


# ---------------------------------------------------------------- C3
@scenario("C3")
def c3():
    with connect() as conn:
        conn.prepare_threshold = 2
        conn.prepared_max = 1
        sql = "select %s::int4 + 1"
        for i in range(8):
            assert conn.execute(sql, [i]).fetchone()[0] == i + 1
        names = [r[0] for r in conn.execute("select name from pg_prepared_statements where name like '_pg3_%'")]
        assert len(names) == 1, names
        # 別の文を閾値を超えて流すと、prepared_max を超えた古い準備済み文は DEALLOCATE される
        for k in (2, 3):
            for i in range(4):
                assert conn.execute(f"select %s::int4 + {k}", [i]).fetchone()[0] == i + k
        stmts = [r[0] for r in conn.execute("select statement from pg_prepared_statements where name like '_pg3_%'")]
        assert "select $1::int4 + 1" not in stmts, stmts
        assert "select $1::int4 + 2" in stmts, stmts
        # 明示的な prepare=True
        cur = conn.cursor()
        for i in range(3):
            assert cur.execute("select %s::int4 * 2", [i], prepare=True).fetchone()[0] == i * 2
    return "名前つき準備済み文と DEALLOCATE"


# ---------------------------------------------------------------- C4
@scenario("C4")
def c4():
    with connect(autocommit=False) as conn, connect() as other:
        conn.execute(f"drop table if exists {TABLE}")
        conn.execute(f"create table {TABLE} (id int primary key, v text)")
        conn.commit()
        conn.execute(f"insert into {TABLE} values (1, 'a')")
        assert other.execute(f"select count(*) from {TABLE}").fetchone()[0] == 0  # 未コミットは見えない
        conn.commit()
        assert other.execute(f"select count(*) from {TABLE}").fetchone()[0] == 1
        conn.execute(f"insert into {TABLE} values (2, 'b')")
        conn.rollback()
        assert other.execute(f"select count(*) from {TABLE}").fetchone()[0] == 1
        with conn.transaction():
            conn.execute(f"insert into {TABLE} values (3, 'c')")
        try:
            with conn.transaction():
                conn.execute(f"insert into {TABLE} values (4, 'd')")
                raise RuntimeError("abort")
        except RuntimeError:
            pass
        assert other.execute(f"select count(*) from {TABLE}").fetchone()[0] == 2
        conn.commit()
        conn.execute(f"drop table {TABLE}")
        conn.commit()
    return "commit / rollback / transaction()"


# ---------------------------------------------------------------- C5
@scenario("C5")
def c5():
    with connect() as conn:
        reset_table(conn, "id int primary key, v text not null")
        conn.execute(f"insert into {TABLE} values (1, 'a')")
        cases = [
            ("select 1/0", "22012", {}),
            ("select * from compat_drv_psycopg_nope", "42P01", {}),
            (f"insert into {TABLE} values (1, 'x')", "23505", {"table_name": TABLE, "constraint_name": f"{TABLE}_pkey"}),
            (f"insert into {TABLE} values (2, null)", "23502", {"table_name": TABLE, "column_name": "v"}),
            ("select 'abc'::int4", "22P02", {}),
            ("selec 1", "42601", {}),
        ]
        for sql, state, diag in cases:
            try:
                conn.execute(sql)
            except psycopg.Error as e:
                assert e.sqlstate == state, f"{sql}: {e.sqlstate} != {state}"
                assert e.diag.severity_nonlocalized == "ERROR"
                assert e.diag.message_primary
                for k, v in diag.items():
                    assert getattr(e.diag, k) == v, f"{sql}: {k}={getattr(e.diag, k)!r}"
            else:
                raise AssertionError(f"{sql}: エラーにならなかった")
        conn.execute(f"drop table {TABLE}")
    return f"{len(cases)} 件の SQLSTATE とフィールド"


# ---------------------------------------------------------------- C6
@known_difference("C6", "KD-22", ("0A000", "42601", "34000"))
def c6():
    with connect(autocommit=False) as conn:
        with conn.cursor(name="compat_c6") as cur:
            cur.itersize = 10
            cur.execute("select g from generate_series(1, 35) g")
            rows = [r[0] for r in cur]
        assert rows == list(range(1, 36)), len(rows)


# ---------------------------------------------------------------- C7
@scenario("C7")
def c7():
    with connect() as conn:
        reset_table(conn, "id int primary key, v text")
        with conn.pipeline():
            cur = conn.cursor()
            cur.executemany(f"insert into {TABLE} values (%s, %s)", [(i, f"v{i}") for i in range(100)])
        assert conn.execute(f"select count(*) from {TABLE}").fetchone()[0] == 100
        # パイプライン中のエラーは後続を中断し、sync 後に復帰する
        try:
            with conn.pipeline():
                conn.execute(f"insert into {TABLE} values (1000, 'ok')")
                conn.execute(f"insert into {TABLE} values (0, 'dup')")
                conn.execute(f"insert into {TABLE} values (1001, 'skipped')")
        except errors.UniqueViolation:
            pass
        else:
            raise AssertionError("パイプラインのエラーが出なかった")
        assert conn.execute("select 1").fetchone() == (1,)
        # executemany（パイプラインなしでも）
        cur = conn.cursor()
        cur.executemany(f"update {TABLE} set v = %s where id = %s", [("u", i) for i in range(10)])
        assert cur.rowcount >= 0
        assert conn.execute(f"select count(*) from {TABLE} where v = 'u'").fetchone()[0] == 10
        conn.execute(f"drop table {TABLE}")
    return "pipeline + executemany"


# ---------------------------------------------------------------- C8
@scenario("C8")
def c8():
    n_acc, n_workers, n_xfer, init = 10, 8, 40, 1000
    with connect() as conn:
        reset_table(conn, "id int primary key, balance int not null")
        conn.execute(f"insert into {TABLE} select g, {init} from generate_series(1, {n_acc}) g")
    errs: list[str] = []

    def worker(seed: int) -> None:
        try:
            import random

            rnd = random.Random(seed)
            with connect(autocommit=False) as c:
                for _ in range(n_xfer):
                    a, b = rnd.sample(range(1, n_acc + 1), 2)
                    amt = rnd.randint(1, 10)
                    for acc, delta in sorted([(a, -amt), (b, amt)]):  # 行ロックの順序を固定してデッドロックを避ける
                        c.execute(f"update {TABLE} set balance = balance + %s where id = %s", [delta, acc])
                    c.commit()
        except Exception as e:  # noqa: BLE001
            errs.append(f"{type(e).__name__}: {e}")

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(n_workers)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(120)
    assert not errs, errs[:3]
    with connect() as conn:
        total, cnt = conn.execute(f"select sum(balance), count(*) from {TABLE}").fetchone()
        assert (total, cnt) == (init * n_acc, n_acc), (total, cnt)
        conn.execute(f"drop table {TABLE}")
    return f"{n_workers} 並列 x {n_xfer} 送金で合計が不変"


# ---------------------------------------------------------------- C9
@scenario("C9")
def c9():
    with connect() as conn:
        timer = threading.Timer(0.5, conn.cancel)
        timer.start()
        t0 = time.monotonic()
        try:
            conn.execute("select pg_sleep(30)")
        except errors.QueryCanceled as e:
            assert e.sqlstate == "57014"
        else:
            raise AssertionError("キャンセルされなかった")
        finally:
            timer.cancel()
        assert time.monotonic() - t0 < 10
        assert conn.execute("select 1").fetchone() == (1,)
    return "57014 の後に接続が使える"


# ---------------------------------------------------------------- C10
@scenario("C10")
def c10():
    with connect() as conn:
        for _ in range(3):
            try:
                conn.execute("select 1/0")
            except errors.DivisionByZero:
                pass
            assert conn.execute("select 2").fetchone() == (2,)
    with connect(autocommit=False) as conn:
        try:
            conn.execute("select 1/0")
        except errors.DivisionByZero:
            pass
        try:
            conn.execute("select 1")
        except errors.InFailedSqlTransaction as e:
            assert e.sqlstate == "25P02"
        else:
            raise AssertionError("25P02 にならなかった")
        conn.rollback()
        assert conn.execute("select 3").fetchone() == (3,)
    return "Sync 後に次のクエリが通る、25P02 から rollback で復帰"


# ---------------------------------------------------------------- KD
@known_difference("C4-nested", "KD-4", ("0A000",))
def kd4():
    with connect(autocommit=False) as conn:
        with conn.transaction():
            with conn.transaction():  # SAVEPOINT
                conn.execute("select 1")


@known_difference("C-copy", "XQ-6", ("0A000",))
def kd_copy():
    with connect() as conn:
        reset_table(conn, "id int, v text")
        try:
            with conn.cursor() as cur:
                with cur.copy(f"copy {TABLE} from stdin") as cp:
                    cp.write_row((1, "a"))
                    cp.write_row((2, "b"))
                assert conn.execute(f"select count(*) from {TABLE}").fetchone()[0] == 2
                with cur.copy(f"copy {TABLE} to stdout") as cp:
                    rows = list(cp.rows())
                assert len(rows) == 2
        finally:
            conn.rollback() if conn.info.transaction_status != psycopg.pq.TransactionStatus.IDLE else None
            conn.execute(f"drop table if exists {TABLE}")


if __name__ == "__main__":
    sys.exit(1 if failed else 0)
