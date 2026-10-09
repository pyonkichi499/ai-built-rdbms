# PostgreSQL の src/test/isolation/specs/fk-deadlock2.spec（REL_17_STABLE、commit d843a801b0f3）。
# Portions Copyright (c) 1996-2025, PostgreSQL Global Development Group
# Portions Copyright (c) 1994, The Regents of the University of California
# PostgreSQL License: https://www.postgresql.org/about/licence/

setup
{
  CREATE TABLE A (
	AID integer not null,
	Col1 integer,
	PRIMARY KEY (AID)
  );

  CREATE TABLE B (
	BID integer not null,
	AID integer not null,
	Col2 integer,
	PRIMARY KEY (BID),
	FOREIGN KEY (AID) REFERENCES A(AID)
  );

  INSERT INTO A (AID) VALUES (1);
  INSERT INTO B (BID,AID) VALUES (2,1);
}

teardown
{
  DROP TABLE a, b;
}

session s1
setup		{ BEGIN; SET deadlock_timeout = '100ms'; }
step s1u1	{ UPDATE A SET Col1 = 1 WHERE AID = 1; }
step s1u2	{ UPDATE B SET Col2 = 1 WHERE BID = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN; SET deadlock_timeout = '10s'; }
step s2u1	{ UPDATE B SET Col2 = 1 WHERE BID = 2; }
step s2u2	{ UPDATE B SET Col2 = 1 WHERE BID = 2; }
step s2c	{ COMMIT; }

permutation s1u1 s1u2 s1c s2u1 s2u2 s2c
permutation s1u1 s1u2 s2u1 s1c s2u2 s2c
permutation s1u1 s2u1 s1u2 s2u2 s2c s1c
permutation s1u1 s2u1 s2u2 s1u2 s2c s1c
permutation s1u1 s2u1 s2u2 s2c s1u2 s1c
permutation s2u1 s1u1 s1u2 s2u2 s2c s1c
permutation s2u1 s1u1 s2u2 s1u2 s2c s1c
permutation s2u1 s1u1 s2u2 s2c s1u2 s1c
permutation s2u1 s2u2 s1u1 s1u2 s2c s1c
permutation s2u1 s2u2 s1u1 s2c s1u2 s1c
permutation s2u1 s2u2 s2c s1u1 s1u2 s1c
