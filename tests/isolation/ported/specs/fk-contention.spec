# PostgreSQL の src/test/isolation/specs/fk-contention.spec（REL_17_STABLE、commit d843a801b0f3）。
# Portions Copyright (c) 1996-2025, PostgreSQL Global Development Group
# Portions Copyright (c) 1994, The Regents of the University of California
# PostgreSQL License: https://www.postgresql.org/about/licence/

setup
{
  CREATE TABLE foo (a int PRIMARY KEY, b text);
  CREATE TABLE bar (a int NOT NULL REFERENCES foo);
  INSERT INTO foo VALUES (42);
}

teardown
{
  DROP TABLE foo, bar;
}

session s1
setup		{ BEGIN; }
step ins	{ INSERT INTO bar VALUES (42); }
step com	{ COMMIT; }

session s2
step upd	{ UPDATE foo SET b = 'Hello World'; }
