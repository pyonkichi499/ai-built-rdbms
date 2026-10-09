# PostgreSQL の src/test/isolation/specs/deadlock-soft.spec（REL_17_STABLE、commit d843a801b0f3）。
# Portions Copyright (c) 1996-2025, PostgreSQL Global Development Group
# Portions Copyright (c) 1994, The Regents of the University of California
# PostgreSQL License: https://www.postgresql.org/about/licence/

# Four-process deadlock with two hard edges and two soft edges.
# d2 waits for e1 (soft edge), e1 waits for d1 (hard edge),
# d1 waits for e2 (soft edge), e2 waits for d2 (hard edge).
# The deadlock detector resolves the deadlock by reversing the d1-e2 edge,
# unblocking d1.

setup
{
  CREATE TABLE a1 ();
  CREATE TABLE a2 ();
}

teardown
{
  DROP TABLE a1, a2;
}

session d1
setup		{ BEGIN; SET deadlock_timeout = '10s'; }
step d1a1	{ LOCK TABLE a1 IN ACCESS SHARE MODE; }
step d1a2	{ LOCK TABLE a2 IN ACCESS SHARE MODE; }
step d1c	{ COMMIT; }

session d2
setup		{ BEGIN; SET deadlock_timeout = '10ms'; }
step d2a2	{ LOCK TABLE a2 IN ACCESS SHARE MODE; }
step d2a1	{ LOCK TABLE a1 IN ACCESS SHARE MODE; }
step d2c	{ COMMIT; }

session e1
setup		{ BEGIN; SET deadlock_timeout = '10s'; }
step e1l	{ LOCK TABLE a1 IN ACCESS EXCLUSIVE MODE; }
step e1c	{ COMMIT; }

session e2
setup		{ BEGIN; SET deadlock_timeout = '10s'; }
step e2l	{ LOCK TABLE a2 IN ACCESS EXCLUSIVE MODE; }
step e2c	{ COMMIT; }

permutation d1a1 d2a2 e1l e2l d1a2 d2a1 d1c e1c d2c e2c
