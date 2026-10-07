-- family: subquery
-- profiles: default,no_hashjoin,no_nestloop,no_hashjoin_no_nestloop,no_indexscan,no_seqscan,combined
-- setup
CREATE TABLE pv_sq_t (a int PRIMARY KEY, b int)
CREATE TABLE pv_sq_u (d int, b int)
INSERT INTO pv_sq_t SELECT g, g % 8 FROM generate_series(1, 60) g
INSERT INTO pv_sq_u SELECT g % 40, g % 8 FROM generate_series(1, 80) g
-- q: rowsort II
SELECT a, (SELECT max(d) FROM pv_sq_u) FROM pv_sq_t WHERE a < 5
-- q: rowsort II
SELECT a, (SELECT count(*) FROM pv_sq_u u WHERE u.b = t.b) FROM pv_sq_t t WHERE a < 10
-- q: rowsort I
SELECT a FROM pv_sq_t t WHERE b = (SELECT min(b) FROM pv_sq_u u WHERE u.d = t.a)
-- q: rowsort I
SELECT a FROM pv_sq_t WHERE a > (SELECT avg(d) FROM pv_sq_u)
-- q: rowsort I
SELECT a FROM pv_sq_t WHERE a NOT IN (SELECT d FROM pv_sq_u WHERE d > 20)
-- q: rowsort I
SELECT a FROM pv_sq_t WHERE b = ANY (SELECT b FROM pv_sq_u WHERE d = 3)
-- q: rowsort I
SELECT a FROM pv_sq_t WHERE a > ALL (SELECT d FROM pv_sq_u WHERE d < 30)
-- q: rowsort II
SELECT s.a, s.c FROM (SELECT a, b * 2 AS c FROM pv_sq_t WHERE a < 20) s WHERE s.c > 10
-- q: rowsort II
SELECT s.b, s.n FROM (SELECT b, count(*) AS n FROM pv_sq_u GROUP BY b) s JOIN pv_sq_t t ON t.a = s.b
-- q: rowsort I
WITH c AS (SELECT a FROM pv_sq_t WHERE a < 10) SELECT a FROM c
-- q: rowsort II
WITH c AS (SELECT a, b FROM pv_sq_t WHERE a < 10) SELECT c1.a, c2.a FROM c c1 JOIN c c2 ON c1.b = c2.b AND c1.a < c2.a
-- q: rowsort I
WITH c AS MATERIALIZED (SELECT d FROM pv_sq_u WHERE d < 5) SELECT a FROM pv_sq_t WHERE a IN (SELECT d FROM c)
-- q: rowsort I
WITH c AS NOT MATERIALIZED (SELECT d FROM pv_sq_u WHERE d < 5) SELECT d FROM c
-- q: rowsort I
WITH c AS (SELECT d FROM pv_sq_u) SELECT 1 WHERE EXISTS (SELECT 1 FROM c WHERE d = 7)
-- q: rowsort II
SELECT a, (SELECT d FROM pv_sq_u u WHERE u.d = t.a + 100) FROM pv_sq_t t WHERE a < 4
-- q: rowsort I
SELECT a FROM pv_sq_t t WHERE EXISTS (SELECT 1 FROM pv_sq_u u WHERE u.d = t.a AND u.b = (SELECT max(b) FROM pv_sq_u))
-- q: rowsort I
SELECT (SELECT sum(a) FROM pv_sq_t WHERE b = s.b) FROM (SELECT DISTINCT b FROM pv_sq_u) s
-- teardown
DROP TABLE pv_sq_t
DROP TABLE pv_sq_u
