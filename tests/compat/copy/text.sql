create table compat_c1 (a int, b text, c int);
copy compat_c1 from stdin;
1	foo	10
2	a\tb	20
3	\N	30
4	back\\slash	40
5	\x41\101	50
6	nl\nx	60
\.
select * from compat_c1 order by a;
select a, length(b), b is null from compat_c1 order by a;
drop table compat_c1;
