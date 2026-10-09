\set VERBOSITY terse
drop table if exists tsx_child2;
drop table if exists tsx_child;
drop table if exists tsx_parent;
create table tsx_parent(id int primary key, k int not null unique, name text);
create table tsx_child(
  id int primary key,
  pid int not null references tsx_parent(id) on delete cascade on update restrict,
  kk int,
  constraint tsx_child_kk_fkey foreign key (kk) references tsx_parent(k) on delete set null,
  unique (id, pid)
);
create table tsx_child2(a int, b int, foreign key (a, b) references tsx_child(id, pid) match full, c int references tsx_parent);
\d tsx_child
\d tsx_child2
\d tsx_parent
select conname, contype, confupdtype, confdeltype, confmatchtype, pg_get_constraintdef(oid) as def
  from pg_constraint where conrelid in ('tsx_child'::regclass, 'tsx_child2'::regclass) and contype = 'f' order by conname;
select conname, contype, pg_get_constraintdef(oid) as def
  from pg_constraint where conrelid = 'tsx_parent'::regclass order by conname;
drop table tsx_child2;
drop table tsx_child;
drop table tsx_parent;
