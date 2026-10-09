#!/usr/bin/env bash
# 共有テストと既知の差の台帳（tests/known-diffs.toml）の突き合わせ。
# 設計: spec/design/m5/10-tests-plan.md §6.6（L1〜L8）、§7.1、§7.3 の 10。
#
#   tests/tools/lint-tests.sh [--release] [--include-legacy] [--root DIR] [--design-dir DIR]
#                             [--only L1,L2,...] [--self-test]
#
#   --release         M5 の完了判定。`skipif yuzhu` を 0 件にし、台帳の affects の実在を必須にする（L2・L6・L10）
#   --include-legacy  L1・L2 を M1〜M4 のテストにも適用する（既定は M5 のテストだけ。M1〜M4 は
#                     台帳より前に書かれ、`# KNOWN-DIFF:` などの旧い書式と旧い KD 番号を使っている）
#   --root DIR        tests/ に相当するディレクトリ（既定: このスクリプトの 2 つ上）。fixture 用
#   --design-dir DIR  99-questions.md のあるディレクトリ（既定: <root>/../spec/design/m5。相対パスは root から）
#   --only LIST       指定した検査だけ実行する（カンマ区切り。L9・L10 を含む）
#   --self-test       tests/tools/lint-fixtures/ の fixture で、正しいものは成功し、壊したものは
#                     期待した検査で失敗することを確かめる
#
# 検査:
#   L1  onlyif yuzhu の直前のコメントに `# KD-<n>` があり、その KD が台帳にある
#   L2  skipif yuzhu の直前のコメントに `# 未実装: <章>-<WP>` がある（--release では 0 件）
#   L3  isolation/ported/expected/*.yuzhu-m5.out が台帳の affects に載っている
#   L4  isolation/ported/skip.txt の各行の KD が台帳にある
#   L5  slt/m5 の CREATE 対象名が接頭辞 `<dir>_<stem>_` に従い、先に同名の DROP ... IF EXISTS がある
#   L6  台帳の affects が実在する（glob は 1 件以上。実装中は警告、--release で失敗）
#   L7  `statement error` に `db error:` の接頭辞が残っていない
#   L8  kind が unsupported / both の KD の pattern が、onlyif yuzhu なしの共有テストに現れない
#   L9  台帳の書式（必須項目・ID の一意・kind・questions の実在）
#   L10 （--release のみ）台帳の KD がテストか確認事項に紐づく
#
# 終了コード: 0 = 成功、1 = 失敗あり、2 = 使い方の誤り
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
DESIGN=""
RELEASE=0
LEGACY=0
ONLY=""
SELF_TEST=0

