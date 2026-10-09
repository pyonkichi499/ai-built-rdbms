\set VERBOSITY terse
drop database if exists tsx_db1;
create database tsx_db1;
\l tsx_*
select datname, pg_get_userbyid(datdba) as owner, datistemplate, datallowconn, datconnlimit from pg_database where datname like 'tsx\_%' order by 1;
drop database tsx_db1;
\l tsx_*
