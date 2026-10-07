create sequence compat_sq1;
create sequence compat_sq2 as integer increment by 5 minvalue 10 maxvalue 1000 start 100 cache 3 cycle;
create sequence compat_sq3 as bigint increment by -2 start -1 minvalue -1000 maxvalue -1;
create table compat_ser (id serial primary key, big bigserial, v int);
\d compat_sq1
\d compat_sq2
\d compat_sq3
\d compat_ser_id_seq
\d compat_ser_big_seq
drop table compat_ser;
drop sequence compat_sq1, compat_sq2, compat_sq3;
