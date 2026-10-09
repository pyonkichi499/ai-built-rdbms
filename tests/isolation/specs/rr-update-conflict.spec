# Repeatable Read でスナップショット後にコミットされた更新・削除に当たると 40001。
# 待った後で相手がコミットしても 40001、中断なら続行。スナップショットは最初の文で取る。
# 挿入（ファントム）だけ・読むだけの RR はエラーにならない。

setup
{
  CREATE TABLE rru_t (id int, v int);
  INSERT INTO rru_t VALUES (1, 10), (2, 20), (3, 30);
}

teardown
{
  DROP TABLE rru_t;
}

session a
step a_upd1 { UPDATE rru_t SET v = v + 1 WHERE id = 1; }
step a_del2 { DELETE FROM rru_t WHERE id = 2; }
step a_ins { INSERT INTO rru_t VALUES (4, 40); }
step a_updall { UPDATE rru_t SET v = v + 1000; }
step a_b { BEGIN; }
step a_b_upd1 { UPDATE rru_t SET v = v + 1 WHERE id = 1; }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_rr { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step b_sel { SELECT id, v FROM rru_t ORDER BY id; }
step b_sum { SELECT sum(v) FROM rru_t; }
step b_cnt { SELECT count(*) FROM rru_t; }
step b_upd1 { UPDATE rru_t SET v = v + 100 WHERE id = 1; }
step b_upd2 { UPDATE rru_t SET v = v + 100 WHERE id = 2; }
step b_del2 { DELETE FROM rru_t WHERE id = 2; }
step b_forupd1 { SELECT id, v FROM rru_t WHERE id = 1 FOR UPDATE; }
step b_ins { INSERT INTO rru_t VALUES (5, 50); }
step b_c { COMMIT; }
step b_r { ROLLBACK; }

session c
step c_sel { SELECT id, v FROM rru_t ORDER BY id; }

# 4: コミット済みの更新に当たる
permutation b_rr b_sel a_upd1 b_upd1 b_r c_sel
# 4b: 相手が中断なら続行できる
permutation b_rr b_sel a_b a_b_upd1 b_upd1 a_r b_c c_sel
# 4c: 待った後で相手がコミットしたら 40001
permutation b_rr b_sel a_b a_b_upd1 b_upd1 a_c b_r c_sel
# 4d: 削除に当たると concurrent delete
permutation b_rr b_sel a_del2 b_upd2 b_r c_sel
permutation b_rr b_sel a_del2 b_del2 b_r c_sel
# 4e: スナップショットは BEGIN ではなく最初の文で取る
permutation b_rr a_upd1 b_upd1 b_c c_sel
# 4f: FOR UPDATE も 40001
permutation b_rr b_sel a_upd1 b_forupd1 b_r c_sel
# 4g: 挿入されただけならエラーにならず、件数も変わらない
permutation b_rr b_cnt a_ins b_cnt b_c c_sel
# 13: 読むだけ・挿入だけの RR はエラーにならない
permutation b_rr b_sum a_updall b_sum b_ins b_c c_sel
