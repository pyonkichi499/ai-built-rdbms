create table compat_e1 (a int not null, b text, c int check (c > 0));
copy compat_e1 from stdin;
1	x
2	y	0
\.
select count(*) from compat_e1;
copy compat_e1 from stdin;
1	x	5	extra
\.
copy compat_e1 from stdin;
abc	x	5
\.
copy compat_e1 from stdin;
\N	x	5
\.
copy compat_e1 from stdin;
5	x	-1
\.
copy compat_e1 from stdin;
6	x	99999999999
\.
select count(*) from compat_e1;
copy compat_e1 (a, nosuch) from stdin;
\.
copy compat_e1 (a, a) from stdin;
\.
copy nosuch_table from stdin;
\.
copy compat_e1 from stdin with (format foo);
\.
copy compat_e1 from stdin with (delimiter 'ab');
\.
copy compat_e1 from stdin with (null_x 'a');
\.
drop table compat_e1;
