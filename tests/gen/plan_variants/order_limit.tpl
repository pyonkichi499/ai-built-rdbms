-- family: order_limit
-- profiles: default,no_indexscan,no_seqscan,no_sort,combined
-- setup
CREATE TABLE pv_ol_t (a int PRIMARY KEY, b int, c int)
CREATE INDEX pv_ol_t_b ON pv_ol_t (b, a)
INSERT INTO pv_ol_t SELECT g, g % 11, (g * 7) % 23 FROM generate_series(1, 150) g
INSERT INTO pv_ol_t VALUES (151, NULL, 1), (152, NULL, 2)
-- q: ordered I
SELECT a FROM pv_ol_t ORDER BY a LIMIT 5
-- q: ordered I
SELECT a FROM pv_ol_t ORDER BY a DESC LIMIT 5
-- q: ordered I
SELECT a FROM pv_ol_t ORDER BY a LIMIT 5 OFFSET 7
-- q: ordered I
SELECT a FROM pv_ol_t ORDER BY a DESC LIMIT 4 OFFSET 150
-- q: ordered II
SELECT b, a FROM pv_ol_t ORDER BY b, a LIMIT 12
-- q: ordered II
SELECT b, a FROM pv_ol_t ORDER BY b DESC, a DESC LIMIT 12
-- q: ordered II
SELECT b, a FROM pv_ol_t ORDER BY b NULLS FIRST, a LIMIT 6
-- q: ordered II
SELECT b, a FROM pv_ol_t ORDER BY b DESC NULLS LAST, a LIMIT 6
-- q: ordered II
SELECT b, a FROM pv_ol_t WHERE b = 3 ORDER BY a DESC LIMIT 3
-- q: ordered II
SELECT c, a FROM pv_ol_t ORDER BY c, a LIMIT 9
-- q: ordered II
SELECT c, a FROM pv_ol_t WHERE a > 100 ORDER BY c DESC, a LIMIT 9
-- q: ordered I
SELECT a FROM pv_ol_t WHERE b > 5 ORDER BY a LIMIT 3
-- q: ordered I
SELECT a FROM pv_ol_t ORDER BY a OFFSET 148
-- q: ordered I
SELECT a FROM pv_ol_t ORDER BY a LIMIT 0
-- q: ordered II
SELECT a, b FROM pv_ol_t ORDER BY a % 10, a LIMIT 7
-- q: ordered I
SELECT a FROM (SELECT a, b FROM pv_ol_t ORDER BY b, a LIMIT 20) s ORDER BY a DESC LIMIT 4
-- teardown
DROP TABLE pv_ol_t
