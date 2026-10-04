# 同じ行を更新する 2 つの書き込みは待ち合う（m3-tx-semantics §4.1 の 4 パターン）。
#
# A が先に更新（未コミット）、B が後から同じ行を更新する。B は A の終了まで待ち、
# A の COMMIT なら最新の値を基に、ROLLBACK なら元の値を基に実行される。
# 別の行への更新は PostgreSQL では待たない（yuzhu の M3 は待つ）ので、必ず同じ行を使う。

setup
{
  CREATE TABLE www_t (id int, v int);
  INSERT INTO www_t VALUES (1, 10), (2, 20), (3, 30);
}

teardown
{
  DROP TABLE www_t;
}

session a
step a_b { BEGIN; }
step a_inc { UPDATE www_t SET v = v + 1 WHERE id = 1; }
step a_zero { UPDATE www_t SET v = 0 WHERE id = 2; }
step a_del { DELETE FROM www_t WHERE id = 3; }
step a_set500 { UPDATE www_t SET v = 500 WHERE id = 1; }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_b { BEGIN; }
step b_mul { UPDATE www_t SET v = v * 10 WHERE id = 1; }
step b_cond { UPDATE www_t SET v = -1 WHERE id = 2 AND v = 20; }
step b_upd3 { UPDATE www_t SET v = 99 WHERE id = 3; }
step b_del1 { DELETE FROM www_t WHERE id = 1; }
step b_c { COMMIT; }

session c
step c_sel { SELECT id, v FROM www_t ORDER BY id; }

# A の結果 11 を基に 110
permutation a_b a_inc b_b b_mul a_c b_c c_sel
# 最新版が条件を満たさないので 0 行
permutation a_b a_zero b_b b_cond a_c b_c c_sel
# A が削除した行は更新できない
permutation a_b a_del b_b b_upd3 a_c b_c c_sel
# A のロールバックで元の版（10）を基に 100
permutation a_b a_set500 b_b b_mul a_r b_c c_sel
# DELETE も待つ
permutation a_b a_inc b_b b_del1 a_c b_c c_sel
permutation a_b a_inc b_b b_del1 a_r b_c c_sel
# 自動コミットの文でも同じ
permutation a_b a_inc b_mul a_c c_sel
