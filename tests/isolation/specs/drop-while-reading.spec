# 読み取り中のテーブルの DROP は、読み取りのトランザクションが終わるまで待つ。
# 待っている間も、読み取り側は同じテーブルを読み続けられる。

setup
{
  CREATE TABLE dwr_t (id int, v int);
  INSERT INTO dwr_t VALUES (1, 10), (2, 20);
}

teardown
{
  DROP TABLE IF EXISTS dwr_t;
}

session a
step a_b { BEGIN; }
step a_sel { SELECT id, v FROM dwr_t ORDER BY id; }
step a_c { COMMIT; }

session b
step b_drop { DROP TABLE dwr_t; }

session c
step c_sel { SELECT count(*) FROM dwr_t; }

permutation a_b a_sel b_drop a_sel a_c
permutation a_b a_sel b_drop a_c c_sel
