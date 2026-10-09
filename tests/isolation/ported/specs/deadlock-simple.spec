# PostgreSQL の src/test/isolation/specs/deadlock-simple.spec（REL_17_STABLE、commit d843a801b0f3）。
# Portions Copyright (c) 1996-2025, PostgreSQL Global Development Group
# Portions Copyright (c) 1994, The Regents of the University of California
# PostgreSQL License: https://www.postgresql.org/about/licence/

# The deadlock detector has a special case for "simple" deadlocks.  A simple
# deadlock occurs when we attempt a lock upgrade while another process waits
# for a lock upgrade on the same object; and the sought locks conflict with
# those already held, so that neither process can complete its upgrade until
# the other releases locks.  Test this scenario.

setup
{
  CREATE TABLE a1 ();
}

teardown
{
  DROP TABLE a1;
}

session s1
setup		{ BEGIN; }
step s1as	{ LOCK TABLE a1 IN ACCESS SHARE MODE; }
step s1ae	{ LOCK TABLE a1 IN ACCESS EXCLUSIVE MODE; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN; }
step s2as	{ LOCK TABLE a1 IN ACCESS SHARE MODE; }
step s2ae	{ LOCK TABLE a1 IN ACCESS EXCLUSIVE MODE; }
step s2c	{ COMMIT; }

permutation s1as s2as s1ae s2ae s1c s2c
