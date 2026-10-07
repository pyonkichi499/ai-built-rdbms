create table compat_m1 (a int, b text);
copy compat_m1 from stdin;
1	ok
22	x	5\.
\.
select * from compat_m1 order by a;
copy compat_m1 from stdin;
3	line
4	in\.valid
\.
select count(*) from compat_m1;
copy compat_m1 from stdin;
5	bad	
\.
select count(*) from compat_m1;
drop table compat_m1;