usage() { sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        --release) RELEASE=1; shift ;;
        --include-legacy) LEGACY=1; shift ;;
        --root) ROOT="$(cd "$2" && pwd)" || exit 2; shift 2 ;;
        --design-dir) DESIGN="$2"; shift 2 ;;
        --only) ONLY="$2"; shift 2 ;;
        --self-test) SELF_TEST=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "不明な引数: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [ "$SELF_TEST" -eq 1 ]; then
    FIXTURES="$HERE/lint-fixtures"
    n_ok=0
    n_ng=0
    for fx in "$FIXTURES"/*/; do
        fx="${fx%/}"
        name="$(basename "$fx")"
        case "$name" in q-*) continue ;; esac
        [ -f "$fx/EXPECT" ] || continue
        expect="$(head -n1 "$fx/EXPECT")"
        args=()
        if [ -f "$fx/ARGS" ]; then read -r -a args < "$fx/ARGS"; fi
        out="$("$0" --root "$fx" "${args[@]}" 2>&1)"
        rc=$?
        case "$expect" in
            pass)
                if [ "$rc" -eq 0 ]; then
                    echo "ok   $name (成功)"; n_ok=$((n_ok + 1))
                else
                    echo "NG   $name: 成功するはずが rc=$rc"; echo "$out" | sed 's/^/       /'; n_ng=$((n_ng + 1))
                fi ;;
            fail\ *)
                want="${expect#fail }"
                if [ "$rc" -eq 1 ] && echo "$out" | grep -q "^FAIL $want "; then
                    echo "ok   $name (期待どおり $want で失敗)"; n_ok=$((n_ok + 1))
                else
                    echo "NG   $name: $want で失敗するはずが rc=$rc"; echo "$out" | sed 's/^/       /'; n_ng=$((n_ng + 1))
                fi ;;
            *) echo "NG   $name: EXPECT の書式が不正: $expect"; n_ng=$((n_ng + 1)) ;;
        esac
    done
    echo "self-test: ok $n_ok / NG $n_ng"
    [ "$n_ng" -eq 0 ] && [ "$n_ok" -gt 0 ]
    exit $?
fi

LEDGER="$ROOT/known-diffs.toml"
if [ -z "$DESIGN" ]; then DESIGN="$ROOT/../spec/design/m5"; fi
case "$DESIGN" in /*) ;; *) DESIGN="$ROOT/$DESIGN" ;; esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
NFAIL=0
NWARN=0
US=$'\x1f'

want() { # 検査 ID が実行対象か
    [ -z "$ONLY" ] && return 0
    case ",$ONLY," in *",$1,"*) return 0 ;; esac
    return 1
}
fail() { echo "FAIL $1 $2"; NFAIL=$((NFAIL + 1)); }
warn() { echo "WARN $1 $2"; NWARN=$((NWARN + 1)); }
rel() { echo "${1#"$ROOT"/}"; }

# ---------------------------------------------------------------- 台帳の読み込み
# 台帳の簡易パーサ。書式: [[kd]] で始まるテーブル、`key = "文字列"` / `key = '文字列'` /
# `key = [ "a", 'b' ]`（複数行可）。それ以外（数値・入れ子の表など）は使わない。
if [ ! -f "$LEDGER" ]; then
    fail L9 "台帳がない: $(rel "$LEDGER")"
    echo "lint-tests: FAIL $NFAIL / WARN $NWARN"
    exit 1
fi

awk '
function scan(txt,   i, n, c, s) {
    # txt の中の文字列リテラルを OUT[1..cnt] に集める。閉じ括弧の位置を CLOSED に立てる
    n = length(txt); i = 1; cnt = 0; CLOSED = 0
    while (i <= n) {
        c = substr(txt, i, 1)
        if (c == "\"") {
            s = ""; i++
            while (i <= n) {
                c = substr(txt, i, 1)
                if (c == "\\") { i++; c = substr(txt, i, 1); if (c == "n") c = " "; s = s c }
                else if (c == "\"") break
                else s = s c
                i++
            }
            cnt++; OUT[cnt] = s; i++
        } else if (c == "\047") {
            s = ""; i++
            while (i <= n && substr(txt, i, 1) != "\047") { s = s substr(txt, i, 1); i++ }
            cnt++; OUT[cnt] = s; i++
        } else if (c == "#") { break }
        else if (c == "]") { CLOSED = 1; break }
        else i++
    }
    return cnt
}
function emit(key, isarr,   j, k) {
    k = scan(buf)
    if (!isarr && k != 1) { printf "E\x1f%d\x1f%s\x1f値が文字列 1 つではない\n", NR, key; return }
    if (isarr) { printf "A\x1f%d\x1f%s\x1f\n", n, key }
    for (j = 1; j <= k; j++) printf "K\x1f%d\x1f%s\x1f%s\n", n, key, OUT[j]
}
BEGIN { n = 0; inarr = 0 }
{
    line = $0
    if (inarr) {
        buf = buf " " line
        scan(line)
        if (CLOSED) { emit(key, 1); inarr = 0 }
        next
    }
    if (line ~ /^[ \t]*(#|$)/) next
    if (line ~ /^\[\[kd\]\]/) { n++; next }
    if (line ~ /^\[/) { printf "E\x1f%d\x1f\x1f未対応のテーブル\n", NR; next }
    if (line ~ /^[A-Za-z_][A-Za-z_0-9]*[ \t]*=/) {
        if (n == 0) { printf "E\x1f%d\x1f\x1f[[kd]] の前にキーがある\n", NR; next }
        key = line; sub(/[ \t]*=.*/, "", key)
        val = line; sub(/^[^=]*=[ \t]*/, "", val)
        if (val ~ /^\[/) {
            buf = substr(val, 2)
            scan(buf)
            if (CLOSED) emit(key, 1); else inarr = 1
        } else {
            buf = val
            emit(key, 0)
        }
        next
    }
    printf "E\x1f%d\x1f\x1f解釈できない行\n", NR
}
END { if (inarr) printf "E\x1f%d\x1f%s\x1f配列が閉じていない\n", NR, key }
' "$LEDGER" > "$TMP/ledger.raw"

