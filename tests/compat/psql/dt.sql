create table compat_t1 (id int primary key, name text not null, price numeric(10,2) default 0);
create table compat_t2 (k int unique, v varchar(20), ts timestamp);
create table compat_t3 (a int, b int);
\dt
\dt compat_t1
\dt compat_t*
\dt compat_nothing*
\dt public.*
drop table compat_t1, compat_t2, compat_t3;
\dt
