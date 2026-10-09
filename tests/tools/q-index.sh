#!/usr/bin/env bash
# 設計書の確認事項 ID と 99-questions.md の通し番号の表を突き合わせる。
# 設計: spec/design/m5/10-tests-plan.md §7.1（q-index.sh）、§10.2。
#
#   tests/tools/q-index.sh [--check] [--list] [--design-dir DIR] [--self-test]
#
#   --check           突き合わせて、食い違いがあれば失敗する（既定）
#   --list            章ごとの確認事項の件数と通し番号の範囲を表示する
#   --design-dir DIR  設計書のディレクトリ（既定: spec/design/m5）
#   --self-test       tests/tools/lint-fixtures/q-* の fixture で、正しいものは成功し、
#                     壊したものは失敗することを確かめる
#
# 章（00〜10）の定義の書式（どれか）:
#   | M5-<略号>-Q<n> | ...        表の行
#   - **M5-<略号>-Q<n> ...        箇条書き
#   ### M5-<略号>-Q<n>: ...       見出し
# 00 章は契約レベルの `| M5-Q<n> | ...`（n ≦ 99-questions.md の 00-Q の件数）を 00-Q<n> として扱う。
# 99-questions.md の表は `| M5-Q<通し> | <ID> | ...`（ID は <略号>-Q<n>、00-Q<n>、R-<n> など）。
#
# 検査:
#   Q1 通し番号 M5-Q1..N が 99 で一意かつ連続
#   Q2 章の定義 ⊆ 99 の表（欠けている ID を報告）
#   Q3 99 の表（章の ID）⊆ 章の定義（表にあるが章にない ID を報告）
#   Q4 章内の番号が 1..最大 で連続
#   Q5 99 の章見出し `（M5-<略号>-Q1〜Q<k>。通し Q<a>〜Q<b>）` が実際の件数・範囲と一致
#   Q6 設計書のどこかで参照されている M5-Q<n> が 1..N、M5-<略号>-Q<n> が定義済み
#
# 終了コード: 0 = 一致、1 = 食い違いあり、2 = 使い方の誤り
set -uo pipefail
export LC_ALL=C.UTF-8

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESIGN="$HERE/../../spec/design/m5"
MODE=check
SELF_TEST=0

while [ $# -gt 0 ]; do
    case "$1" in
        --check) MODE=check; shift ;;
        --list) MODE=list; shift ;;
        --design-dir) DESIGN="$2"; shift 2 ;;
        --self-test) SELF_TEST=1; shift ;;
        -h|--help) sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "不明な引数: $1" >&2; exit 2 ;;
    esac
done

if [ "$SELF_TEST" -eq 1 ]; then
    n_ok=0
    n_ng=0
    for fx in "$HERE"/lint-fixtures/q-*/; do
        fx="${fx%/}"
        name="$(basename "$fx")"
        [ -f "$fx/EXPECT" ] || continue
        expect="$(head -n1 "$fx/EXPECT")"
        out="$("$0" --design-dir "$fx" 2>&1)"
        rc=$?
        case "$expect" in
            pass)
                if [ "$rc" -eq 0 ]; then echo "ok   $name (成功)"; n_ok=$((n_ok + 1))
                else echo "NG   $name: 成功するはずが rc=$rc"; echo "$out" | sed 's/^/       /'; n_ng=$((n_ng + 1)); fi ;;
            fail\ *)
                want="${expect#fail }"
                if [ "$rc" -eq 1 ] && echo "$out" | grep -q "^FAIL $want "; then
                    echo "ok   $name (期待どおり $want で失敗)"; n_ok=$((n_ok + 1))
                else echo "NG   $name: $want で失敗するはずが rc=$rc"; echo "$out" | sed 's/^/       /'; n_ng=$((n_ng + 1)); fi ;;
            *) echo "NG   $name: EXPECT の書式が不正: $expect"; n_ng=$((n_ng + 1)) ;;
        esac
    done
    echo "self-test: ok $n_ok / NG $n_ng"
    [ "$n_ng" -eq 0 ] && [ "$n_ok" -gt 0 ]
    exit $?
fi

Q99="$DESIGN/99-questions.md"
if [ ! -f "$Q99" ]; then
    echo "FAIL Q1 99-questions.md がない: $Q99"
    exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
NFAIL=0
fail() { echo "FAIL $1 $2"; NFAIL=$((NFAIL + 1)); }

# 99 の表: 通し番号<TAB>ID
grep -E '^\| M5-Q[0-9]+ \|' "$Q99" |
    awk -F'|' '{ a = $2; b = $3; gsub(/[ \t]/, "", a); gsub(/[ \t]/, "", b); sub(/^M5-Q/, "", a); print a "\t" b }' > "$TMP/t99.tsv"

# 章の定義: 章ファイル（00〜10）から <略号>-Q<n> を集める
: > "$TMP/defs.raw"
for f in "$DESIGN"/[0-1][0-9]-*.md; do
    [ -f "$f" ] || continue
    base="$(basename "$f")"
    grep -hoE '^(\| |- \*\*|#+ )(★)?M5-[A-Z]+-Q[0-9]+' "$f" | sed -E 's/^(\| |- \*\*|#+ )(★)?M5-//' >> "$TMP/defs.raw"
done
# 00 章（契約）: | M5-Q<n> | の行を 00-Q<n> とする
if [ -f "$DESIGN/00-contracts.md" ]; then
    grep -hoE '^\| M5-Q[0-9]+ ' "$DESIGN/00-contracts.md" | sed -E 's/^\| M5-Q([0-9]+) /00-Q\1/' >> "$TMP/defs.raw"
