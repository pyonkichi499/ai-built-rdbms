# 行ロックの強度の衝突表のうち、PostgreSQL と yuzhu で同じになる組み合わせだけ。
# （KEY SHARE × NO KEY UPDATE と、更新者 × KEY SHARE は含めない: KD-1）
# 衝突は NOWAIT の 55P03 で観測し、両立する組み合わせは成功する。

setup
{
  CREATE TABLE lsm_t (id int PRIMARY KEY, v int);
  INSERT INTO lsm_t VALUES (1, 10);
}

teardown
{
  DROP TABLE lsm_t;
}

session a
step a_b { BEGIN; }
step a_update { SELECT id FROM lsm_t WHERE id = 1 FOR UPDATE; }
step a_share { SELECT id FROM lsm_t WHERE id = 1 FOR SHARE; }
step a_keyshare { SELECT id FROM lsm_t WHERE id = 1 FOR KEY SHARE; }
step a_c { COMMIT; }

session b
step b_b { BEGIN; }
step b_update { SELECT id FROM lsm_t WHERE id = 1 FOR UPDATE NOWAIT; }
step b_share { SELECT id FROM lsm_t WHERE id = 1 FOR SHARE NOWAIT; }
step b_keyshare { SELECT id FROM lsm_t WHERE id = 1 FOR KEY SHARE NOWAIT; }
step b_c { COMMIT; }
step b_r { ROLLBACK; }

# FOR UPDATE は他のすべてと衝突する
permutation a_b a_update b_b b_update b_r a_c
permutation a_b a_update b_b b_share b_r a_c
permutation a_b a_update b_b b_keyshare b_r a_c
# SHARE × SHARE は両立、SHARE × UPDATE は衝突
permutation a_b a_share b_b b_share b_c a_c
permutation a_b a_share b_b b_update b_r a_c
# KEY SHARE × KEY SHARE は両立、KEY SHARE × FOR UPDATE は衝突
permutation a_b a_keyshare b_b b_keyshare b_c a_c
permutation a_b a_keyshare b_b b_update b_r a_c