declare -A IDX_ID=() KD_KIND=() KD_PATTERN=() KD_AFFECTS=() KD_QUESTIONS=() KD_HAS=()
declare -a KD_ORDER=()
declare -A ID_SEEN=()
declare -A KD_FIELDS_OF=()
while IFS="$US" read -r tag idx key val; do
    case "$tag" in
        E) fail L9 "known-diffs.toml:$idx: $val ($key)" ;;
        A) KD_FIELDS_OF[$idx:$key]=1 ;;
        K)
            KD_FIELDS_OF[$idx:$key]=1
            case "$key" in
                id) IDX_ID[$idx]="$val" ;;
                kind) KD_KIND[$idx]="$val" ;;
                pattern) KD_PATTERN[$idx]="$val" ;;
                affects) KD_AFFECTS[$idx]+="$val"$'\n' ;;
                questions) KD_QUESTIONS[$idx]+="$val"$'\n' ;;
            esac ;;
    esac
done < "$TMP/ledger.raw"

for idx in $(printf '%s\n' "${!IDX_ID[@]}" | sort -n); do
    id="${IDX_ID[$idx]}"
    KD_ORDER+=("$idx")
    ID_SEEN[$id]="$idx"
done
declare -A LEDGER_IDS=()
for idx in "${KD_ORDER[@]}"; do LEDGER_IDS[${IDX_ID[$idx]}]=1; done
in_ledger() { [ -n "${LEDGER_IDS[$1]:-}" ]; }

# ---------------------------------------------------------------- 対象ファイルの収集
list_files() { # list_files <name パターン> <dir>...
    local pat="$1"; shift
    local d
    for d in "$@"; do
        [ -d "$d" ] && find "$d" -type f -name "$pat" -print
    done | LC_ALL=C sort
}

M5_SLT_DIRS=("$ROOT/slt/m5" "$ROOT/restart/m5")
mapfile -t M5_FILES < <(list_files '*.slt' "${M5_SLT_DIRS[@]}")
mapfile -t ALL_SLT < <(list_files '*.slt' "$ROOT/slt" "$ROOT/restart")
if [ "$LEGACY" -eq 1 ]; then
    SCOPE12=("${ALL_SLT[@]}")
else
    SCOPE12=("${M5_FILES[@]}")
fi

# ---------------------------------------------------------------- L9 台帳の書式
if want L9 || want L10; then
    for idx in "${KD_ORDER[@]}"; do :; done
