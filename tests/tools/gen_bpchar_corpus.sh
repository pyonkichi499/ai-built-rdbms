#!/usr/bin/env bash
# bpchar の差分テスト用コーパス（入力 x typmod x 代入 / 明示 / 比較 / length / octet_length / LIKE / ~ / ||）。
#
#   tests/tools/gen_bpchar_corpus.sh                      SQL を標準出力へ出す
#   tests/tools/gen_bpchar_corpus.sh --diff PG_PORT Y_PORT  同じ SQL を PostgreSQL と yuzhu に流して出力を比べる（差分があれば 1）
#
# 09-types-functions.md §12.1 の gen_bpchar_corpus.py の代わり（sandbox に python3 がないので psql + シェルで書く）。
# エラーは SQLSTATE だけ比べる（\set VERBOSITY sqlstate）。
set -euo pipefail

inputs=("''" "'a'" "'a '" "'a  '" "' a'" "'abc'" "'abc  '" "'あ'" "'あい '" "'éa'" "'ab cd'" "'abcdefgh'" "E'a\\tb'" "'a%'" "'日本語テキスト  '")
typmods=("" "(1)" "(3)" "(5)" "(10)")

gen() {
    echo "\\set VERBOSITY sqlstate"
    echo "\\pset null '<null>'"
    echo "select 'null input', null::char(3) is null, length(null::char(3)) is null;"
    for i in "${inputs[@]}"; do
        for t in "${typmods[@]}"; do
            ty="char$t"
            [ -z "$t" ] && ty="bpchar"
            echo "select ${i}::${ty}, length(${i}::${ty}), octet_length(${i}::${ty}), char_length(${i}::${ty}), character_length(${i}::${ty}), (${i}::${ty})::text, '['||(${i}::${ty})::text||']', (${i}::${ty})::varchar, octet_length((${i}::${ty})::text);"
            echo "select ${i}::${ty} || 'x', ${i}::${ty} like 'a%', ${i}::${ty} ~ 'a', ${i}::${ty} ~ ' \$', ${i}::${ty} = 'a', ${i}::${ty} = 'a '::bpchar, ${i}::${ty} < 'b', ${i}::${ty} > 'abc';"
            echo "select cast(${i} as ${ty}), cast(cast(${i} as text) as ${ty}), cast(${i}::varchar as ${ty});"
            echo "create table bp_corpus_t (c ${ty}); insert into bp_corpus_t values (${i}); select c, length(c), octet_length(c), c = ${i}, c = ${i}::text, c::text = ${i} from bp_corpus_t; drop table bp_corpus_t;"
        done
    done
    for a in "${inputs[@]}"; do
        for b in "${inputs[@]}"; do
            echo "select ${a}::char(5) = ${b}::char(8), ${a}::bpchar < ${b}::bpchar, ${a}::char(3) || ${b}::char(3), ${a}::char(4) = ${b};"
        done
    done
    echo "select 'ab'::char(5) in ('ab', 'cd'), 'ab'::char(5) between 'aa' and 'ac', 'x'::char(2) is distinct from 'x ';"
    echo "select max(c), min(c), count(distinct c) from (values ('a'::char(3)), ('a '::char(3)), ('b'::char(3))) v(c);"
    echo "select c, count(*) from (values ('a'::char(3)), ('a '::char(2)), ('b'::char(3))) v(c) group by c order by c;"
    echo "select pg_typeof('a'::char(3)), pg_typeof('a'::char(3) || 'b'), pg_typeof(length('a'::char(3)));"
    echo "select 'a'::char(0);"
    echo "select 'a'::char(-1);"
    echo "select 'abc'::char(2);"
    echo "select 'abc'::char(2) = 'ab';"
    echo "select cast('abc' as char(2));"
    echo "select 1::char(3), 1.5::char(5), true::char(5), now()::char(4) is not null;"
}

if [ "${1:-}" = "--diff" ]; then
    pgp="${2:?PG port}"; yzp="${3:?yuzhu port}"
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
    gen > "$tmp/in.sql"
    for pair in "pg:$pgp" "yuzhu:$yzp"; do
        name="${pair%%:*}"; port="${pair##*:}"
        psql -h 127.0.0.1 -p "$port" -U postgres -d postgres -X -At -f "$tmp/in.sql" 2>&1 \
            | sed -E 's/^psql:[^ ]*:[0-9]+: //' > "$tmp/$name.out" || true
    done
    if diff -u "$tmp/pg.out" "$tmp/yuzhu.out" > "$tmp/diff.txt"; then
        echo "bpchar corpus: identical ($(wc -l < "$tmp/in.sql") statements)"
    else
        head -80 "$tmp/diff.txt"; exit 1
    fi
else
    gen
fi
