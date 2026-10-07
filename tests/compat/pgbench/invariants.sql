select (select sum(abalance) from pgbench_accounts) as a,
       (select sum(tbalance) from pgbench_tellers) as t,
       (select sum(bbalance) from pgbench_branches) as b,
       (select sum(delta) from pgbench_history) as h;