fi
if want L9; then
    seen_ids=" "
    for idx in $(seq 1 "$(grep -c '^\[\[kd\]\]' "$LEDGER")"); do
        id="${IDX_ID[$idx]:-}"
        if [ -z "$id" ]; then fail L9 "known-diffs.toml: [[kd]] #$idx に id がない"; continue; fi
        case "$id" in KD-[0-9]*) ;; *) fail L9 "$id: id が KD-<n> の形でない" ;; esac
        case "$seen_ids" in *" $id "*) fail L9 "$id: id が重複している" ;; esac
        seen_ids+="$id "
        case "${KD_KIND[$idx]:-}" in
            diff | unsupported | both) ;;
            *) fail L9 "$id: kind が diff / unsupported / both のどれでもない: '${KD_KIND[$idx]:-}'" ;;
        esac
        for k in title chapter since; do
            [ -n "${KD_FIELDS_OF[$idx:$k]:-}" ] || fail L9 "$id: $k がない"
        done
        if [ -n "${KD_PATTERN[$idx]:-}" ]; then
            case "${KD_KIND[$idx]:-}" in
                unsupported | both) ;;
                *) fail L9 "$id: pattern は kind が unsupported / both の KD だけに付ける" ;;
            esac
            if ! perl -e 'my $p = shift; eval { qr/$p/i }; exit($@ ? 1 : 0)' "${KD_PATTERN[$idx]}" 2> /dev/null; then
                fail L9 "$id: pattern が正規表現として不正"
            fi
        fi
    done
    # questions の実在（99-questions.md がある場合だけ）
    if [ -f "$DESIGN/99-questions.md" ]; then
        for idx in "${KD_ORDER[@]}"; do
            while IFS= read -r q; do
                [ -n "$q" ] || continue
                found=0
                if [[ "$q" =~ ^M5-Q([0-9]+)$ ]]; then
                    grep -q "^| $q |" "$DESIGN/99-questions.md" && found=1
                elif [[ "$q" =~ ^M5-([A-Z]+-Q[0-9]+)$ ]]; then
                    grep -Eq "^\| M5-Q[0-9]+ \| ${BASH_REMATCH[1]} \|" "$DESIGN/99-questions.md" && found=1
                fi
                [ "$found" -eq 1 ] || fail L9 "${IDX_ID[$idx]}: questions の $q が 99-questions.md にない"
            done <<< "${KD_QUESTIONS[$idx]:-}"
        done
    fi
fi

# ---------------------------------------------------------------- L1 / L2
if want L1 || want L2; then
    kd_list=" ${!LEDGER_IDS[*]} "
    n_skipif=0
    if [ "${#SCOPE12[@]}" -gt 0 ]; then
        awk -v kds="$kd_list" -v root="$ROOT/" '
        function relname(p) { return (index(p, root) == 1) ? substr(p, length(root) + 1) : p }
        FNR == 1 { cb = "" }
        /^[ \t]*$/ { cb = ""; next }
        /^[ \t]*#/ { cb = cb "\n" $0; next }
        /^[ \t]*(onlyif|skipif)[ \t]/ {
            d = $0; gsub(/^[ \t]+|[ \t]+$/, "", d)
            if (d == "onlyif yuzhu") {
                s = cb; found = 0
                while (match(s, /KD-[0-9]+/)) {
                    id = substr(s, RSTART, RLENGTH); found = 1
                    if (index(kds, " " id " ") == 0)
                        printf "L1\x1f%s:%d\x1fonlyif yuzhu の KD が台帳にない: %s\n", relname(FILENAME), FNR, id
                    s = substr(s, RSTART + RLENGTH)
                }
                if (!found) printf "L1\x1f%s:%d\x1fonlyif yuzhu の直前のコメントに # KD-<n> がない\n", relname(FILENAME), FNR
            } else if (d == "skipif yuzhu") {
                printf "S\x1f%s:%d\x1f\n", relname(FILENAME), FNR
                if (cb !~ /未実装:[ \t]*[^ \t]+-[^ \t]+/)
                    printf "L2\x1f%s:%d\x1fskipif yuzhu の直前のコメントに # 未実装: <章>-<WP> がない\n", relname(FILENAME), FNR
            }
            next
        }
        { cb = "" }
        ' "${SCOPE12[@]}" > "$TMP/l12.raw"
        while IFS="$US" read -r tag loc msg; do
            case "$tag" in
                L1) want L1 && fail L1 "$loc: $msg" ;;
                L2) want L2 && fail L2 "$loc: $msg" ;;
                S) n_skipif=$((n_skipif + 1))
                   if [ "$RELEASE" -eq 1 ] && want L2; then fail L2 "$loc: --release では skipif yuzhu を 0 件にする"; fi ;;
            esac
        done < "$TMP/l12.raw"
    fi
    if [ "$LEGACY" -eq 0 ] && want L2; then
        n_legacy=0
        if [ "${#ALL_SLT[@]}" -gt 0 ]; then
            n_legacy="$(grep -hEc '^[[:space:]]*skipif[[:space:]]+yuzhu[[:space:]]*$' "${ALL_SLT[@]}" | awk '{s += $1} END {print s + 0}')"
        fi
        n_legacy=$((n_legacy - n_skipif))
        [ "$n_legacy" -gt 0 ] && echo "NOTE L2 M1〜M4 に skipif yuzhu が $n_legacy 件ある（--include-legacy で検査対象にできる）"
    fi
