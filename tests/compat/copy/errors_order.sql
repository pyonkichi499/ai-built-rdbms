create table compat_eo1 (a int, b text);
copy compat_eo1 (a, b) from stdin with (null 'NULL');
40,NULL
\.
copy compat_eo1 from stdin;
x
\.
copy compat_eo1 from stdin;
x	y	z
\.
create table compat_eo2 (a int primary key, b text);
copy compat_eo2 from stdin;
1	x
1	dup
\.
create table compat_eo3 (a int not null, b text);
copy compat_eo3 from stdin;
\N	x
\.
select count(*) from compat_eo1;
select count(*) from compat_eo2;
drop table compat_eo1, compat_eo2, compat_eo3;
