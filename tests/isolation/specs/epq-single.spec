# Read Committed で、待った後に最新版へ追随する（EvalPlanQual。単一テーブルだけ）。
#  - 待った後は最新版に対して SET を再計算する（更新消失なし）
#  - 最新版で WHERE を再評価して外れたら更新しない
#  - 相手が削除してコミットした行は黙って飛ばす
#  - SELECT ... FOR UPDATE も最新版で再評価する

setup
{
  CREATE TABLE epq_t (id int, v int);
  INSERT INTO epq_t VALUES (1, 10), (2, 20);
}

teardown
{
  DROP TABLE epq_t;
}

session a
step a_b { BEGIN; }
step a_inc { UPDATE epq_t SET v = v + 1 WHERE id = 1; }
step a_set99 { UPDATE epq_t SET v = 99 WHERE id = 1; }
step a_del { DELETE FROM epq_t WHERE id = 1; }
step a_c { COMMIT; }

session b
step b_inc { UPDATE epq_t SET v = v + 1 WHERE id = 1; }
step b_where_old { UPDATE epq_t SET v = v + 1 WHERE v = 10; }
step b_forupd_old { SELECT id, v FROM epq_t WHERE v = 10 FOR UPDATE; }
step b_forupd_id { SELECT id, v FROM epq_t WHERE id = 1 FOR UPDATE; }

session c
step c_sel { SELECT id, v FROM epq_t ORDER BY id; }

permutation a_b a_inc b_inc a_c c_sel
permutation a_b a_set99 b_where_old a_c c_sel
permutation a_b a_del b_inc a_c c_sel
permutation a_b a_set99 b_forupd_id a_c c_sel
permutation a_b a_set99 b_forupd_old a_c c_sel
