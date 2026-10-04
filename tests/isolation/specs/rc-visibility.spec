# Read Committed の可視性（ブロックしない交互実行）。
#
# - 他のトランザクションの未コミットの変更は見えず、自分の変更は見える。
# - Read Committed は文ごとにスナップショットを取り直すので、相手がコミットすると
#   同じトランザクションの次の文から見える。
# - 行ロックを持っている相手がいても、読み取りは待たされない。
# - ロールバックされた変更は最後まで見えない。

setup
{
  CREATE TABLE rcv (id int, val int);
  INSERT INTO rcv VALUES (1, 10);
}

teardown
{
  DROP TABLE rcv;
}

session s1
step s1b { BEGIN; }
step s1r { SELECT id, val FROM rcv ORDER BY id; }
step s1c { COMMIT; }

session s2
step s2b { BEGIN; }
step s2u { UPDATE rcv SET val = 20 WHERE id = 1; }
step s2i { INSERT INTO rcv VALUES (2, 30); }
step s2r { SELECT id, val FROM rcv ORDER BY id; }
step s2c { COMMIT; }
step s2a { ROLLBACK; }

permutation s1b s1r s2b s2u s2i s2r s1r s2c s1r s1c
permutation s1b s2b s2u s2i s1r s2a s1r s1c