fi

# ---------------------------------------------------------------- L3 / L4
if want L3 && [ -d "$ROOT/isolation/ported/expected" ]; then
    for f in "$ROOT"/isolation/ported/expected/*.yuzhu-m5.out; do
        [ -e "$f" ] || continue
        name="$(basename "$f")"; name="${name%.yuzhu-m5.out}"
        ok=0
        for idx in "${KD_ORDER[@]}"; do
            while IFS= read -r pat; do
                [ -n "$pat" ] || continue
                # shellcheck disable=SC2053
                if [[ "isolation/ported/$name" == $pat ]]; then ok=1; break; fi
            done <<< "${KD_AFFECTS[$idx]:-}"
            [ "$ok" -eq 1 ] && break
        done
        [ "$ok" -eq 1 ] || fail L3 "$(rel "$f"): 台帳の affects に isolation/ported/$name がない"
    done
fi
if want L4 && [ -f "$ROOT/isolation/ported/skip.txt" ]; then
    n=0
    while IFS= read -r line || [ -n "$line" ]; do
        n=$((n + 1))
        case "$line" in '' | \#*) continue ;; esac
        if ! [[ "$line" =~ KD-[0-9]+ ]]; then
            fail L4 "isolation/ported/skip.txt:$n: KD の ID がない: $line"
            continue
        fi
        rest="$line"
        while [[ "$rest" =~ (KD-[0-9]+) ]]; do
            in_ledger "${BASH_REMATCH[1]}" || fail L4 "isolation/ported/skip.txt:$n: ${BASH_REMATCH[1]} が台帳にない"
            rest="${rest#*"${BASH_REMATCH[1]}"}"
        done
    done < "$ROOT/isolation/ported/skip.txt"
fi

# ---------------------------------------------------------------- L5
if want L5; then
    for f in "${M5_FILES[@]}"; do
        case "$f" in "$ROOT"/slt/m5/*) ;; *) continue ;; esac
        stem="$(basename "$f" .slt)"
        dir="$(basename "$(dirname "$f")")"
        pfx="$(printf '%s_%s_' "$dir" "$stem" | tr 'A-Z' 'a-z')"
        awk -v pfx="$pfx" -v name="$(rel "$f")" '
        function norm(n) { gsub(/"/, "", n); return tolower(n) }
        {
            l = tolower($0)
            if (l ~ /^[ \t]*#/) next
            if (match(l, /^[ \t]*drop[ \t]+(table|index|sequence|role|user|database)[ \t]+if[ \t]+exists[ \t]+[^;]*/)) {
                t = substr(l, RSTART, RLENGTH)
                sub(/^[ \t]*drop[ \t]+[a-z]+[ \t]+if[ \t]+exists[ \t]+/, "", t)
                m = split(t, parts, ",")
                for (i = 1; i <= m; i++) {
                    p = parts[i]
                    gsub(/^[ \t]+|[ \t]+$/, "", p)
                    sub(/[ \t]+(cascade|restrict).*$/, "", p)
                    sub(/[ \t]*\(.*$/, "", p)
                    if (p != "" && !(norm(p) in dropped)) dropped[norm(p)] = FNR
                }
                next
            }
            if (match(l, /^[ \t]*create[ \t]+(or[ \t]+replace[ \t]+)?(global[ \t]+|local[ \t]+)?(temp[a-z]*[ \t]+|unlogged[ \t]+)?(unique[ \t]+)?(table|index|sequence|role|user|database)[ \t]+(concurrently[ \t]+)?(if[ \t]+not[ \t]+exists[ \t]+)?[^ \t(;]+/)) {
                t = substr(l, RSTART, RLENGTH)
                k = split(t, parts, /[ \t]+/)
                n = norm(parts[k])
                if (n == "on" || n == "exists" || n == "concurrently") next
                if (t ~ /(^|[ \t])mapping[ \t]/) next
                if (index(n, pfx) != 1)
                    printf "L5\x1f%s:%d\x1f作る名前 %s が接頭辞 %s で始まらない\n", name, FNR, n, pfx
                else if (length(n) > 63)
                    printf "L5\x1f%s:%d\x1f名前 %s が 63 バイトを超える\n", name, FNR, n
                if (!(n in created)) {
                    created[n] = FNR
                    if (!(n in dropped))
                        printf "L5\x1f%s:%d\x1f%s を作る前に DROP ... IF EXISTS %s がない\n", name, FNR, n, n
                }
            }
        }
        ' "$f"
    done > "$TMP/l5.raw"
    while IFS="$US" read -r tag loc msg; do
        [ "$tag" = L5 ] && fail L5 "$loc: $msg"
    done < "$TMP/l5.raw"
fi

# ---------------------------------------------------------------- L6
resolve_affect() { # 台帳のパスが 1 件以上に解決できるか（root からの相対）
    local p="$1" c name
    local cands=("$p" "$p.*" "$p/*")
    if [[ "$p" == isolation/ported/* ]]; then
        name="${p#isolation/ported/}"
        cands+=("isolation/ported/specs/$name" "isolation/ported/specs/$name.spec" "isolation/ported/specs/$name*.spec")
    fi
    for c in "${cands[@]}"; do
        # shellcheck disable=SC2086
        (cd "$ROOT" && compgen -G "$c" > /dev/null) && return 0
    done
    return 1
}
declare -A KD_RESOLVED=()
if want L6 || want L10; then
    n_missing=0
    for idx in "${KD_ORDER[@]}"; do
        id="${IDX_ID[$idx]}"
        while IFS= read -r p; do
            [ -n "$p" ] || continue
            if resolve_affect "$p"; then
                KD_RESOLVED[$idx]=$((${KD_RESOLVED[$idx]:-0} + 1))
            else
                n_missing=$((n_missing + 1))
                if want L6; then
                    if [ "$RELEASE" -eq 1 ]; then fail L6 "$id: affects の $p が実在しない"
                    else warn L6 "$id: affects の $p が実在しない（実装中は警告。--release で失敗）"; fi
                fi
            fi
        done <<< "${KD_AFFECTS[$idx]:-}"
    done
fi

# ---------------------------------------------------------------- L7
if want L7; then
    if [ "${#ALL_SLT[@]}" -gt 0 ]; then
        grep -nE '^[[:space:]]*(statement|query)[[:space:]]+error[[:space:]]+db error:' "${ALL_SLT[@]}" 2> /dev/null |
            while IFS= read -r hit; do
                f="${hit%%:*}"; rest="${hit#*:}"; ln="${rest%%:*}"
                echo "L7${US}$(rel "$f"):$ln${US}statement error に db error: の接頭辞が残っている（SQLSTATE か短い部分一致に直す）"
            done > "$TMP/l7.raw"
        while IFS="$US" read -r tag loc msg; do
            [ "$tag" = L7 ] && fail L7 "$loc: $msg"
        done < "$TMP/l7.raw"
    fi
fi

# ---------------------------------------------------------------- L8
if want L8; then
    : > "$TMP/l8.pat"
    for idx in "${KD_ORDER[@]}"; do
        case "${KD_KIND[$idx]:-}" in unsupported | both) ;; *) continue ;; esac
        [ -n "${KD_PATTERN[$idx]:-}" ] || continue
        printf '%s\t%s\n' "${IDX_ID[$idx]}" "${KD_PATTERN[$idx]}" >> "$TMP/l8.pat"
    done
    mapfile -t SPEC_FILES < <(list_files '*.spec' "$ROOT/isolation/specs")
    if [ -s "$TMP/l8.pat" ] && { [ "${#SCOPE12[@]}" -gt 0 ] || [ "${#SPEC_FILES[@]}" -gt 0 ]; }; then
        perl -e '
            use strict; use warnings;
            my ($patfile, $root, @files) = @ARGV;
            my @pats;
            open(my $ph, "<", $patfile) or die;
            while (<$ph>) { chomp; my ($id, $re) = split /\t/, $_, 2; push @pats, [$id, qr/$re/i]; }
            close $ph;
            sub rel { my $p = shift; $p =~ s/^\Q$root\E\/?//; $p }
            for my $f (@files) {
                open(my $fh, "<", $f) or next;
                my @lines = <$fh>; close $fh;
                chomp @lines;
                if ($f =~ /\.spec$/) {
                    for my $i (0 .. $#lines) {
                        my $l = $lines[$i];
                        next if $l =~ /^\s*(#|--)/;
                        for my $p (@pats) {
                            next unless $l =~ $p->[1];
                            my $id = $p->[0];
                            next if $l =~ /\Q$id\E\b/ or ($i > 0 && $lines[$i - 1] =~ /\Q$id\E\b/);
                            printf "L8\x1f%s:%d\x1f%s の未対応の機能が共有 spec に現れる（`# %s` を直前に書くか ported/skip.txt へ）: %s\n", rel($f), $i + 1, $id, $id, $l;
                        }
                    }
                    next;
                }
                # slt: 空行区切りのレコードごとに、onlyif / skipif yuzhu があれば免除する
                my $i = 0;
                while ($i <= $#lines) {
                    if ($lines[$i] =~ /^\s*$/) { $i++; next; }
                    my $start = $i;
                    $i++ while $i <= $#lines && $lines[$i] !~ /^\s*$/;
                    my @rec = @lines[$start .. $i - 1];
                    next if grep { /^\s*(onlyif|skipif)\s+(yuzhu|pg|postgres\w*)\s*$/ } @rec;
                    my $in_result = 0;
                    for my $j (0 .. $#rec) {
                        my $l = $rec[$j];
                        if ($l =~ /^----/) { $in_result = 1; next; }
                        next if $in_result;
                        next if $l =~ /^\s*#/;
                        next if $l =~ /^\s*(statement|query|skipif|onlyif|halt|hash-threshold|include|control|sleep)\b/ && $l !~ /;/ && $l !~ /\b(select|create|alter|drop|set|begin|vacuum|copy)\b/i;
                        for my $p (@pats) {
                            next unless $l =~ $p->[1];
                            printf "L8\x1f%s:%d\x1f%s の未対応の機能が onlyif yuzhu なしで現れる: %s\n", rel($f), $start + $j + 1, $p->[0], $l;
                        }
                    }
                }
            }
        ' "$TMP/l8.pat" "$ROOT" "${SCOPE12[@]}" "${SPEC_FILES[@]}" > "$TMP/l8.raw"
        while IFS="$US" read -r tag loc msg; do
            [ "$tag" = L8 ] && fail L8 "$loc: $msg"
        done < "$TMP/l8.raw"
    fi
fi

# ---------------------------------------------------------------- L10
if want L10 && [ "$RELEASE" -eq 1 ]; then
    for idx in "${KD_ORDER[@]}"; do
        id="${IDX_ID[$idx]}"
        if [ "${KD_RESOLVED[$idx]:-0}" -eq 0 ] && [ -z "${KD_QUESTIONS[$idx]:-}" ]; then
            fail L10 "$id: 実在するテスト（affects）にも確認事項（questions）にも紐づかない"
        fi
    done
fi

echo "lint-tests: FAIL $NFAIL / WARN $NWARN（KD ${#KD_ORDER[@]} 件、M5 のファイル ${#M5_FILES[@]} 件、全 slt ${#ALL_SLT[@]} 件）"
[ "$NFAIL" -eq 0 ]
