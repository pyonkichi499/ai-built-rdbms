# アイドルのトランザクションが idle_in_transaction_session_timeout で切断（FATAL）されると、
# XID ロック（行の待ち）もリレーションロックも解放され、待っていた側が順に進む。
#  b の UPDATE は a の XID を待ち、c の DROP は a と b のテーブルロックを待つ。
#  a が切断されると b が先に進み（元の値 10 を基に更新）、b の文が終わった後で c の DROP が通る。
# a の接続は切断されるので、permutation は 1 つだけにする。

setup
{
  CREATE TABLE isl_t (id int, v int);
  INSERT INTO isl_t VALUES (1, 10);
}

teardown
{
  DROP TABLE IF EXISTS isl_t;
}

session a
step a_set { SET idle_in_transaction_session_timeout = '700ms'; }
step a_b { BEGIN; }
step a_upd { UPDATE isl_t SET v = 999 WHERE id = 1; }

session b
step b_upd { UPDATE isl_t SET v = v + 1 WHERE id = 1 RETURNING v; }

session c
step c_drop { DROP TABLE isl_t; }

permutation a_set a_b a_upd b_upd c_drop
