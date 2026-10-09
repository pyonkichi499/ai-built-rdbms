# FOREIGN KEY の親の削除と子の挿入の競合。子の INSERT と親の DELETE だけを使う。
# 先に動いた側の終了を待つ。子の INSERT がコミットされれば親の DELETE は 23503、
# ROLLBACK なら成功。親の DELETE がコミットされれば子の INSERT は 23503。

setup
{
  CREATE TABLE fpd_p (id int PRIMARY KEY);
  CREATE TABLE fpd_c (id int PRIMARY KEY, pid int REFERENCES fpd_p (id));
  INSERT INTO fpd_p VALUES (1), (2);
}

teardown
{
  DROP TABLE fpd_c;
  DROP TABLE fpd_p;
}

session a
step a_b { BEGIN; }
step a_ins { INSERT INTO fpd_c VALUES (1, 1); }
step a_del { DELETE FROM fpd_p WHERE id = 1; }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_del { DELETE FROM fpd_p WHERE id = 1; }
step b_ins { INSERT INTO fpd_c VALUES (2, 1); }

session c
step c_sel_p { SELECT id FROM fpd_p ORDER BY id; }
step c_sel_c { SELECT id, pid FROM fpd_c ORDER BY id; }

permutation a_b a_ins b_del a_c c_sel_p c_sel_c
permutation a_b a_ins b_del a_r c_sel_p c_sel_c
permutation a_b a_del b_ins a_c c_sel_p c_sel_c
permutation a_b a_del b_ins a_r c_sel_p c_sel_c
