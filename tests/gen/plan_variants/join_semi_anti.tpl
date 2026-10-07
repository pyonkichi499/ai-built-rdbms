-- family: join_semi_anti
-- profiles: default,no_hashjoin,no_nestloop,no_hashjoin_no_nestloop,no_indexscan,no_seqscan,combined
-- setup
CREATE TABLE pv_js_t (a int PRIMARY KEY, b int)
CREATE TABLE pv_js_u (d int, b int)
CREATE INDEX pv_js_u_d ON pv_js_u (d)
INSERT INTO pv_js_t SELECT g, g % 9 FROM generate_series(1, 70) g
INSERT INTO pv_js_u SELECT g % 30, g % 9 FROM generate_series(1, 50) g
INSERT INTO pv_js_u VALUES (NULL, 3), (200, NULL)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE EXISTS (SELECT 1 FROM pv_js_u u WHERE u.d = t.a)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE NOT EXISTS (SELECT 1 FROM pv_js_u u WHERE u.d = t.a)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE a IN (SELECT d FROM pv_js_u)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE a NOT IN (SELECT d FROM pv_js_u)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE a NOT IN (SELECT d FROM pv_js_u WHERE d IS NOT NULL)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE b IN (SELECT b FROM pv_js_u u WHERE u.d = t.a)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE EXISTS (SELECT 1 FROM pv_js_u u WHERE u.d = t.a AND u.b > t.b)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE NOT EXISTS (SELECT 1 FROM pv_js_u u WHERE u.d = t.a AND u.b > t.b)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE a < 10 AND (EXISTS (SELECT 1 FROM pv_js_u u WHERE u.d = t.a) OR b = 1)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE a IN (SELECT d FROM pv_js_u WHERE d < 15) AND b < 5
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE EXISTS (SELECT 1 FROM pv_js_u u WHERE u.d = t.a AND EXISTS (SELECT 1 FROM pv_js_t t2 WHERE t2.a = u.d + 1))
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE a IN (SELECT d FROM pv_js_u WHERE d IN (SELECT a FROM pv_js_t WHERE a % 3 = 0))
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE a = ANY (SELECT d FROM pv_js_u WHERE d > 20)
-- q: rowsort I
SELECT a FROM pv_js_t t WHERE a <> ALL (SELECT d FROM pv_js_u WHERE d IS NOT NULL AND d > 25)
-- q: rowsort I
SELECT count(*) FROM pv_js_t t WHERE NOT EXISTS (SELECT 1 FROM pv_js_u)
-- q: rowsort II
SELECT t.a, (SELECT count(*) FROM pv_js_u u WHERE u.d = t.a) FROM pv_js_t t WHERE t.a < 8
-- teardown
DROP TABLE pv_js_t
DROP TABLE pv_js_u
