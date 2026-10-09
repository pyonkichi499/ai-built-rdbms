# 同じ行を更新する書き込みが 3 つ並んだとき、到着順に 1 つずつ進む。
# A の終了で B だけが再開し、C は B の終了まで待ち続ける。
# 別の行を更新する書き込みは待ち行列に入らない。

setup
{
  CREATE TABLE wq_t (id int, v int);
  INSERT INTO wq_t VALUES (1, 1);
  CREATE TABLE wq2_t (id int, v int);
  INSERT INTO wq2_t VALUES (2, 2);
}

teardown
{
  DROP TABLE wq_t;
  DROP TABLE wq2_t;
}

session a
step a_b { BEGIN; }
step a_upd { UPDATE wq_t SET v = v * 2 WHERE id = 1; }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_b { BEGIN; }
step b_upd { UPDATE wq_t SET v = v + 10 WHERE id = 1; }
step b_upd2 { UPDATE wq2_t SET v = v + 20 WHERE id = 2; }
step b_c { COMMIT; }

session c
step c_b { BEGIN; }
step c_upd { UPDATE wq_t SET v = v * 3 WHERE id = 1; }
step c_c { COMMIT; }
step c_sel { SELECT v FROM wq_t; }
step c_sel2 { SELECT v FROM wq2_t; }

permutation a_b a_upd b_b b_upd c_b c_upd a_c b_c c_c c_sel
permutation a_b a_upd b_b b_upd c_b c_upd a_r b_c c_c c_sel
# A と B は別の行（B は別のテーブルの行）、C は A と同じ行。C だけが待つ
permutation a_b a_upd b_b b_upd2 c_b c_upd a_c b_c c_c c_sel c_sel2
permutation a_b a_upd b_b b_upd2 c_b c_upd a_r b_c c_c c_sel c_sel2
