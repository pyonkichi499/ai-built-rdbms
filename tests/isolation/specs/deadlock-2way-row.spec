# 2 者の行ロックのデッドロック。deadlock_timeout が短い b が後から待ち、b の検査で検出されて b が被害者になる。
# 共有ロック（FOR SHARE）を持ち合ったまま両者が UPDATE する昇格のデッドロックも同じ。

setup
{
  CREATE TABLE dl2_t (id int PRIMARY KEY, v int);
  INSERT INTO dl2_t VALUES (1, 10), (2, 20);
}

teardown
{
  DROP TABLE dl2_t;
}

session a
step a_set { SET deadlock_timeout = '10s'; }
step a_b { BEGIN; }
step a_u1 { UPDATE dl2_t SET v = v + 1 WHERE id = 1; }
step a_u2 { UPDATE dl2_t SET v = v + 1 WHERE id = 2; }
step a_share1 { SELECT id FROM dl2_t WHERE id = 1 FOR SHARE; }
step a_c { COMMIT; }

session b
step b_set { SET deadlock_timeout = '100ms'; }
step b_b { BEGIN; }
step b_u1 { UPDATE dl2_t SET v = v + 10 WHERE id = 1; }
step b_u2 { UPDATE dl2_t SET v = v + 10 WHERE id = 2; }
step b_share1 { SELECT id FROM dl2_t WHERE id = 1 FOR SHARE; }
step b_r { ROLLBACK; }

session c
step c_sel { SELECT id, v FROM dl2_t ORDER BY id; }

permutation a_set b_set a_b b_b a_u1 b_u2 a_u2 b_u1 b_r a_c c_sel
permutation a_set b_set a_b b_b a_share1 b_share1 a_u1 b_u1 b_r a_c c_sel
