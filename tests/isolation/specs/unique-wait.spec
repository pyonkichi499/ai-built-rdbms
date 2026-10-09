# 一意検査は挿入者の終了を待つ。COMMIT なら 23505、ROLLBACK なら成功。

setup
{
  CREATE TABLE uw_t (id int PRIMARY KEY, v int);
}

teardown
{
  DROP TABLE uw_t;
}

session a
step a_b { BEGIN; }
step a_ins { INSERT INTO uw_t VALUES (10, 1); }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_ins { INSERT INTO uw_t VALUES (10, 2); }

session c
step c_sel { SELECT id, v FROM uw_t ORDER BY id; }

permutation a_b a_ins b_ins a_c c_sel
permutation a_b a_ins b_ins a_r c_sel
