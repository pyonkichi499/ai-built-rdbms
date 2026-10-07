create table compat_o1 (a int, b text, c text);
copy compat_o1 from stdin with (delimiter '|', null 'NA');
1|x|NA
2|NA|y
\.
select a, b, c, b is null as bn, c is null as cn from compat_o1 order by a;
truncate compat_o1;
copy compat_o1 (c, a) from stdin;
cc	7
dd	8
\.
select * from compat_o1 order by a;
truncate compat_o1;
copy compat_o1 from stdin with delimiter ',' null as 'Z';
1,two,Z
\.
select * from compat_o1;
truncate compat_o1;
copy compat_o1 from stdin with (header true);
a	b	c
1	x	y
\.
select * from compat_o1;
truncate compat_o1;
copy compat_o1 from stdin with (format text, encoding 'UTF8');
9	é	日本
\.
select * from compat_o1;
drop table compat_o1;
