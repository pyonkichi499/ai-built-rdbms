create sequence compat_sq1;
create sequence compat_sq2 increment by 5 start 100;
create table compat_ser (id serial primary key, big bigserial, small smallserial, v int);
\ds
\ds compat_sq*
\ds compat_ser*
drop table compat_ser;
drop sequence compat_sq1, compat_sq2;
\ds compat_*
