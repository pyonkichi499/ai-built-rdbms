# 待ち中のステップを CancelRequest で中断すると 57014（canceling statement due to user request）。
# `-- @cancel <セッション>` のステップは tests/tools/isolation の拡張で、SQL を送らず
# 対象セッションの実行中の問い合わせに CancelRequest を送る。

setup
{
  CREATE TABLE cnc_t (id int, v int);
  INSERT INTO cnc_t VALUES (1, 10);
}

teardown
{
  DROP TABLE cnc_t;
}

session a
step a_b { BEGIN; }
step a_upd { UPDATE cnc_t SET v = 11 WHERE id = 1; }
step a_c { COMMIT; }

session b
step b_b { BEGIN; }
step b_upd { UPDATE cnc_t SET v = v + 100 WHERE id = 1; }
step b_sel { SELECT v FROM cnc_t; }
step b_r { ROLLBACK; }

session k
step k_cancel_b { -- @cancel b }

session c
step c_sel { SELECT v FROM cnc_t; }

# 自動コミットの文をキャンセル
permutation a_b a_upd b_upd k_cancel_b a_c c_sel
# ブロック内の文をキャンセル → 失敗状態
permutation a_b a_upd b_b b_upd k_cancel_b b_sel b_r a_c c_sel
