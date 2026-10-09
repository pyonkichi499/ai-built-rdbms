# PostgreSQL の src/test/isolation/specs/nowait.spec（REL_17_STABLE、commit d843a801b0f3）。
# Portions Copyright (c) 1996-2025, PostgreSQL Global Development Group
# Portions Copyright (c) 1994, The Regents of the University of California
# PostgreSQL License: https://www.postgresql.org/about/licence/

# Test NOWAIT when regular row locks can't be acquired.

setup
{
  CREATE TABLE foo (
	id int PRIMARY KEY,
	data text NOT NULL
  );
  INSERT INTO foo VALUES (1, 'x');
}

teardown
{
  DROP TABLE foo;
}

session s1
setup		{ BEGIN; }
step s1a	{ SELECT * FROM foo FOR UPDATE NOWAIT; }
step s1b	{ COMMIT; }

session s2
setup		{ BEGIN; }
step s2a	{ SELECT * FROM foo FOR UPDATE NOWAIT; }
step s2b	{ COMMIT; }
