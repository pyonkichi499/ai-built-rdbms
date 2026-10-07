-- family: join_outer
-- profiles: default,no_hashjoin,no_nestloop,no_hashjoin_no_nestloop,no_indexscan,no_seqscan,combined
-- setup
CREATE TABLE pv_jo_t (a int PRIMARY KEY, b int, c text)
CREATE TABLE pv_jo_u (d int PRIMARY KEY, b int, e int)
CREATE TABLE pv_jo_v (f int, g int)
CREATE INDEX pv_jo_u_b ON pv_jo_u (b)
INSERT INTO pv_jo_t SELECT g, g % 6, 't' || g FROM generate_series(1, 60) g
INSERT INTO pv_jo_u SELECT g * 2, g % 4, g % 5 FROM generate_series(1, 40) g
INSERT INTO pv_jo_u VALUES (101, NULL, 3)
INSERT INTO pv_jo_v SELECT g % 12, g FROM generate_series(1, 30) g
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.a = u.d WHERE t.a < 20
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t RIGHT JOIN pv_jo_u u ON t.a = u.d WHERE u.d < 30
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t FULL JOIN pv_jo_u u ON t.a = u.d WHERE t.a IS NULL OR u.d IS NULL OR t.a < 5
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.a = u.d AND u.e = 3 WHERE t.a < 30
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.a = u.d WHERE u.e = 3
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.a = u.d WHERE u.d IS NULL AND t.a < 20
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.b = u.b WHERE t.a < 4
-- q: rowsort III
SELECT t.a, u.d, v.g FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.a = u.d LEFT JOIN pv_jo_v v ON v.f = t.b WHERE t.a < 6
-- q: rowsort III
SELECT t.a, u.d, v.g FROM pv_jo_t t LEFT JOIN (pv_jo_u u JOIN pv_jo_v v ON u.b = v.f) ON t.a = u.d WHERE t.a < 14
-- q: rowsort II
SELECT t.a, v.g FROM pv_jo_t t LEFT JOIN pv_jo_v v ON t.a = v.f WHERE t.a BETWEEN 5 AND 15
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t FULL JOIN pv_jo_u u ON t.a = u.d AND t.a > 10 WHERE t.a IS NULL OR u.d IS NULL
-- q: rowsort II
SELECT u.d, v.g FROM pv_jo_u u RIGHT JOIN pv_jo_v v ON u.d = v.f WHERE v.g < 15
-- q: rowsort I
SELECT count(*) FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.a = u.d
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.a = u.d WHERE u.d IS NOT NULL AND t.a < 25
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_t t LEFT JOIN pv_jo_u u ON t.a = u.d AND t.b = u.b WHERE t.a < 10
-- q: rowsort II
SELECT t.a, u.d FROM pv_jo_u u LEFT JOIN pv_jo_t t ON t.a = u.d WHERE u.d > 70
-- teardown
DROP TABLE pv_jo_t
DROP TABLE pv_jo_u
DROP TABLE pv_jo_v
