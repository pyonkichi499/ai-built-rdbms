# PostgreSQL の src/test/isolation/specs/nowait-3.spec（REL_17_STABLE、commit d843a801b0f3）。
# Portions Copyright (c) 1996-2025, PostgreSQL Global Development Group
# Portions Copyright (c) 1994, The Regents of the University of California
# PostgreSQL License: https://www.postgresql.org/about/licence/

# Test NOWAIT with tuple locks.

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
step s1a	{ SELECT * FROM foo FOR UPDATE; }
step s1b	{ COMMIT; }

session s2
setup		{ BEGIN; }
step s2a	{ SELECT * FROM foo FOR UPDATE; }
step s2b	{ COMMIT; }

session s3
setup		{ BEGIN; }
step s3a	{ SELECT * FROM foo FOR UPDATE NOWAIT; }
step s3b	{ COMMIT; }

# s3 skips to second record due to tuple lock held by s2
permutation s1a s2a s3a s1b s2b s3b
