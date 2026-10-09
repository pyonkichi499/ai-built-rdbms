# テーブルロックのデッドロック。別々のテーブルを ACCESS EXCLUSIVE で取り、互いのテーブルを読む。

setup
{
  CREATE TABLE dlt_x (id int);
  CREATE TABLE dlt_y (id int);
  INSERT INTO dlt_x VALUES (1);
  INSERT INTO dlt_y VALUES (1);
}

teardown
{
  DROP TABLE dlt_x;
  DROP TABLE dlt_y;
}

session a
step a_set { SET deadlock_timeout = '10s'; }
step a_b { BEGIN; }
step a_lock { LOCK TABLE dlt_x IN ACCESS EXCLUSIVE MODE; }
step a_sel { SELECT count(*) FROM dlt_y; }
step a_c { COMMIT; }

session b
step b_set { SET deadlock_timeout = '100ms'; }
step b_b { BEGIN; }
step b_lock { LOCK TABLE dlt_y IN ACCESS EXCLUSIVE MODE; }
step b_sel { SELECT count(*) FROM dlt_x; }
step b_r { ROLLBACK; }

permutation a_set b_set a_b b_b a_lock b_lock a_sel b_sel b_r a_c
