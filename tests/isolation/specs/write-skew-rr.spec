# Repeatable Read（スナップショット分離）では write skew を防げないこと。
#
# 「当番の医師が最低 1 人いる」という不変条件を、2 人が同時に
# 「自分以外にも当番がいるので自分は外れる」と判断して破る古典的な例。
# 互いに別の行を更新するので行ロックの競合は起きず、両方ともコミットに成功し、
# 当番が 0 人になる（PostgreSQL の Repeatable Read の正しい挙動）。
# SERIALIZABLE なら片方が 40001 で失敗するが、それはこのテストの範囲外。

setup
{
  CREATE TABLE doctors (name text, on_call bool);
  INSERT INTO doctors VALUES ('alice', true), ('bob', true);
}

teardown
{
  DROP TABLE doctors;
}

session s1
setup { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1r { SELECT count(*) AS on_call FROM doctors WHERE on_call; }
step s1w { UPDATE doctors SET on_call = false WHERE name = 'alice'; }
step s1c { COMMIT; }

session s2
setup { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s2r { SELECT count(*) AS on_call FROM doctors WHERE on_call; }
step s2w { UPDATE doctors SET on_call = false WHERE name = 'bob'; }
step s2c { COMMIT; }

session s3
step s3check { SELECT name, on_call FROM doctors ORDER BY name; }

permutation s1r s2r s1w s2w s1c s2c s3check
# s1 の更新が未コミットのうちに s2 がスナップショットを取るので、s2 にも 2 人に見える。
permutation s1r s1w s2r s2w s1c s2c s3check
