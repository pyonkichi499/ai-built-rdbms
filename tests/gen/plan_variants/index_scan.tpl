-- family: index_scan
-- profiles: default,no_indexscan,no_seqscan,no_sort,combined
-- setup
CREATE TABLE pv_ix_t (a int PRIMARY KEY, b int, c int, d text)
CREATE INDEX pv_ix_t_b ON pv_ix_t (b)
CREATE INDEX pv_ix_t_cb ON pv_ix_t (c, b DESC)
CREATE INDEX pv_ix_t_d ON pv_ix_t (d NULLS FIRST)
INSERT INTO pv_ix_t SELECT g, g % 13, g % 5, 'k' || (g % 17) FROM generate_series(1, 200) g
INSERT INTO pv_ix_t VALUES (201, NULL, NULL, NULL), (202, NULL, 1, NULL)
-- q: rowsort IIIT
SELECT * FROM pv_ix_t WHERE a = 42
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE a BETWEEN 10 AND 20
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE a > 195
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE a < 5 OR a > 198
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE b = 3 AND a < 100
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE b IS NULL
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE b IS NOT NULL AND b > 11
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE c = 2 AND b >= 5
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE c = 4
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE b = 7 AND c = 1
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE d = 'k3'
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE d IS NULL
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE d >= 'k5' AND d < 'k8'
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE a IN (3, 33, 133, 999)
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE a = 5 OR b = 5
-- q: ordered I
SELECT a FROM pv_ix_t WHERE b = 4 ORDER BY a
-- q: ordered II
SELECT c, b FROM pv_ix_t WHERE c = 3 ORDER BY b DESC
-- q: rowsort I
SELECT a FROM pv_ix_t WHERE a = -1
-- teardown
DROP TABLE pv_ix_t
