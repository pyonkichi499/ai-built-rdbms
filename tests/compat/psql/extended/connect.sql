\set VERBOSITY terse
drop database if exists tsx_db2;
create database tsx_db2;
\c tsx_db2
select current_database() as db, current_user as usr;
create table tsx_in_db2(id int);
insert into tsx_in_db2 values (1), (2);
select count(*) from tsx_in_db2;
\c postgres
select current_database() as db;
select count(*) from pg_class where relname = 'tsx_in_db2';
\c tsx_db2
select count(*) from pg_class where relname = 'tsx_in_db2';
\c postgres
drop database tsx_db2;
\c tsx_db2
select current_database() as db;
