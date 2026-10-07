create table compat_k1 (a int);
copy compat_k1 from stdin; select count(*) from compat_k1;
1
2
3
\.
select * from compat_k1 order by a;
begin;
copy compat_k1 from stdin;
4
\.
rollback;
select count(*) from compat_k1;
begin;
truncate compat_k1;
copy compat_k1 from stdin with (freeze on);
7
\.
commit;
select * from compat_k1;
begin;
copy compat_k1 from stdin with (freeze on);
8
\.
rollback;
drop table compat_k1;
