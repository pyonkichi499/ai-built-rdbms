// yuzhu 互換テスト: pgJDBC のシナリオ C1〜C10（10-tests §6.10.2）。
//
// 接続先は環境変数（PGHOST / PGPORT / PGUSER / PGDATABASE / PGPASSWORD）。TARGET=pg|yuzhu、AUTH=trust|scram。
// 結果は 1 シナリオ 1 行 `RESULT <id> <ok|FAIL|KD> <詳細>` で標準出力に出す（run.sh が表にする）。
// アサーションは言語側（値の往復、SQLSTATE、行数）。KD（既知の未対応）に当たるシナリオは、
// yuzhu では指定の SQLSTATE で失敗することを期待し、PostgreSQL では成功することを期待する。
import java.math.BigDecimal;
import java.sql.*;
import java.time.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

import org.postgresql.PGConnection;
import org.postgresql.util.PGInterval;
import org.postgresql.util.PSQLException;
import org.postgresql.util.ServerErrorMessage;

public class Main {
    static final String TARGET = env("TARGET", "pg");
    static final String AUTH = env("AUTH", "trust");
    static final String T = "compat_drv_pgjdbc";
    static boolean failed = false;

    interface Body { String run() throws Exception; }

    static String env(String k, String d) {
        String v = System.getenv(k);
        return v == null || v.isEmpty() ? d : v;
    }

    static synchronized void report(String id, String status, String detail) {
        if (status.equals("FAIL")) failed = true;
        String d = detail == null ? "" : detail.replace('\n', ' ').replace('\r', ' ');
        System.out.println(("RESULT " + id + " " + status + " " + d).stripTrailing());
        System.out.flush();
    }

    static void scenario(String id, Body b) {
        try {
            String d = b.run();
            report(id, "ok", d == null ? "" : d);
        } catch (Throwable e) {
            report(id, "FAIL", describe(e));
        }
    }

    static String describe(Throwable e) {
        String s = e.getClass().getSimpleName() + ": " + e.getMessage();
        if (e instanceof SQLException se) s += " [" + se.getSQLState() + "]";
        return s;
    }

    /** KD シナリオ。yuzhu は states のどれかで失敗する（期待）、pg は成功する（期待）。 */
    static void knownDifference(String id, String kd, Set<String> states, Body b) {
        try {
            b.run();
        } catch (SQLException e) {
            if (TARGET.equals("yuzhu") && states.contains(e.getSQLState())) {
                report(id, "KD", kd + ": 期待どおり " + e.getSQLState() + " で失敗");
            } else {
                report(id, "FAIL", "想定外のエラー " + describe(e));
            }
            return;
        } catch (Throwable e) {
            report(id, "FAIL", describe(e));
            return;
        }
        if (TARGET.equals("yuzhu")) report(id, "FAIL", kd + ": 失敗するはずが成功した（期待を反転する）");
        else report(id, "ok", kd + ": PostgreSQL では成功");
    }

    static void check(boolean cond, String msg) {
        if (!cond) throw new AssertionError(msg);
    }

    static void eq(Object got, Object want, String what) {
        if (!Objects.equals(got, want)) throw new AssertionError(what + ": " + got + " != " + want);
    }

    static Connection connect(String extra) throws SQLException {
        String url = "jdbc:postgresql://" + env("PGHOST", "127.0.0.1") + ":" + env("PGPORT", "55432") + "/"
                + env("PGDATABASE", "postgres") + "?connectTimeout=10&loginTimeout=10" + (extra.isEmpty() ? "" : "&" + extra);
        Properties p = new Properties();
        p.setProperty("user", env("PGUSER", "postgres"));
        String pw = System.getenv("PGPASSWORD");
        if (pw != null && !pw.isEmpty()) p.setProperty("password", pw);
        return DriverManager.getConnection(url, p);
    }

    static Connection connect() throws SQLException {
        return connect("");
    }

    static void exec(Connection c, String sql) throws SQLException {
        try (Statement s = c.createStatement()) { s.execute(sql); }
    }

