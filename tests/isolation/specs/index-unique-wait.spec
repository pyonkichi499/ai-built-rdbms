# 同じキーを入れる 2 つの書き込みは待ち合う（PRIMARY KEY / UNIQUE）。
# 先の INSERT が COMMIT なら後のものは 23505、ROLLBACK なら後のものが成功する。

setup
{
  CREATE TABLE iso_uw (id int PRIMARY KEY, s text UNIQUE);
}

teardown
{
  DROP TABLE iso_uw;
}

session a
step a_b { BEGIN; }
step a_ins { INSERT INTO iso_uw VALUES (1, 'x'); }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_ins { INSERT INTO iso_uw VALUES (1, 'y'); }
step b_ins_s { INSERT INTO iso_uw VALUES (2, 'x'); }

session c
step c_sel { SELECT id, s FROM iso_uw ORDER BY id; }

permutation a_b a_ins b_ins a_c c_sel
permutation a_b a_ins b_ins a_r c_sel
permutation a_b a_ins b_ins_s a_c c_sel
permutation a_b a_ins b_ins_s a_r c_sel
