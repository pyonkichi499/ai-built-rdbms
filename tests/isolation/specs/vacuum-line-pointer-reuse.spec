# VACUUM が行を消せるかは、最も古いスナップショット（horizon）で決まる。ctid で観測する。
#  - RR のスナップショットが残っている間は VACUUM は削除済みの行を消さず、新しい INSERT の ctid は進む
#  - スナップショットが無ければ VACUUM が消し、その行ポインタが再利用される

setup
{
  CREATE TABLE vlp_t (id int, v int);
  INSERT INTO vlp_t VALUES (1, 10), (2, 20), (3, 30);
}

teardown
{
  DROP TABLE vlp_t;
}

session a
step a_rr { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step a_sel { SELECT count(*) FROM vlp_t; }
step a_c { COMMIT; }

session b
step b_del { DELETE FROM vlp_t WHERE id = 3; }
step b_vacuum { VACUUM vlp_t; }
step b_ins { INSERT INTO vlp_t VALUES (4, 40); }
step b_ctid { SELECT id, ctid FROM vlp_t ORDER BY id; }

permutation a_rr a_sel b_del b_vacuum b_ins b_ctid a_c
permutation b_del b_vacuum b_ins b_ctid
