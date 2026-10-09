'use strict';
// yuzhu 互換テスト: node-postgres のシナリオ C1〜C10（10-tests §6.10.2）。
//
// 接続先は環境変数（PGHOST / PGPORT / PGUSER / PGDATABASE / PGPASSWORD）。TARGET=pg|yuzhu、AUTH=trust|scram。
// 結果は 1 シナリオ 1 行 `RESULT <id> <ok|FAIL|KD> <詳細>` で標準出力に出す（run.sh が表にする）。
// node-postgres はパラメータをテキストで送り、型 OID はすべて 0（サーバの推論が必須）。
const assert = require('node:assert/strict');
const net = require('node:net');
const { Client, Pool, types } = require('pg');
const Cursor = require('pg-cursor');

const TARGET = process.env.TARGET || 'pg';
const AUTH = process.env.AUTH || 'trust';
const T = 'compat_drv_nodepg';
let failed = false;

function report(id, status, detail = '') {
  if (status === 'FAIL') failed = true;
  console.log(`RESULT ${id} ${status} ${String(detail).replace(/\s+/g, ' ')}`.trimEnd());
}

function config(extra = {}) {
  const c = {
    host: process.env.PGHOST || '127.0.0.1',
    port: Number(process.env.PGPORT || 55432),
    user: process.env.PGUSER || 'postgres',
    database: process.env.PGDATABASE || 'postgres',
    connectionTimeoutMillis: 10000,
    ...extra,
  };
  if (process.env.PGPASSWORD) c.password = process.env.PGPASSWORD;
  return c;
}

async function connect(extra) {
  const c = new Client(config(extra));
  await c.connect();
  await c.query("set timezone = 'UTC'");
  return c;
}

function describe(e) {
  return `${e && e.name}: ${e && e.message}${e && e.code ? ` [${e.code}]` : ''}`;
}

async function scenario(id, fn) {
  try {
    report(id, 'ok', (await fn()) || '');
  } catch (e) {
    report(id, 'FAIL', describe(e));
  }
}

/** KD シナリオ。yuzhu は codes のどれかで失敗する（期待）、pg は成功する（期待）。 */
async function knownDifference(id, kd, codes, fn) {
  try {
    await fn();
  } catch (e) {
    if (TARGET === 'yuzhu' && codes.includes(e.code)) report(id, 'KD', `${kd}: 期待どおり ${e.code} で失敗`);
    else report(id, 'FAIL', `想定外のエラー ${describe(e)}`);
    return;
  }
  if (TARGET === 'yuzhu') report(id, 'FAIL', `${kd}: 失敗するはずが成功した（期待を反転する）`);
  else report(id, 'ok', `${kd}: PostgreSQL では成功`);
}

async function rejectsWith(code, p, what) {
  try {
    await p;
  } catch (e) {
    assert.equal(e.code, code, `${what}: ${describe(e)}`);
    return e;
  }
  assert.fail(`${what}: ${code} が起きなかった`);
}

const rawTypes = { getTypeParser: () => (v) => v };

