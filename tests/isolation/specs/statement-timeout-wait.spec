# 待ち中に statement_timeout が切れると 57014（canceling statement due to statement timeout）。

setup
{
  CREATE TABLE sto_t (id int, v int);
  INSERT INTO sto_t VALUES (1, 10);
}

teardown
{
  DROP TABLE sto_t;
}

session a
step a_b { BEGIN; }
step a_upd { UPDATE sto_t SET v = 11 WHERE id = 1; }
step a_c { COMMIT; }

session b
step b_set { SET statement_timeout = '500ms'; }
step b_b { BEGIN; }
step b_upd { UPDATE sto_t SET v = v + 100 WHERE id = 1; }
step b_sel { SELECT v FROM sto_t; }
step b_r { ROLLBACK; }

session c
step c_sel { SELECT v FROM sto_t; }

permutation b_set a_b a_upd b_upd a_c c_sel
permutation b_set a_b a_upd b_b b_upd b_sel b_r a_c c_sel
