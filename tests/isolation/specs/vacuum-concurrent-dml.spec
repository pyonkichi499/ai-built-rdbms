# VACUUM は未コミットの INSERT / UPDATE / DELETE を待たず（ShareUpdateExclusive）、
# 見えるデータを変えない。VERBOSE は使わない（NOTICE の文言は出力に出さない）。

setup
{
  CREATE TABLE vcd_t (id int, v int);
  INSERT INTO vcd_t VALUES (1, 10), (2, 20), (3, 30);
}

teardown
{
  DROP TABLE vcd_t;
}

session a
step a_b { BEGIN; }
step a_ins { INSERT INTO vcd_t VALUES (4, 40); }
step a_upd { UPDATE vcd_t SET v = v + 1 WHERE id = 1; }
step a_del { DELETE FROM vcd_t WHERE id = 2; }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_vacuum { VACUUM vcd_t; }
step b_sel { SELECT id, v FROM vcd_t ORDER BY id; }

permutation a_b a_ins b_vacuum b_sel a_c b_vacuum b_sel
permutation a_b a_upd b_vacuum b_sel a_c b_vacuum b_sel
permutation a_b a_del b_vacuum b_sel a_r b_vacuum b_sel
permutation a_b a_del b_vacuum b_sel a_c b_vacuum b_sel
