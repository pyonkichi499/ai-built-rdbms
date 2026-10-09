# テーブルロックの待ち行列。DROP が待つと、後続の SELECT も FIFO でその後ろに並ぶ。
# 待ち明けにはテーブルが無く 42P01。ALTER TABLE の待ちは ROLLBACK で再開。LOCK TABLE はブロック外で 25P01。
# TRUNCATE は ACCESS EXCLUSIVE なので lock_timeout で 55P03。

setup
{
  CREATE TABLE ltq_t (id int, v int);
  INSERT INTO ltq_t VALUES (1, 10);
}

teardown
{
  DROP TABLE IF EXISTS ltq_t;
}

session a
step a_b { BEGIN; }
step a_sel { SELECT count(*) FROM ltq_t; }
step a_drop { DROP TABLE ltq_t; }
step a_alter { ALTER TABLE ltq_t ADD COLUMN w int; }
step a_c { COMMIT; }
step a_r { ROLLBACK; }

session b
step b_drop { DROP TABLE ltq_t; }
step b_sel { SELECT count(*) FROM ltq_t; }
step b_ins { INSERT INTO ltq_t VALUES (2, 20); }
step b_lock { LOCK TABLE ltq_t; }
step b_set_lt { SET lock_timeout = '300ms'; }
step b_sel2 { SELECT count(*) FROM ltq_t; }
step b_truncate { TRUNCATE ltq_t; }

session c
step c_sel { SELECT count(*) FROM ltq_t; }

# 8
permutation a_b a_sel b_drop c_sel a_c
# 8b
permutation a_b a_drop b_sel a_c
# 8c
permutation a_b a_alter b_ins a_r c_sel
# 8e
permutation b_lock
# 8f
permutation a_b a_sel b_set_lt b_truncate b_sel2 a_c c_sel
