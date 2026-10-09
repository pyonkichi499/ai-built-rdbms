-- psql 18 以降の \parse / \bind_named / \close_stmt（psql 17 には無い。U-8）。run.sh は psql が 18 以上のときだけ流す。
\set VERBOSITY terse
select $1::int + 1 as r \parse tsx_s1
\bind_named tsx_s1 41 \g
\bind_named tsx_s1 1 \g
select $1::text || '!' as r \parse tsx_s2
\bind_named tsx_s2 hi \g
\close_stmt tsx_s1
\bind_named tsx_s1 1 \g
\bind_named tsx_s2 again \g
\close_stmt tsx_s2