// ---------------------------------------------------------------- C2 の表
// [名前, SQL 型, パラメータ（JS 値）, 既定パーサの結果の検証, テキストの期待値（セッションは UTC）]
const buf = Buffer.from([0, 1, 0xff, 0x80, 0xe3]);
const C2 = [
  ['int2', 'int2', -32768, (v) => assert.equal(v, -32768), '-32768'],
  ['int4', 'int4', 2147483647, (v) => assert.equal(v, 2147483647), '2147483647'],
  ['int8', 'int8', '-9223372036854775808', (v) => assert.equal(v, '-9223372036854775808'), '-9223372036854775808'], // int8 は文字列
  ['float4', 'float4', 1.5, (v) => assert.equal(v, 1.5), '1.5'],
  ['float8', 'float8', 0.1, (v) => assert.equal(v, 0.1), '0.1'],
  ['bool', 'bool', true, (v) => assert.equal(v, true), 't'],
  ['text', 'text', 'héllo 日本語', (v) => assert.equal(v, 'héllo 日本語'), 'héllo 日本語'],
  ['varchar', 'varchar', 'abc', (v) => assert.equal(v, 'abc'), 'abc'],
  ['bpchar', 'char(5)', 'ab', (v) => assert.equal(v, 'ab   '), 'ab   '],
  ['numeric', 'numeric', '12345.678900', (v) => assert.equal(v, '12345.678900'), '12345.678900'], // numeric は文字列
  ['numeric_nan', 'numeric', 'NaN', (v) => assert.equal(v, 'NaN'), 'NaN'],
  ['date', 'date', '2024-02-29', (v) => assert.equal(v.getFullYear() * 10000 + (v.getMonth() + 1) * 100 + v.getDate(), 20240229), '2024-02-29'],
  ['timestamp', 'timestamp', '2024-02-29 12:34:56.789012', (v) => assert.equal(v.getMilliseconds(), 789), '2024-02-29 12:34:56.789012'],
  ['timestamptz_utc', 'timestamptz', '2024-02-29 12:34:56.789012+00', (v) => assert.equal(v.getTime(), Date.UTC(2024, 1, 29, 12, 34, 56, 789)), '2024-02-29 12:34:56.789012+00'],
  ['timestamptz_jst', 'timestamptz', '2024-02-29 12:34:56.789012+09', (v) => assert.equal(v.getTime(), Date.UTC(2024, 1, 29, 3, 34, 56, 789)), '2024-02-29 03:34:56.789012+00'],
  ['timestamptz_date', 'timestamptz', new Date(Date.UTC(2024, 1, 29, 3, 34, 56, 789)), (v) => assert.equal(v.getTime(), Date.UTC(2024, 1, 29, 3, 34, 56, 789)), '2024-02-29 03:34:56.789+00'], // Date はローカル時刻 + オフセットで送られる
  ['interval', 'interval', '1 mon 2 days 03:04:05.678', (v) => assert.deepEqual({ ...v }, { months: 1, days: 2, hours: 3, minutes: 4, seconds: 5, milliseconds: 678 }), '1 mon 2 days 03:04:05.678'],
  ['time', 'time', '03:04:05.678', (v) => assert.equal(v, '03:04:05.678'), '03:04:05.678'],
  ['bytea_empty', 'bytea', Buffer.alloc(0), (v) => assert.ok(Buffer.isBuffer(v) && v.length === 0), '\\x'],
  ['bytea_binary', 'bytea', buf, (v) => assert.ok(buf.equals(v)), '\\x0001ff80e3'],
  ['uuid', 'uuid', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', (v) => assert.equal(v, 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'), 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'],
  ['int4_array', 'int4[]', [1, null, 3], (v) => assert.deepEqual(v, [1, null, 3]), '{1,NULL,3}'],
  ['text_array', 'text[]', ['a', 'b c', null], (v) => assert.deepEqual(v, ['a', 'b c', null]), '{a,"b c",NULL}'],
  ['null', 'int4', null, (v) => assert.equal(v, null), null],
];

