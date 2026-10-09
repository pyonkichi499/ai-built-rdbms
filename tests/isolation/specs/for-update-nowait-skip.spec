# 行ロックの NOWAIT / SKIP LOCKED / lock_timeout。行ロックは通常の SELECT を止めない。

setup
{
  CREATE TABLE fun_t (id int PRIMARY KEY, v int);
  INSERT INTO fun_t VALUES (1, 10), (2, 20), (3, 30);
}

teardown
{
  DROP TABLE fun_t;
}

session a
step a_b { BEGIN; }
step a_lock1 { SELECT id FROM fun_t WHERE id = 1 FOR UPDATE; }
step a_c { COMMIT; }

session b
step b_nowait { SELECT id FROM fun_t WHERE id = 1 FOR UPDATE NOWAIT; }
step b_share_nowait { SELECT id FROM fun_t WHERE id = 1 FOR SHARE NOWAIT; }
step b_skip { SELECT id FROM fun_t ORDER BY id FOR UPDATE SKIP LOCKED; }
step b_set_lt { SET lock_timeout = '200ms'; }
step b_upd { UPDATE fun_t SET v = v + 1 WHERE id = 1; }
step b_sel { SELECT id, v FROM fun_t ORDER BY id; }

permutation a_b a_lock1 b_nowait a_c
permutation a_b a_lock1 b_share_nowait a_c
permutation a_b a_lock1 b_skip a_c
permutation a_b a_lock1 b_set_lt b_upd b_sel a_c b_sel
permutation a_b a_lock1 b_sel a_c