    static long count(Connection c, String sql) throws SQLException {
        try (Statement s = c.createStatement(); ResultSet r = s.executeQuery(sql)) {
            r.next();
            return r.getLong(1);
        }
    }

    static void resetTable(Connection c, String name, String ddl) throws SQLException {
        exec(c, "drop table if exists " + name);
        exec(c, "create table " + name + " (" + ddl + ")");
    }

    static String state(Throwable e) {
        return e instanceof SQLException se ? se.getSQLState() : "(" + e.getClass().getSimpleName() + ")";
    }

    // ---------------------------------------------------------------- C2 の表
    record Case(String name, String type, Setter set, Getter get) {}

    interface Setter { void set(PreparedStatement ps) throws Exception; }
    interface Getter { void check(ResultSet rs) throws Exception; }

    static final ZoneOffset JST = ZoneOffset.ofHours(9);
    static final LocalDateTime TS = LocalDateTime.of(2024, 2, 29, 12, 34, 56, 789012000);
    static final UUID UUID1 = UUID.fromString("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    static final byte[] BIN = {0, 1, (byte) 0xFF, (byte) 0x80, (byte) 0xE3};

    static List<Case> c2Cases(Connection c) {
        List<Case> l = new ArrayList<>();
        l.add(new Case("int2", "int2", ps -> ps.setShort(1, (short) -32768), rs -> eq(rs.getShort(1), (short) -32768, "int2")));
        l.add(new Case("int4", "int4", ps -> ps.setInt(1, Integer.MAX_VALUE), rs -> eq(rs.getInt(1), Integer.MAX_VALUE, "int4")));
        l.add(new Case("int8", "int8", ps -> ps.setLong(1, Long.MIN_VALUE), rs -> eq(rs.getLong(1), Long.MIN_VALUE, "int8")));
        l.add(new Case("float4", "float4", ps -> ps.setFloat(1, 1.5f), rs -> eq(rs.getFloat(1), 1.5f, "float4")));
        l.add(new Case("float8", "float8", ps -> ps.setDouble(1, 0.1), rs -> eq(rs.getDouble(1), 0.1, "float8")));
        l.add(new Case("bool", "bool", ps -> ps.setBoolean(1, true), rs -> eq(rs.getBoolean(1), true, "bool")));
        l.add(new Case("text", "text", ps -> ps.setString(1, "héllo 日本語"), rs -> eq(rs.getString(1), "héllo 日本語", "text")));
        l.add(new Case("varchar", "varchar", ps -> ps.setString(1, "abc"), rs -> eq(rs.getString(1), "abc", "varchar")));
        l.add(new Case("bpchar", "char(5)", ps -> ps.setString(1, "ab"), rs -> eq(rs.getString(1), "ab   ", "bpchar")));
        l.add(new Case("numeric", "numeric", ps -> ps.setBigDecimal(1, new BigDecimal("12345.678900")),
                rs -> eq(rs.getBigDecimal(1), new BigDecimal("12345.678900"), "numeric")));
        l.add(new Case("numeric_nan", "numeric", ps -> ps.setDouble(1, Double.NaN), rs -> check(Double.isNaN(rs.getDouble(1)), "numeric NaN")));
        l.add(new Case("date", "date", ps -> ps.setObject(1, LocalDate.of(2024, 2, 29)),
                rs -> eq(rs.getObject(1, LocalDate.class), LocalDate.of(2024, 2, 29), "date")));
        l.add(new Case("timestamp", "timestamp", ps -> ps.setObject(1, TS),
                rs -> eq(rs.getObject(1, LocalDateTime.class), TS, "timestamp")));
        for (ZoneOffset z : new ZoneOffset[] {ZoneOffset.UTC, JST}) {
            OffsetDateTime v = TS.atOffset(z);
            l.add(new Case("timestamptz_" + (z == JST ? "jst" : "utc"), "timestamptz", ps -> ps.setObject(1, v),
                    rs -> check(rs.getObject(1, OffsetDateTime.class).toInstant().equals(v.toInstant()), "timestamptz " + z)));
        }
        Timestamp jts = Timestamp.valueOf("2024-02-29 12:34:56.789012");
        l.add(new Case("timestamp_legacy", "timestamp", ps -> ps.setTimestamp(1, jts), rs -> eq(rs.getTimestamp(1), jts, "setTimestamp/getTimestamp")));
        l.add(new Case("timestamptz_legacy", "timestamptz", ps -> ps.setTimestamp(1, jts), rs -> eq(rs.getTimestamp(1), jts, "timestamptz legacy")));
        l.add(new Case("interval", "interval", ps -> ps.setObject(1, new PGInterval("1 mon 2 days 03:04:05.678")), rs -> {
            PGInterval i = (PGInterval) rs.getObject(1);
            check(i.getYears() == 0 && i.getMonths() == 1 && i.getDays() == 2 && i.getHours() == 3 && i.getMinutes() == 4
                    && Math.abs(i.getSeconds() - 5.678) < 1e-9, "interval " + i);
        }));
        l.add(new Case("time", "time", ps -> ps.setObject(1, LocalTime.of(3, 4, 5, 678000000)),
                rs -> eq(rs.getObject(1, LocalTime.class), LocalTime.of(3, 4, 5, 678000000), "time")));
        l.add(new Case("bytea_empty", "bytea", ps -> ps.setBytes(1, new byte[0]), rs -> check(Arrays.equals(rs.getBytes(1), new byte[0]), "bytea empty")));
        l.add(new Case("bytea_binary", "bytea", ps -> ps.setBytes(1, BIN), rs -> check(Arrays.equals(rs.getBytes(1), BIN), "bytea binary")));
        l.add(new Case("uuid", "uuid", ps -> ps.setObject(1, UUID1), rs -> eq(rs.getObject(1, UUID.class), UUID1, "uuid")));
        l.add(new Case("int4_array", "int4[]", ps -> ps.setArray(1, c.createArrayOf("int4", new Integer[] {1, null, 3})),
                rs -> check(Arrays.equals((Integer[]) rs.getArray(1).getArray(), new Integer[] {1, null, 3}), "int4[]")));
        l.add(new Case("text_array", "text[]", ps -> ps.setArray(1, c.createArrayOf("text", new String[] {"a", "b c", null})),
                rs -> check(Arrays.equals((String[]) rs.getArray(1).getArray(), new String[] {"a", "b c", null}), "text[]")));
        l.add(new Case("null", "int4", ps -> ps.setNull(1, Types.INTEGER), rs -> { rs.getInt(1); check(rs.wasNull(), "null"); }));
        return l;
    }

    /** 1 つの接続で全型を repeat 回ずつ往復する。repeat が prepareThreshold を超えるとバイナリ・サーバ側準備に切り替わる。 */
    static int c2Roundtrip(String extra, int repeat) throws Exception {
        int n = 0;
        try (Connection c = connect(extra)) {
            for (Case k : c2Cases(c)) {
                for (int i = 0; i < repeat; i++) {
                    try (PreparedStatement ps = c.prepareStatement("select ?::" + k.type)) {
                        k.set.set(ps);
                        try (ResultSet rs = ps.executeQuery()) {
                            check(rs.next(), k.name + ": 行が無い");
                            k.get.check(rs);
                        }
                    } catch (Throwable e) {
                        throw new AssertionError(k.name + " (" + extra + ", " + (i + 1) + " 回目): " + describe(e), e);
                    }
                    n++;
                }
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        // ------------------------------------------------------------ C1
        scenario("C1", () -> {
            try (Connection c = connect()) {
                try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("select 1")) {
                    check(r.next() && r.getInt(1) == 1, "select 1");
                }
                check(TARGET.equals("yuzhu") || c.getMetaData().getDatabaseMajorVersion() >= 17, "server version");
                eq(((PGConnection) c).getParameterStatus("client_encoding"), "UTF8", "client_encoding");
            }
            if (AUTH.equals("scram")) {
                Properties p = new Properties();
                p.setProperty("user", env("PGUSER", "postgres"));
                p.setProperty("password", "wrong-password");
                try (Connection c = DriverManager.getConnection(
                        "jdbc:postgresql://" + env("PGHOST", "127.0.0.1") + ":" + env("PGPORT", "55432") + "/" + env("PGDATABASE", "postgres"), p)) {
                    throw new AssertionError("誤ったパスワードで接続できた");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "28P01", "認証失敗の SQLSTATE");
                }
                return "scram + 認証失敗 28P01";
            }
            return "trust";
        });

        // ------------------------------------------------------------ C2
        scenario("C2", () -> {
            int n = 0;
            // テキスト（バイナリ転送なし、サーバ側準備なし）
            n += c2Roundtrip("binaryTransfer=false&prepareThreshold=0", 2);
            // 既定（prepareThreshold=5、binaryTransfer=true）: 5 回目を超えて 12 回ループし、バイナリへの切り替えを踏む
            n += c2Roundtrip("prepareThreshold=5&binaryTransfer=true", 12);
            // 最初からサーバ側準備（Describe 経由のバイナリ）
            n += c2Roundtrip("prepareThreshold=1&binaryTransfer=true", 3);
            return n + " 往復（text / 12 回ループ / threshold=1）";
        });

        // ------------------------------------------------------------ C3
        scenario("C3", () -> {
            try (Connection c = connect("prepareThreshold=5")) {
                resetTable(c, T + "_3", "id int4 primary key, v text");
                // 同じ文を 12 回（5 回目まではアンネームド、以降は名前つきサーバ側準備）
                try (PreparedStatement ins = c.prepareStatement("insert into " + T + "_3 values (?, ?)");
                     PreparedStatement sel = c.prepareStatement("select v from " + T + "_3 where id = ?")) {
                    for (int i = 1; i <= 12; i++) {
                        ins.setInt(1, i);
                        ins.setString(2, "v" + i);
                        eq(ins.executeUpdate(), 1, "insert " + i);
                    }
                    for (int round = 0; round < 3; round++) {
                        for (int i = 1; i <= 12; i++) {
                            sel.setInt(1, i);
                            try (ResultSet rs = sel.executeQuery()) {
                                check(rs.next(), "行が無い " + i);
                                eq(rs.getString(1), "v" + i, "select " + i);
                            }
                        }
                    }
                    check(((org.postgresql.PGStatement) sel).getPrepareThreshold() == 5, "threshold");
                }
                long named = count(c, "select count(*) from pg_prepared_statements where name like 'S\\_%'");
                check(named >= 2, "名前つきの準備済み文が無い: " + named);
                // Statement（Simple でなく Extended の無名文）
                try (Statement st = c.createStatement()) {
                    for (int i = 0; i < 12; i++) {
                        try (ResultSet rs = st.executeQuery("select " + i + "::int4 + 1")) {
                            rs.next();
                            eq(rs.getInt(1), i + 1, "Statement");
                        }
                    }
                }
                exec(c, "drop table " + T + "_3");
                return "12 回 x 3 周、名前つき " + named;
            }
        });

        // ------------------------------------------------------------ C4
        scenario("C4", () -> {
            try (Connection c = connect()) {
                resetTable(c, T + "_4", "id int4 primary key, v int4");
                c.setAutoCommit(false);
                exec(c, "insert into " + T + "_4 values (1, 10)");
                c.rollback();
                eq(count(c, "select count(*) from " + T + "_4"), 0L, "rollback 後の行数");
                exec(c, "insert into " + T + "_4 values (1, 10)");
                c.commit();
                // 別接続からコミット済みが見える
                try (Connection c2 = connect()) {
                    eq(count(c2, "select count(*) from " + T + "_4"), 1L, "別接続の行数");
                }
                // 分離レベル
                c.setTransactionIsolation(Connection.TRANSACTION_REPEATABLE_READ);
                eq(count(c, "select count(*) from " + T + "_4"), 1L, "RR");
                try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("show transaction_isolation")) {
                    r.next();
                    eq(r.getString(1), "repeatable read", "isolation");
                }
                c.commit();
                c.setAutoCommit(true);
                exec(c, "drop table " + T + "_4");
                return "commit / rollback / RR";
            }
        });
        knownDifference("C4-savepoint", "KD-4", Set.of("0A000"), () -> {
            try (Connection c = connect()) {
                resetTable(c, T + "_4s", "id int4 primary key");
                try {
                    c.setAutoCommit(false);
                    exec(c, "insert into " + T + "_4s values (1)");
                    Savepoint sp = c.setSavepoint();
                    exec(c, "insert into " + T + "_4s values (2)");
                    c.rollback(sp);
                    c.commit();
                    eq(count(c, "select count(*) from " + T + "_4s"), 1L, "savepoint");
                } finally {
                    try { c.rollback(); } catch (SQLException ignore) { }
                    c.setAutoCommit(true);
                    exec(c, "drop table if exists " + T + "_4s");
                }
            }
            return null;
        });

        // ------------------------------------------------------------ C5
        scenario("C5", () -> {
            try (Connection c = connect()) {
                resetTable(c, T + "_5", "id int4 constraint " + T + "_5_pk primary key, n text not null");
                exec(c, "insert into " + T + "_5 values (1, 'a')");
                try {
                    exec(c, "insert into " + T + "_5 values (1, 'b')");
                    throw new AssertionError("一意制約違反が起きなかった");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "23505", "unique");
                    ServerErrorMessage m = e.getServerErrorMessage();
                    eq(m.getConstraint(), T + "_5_pk", "constraint");
                    eq(m.getTable(), T + "_5", "table");
                    eq(m.getSchema(), "public", "schema");
                    eq(m.getSeverity(), "ERROR", "severity");
                    check(m.getMessage().contains("duplicate key"), "message " + m.getMessage());
                }
                try {
                    exec(c, "insert into " + T + "_5 values (2, null)");
                    throw new AssertionError("NOT NULL 違反が起きなかった");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "23502", "not null");
                    eq(e.getServerErrorMessage().getColumn(), "n", "column");
                }
                try {
                    exec(c, "select * from no_such_table_xyz");
                    throw new AssertionError("42P01 が起きなかった");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "42P01", "undefined table");
                    check(e.getServerErrorMessage().getPosition() > 0, "position");
                }
                try {
                    exec(c, "select 1/0");
                    throw new AssertionError("22012 が起きなかった");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "22012", "division by zero");
                }
                try {
                    exec(c, "selec 1");
                    throw new AssertionError("42601 が起きなかった");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "42601", "syntax");
                }
                // setString（varchar）と int 列の比較は PostgreSQL と同じ 42883
                try (PreparedStatement ps = c.prepareStatement("select * from " + T + "_5 where id = ?")) {
                    ps.setString(1, "1");
                    ps.executeQuery();
                    throw new AssertionError("varchar = int4 が通った");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "42883", "varchar = int4");
                }
                // setObject(..., Types.OTHER) / 型指定なし（OID 0）なら通る
                try (PreparedStatement ps = c.prepareStatement("select count(*) from " + T + "_5 where id = ?")) {
                    ps.setObject(1, "1", Types.OTHER);
                    try (ResultSet rs = ps.executeQuery()) { rs.next(); eq(rs.getLong(1), 1L, "unknown"); }
                }
                exec(c, "drop table " + T + "_5");
                return "23505 23502 42P01 22012 42601 42883 とフィールド";
            }
        });

        // ------------------------------------------------------------ C6
        scenario("C6", () -> {
            try (Connection c = connect("prepareThreshold=2")) {
                resetTable(c, T + "_6", "id int4 primary key, v text");
                try (PreparedStatement ins = c.prepareStatement("insert into " + T + "_6 values (?, ?)")) {
                    for (int i = 1; i <= 95; i++) { ins.setInt(1, i); ins.setString(2, "r" + i); ins.addBatch(); }
                    ins.executeBatch();
                }
                c.setAutoCommit(false);
                for (int round = 0; round < 4; round++) {
                    try (PreparedStatement ps = c.prepareStatement("select id from " + T + "_6 order by id")) {
                        ps.setFetchSize(10);
                        int n = 0, prev = 0;
                        try (ResultSet rs = ps.executeQuery()) {
                            while (rs.next()) { check(rs.getInt(1) == prev + 1, "順序"); prev = rs.getInt(1); n++; }
                        }
                        eq(n, 95, "fetchSize=10 の行数（" + round + " 周目）");
                    }
                }
                // 途中で閉じて、同じ接続で次のクエリが通る
                try (Statement st = c.createStatement()) {
                    st.setFetchSize(5);
                    try (ResultSet rs = st.executeQuery("select id from " + T + "_6 order by id")) {
                        for (int i = 0; i < 7; i++) check(rs.next(), "next");
                    }
                }
                eq(count(c, "select count(*) from " + T + "_6"), 95L, "close 後のクエリ");
                // 2 つのポータルを交互に読む
                try (Statement a = c.createStatement(); Statement b = c.createStatement()) {
                    a.setFetchSize(4); b.setFetchSize(4);
                    try (ResultSet ra = a.executeQuery("select id from " + T + "_6 order by id");
                         ResultSet rb = b.executeQuery("select id from " + T + "_6 order by id desc")) {
                        for (int i = 0; i < 20; i++) {
                            check(ra.next() && rb.next(), "交互");
                            eq(ra.getInt(1), i + 1, "a");
                            eq(rb.getInt(1), 95 - i, "b");
                        }
                    }
                }
                c.commit();
                c.setAutoCommit(true);
                exec(c, "drop table " + T + "_6");
                return "fetchSize=10 で 95 行 x 4 周、途中 close、交互読み";
            }
        });

        // ------------------------------------------------------------ C7
        scenario("C7", () -> {
            try (Connection c = connect()) {
                resetTable(c, T + "_7", "id serial primary key, v text");
                try (PreparedStatement ps = c.prepareStatement("insert into " + T + "_7 (v) values (?)")) {
                    for (int i = 0; i < 100; i++) { ps.setString(1, "b" + i); ps.addBatch(); }
                    int[] r = ps.executeBatch();
                    eq(r.length, 100, "updateCounts の長さ");
                    for (int x : r) check(x == 1 || x == Statement.SUCCESS_NO_INFO, "updateCount " + x);
                }
                eq(count(c, "select count(*) from " + T + "_7"), 100L, "batch の行数");
                // Statement の addBatch（混在）
                try (Statement st = c.createStatement()) {
                    st.addBatch("insert into " + T + "_7 (v) values ('s1')");
                    st.addBatch("update " + T + "_7 set v = 's2' where v = 's1'");
                    st.addBatch("delete from " + T + "_7 where v = 'b0'");
                    int[] r = st.executeBatch();
                    eq(Arrays.toString(r), "[1, 1, 1]", "Statement batch");
                }
                // getGeneratedKeys（RETURNING）
                try (PreparedStatement ps = c.prepareStatement("insert into " + T + "_7 (v) values (?)", Statement.RETURN_GENERATED_KEYS)) {
                    ps.setString(1, "g1");
                    ps.executeUpdate();
                    try (ResultSet k = ps.getGeneratedKeys()) {
                        check(k.next(), "generated key");
                        check(k.getInt("id") > 100, "id " + k.getInt("id"));
                    }
                }
                try (PreparedStatement ps = c.prepareStatement("insert into " + T + "_7 (v) values (?)", new String[] {"id"})) {
                    List<Integer> keys = new ArrayList<>();
                    for (int i = 0; i < 3; i++) { ps.setString(1, "k" + i); ps.addBatch(); }
                    ps.executeBatch();
                    try (ResultSet k = ps.getGeneratedKeys()) { while (k.next()) keys.add(k.getInt(1)); }
                    eq(keys.size(), 3, "batch の generated keys");
                    check(keys.get(0) < keys.get(1) && keys.get(1) < keys.get(2), "keys 昇順 " + keys);
                }
                // 失敗する要素を含むバッチ → BatchUpdateException（23505）
                try (PreparedStatement ps = c.prepareStatement("insert into " + T + "_7 (id, v) values (?, ?)")) {
                    ps.setInt(1, 1000); ps.setString(2, "x"); ps.addBatch();
                    ps.setInt(1, 1000); ps.setString(2, "y"); ps.addBatch();
                    try {
                        ps.executeBatch();
                        throw new AssertionError("バッチの一意制約違反が起きなかった");
                    } catch (BatchUpdateException e) {
                        eq(e.getSQLState(), "23505", "batch error");
                    }
                }
                eq(count(c, "select 1"), 1L, "batch 失敗後");
                exec(c, "drop table " + T + "_7");
                return "addBatch 100、混在、getGeneratedKeys、バッチ失敗";
            }
        });

        // ------------------------------------------------------------ C8
        scenario("C8", () -> {
            final int accounts = 8, threads = 8, perThread = 40;
            try (Connection c = connect()) {
                resetTable(c, T + "_8", "id int4 primary key, balance int8 not null");
                try (PreparedStatement ps = c.prepareStatement("insert into " + T + "_8 values (?, 1000)")) {
                    for (int i = 1; i <= accounts; i++) { ps.setInt(1, i); ps.addBatch(); }
                    ps.executeBatch();
                }
                ExecutorService ex = Executors.newFixedThreadPool(threads);
                List<Future<Integer>> fs = new ArrayList<>();
                for (int t = 0; t < threads; t++) {
                    final int seed = t;
                    fs.add(ex.submit(() -> {
                        Random rnd = new Random(seed);
                        int retries = 0;
                        try (Connection w = connect()) {
                            w.setAutoCommit(false);
                            for (int i = 0; i < perThread; i++) {
                                int a = 1 + rnd.nextInt(accounts), b = 1 + rnd.nextInt(accounts);
                                if (a == b) continue;
                                int amount = 1 + rnd.nextInt(50);
                                while (true) {
                                    try {
                                        try (PreparedStatement ps = w.prepareStatement("update " + T + "_8 set balance = balance + ? where id = ?")) {
                                            // 行ロックの順序を固定する（デッドロック回避）
                                            int first = Math.min(a, b);
                                            for (int id : new int[] {first, Math.max(a, b)}) {
                                                ps.setInt(1, id == a ? -amount : amount);
                                                ps.setInt(2, id);
                                                eq(ps.executeUpdate(), 1, "update");
                                            }
                                        }
                                        w.commit();
                                        break;
                                    } catch (SQLException e) {
                                        w.rollback();
                                        if (e.getSQLState().equals("40001") || e.getSQLState().equals("40P01")) { retries++; continue; }
                                        throw e;
                                    }
                                }
                            }
                        }
                        return retries;
                    }));
                }
                int retries = 0;
                try {
                    for (Future<Integer> f : fs) retries += f.get(120, TimeUnit.SECONDS);
                } finally {
                    ex.shutdownNow();
                }
                eq(count(c, "select sum(balance) from " + T + "_8"), 1000L * accounts, "総額の保存");
                exec(c, "drop table " + T + "_8");
                return threads + " 並列 x " + perThread + " 送金、総額一致、リトライ " + retries;
            }
        });

        // ------------------------------------------------------------ C9
        scenario("C9", () -> {
            // Connection に cancel は無い。Statement.cancel() を別スレッドから呼ぶ。
            try (Connection c = connect(); Statement st = c.createStatement()) {
                ScheduledExecutorService ses = Executors.newSingleThreadScheduledExecutor();
                ses.schedule(() -> { try { st.cancel(); } catch (SQLException ignore) { } }, 500, TimeUnit.MILLISECONDS);
                long t0 = System.nanoTime();
                try {
                    st.execute("select pg_sleep(20)");
                    throw new AssertionError("キャンセルされなかった");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "57014", "cancel の SQLSTATE");
                } finally {
                    ses.shutdownNow();
                }
                long ms = (System.nanoTime() - t0) / 1_000_000;
                check(ms < 10_000, "キャンセルが遅い " + ms + "ms");
                // 同じ接続で次のクエリが通る
                try (ResultSet rs = st.executeQuery("select 42")) { rs.next(); eq(rs.getInt(1), 42, "cancel 後"); }
                // setQueryTimeout も 57014
                st.setQueryTimeout(1);
                try {
                    st.execute("select pg_sleep(20)");
                    throw new AssertionError("タイムアウトしなかった");
                } catch (PSQLException e) {
                    eq(e.getSQLState(), "57014", "queryTimeout");
                }
                st.setQueryTimeout(0);
                try (ResultSet rs = st.executeQuery("select 1")) { rs.next(); }
                return "cancel と queryTimeout で 57014（" + ms + "ms）、接続は継続";
            }
        });

        // ------------------------------------------------------------ C10
        scenario("C10", () -> {
            for (String extra : new String[] {"", "prepareThreshold=1", "preferQueryMode=simple"}) {
                try (Connection c = connect(extra)) {
                    for (int i = 0; i < 8; i++) {
                        try (PreparedStatement ps = c.prepareStatement("select 1 / ?")) {
                            ps.setInt(1, 0);
                            ps.executeQuery();
                            throw new AssertionError("22012 が起きなかった");
                        } catch (PSQLException e) {
                            eq(e.getSQLState(), "22012", "失敗 (" + extra + ")");
                        }
                        try (PreparedStatement ps = c.prepareStatement("select 1 / ?")) {
                            ps.setInt(1, 1);
                            try (ResultSet rs = ps.executeQuery()) { rs.next(); eq(rs.getInt(1), 1, "復帰 (" + extra + ")"); }
                        }
                    }
                    // トランザクション内の失敗 → 25P02 → rollback で復帰
                    c.setAutoCommit(false);
                    try { exec(c, "select 1/0"); } catch (PSQLException e) { eq(e.getSQLState(), "22012", "txn 内の失敗"); }
                    try {
                        exec(c, "select 1");
                        throw new AssertionError("中断したトランザクションで通った");
                    } catch (PSQLException e) {
                        eq(e.getSQLState(), "25P02", "in_failed_sql_transaction");
                    }
                    c.rollback();
                    eq(count(c, "select 7"), 7L, "rollback 後");
                    c.commit();
                }
            }
            return "extended / threshold=1 / simple で失敗後に復帰";
        });

        // ------------------------------------------------------------ モード
        scenario("C-mode", () -> {
            int n = 0;
            for (String mode : new String[] {"simple", "extended", "extendedForPrepared", "extendedCacheEverything"}) {
                try (Connection c = connect("preferQueryMode=" + mode)) {
                    resetTable(c, T + "_m", "id int4 primary key, s text, b bytea, ts timestamptz");
                    try (PreparedStatement ps = c.prepareStatement("insert into " + T + "_m values (?, ?, ?, ?)")) {
                        for (int i = 0; i < 12; i++) {
                            ps.setInt(1, i);
                            ps.setString(2, "s'" + i);
                            ps.setBytes(3, BIN);
                            ps.setObject(4, TS.atOffset(JST));
                            ps.addBatch();
                        }
                        ps.executeBatch();
                    }
                    for (int i = 0; i < 12; i++) {
                        try (PreparedStatement ps = c.prepareStatement("select s, b, ts from " + T + "_m where id = ?")) {
                            ps.setInt(1, i);
                            try (ResultSet rs = ps.executeQuery()) {
                                check(rs.next(), "行");
                                eq(rs.getString(1), "s'" + i, mode + " s");
                                check(Arrays.equals(rs.getBytes(2), BIN), mode + " b");
                                check(rs.getObject(3, OffsetDateTime.class).toInstant().equals(TS.atOffset(JST).toInstant()), mode + " ts");
                            }
                        }
                        n++;
                    }
                    exec(c, "drop table " + T + "_m");
                }
            }
            return n + " 行を 4 モードで往復";
        });

        // ------------------------------------------------------------ KD-27
        knownDifference("C-meta", "KD-27", Set.of("42P01", "0A000", "42883", "42703"), () -> {
            try (Connection c = connect()) {
                DatabaseMetaData md = c.getMetaData();
                try (ResultSet rs = md.getTables(null, null, "%", new String[] {"TABLE"})) {
                    while (rs.next()) rs.getString("TABLE_NAME");
                }
            }
            return null;
        });

        System.exit(failed ? 1 : 0);
    }
}
