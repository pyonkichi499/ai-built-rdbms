create table compat_f1 (a int, b text);
\! printf '1\tx\n2\ty\n' > /tmp/compat_copy_in.txt
\copy compat_f1 from '/tmp/compat_copy_in.txt'
select * from compat_f1 order by a;
\copy compat_f1 (b, a) from '/tmp/compat_copy_in.txt'
select count(*) from compat_f1;
\! rm -f /tmp/compat_copy_in.txt
drop table compat_f1;
