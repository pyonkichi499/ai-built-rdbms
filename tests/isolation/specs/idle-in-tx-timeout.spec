# A が書き込んだまま放置されると idle_in_transaction_session_timeout で切断（FATAL 25P03）され、
# 変更はロールバックされる。待っていた B が進み、元の値を基に更新する。

setup
{
  CREATE TABLE iit_t (id int, v int);
  INSERT INTO iit_t VALUES (1, 10);
}

teardown
{
  DROP TABLE iit_t;
}

session a
step a_set { SET idle_in_transaction_session_timeout = '700ms'; }
step a_b { BEGIN; }
step a_upd { UPDATE iit_t SET v = 999 WHERE id = 1; }

session b
step b_upd { UPDATE iit_t SET v = v + 1 WHERE id = 1; }
step b_sel { SELECT v FROM iit_t; }

session c
step c_sel { SELECT v FROM iit_t; }

permutation a_set a_b a_upd b_upd c_sel b_sel
