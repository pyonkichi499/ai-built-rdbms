# 更新の消失（lost update）が起きないこと。
#
# 同じ行を 2 つのトランザクションが UPDATE すると、後から来た方は先の方の
# コミット／ロールバックまで待たされる。
#
# - Read Committed: 先がコミットしたら、後は最新の行を読み直して更新する（100-10-20 = 70）。
#   先がロールバックしたら、元の行を更新する（100-20 = 80）。
# - Repeatable Read: 先がコミットしたら、後は
#   ERROR 40001 could not serialize access due to concurrent update になる（残高は 90）。
#   先がロールバックしたら、後の更新は成功する（80）。

setup
{
  CREATE TABLE acct (id int, balance int);
  INSERT INTO acct VALUES (1, 100);
}

teardown
{
  DROP TABLE acct;
}

session s1
step s1b { BEGIN; }
step s1rr { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1u { UPDATE acct SET balance = balance - 10 WHERE id = 1; }
step s1c { COMMIT; }
step s1a { ROLLBACK; }

session s2
step s2b { BEGIN; }
step s2rr { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s2u { UPDATE acct SET balance = balance - 20 WHERE id = 1; }
step s2c { COMMIT; }
step s2sel { SELECT balance FROM acct WHERE id = 1; }

permutation s1b s1u s2b s2u s1c s2c s2sel
permutation s1b s1u s2b s2u s1a s2c s2sel
permutation s1rr s1u s2rr s2u s1c s2c s2sel
permutation s1rr s1u s2rr s2u s1a s2c s2sel
