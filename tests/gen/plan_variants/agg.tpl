-- family: agg
-- profiles: default,no_hashagg,no_sort,no_indexscan,no_seqscan,combined
-- setup
CREATE TABLE pv_ag_t (a int PRIMARY KEY, g int, h int, v int)
CREATE INDEX pv_ag_t_g ON pv_ag_t (g)
CREATE TABLE pv_ag_e (a int, v int)
INSERT INTO pv_ag_t SELECT i, i % 7, i % 3, (i * 37) % 101 FROM generate_series(1, 150) i
INSERT INTO pv_ag_t VALUES (151, NULL, 1, NULL), (152, NULL, NULL, 5)
-- q: rowsort III
SELECT g, count(*), sum(v) FROM pv_ag_t GROUP BY g
-- q: rowsort III
SELECT g, h, count(*) FROM pv_ag_t GROUP BY g, h
-- q: rowsort II
SELECT g, count(*) FROM pv_ag_t GROUP BY g HAVING count(*) > 20
-- q: rowsort II
SELECT g, max(v) FROM pv_ag_t WHERE a < 100 GROUP BY g HAVING min(v) < 10
-- q: rowsort I
SELECT DISTINCT g FROM pv_ag_t
-- q: rowsort II
SELECT DISTINCT g, h FROM pv_ag_t WHERE a < 60
-- q: rowsort I
SELECT count(DISTINCT v) FROM pv_ag_t
-- q: rowsort II
SELECT g, count(DISTINCT h) FROM pv_ag_t GROUP BY g
-- q: rowsort II
SELECT g, count(*) FILTER (WHERE v > 50) FROM pv_ag_t GROUP BY g
-- q: rowsort III
SELECT a, g, count(*) FROM pv_ag_t WHERE a < 6 GROUP BY a
-- q: rowsort IIII
SELECT count(*), count(v), sum(v), max(v) FROM pv_ag_t
-- q: rowsort IIII
SELECT count(*), sum(v), min(v), max(v) FROM pv_ag_e
-- q: rowsort II
SELECT a, sum(v) FROM pv_ag_e GROUP BY a
-- q: rowsort R
SELECT avg(v) FROM pv_ag_t WHERE g = 3
-- q: ordered II
SELECT g, sum(v) FROM pv_ag_t GROUP BY g ORDER BY g
-- q: ordered II
SELECT g, count(*) FROM pv_ag_t WHERE g IS NOT NULL GROUP BY g HAVING sum(v) > 100 ORDER BY 2 DESC, 1
-- q: rowsort I
SELECT g + h FROM pv_ag_t GROUP BY g + h
-- q: rowsort II
SELECT h, count(*) FROM (SELECT g, h FROM pv_ag_t WHERE g = 1) s GROUP BY h
-- teardown
DROP TABLE pv_ag_t
DROP TABLE pv_ag_e
