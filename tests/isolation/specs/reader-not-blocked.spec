# 書き込み中の相手がいても、読み取りは待たず、古い（コミット済みの）値を見る。
# 書き込みが待っている間も同じ。

setup
{
  CREATE TABLE rnb_t (id int, v int);
  INSERT INTO rnb_t VALUES (1, 10), (2, 20);
}

teardown
{
  DROP TABLE rnb_t;
}

session a
step a_b { BEGIN; }
step a_upd { UPDATE rnb_t SET v = 11 WHERE id = 1; }
step a_ins { INSERT INTO rnb_t VALUES (3, 30); }
step a_del { DELETE FROM rnb_t WHERE id = 2; }
step a_lockre { LOCK TABLE rnb_t IN ROW EXCLUSIVE MODE; }
step a_lockae { LOCK TABLE rnb_t IN ACCESS EXCLUSIVE MODE; }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_upd { UPDATE rnb_t SET v = v + 100 WHERE id = 1; }

session r
step r_sel { SELECT id, v FROM rnb_t ORDER BY id; }
step r_b { BEGIN; }
step r_c { COMMIT; }

permutation a_b a_upd a_ins a_del r_sel a_c r_sel
permutation a_b a_upd a_ins a_del r_sel a_r r_sel
# 待っている書き込みがあっても読み取りは進む
permutation a_b a_upd b_upd r_sel a_c r_sel
permutation a_b a_upd b_upd r_sel a_r r_sel
# 読み取りトランザクションを開いたままでも、書き込みは待たされない（読み取りはロックを持たない）
permutation r_b r_sel a_b a_upd a_c r_sel r_c
# テーブルロック: ROW EXCLUSIVE は読み取りと衝突しない
permutation a_b a_lockre r_sel a_c r_sel
# ACCESS EXCLUSIVE は読み取りを待たせ、解放で再開する
permutation a_b a_lockae r_sel a_c
permutation a_b a_lockae r_sel a_r
