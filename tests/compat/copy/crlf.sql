create table compat_c2 (a int, b text);
\! printf '1\tcrlf\r\n2\tx\r\n' > /tmp/compat_crlf.txt
\copy compat_c2 from '/tmp/compat_crlf.txt'
select a, b, length(b) from compat_c2 order by a;
\! rm -f /tmp/compat_crlf.txt
copy compat_c2 from stdin;
3	before end
\.
select count(*) from compat_c2;
drop table compat_c2;
