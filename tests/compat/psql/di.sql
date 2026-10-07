create table compat_i1 (id int primary key, a int, b text, c int unique);
create index compat_i1_a on compat_i1 (a);
create index compat_i1_bc on compat_i1 (b, c desc);
create unique index compat_i1_ab on compat_i1 (a, b);
create table compat_i2 (x int);
create index compat_i2_x on compat_i2 (x);
\di
\di compat_i1*
\di *_pkey
\di compat_i2_x
drop table compat_i1, compat_i2;
\di compat_*
