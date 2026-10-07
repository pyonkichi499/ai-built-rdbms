create table compat_ty (i int, bi bigint, n numeric(8,2), f float8, d date, t text, v varchar(5), c char(4), b bool);
copy compat_ty from stdin;
1	9000000000	123.45	1.5	2024-02-29	txt	abc	ab	t
\N	\N	\N	\N	\N	\N	\N	\N	\N
\.
select * from compat_ty order by i;
copy compat_ty (v) from stdin;
toolongvalue
\.
copy compat_ty (b) from stdin;
maybe
\.
copy compat_ty (d) from stdin;
2024-13-01
\.
copy compat_ty (n) from stdin;
123456789
\.
select count(*) from compat_ty;
drop table compat_ty;
