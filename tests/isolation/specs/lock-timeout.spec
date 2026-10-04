# lock_timeout で待ちが打ち切られ 55P03。ブロック内なら以降は失敗状態（25P02）で、
# ROLLBACK すれば復帰する。A がコミットした後は同じ文が成功する。

setup
{
  CREATE TABLE lto_t (id int, v int);
  INSERT INTO lto_t VALUES (1, 10);
}

teardown
{
  DROP TABLE lto_t;
}

session a
step a_b { BEGIN; }
step a_upd { UPDATE lto_t SET v = 11 WHERE id = 1; }
step a_c { COMMIT; }

session b
step b_set { SET lock_timeout = '500ms'; }
step b_b { BEGIN; }
step b_upd { UPDATE lto_t SET v = v + 100 WHERE id = 1; }
step b_sel { SELECT v FROM lto_t; }
step b_r { ROLLBACK; }
step b_c { COMMIT; }

session c
step c_sel { SELECT v FROM lto_t; }

# 自動コミットの文: 打ち切られた後はそのまま使える
permutation b_set a_b a_upd b_upd a_c c_sel
# ブロック内: 打ち切られた後は 25P02、ROLLBACK で復帰
permutation b_set a_b a_upd b_b b_upd b_sel b_r a_c c_sel
# COMMIT は失敗状態でも受け付けられ、ROLLBACK として扱われる
permutation b_set a_b a_upd b_b b_upd b_c a_c c_sel
# 打ち切られた後、A のコミットを待って再実行すると成功する
permutation b_set a_b a_upd b_upd a_c b_upd c_sel
