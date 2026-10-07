#!/usr/bin/env bash
# regex_cases.in（pattern<TAB>flags<TAB>subject。COPY のテキスト形式）に PostgreSQL 17 の結果を付けて
# regex_psql.tsv（pattern, flags, subject, expected）を作る。expected は t / f / ERR:<SQLSTATE>:<メッセージ>。
# flags は空（~）か i（~*）。python は使わない（sandbox に無い）。
#
#   PGHOST=127.0.0.1 PGPORT=55432 PGUSER=postgres tests/data/gen_regex_corpus.sh
#
# regex_cases.in の手書き部分は regex_curated.raw（⏎ = 改行、バックスラッシュはそのまま）から
# awk で作った。後半の乱数部分は 09 §9.2 の構文だけから作ったもの（後方参照・先読みは含めない）。
set -euo pipefail
cd "$(dirname "$0")"
export PGHOST="${PGHOST:-127.0.0.1}" PGPORT="${PGPORT:-55432}" PGUSER="${PGUSER:-postgres}"
psql -X -q -v ON_ERROR_STOP=1 <<SQL > regex_psql.tsv
create temp table c(id bigserial, pattern text, flags text, subject text, expected text);
\\copy c(pattern, flags, subject) from '$PWD/regex_cases.in'
create function pg_temp.try_re(p text, f text, s text) returns text language plpgsql as \$f\$
begin
  begin
    if f = 'i' then return (s ~* p)::text; else return (s ~ p)::text; end if;
  exception when others then return 'ERR:' || sqlstate || ':' || sqlerrm;
  end;
end \$f\$;
update c set expected = case pg_temp.try_re(pattern, flags, subject)
  when 'true' then 't' when 'false' then 'f' else pg_temp.try_re(pattern, flags, subject) end;
\\copy (select pattern, flags, subject, expected from c order by id) to '/dev/stdout'
SQL
