# 索引つきの表で、読み手は書き手を待たず、コミット済みの版だけを索引走査でも見る（B+Tree の分割中・ロールバック後も）。
# 書き手が INSERT / UPDATE（キーの変更を含む）した後、COMMIT なら新しい値、ROLLBACK なら元の値が見える。

setup
{
  CREATE TABLE iso_ix (id int PRIMARY KEY, v int);
  CREATE INDEX iso_ix_v ON iso_ix (v);
  INSERT INTO iso_ix SELECT g, g % 10 FROM generate_series(1, 200) g;
}

teardown
{
  DROP TABLE iso_ix;
}

session w
step w_begin { BEGIN; }
step w_ins { INSERT INTO iso_ix SELECT g, g % 10 FROM generate_series(201, 3000) g; }
step w_upd { UPDATE iso_ix SET v = v + 100 WHERE id <= 50; }
step w_commit { COMMIT; }
step w_rollback { ROLLBACK; }

session r
step r_set { SET enable_seqscan = off; }
step r_idx { SELECT count(*), sum(v) FROM iso_ix WHERE v BETWEEN 0 AND 9; }
step r_pk { SELECT count(*) FROM iso_ix WHERE id BETWEEN 1 AND 3000; }

permutation r_set w_begin w_ins w_upd r_idx r_pk w_commit r_idx r_pk
permutation r_set w_begin w_ins w_upd r_idx r_pk w_rollback r_idx r_pk
