# nextval は他のトランザクションを待たず、ROLLBACK しても値は戻らない（CACHE 5 は 5 個ずつ払い出す）。

setup
{
  CREATE SEQUENCE iso_sq;
  CREATE SEQUENCE iso_sq5 CACHE 5;
}

teardown
{
  DROP SEQUENCE iso_sq;
  DROP SEQUENCE iso_sq5;
}

session a
step a_begin { BEGIN; }
step a_next { SELECT nextval('iso_sq') AS s, nextval('iso_sq5') AS s5; }
step a_roll { ROLLBACK; }

session b
step b_next { SELECT nextval('iso_sq') AS s, nextval('iso_sq5') AS s5; }
step b_last { SELECT lastval() AS last; }

permutation a_begin a_next b_next a_roll b_next b_last
