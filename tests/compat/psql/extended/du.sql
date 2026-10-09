\set VERBOSITY terse
drop role if exists tsx_plain;
drop role if exists tsx_admin;
drop role if exists tsx_nologin;
create role tsx_plain login;
create role tsx_admin login superuser createdb createrole connection limit 5;
create role tsx_nologin nologin password 'x' valid until '2099-01-01 00:00:00+00';
\du tsx_*
\du+ tsx_plain
select rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolreplication, rolconnlimit, rolbypassrls
  from pg_roles where rolname like 'tsx\_%' order by rolname;
alter role tsx_plain nologin createdb;
\du tsx_plain
drop role tsx_plain;
drop role tsx_admin;
drop role tsx_nologin;
\du tsx_*