fi
sort -u "$TMP/defs.raw" > "$TMP/defs.txt"

# 99 の章の ID（<略号>-Q<n>。大文字の略号のみ。00 を含む）
awk -F'\t' '$2 ~ /^([A-Z]+|00)-Q[0-9]+$/ { print $2 }' "$TMP/t99.tsv" | sort -u > "$TMP/ids99.txt"

# Q1 通し番号
total="$(wc -l < "$TMP/t99.tsv")"
dups="$(cut -f1 "$TMP/t99.tsv" | sort -n | uniq -d | tr '\n' ' ')"
[ -z "$dups" ] || fail Q1 "通し番号が重複している: $dups"
prev=0
while IFS=$'\t' read -r n _; do
    if [ "$n" -ne $((prev + 1)) ]; then fail Q1 "通し番号が連続しない: M5-Q$prev の次が M5-Q$n"; fi
    prev="$n"
done < <(sort -n "$TMP/t99.tsv")

# Q2 / Q3
while IFS= read -r id; do
    grep -qxF "$id" "$TMP/ids99.txt" || fail Q2 "章に M5-$id の定義があるが 99-questions.md の表にない"
done < "$TMP/defs.txt"
while IFS= read -r id; do
    grep -qxF "$id" "$TMP/defs.txt" || fail Q3 "99-questions.md の表に $id があるが、章に M5-$id の定義がない"
done < "$TMP/ids99.txt"

# Q4 章内の連続
sed -E 's/-Q[0-9]+$//' "$TMP/defs.txt" | sort -u > "$TMP/abbrs.txt"
: > "$TMP/summary.txt"
while IFS= read -r ab; do
    nums="$(grep -E "^${ab}-Q[0-9]+$" "$TMP/defs.txt" | sed -E 's/.*-Q//' | sort -n)"
    cnt="$(echo "$nums" | wc -l)"
    max="$(echo "$nums" | tail -n1)"
    first="$(grep -E "^[0-9]+	${ab}-Q" "$TMP/t99.tsv" | cut -f1 | sort -n | head -n1)"
    last="$(grep -E "^[0-9]+	${ab}-Q" "$TMP/t99.tsv" | cut -f1 | sort -n | tail -n1)"
    echo "$ab	$cnt	$max	${first:-}	${last:-}" >> "$TMP/summary.txt"
    if [ "$cnt" -ne "$max" ]; then
        missing="$(seq 1 "$max" | grep -vxFf <(echo "$nums") | sed "s/^/Q/" | tr '\n' ' ')"
        fail Q4 "$ab の確認事項の番号が連続しない（欠け: $missing）"
    fi
    # Q5 99 の章の見出し
    if [ "$ab" != "00" ]; then
        head="$(grep -E "^### [0-9]+ [A-Z]+ （|^### [0-9]+ ${ab}（|^### [0-9]+ ${ab} " "$Q99" | grep -F "M5-${ab}-Q1" | head -n1)"
        if [ -z "$head" ]; then
            fail Q5 "99-questions.md に $ab の章見出し（M5-${ab}-Q1〜）がない"
        else
            hk="$(echo "$head" | sed -E 's/.*M5-[A-Z]+-Q1`?〜`?Q([0-9]+).*/\1/')"
            ha="$(echo "$head" | sed -E 's/.*通し Q([0-9]+)〜Q([0-9]+).*/\1/')"
            hb="$(echo "$head" | sed -E 's/.*通し Q([0-9]+)〜Q([0-9]+).*/\2/')"
            [ "$hk" = "$max" ] || fail Q5 "$ab の見出しは Q1〜Q$hk だが、章の定義は Q1〜Q$max"
            if [ -n "${first:-}" ] && { [ "$ha" != "$first" ] || [ "$hb" != "$last" ]; }; then
                fail Q5 "$ab の見出しの通し Q$ha〜Q$hb が表の Q$first〜Q$last と違う"
            fi
        fi
    fi
done < "$TMP/abbrs.txt"

# Q6 参照の妥当性
for f in "$DESIGN"/*.md; do
    [ -f "$f" ] || continue
    base="$(basename "$f")"
    grep -noE 'M5-Q[0-9]+' "$f" | while IFS=: read -r ln ref; do
        n="${ref#M5-Q}"
        if [ "$n" -lt 1 ] || [ "$n" -gt "$total" ]; then echo "FAIL Q6 $base:$ln: $ref は通し番号の範囲 1..$total の外"; fi
    done
    grep -noE 'M5-[A-Z]+-Q[0-9]+' "$f" | while IFS=: read -r ln ref; do
        id="${ref#M5-}"
        grep -qxF "$id" "$TMP/defs.txt" || echo "FAIL Q6 $base:$ln: $ref は章に定義がない"
    done
done > "$TMP/q6.txt"
if [ -s "$TMP/q6.txt" ]; then
    cat "$TMP/q6.txt"
    NFAIL=$((NFAIL + $(wc -l < "$TMP/q6.txt")))
fi

if [ "$MODE" = list ]; then
    printf '%-4s %5s %5s  %s\n' 章 件数 最大 通し番号
    while IFS=$'\t' read -r ab cnt max first last; do
        printf '%-4s %5s %5s  Q%s〜Q%s\n' "$ab" "$cnt" "$max" "${first:--}" "${last:--}"
    done < "$TMP/summary.txt"
fi

echo "q-index: FAIL $NFAIL（99 の通し番号 $total 件、章の定義 $(wc -l < "$TMP/defs.txt") 件）"
[ "$NFAIL" -eq 0 ]