// ---------------------------------------------------------------- main
(async () => {
  // ------------------------------------------------------------ C1
  await scenario('C1', async () => {
    const c = await connect();
    try {
      const r = await c.query('select 1 as one');
      assert.equal(r.rows[0].one, 1);
      assert.equal(r.fields[0].name, 'one');
      assert.equal(r.fields[0].dataTypeID, 23);
      assert.equal((await c.query('show client_encoding')).rows[0].client_encoding, 'UTF8');
    } finally {
      await c.end();
    }
    if (AUTH === 'scram') {
      const bad = new Client(config({ password: 'wrong-password' }));
      await rejectsWith('28P01', bad.connect(), '誤ったパスワード');
      await bad.end().catch(() => {});
      return 'scram + 認証失敗 28P01';
    }
    return 'trust';
  });

  // ------------------------------------------------------------ C2
  await scenario('C2', async () => {
    const c = await connect();
    let n = 0;
    try {
      for (const [name, type, param, check, text] of C2) {
        try {
          // 1. 明示キャスト（OID 0 の unknown を $1::type で解決）+ 既定パーサ（型 OID から解釈）
          const r = await c.query(`select $1::${type} as v`, [param]);
          check(r.rows[0].v);
          // 2. テキストのまま比べる
          const raw = await c.query({ text: `select $1::${type} as v`, values: [param], types: rawTypes });
          assert.equal(raw.rows[0].v, text, `${name} raw`);
          // 3. 挿入先の列の型から推論（キャストなし）
          await c.query(`drop table if exists ${T}_2`);
          await c.query(`create table ${T}_2 (v ${type})`);
          await c.query(`insert into ${T}_2 values ($1)`, [param]);
          const back = await c.query({ text: `select v from ${T}_2`, types: rawTypes });
          assert.equal(back.rows[0].v, text, `${name} insert 推論`);
          // 4. 比較の相手から推論（where v = $1）
          if (param !== null) {
            const w = await c.query(`select count(*)::int4 as n from ${T}_2 where v = $1`, [param]);
            assert.equal(w.rows[0].n, 1, `${name} where 推論`);
          }
        } catch (e) {
          throw new Error(`${name}: ${describe(e)}`);
        }
        n++;
      }
      // rowMode: 'array'
      const ra = await c.query({ text: 'select $1::int4, $2::text', values: [1, 'x'], rowMode: 'array' });
      assert.deepEqual(ra.rows, [[1, 'x']]);
      // 推論（キャストなし）
      assert.equal((await c.query('select $1::int4 + 1 as v', ['41'])).rows[0].v, 42);
      assert.equal((await c.query('select $1 as v', ['abc'])).rows[0].v, 'abc');
      assert.equal((await c.query('select $1 = 5::int8 as v', ['5'])).rows[0].v, true);
      assert.equal((await c.query('select $1 + 1 as v', [41])).rows[0].v, 42);
      // 型 OID（結果の列）
      const f = await c.query('select 1::int2 a, 1::int8 b, 1::numeric c, now() d, true e, \'x\'::text f, \'x\'::bytea g, array[1]::int4[] h');
      assert.deepEqual(f.fields.map((x) => x.dataTypeID), [21, 20, 1700, 1184, 16, 25, 17, 1007]);
      await c.query(`drop table if exists ${T}_2`);
    } finally {
      await c.end();
    }
    return `${n} 型を 明示キャスト / 推論 / テキスト で往復`;
  });

  // ------------------------------------------------------------ C3
  await scenario('C3', async () => {
    const c = await connect();
    try {
      await c.query(`drop table if exists ${T}_3`);
      await c.query(`create table ${T}_3 (id int4 primary key, v text)`);
      for (let i = 1; i <= 12; i++) {
        const r = await c.query({ name: 'ins3', text: `insert into ${T}_3 values ($1, $2)`, values: [i, `v${i}`] });
        assert.equal(r.rowCount, 1);
      }
      for (let round = 0; round < 3; round++) {
        for (let i = 1; i <= 12; i++) {
          const r = await c.query({ name: 'sel3', text: `select v from ${T}_3 where id = $1`, values: [i] });
          assert.equal(r.rows[0].v, `v${i}`);
        }
      }
      const ps = await c.query({ text: "select name, statement from pg_prepared_statements where name in ('ins3', 'sel3') order by name", types: rawTypes });
      assert.deepEqual(ps.rows.map((x) => x.name), ['ins3', 'sel3']);
      // 名前つきの文を並べて実行（2 回目以降は Parse を省く）
      const rs = await Promise.all([1, 2, 3, 4, 5].map((i) => c.query({ name: 'sel3', text: `select v from ${T}_3 where id = $1`, values: [i] })));
      assert.deepEqual(rs.map((r) => r.rows[0].v), ['v1', 'v2', 'v3', 'v4', 'v5']);
      await c.query(`drop table ${T}_3`);
    } finally {
      await c.end();
    }
    return '名前つき 2 文を 12 回 + 3 周';
  });

  // ------------------------------------------------------------ C4
  await scenario('C4', async () => {
    const c = await connect();
    const c2 = await connect();
    try {
      await c.query(`drop table if exists ${T}_4`);
      await c.query(`create table ${T}_4 (id int4 primary key, v int4)`);
      await c.query('begin');
      await c.query(`insert into ${T}_4 values (1, 10)`);
      await c2.query(`select count(*)::int4 as n from ${T}_4`).then((r) => assert.equal(r.rows[0].n, 0, 'コミット前は見えない'));
      await c.query('rollback');
      assert.equal((await c.query(`select count(*)::int4 as n from ${T}_4`)).rows[0].n, 0);
      await c.query('begin');
      await c.query(`insert into ${T}_4 values ($1, $2)`, [1, 10]);
      await c.query('commit');
      assert.equal((await c2.query(`select count(*)::int4 as n from ${T}_4`)).rows[0].n, 1);
      await c.query('begin isolation level repeatable read');
      assert.equal((await c.query('show transaction_isolation')).rows[0].transaction_isolation, 'repeatable read');
      await c.query('commit');
      await c.query(`drop table ${T}_4`);
    } finally {
      await c.end();
      await c2.end();
    }
    return 'commit / rollback / RR / 可視性';
  });
  await knownDifference('C4-savepoint', 'KD-4', ['0A000'], async () => {
    const c = await connect();
    try {
      await c.query(`drop table if exists ${T}_4s`);
      await c.query(`create table ${T}_4s (id int4 primary key)`);
      await c.query('begin');
      await c.query(`insert into ${T}_4s values (1)`);
      await c.query('savepoint sp');
      await c.query(`insert into ${T}_4s values (2)`);
      await c.query('rollback to savepoint sp');
      await c.query('commit');
      assert.equal((await c.query(`select count(*)::int4 as n from ${T}_4s`)).rows[0].n, 1);
    } finally {
      await c.query('rollback').catch(() => {});
      await c.query(`drop table if exists ${T}_4s`).catch(() => {});
      await c.end();
    }
  });

  // ------------------------------------------------------------ C5
  await scenario('C5', async () => {
    const c = await connect();
    try {
      await c.query(`drop table if exists ${T}_5`);
      await c.query(`create table ${T}_5 (id int4 constraint ${T}_5_pk primary key, n text not null)`);
      await c.query(`insert into ${T}_5 values (1, 'a')`);
      let e = await rejectsWith('23505', c.query(`insert into ${T}_5 values ($1, $2)`, [1, 'b']), 'unique');
      assert.equal(e.constraint, `${T}_5_pk`);
      assert.equal(e.table, `${T}_5`);
      assert.equal(e.schema, 'public');
      assert.equal(e.severity, 'ERROR');
      assert.match(e.message, /duplicate key/);
      assert.ok(e.detail && e.detail.includes('(id)=(1)'), `detail ${e.detail}`);
      e = await rejectsWith('23502', c.query(`insert into ${T}_5 values ($1, $2)`, [2, null]), 'not null');
      assert.equal(e.column, 'n');
      e = await rejectsWith('42P01', c.query('select * from no_such_table_xyz'), 'undefined table');
      assert.ok(Number(e.position) > 0, 'position');
      await rejectsWith('22012', c.query('select 1/0'), 'division by zero');
      await rejectsWith('42601', c.query('selec 1'), 'syntax');
      await rejectsWith('42703', c.query(`select nope from ${T}_5`), 'undefined column');
      await rejectsWith('22P02', c.query('select $1::int4', ['abc']), 'invalid text representation');
      await c.query(`drop table ${T}_5`);
    } finally {
      await c.end();
    }
    return '23505 23502 42P01 22012 42601 42703 22P02 とフィールド';
  });

  // ------------------------------------------------------------ C6
  await scenario('C6', async () => {
    const c = await connect();
    try {
      await c.query(`drop table if exists ${T}_6`);
      await c.query(`create table ${T}_6 (id int4 primary key, v text)`);
      for (let i = 1; i <= 95; i++) await c.query(`insert into ${T}_6 values ($1, $2)`, [i, `r${i}`]);
      await c.query('begin');
      for (let round = 0; round < 3; round++) {
        const cur = c.query(new Cursor(`select id from ${T}_6 order by id`));
        let total = 0, prev = 0, reads = 0;
        for (;;) {
          const rows = await cur.read(10);
          if (rows.length === 0) break;
          reads++;
          assert.ok(rows.length <= 10);
          for (const r of rows) { assert.equal(r.id, ++prev); total++; }
        }
        assert.equal(total, 95, `read(10) の行数（${round} 周目）`);
        assert.equal(reads, 10);
        await cur.close();
      }
      // 途中で閉じて、同じ接続で次のクエリが通る
      const cur = c.query(new Cursor(`select id from ${T}_6 order by id`, [], { rowMode: 'array' }));
      const first = await cur.read(7);
      assert.equal(first.length, 7);
      assert.deepEqual(first[6], [7]);
      await cur.close();
      assert.equal((await c.query(`select count(*)::int4 as n from ${T}_6`)).rows[0].n, 95);
      // パラメータ付き
      const cp = c.query(new Cursor(`select id from ${T}_6 where id > $1 order by id`, [90]));
      assert.equal((await cp.read(3)).length, 3);
      assert.equal((await cp.read(100)).length, 2);
      await cp.close();
      await c.query('commit');
      await c.query(`drop table ${T}_6`);
    } finally {
      await c.end();
    }
    return 'pg-cursor read(10) で 95 行 x 3 周、途中 close、パラメータ付き';
  });

  // ------------------------------------------------------------ C7
  await scenario('C7', async () => {
    const c = await connect();
    try {
      await c.query(`drop table if exists ${T}_7`);
      await c.query(`create table ${T}_7 (id serial primary key, v text)`);
      // await せずに 100 本を 1 接続へ流す（クライアント側のキューで連続して送られる）
      const rs = await Promise.all(Array.from({ length: 100 }, (_, i) => c.query(`insert into ${T}_7 (v) values ($1) returning id`, [`b${i}`])));
      const ids = rs.map((r) => r.rows[0].id);
      assert.deepEqual(ids, [...ids].sort((a, b) => a - b));
      assert.equal(new Set(ids).size, 100);
      assert.equal((await c.query(`select count(*)::int4 as n from ${T}_7`)).rows[0].n, 100);
      // 途中の失敗が後続を巻き込まない
      const rs2 = await Promise.allSettled([
        c.query(`insert into ${T}_7 (id, v) values ($1, 'x')`, [1000]),
        c.query(`insert into ${T}_7 (id, v) values ($1, 'y')`, [1000]),
        c.query('select 1 as one'),
      ]);
      assert.equal(rs2[0].status, 'fulfilled');
      assert.equal(rs2[1].status, 'rejected');
      assert.equal(rs2[1].reason.code, '23505');
      assert.equal(rs2[2].status, 'fulfilled');
      // Simple Query のマルチステートメント（結果が配列で返る）
      const multi = await c.query(`insert into ${T}_7 (v) values ('m1'); select count(*)::int4 as n from ${T}_7`);
      assert.equal(multi.length, 2);
      assert.equal(multi[0].rowCount, 1);
      assert.equal(multi[1].rows[0].n, 102);
      await c.query(`drop table ${T}_7`);
    } finally {
      await c.end();
    }
    return '非同期 100 本のパイプライン、失敗の分離、マルチステートメント';
  });

  // ------------------------------------------------------------ C8
  await scenario('C8', async () => {
    const accounts = 8, workers = 10, perWorker = 30;
    const setup = await connect();
    const pool = new Pool(config({ max: 10 }));
    let retries = 0;
    try {
      await setup.query(`drop table if exists ${T}_8`);
      await setup.query(`create table ${T}_8 (id int4 primary key, balance int8 not null)`);
      for (let i = 1; i <= accounts; i++) await setup.query(`insert into ${T}_8 values ($1, 1000)`, [i]);
      let seed = 12345;
      const rnd = (n) => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed % n; };
      const transfers = Array.from({ length: workers * perWorker }, () => ({ a: 1 + rnd(accounts), b: 1 + rnd(accounts), amount: 1 + rnd(50) }));
      const worker = async (w) => {
        const client = await pool.connect();
        try {
          for (const { a, b, amount } of transfers.slice(w * perWorker, (w + 1) * perWorker)) {
            if (a === b) continue;
            for (;;) {
              try {
                await client.query('begin');
                for (const id of [Math.min(a, b), Math.max(a, b)]) { // 行ロックの順序を固定する
                  const r = await client.query(`update ${T}_8 set balance = balance + $1 where id = $2`, [id === a ? -amount : amount, id]);
                  assert.equal(r.rowCount, 1);
                }
                await client.query('commit');
                break;
              } catch (e) {
                await client.query('rollback');
                if (e.code === '40001' || e.code === '40P01') { retries++; continue; }
                throw e;
              }
            }
          }
        } finally {
          client.release();
        }
      };
      await Promise.all(Array.from({ length: workers }, (_, w) => worker(w)));
      assert.equal(pool.totalCount <= 10, true);
      const sum = await setup.query(`select sum(balance) as s from ${T}_8`);
      assert.equal(sum.rows[0].s, String(1000 * accounts), '総額の保存'); // sum(int8) は numeric
      // pool.query を 50 本
      const qs = await Promise.all(Array.from({ length: 50 }, (_, i) => pool.query('select $1::int4 as v', [i])));
      assert.deepEqual(qs.map((r) => r.rows[0].v), Array.from({ length: 50 }, (_, i) => i));
      await setup.query(`drop table ${T}_8`);
    } finally {
      await pool.end();
      await setup.end();
    }
    return `Pool 10 クライアント、${workers} 並列 x ${perWorker} 送金、総額一致、リトライ ${retries}`;
  });

  // ------------------------------------------------------------ C9
  await scenario('C9', async () => {
    const c = await connect();
    try {
      // プロトコルの CancelRequest（別接続で pid と secret key を送る）
      const sendCancel = () => new Promise((resolve, reject) => {
        const s = net.connect({ host: config().host, port: config().port }, () => {
          const b = Buffer.alloc(16);
          b.writeInt32BE(16, 0);
          b.writeInt32BE(80877102, 4);
          b.writeInt32BE(c.processID, 8);
          b.writeInt32BE(c.secretKey, 12);
          s.write(b);
        });
        s.on('close', resolve);
        s.on('error', reject);
        s.setTimeout(5000, () => { s.destroy(); resolve(); });
      });
      const t0 = Date.now();
      const p = c.query('select pg_sleep(20)');
      setTimeout(() => sendCancel().catch(() => {}), 500);
      await rejectsWith('57014', p, 'CancelRequest');
      const ms = Date.now() - t0;
      assert.ok(ms < 10000, `キャンセルが遅い ${ms}ms`);
      assert.equal((await c.query('select 42 as v')).rows[0].v, 42, 'cancel 後');
      // pg_cancel_backend（別接続から SQL で）
      const killer = await connect();
      try {
        const pid = c.processID;
        const p2 = c.query('select pg_sleep(20)');
        setTimeout(() => killer.query('select pg_cancel_backend($1)', [pid]).catch(() => {}), 500);
        await rejectsWith('57014', p2, 'pg_cancel_backend');
      } finally {
        await killer.end();
      }
      assert.equal((await c.query('select 1 as v')).rows[0].v, 1);
      // statement_timeout
      await c.query("set statement_timeout = '500ms'");
      await rejectsWith('57014', c.query('select pg_sleep(20)'), 'statement_timeout');
      await c.query('set statement_timeout = 0');
      assert.equal((await c.query('select 1 as v')).rows[0].v, 1);
      return `CancelRequest と pg_cancel_backend と statement_timeout で 57014（${ms}ms）、接続は継続`;
    } finally {
      await c.end();
    }
  });

  // ------------------------------------------------------------ C10
  await scenario('C10', async () => {
    const c = await connect();
    try {
      for (let i = 0; i < 8; i++) {
        await rejectsWith('22012', c.query('select 1 / $1::int4', [0]), '失敗');
        assert.equal((await c.query('select 1 / $1::int4 as v', [1])).rows[0].v, 1);
        // 名前つき文の失敗後も同じ名前が使える
        await rejectsWith('22012', c.query({ name: 'fail10', text: 'select 1 / $1::int4', values: [0] }), '名前つきの失敗');
        assert.equal((await c.query({ name: 'fail10', text: 'select 1 / $1::int4', values: [1] })).rows[0]['?column?'], 1);
      }
      // 失敗した Parse の後の名前つき文
      await rejectsWith('42P01', c.query({ name: 'bad10', text: 'select * from no_such_table_xyz', values: [] }), 'Parse の失敗');
      assert.equal((await c.query('select 2 as v')).rows[0].v, 2);
      // トランザクション内の失敗 → 25P02 → rollback で復帰
      await c.query('begin');
      await rejectsWith('22012', c.query('select 1/0'), 'txn 内の失敗');
      await rejectsWith('25P02', c.query('select 1'), 'in_failed_sql_transaction');
      await rejectsWith('25P02', c.query('select $1::int4', [1]), 'extended でも 25P02');
      await c.query('rollback');
      assert.equal((await c.query('select 7 as v')).rows[0].v, 7);
    } finally {
      await c.end();
    }
    return 'Sync 後に次のクエリが通る（extended / 名前つき / txn 中断）';
  });

  process.exit(failed ? 1 : 0);
})().catch((e) => {
  report('(実行)', 'FAIL', describe(e));
  process.exit(1);
});
