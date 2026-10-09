-- psql 17 の \bind（名前なし文の Extended Query: Parse / Bind / Describe / Execute / Sync）
\set VERBOSITY terse
select $1::int + $2::int as s \bind 3 4 \g
select $1::text as t, $2::text is null as n \bind 'a b' null \g
select $1::numeric(10,2) as n, $2::date as d, $3::bool as b \bind 12.345 2024-02-29 true \g
select $1::text[] as a \bind '{1,NULL,"x y"}' \g
select $1::int as x \bind 7 \g
\bind 8 \g
select 1 as one \bind \g
select $1::int \bind abc \g
select $1::int \bind \g
select $1::int, $2::int \bind 1 \g
select 1; select 2 \bind \g
select $1 \bind 1 \g
drop table if exists tsx_bind;
create table tsx_bind(id int primary key, v text);
insert into tsx_bind values ($1, $2) returning id, v \bind 1 one \g
insert into tsx_bind values ($1, $2) \bind 1 dup \g
insert into tsx_bind values ($1, $2) \bind 2 two \g
select id, v from tsx_bind where id = $1 \bind 2 \g
update tsx_bind set v = $2 where id = $1 returning v \bind 1 uno \g
delete from tsx_bind where id = $1 \bind 2 \g
select id, v from tsx_bind order by id;
select $1::int from tsx_bind where id = $2 \bind 5 99 \g
select 'after error still usable' as msg;
drop table tsx_bind;
