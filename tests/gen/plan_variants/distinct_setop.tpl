-- family: distinct_setop
-- profiles: default,no_hashagg,no_sort,no_indexscan,no_seqscan,combined
-- setup
CREATE TABLE pv_ds_t (a int, b int)
CREATE TABLE pv_ds_u (a int, b int)
CREATE INDEX pv_ds_t_a ON pv_ds_t (a)
INSERT INTO pv_ds_t SELECT g % 20, g % 6 FROM generate_series(1, 90) g
INSERT INTO pv_ds_u SELECT g % 25, g % 4 FROM generate_series(1, 70) g
INSERT INTO pv_ds_t VALUES (NULL, 1), (NULL, 1)
-- q: ordered II
SELECT DISTINCT ON (a) a, b FROM pv_ds_t WHERE a IS NOT NULL ORDER BY a, b DESC
-- q: rowsort I
SELECT a FROM pv_ds_t UNION SELECT a FROM pv_ds_u
-- q: rowsort I
SELECT a FROM pv_ds_t UNION ALL SELECT a FROM pv_ds_u
-- q: rowsort I
SELECT a FROM pv_ds_t INTERSECT SELECT a FROM pv_ds_u
-- q: rowsort I
SELECT a FROM pv_ds_t INTERSECT ALL SELECT a FROM pv_ds_u
-- q: rowsort I
SELECT a FROM pv_ds_t EXCEPT SELECT a FROM pv_ds_u
-- q: rowsort I
SELECT a FROM pv_ds_u EXCEPT ALL SELECT a FROM pv_ds_t
-- q: rowsort II
SELECT a, b FROM pv_ds_t UNION SELECT a, b FROM pv_ds_u
-- q: rowsort I
SELECT a FROM pv_ds_t WHERE b = 1 UNION SELECT a FROM pv_ds_u WHERE b = 2 UNION SELECT a FROM pv_ds_t WHERE b = 3
-- q: rowsort I
SELECT a FROM pv_ds_t EXCEPT (SELECT a FROM pv_ds_u INTERSECT SELECT a FROM pv_ds_t WHERE a > 5)
-- q: ordered I
SELECT a FROM pv_ds_t UNION SELECT a FROM pv_ds_u ORDER BY 1 DESC
-- q: ordered I
(SELECT a FROM pv_ds_t ORDER BY a LIMIT 5) UNION ALL (SELECT a FROM pv_ds_u ORDER BY a DESC LIMIT 5) ORDER BY 1
-- q: rowsort I
SELECT DISTINCT a FROM pv_ds_t WHERE a > 10
-- q: rowsort I
SELECT count(*) FROM (SELECT a FROM pv_ds_t UNION SELECT a FROM pv_ds_u) s
-- q: rowsort II
SELECT a, count(*) FROM (SELECT a FROM pv_ds_t UNION ALL SELECT a FROM pv_ds_u) s GROUP BY a
-- teardown
DROP TABLE pv_ds_t
DROP TABLE pv_ds_u
