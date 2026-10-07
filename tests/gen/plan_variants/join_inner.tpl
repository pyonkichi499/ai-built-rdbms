-- family: join_inner
-- profiles: default,no_hashjoin,no_nestloop,no_hashjoin_no_nestloop,no_indexscan,no_seqscan,combined
-- setup
CREATE TABLE pv_ji_t (a int PRIMARY KEY, b int, c text)
CREATE TABLE pv_ji_u (d int PRIMARY KEY, b int, e text)
CREATE TABLE pv_ji_w (x int, y int)
CREATE INDEX pv_ji_t_b ON pv_ji_t (b)
CREATE INDEX pv_ji_u_b ON pv_ji_u (b)
INSERT INTO pv_ji_t SELECT g, g % 7, 'x' || g FROM generate_series(1, 120) g
INSERT INTO pv_ji_t VALUES (121, NULL, 'nul1'), (122, NULL, 'nul2')
INSERT INTO pv_ji_u SELECT g, g % 5, 'y' || g FROM generate_series(1, 80) g
INSERT INTO pv_ji_u VALUES (81, NULL, 'nul3')
INSERT INTO pv_ji_w SELECT g % 10, g FROM generate_series(1, 40) g
-- q: rowsort IT
SELECT t.a, u.d FROM pv_ji_t t JOIN pv_ji_u u ON t.a = u.d WHERE t.a < 30
-- q: rowsort ITT
SELECT t.a, t.c, u.e FROM pv_ji_t t JOIN pv_ji_u u ON t.a = u.d
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t JOIN pv_ji_u u ON t.b = u.b WHERE t.a < 12 AND u.d < 12
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t, pv_ji_u u WHERE t.b = u.b AND t.a < 8 AND u.d < 8
-- q: rowsort IT
SELECT t.a, u.e FROM pv_ji_t t JOIN pv_ji_u u ON t.b = u.b AND t.a = u.d
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t JOIN pv_ji_u u ON t.a + 1 = u.d WHERE t.a < 20
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t JOIN pv_ji_u u ON t.a = u.d AND t.b > u.b
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t JOIN pv_ji_u u ON t.a = u.d AND u.e <> 'y5'
-- q: rowsort II
SELECT t1.a, t2.a FROM pv_ji_t t1 JOIN pv_ji_t t2 ON t1.a = t2.b WHERE t1.a < 20
-- q: rowsort III
SELECT t.a, u.d, w.y FROM pv_ji_t t JOIN pv_ji_u u ON t.a = u.d JOIN pv_ji_w w ON w.x = t.b WHERE t.a < 10
-- q: rowsort III
SELECT t.a, u.d, w.y FROM pv_ji_t t, pv_ji_u u, pv_ji_w w WHERE t.a = u.d AND u.b = w.x AND t.a < 6
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t JOIN pv_ji_u u ON t.b = u.b WHERE t.b IS NULL
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t JOIN pv_ji_u u ON t.b = u.b WHERE t.b IS NOT NULL AND t.a < 4 AND u.d < 6
-- q: rowsort II
SELECT t.a, w.y FROM pv_ji_t t JOIN pv_ji_w w ON t.a = w.y
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t CROSS JOIN pv_ji_u u WHERE t.a < 4 AND u.d < 4
-- q: rowsort I
SELECT count(*) FROM pv_ji_t t JOIN pv_ji_u u ON t.b = u.b
-- q: rowsort II
SELECT t.a, u.d FROM pv_ji_t t JOIN pv_ji_u u ON t.a = u.d WHERE t.a BETWEEN 10 AND 20 AND u.d % 2 = 0
-- q: rowsort II
SELECT a, d FROM pv_ji_t JOIN pv_ji_u ON a = d WHERE c LIKE 'x1%'
-- q: rowsort I
SELECT a FROM pv_ji_t NATURAL JOIN (SELECT d AS a FROM pv_ji_u WHERE d < 9) s
-- q: rowsort II
SELECT a, b FROM pv_ji_t JOIN pv_ji_u USING (b) WHERE a < 10 AND d < 10
-- teardown
DROP TABLE pv_ji_t
DROP TABLE pv_ji_u
DROP TABLE pv_ji_w
