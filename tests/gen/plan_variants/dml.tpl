-- family: dml
-- profiles: default,no_hashjoin,no_nestloop,no_hashjoin_no_nestloop,no_indexscan,no_seqscan,combined
-- setup
CREATE TABLE pv_dm_t (a int PRIMARY KEY, b int, c text)
CREATE TABLE pv_dm_u (d int, e int)
INSERT INTO pv_dm_t SELECT g, g % 5, 'r' || g FROM generate_series(1, 50) g
INSERT INTO pv_dm_u SELECT g * 3, g FROM generate_series(1, 20) g
-- s
BEGIN
-- s
UPDATE pv_dm_t SET b = b + 100 FROM pv_dm_u WHERE pv_dm_t.a = pv_dm_u.d
-- q: rowsort I
SELECT count(*) FROM pv_dm_t WHERE b >= 100
-- q: rowsort II
SELECT a, b FROM pv_dm_t WHERE b >= 100 AND a < 20
-- s
ROLLBACK
-- s
BEGIN
-- s
DELETE FROM pv_dm_t USING pv_dm_u WHERE pv_dm_t.a = pv_dm_u.d AND pv_dm_u.e > 10
-- q: rowsort I
SELECT count(*) FROM pv_dm_t
-- q: rowsort I
SELECT a FROM pv_dm_t WHERE a > 30 AND a < 50
-- s
ROLLBACK
-- s
BEGIN
-- s
INSERT INTO pv_dm_u SELECT a, b FROM pv_dm_t WHERE a IN (SELECT d FROM pv_dm_u)
-- q: rowsort I
SELECT count(*) FROM pv_dm_u
-- s
ROLLBACK
-- s
BEGIN
-- s
UPDATE pv_dm_t SET c = 'z' WHERE a IN (SELECT d FROM pv_dm_u WHERE e < 5)
-- q: rowsort II
SELECT a, c FROM pv_dm_t WHERE c = 'z'
-- s
ROLLBACK
-- s
BEGIN
-- s
DELETE FROM pv_dm_t WHERE NOT EXISTS (SELECT 1 FROM pv_dm_u WHERE d = a)
-- q: rowsort I
SELECT a FROM pv_dm_t
-- s
ROLLBACK
-- q: rowsort I
SELECT count(*) FROM pv_dm_t
-- teardown
DROP TABLE pv_dm_t
DROP TABLE pv_dm_u
