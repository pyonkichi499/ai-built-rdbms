#!/usr/bin/env bash
# 認証の互換テスト（auth/run.sh）の共通部分。10 §6.10.4。
#
# シナリオごとに、認証の設定（hba と authentication_timeout）を自由に変えられる専用のサーバを起動する
# （compat/run.sh が渡す共有サーバの認証方式には依存しない）。種類は TARGET（pg | yuzhu）と同じ。
# A4 だけは両方（yuzhu と PostgreSQL）を同時に起動する。起動したものは終了時にすべて止めて消す。
#
# 前提:
#   pg    initdb / pg_ctl（PATH 上。コンテナでは nss_wrapper で補う。sandbox/pg.sh と同じ）
#   yuzhu yuzhu-initdb / yuzhu-server（YUZHU_BIN_DIR、なければ ${CARGO_TARGET_DIR:-impl/rust/target}/release）
# 環境変数: AUTH_PORT_BASE（既定 7430。インスタンスごとに +1）、AUTH_KEEP=1（終了時に作業ディレクトリとログを残す）

AUTH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPAT_DIR="$(cd "$AUTH_DIR/.." && pwd)"
REPO_ROOT="$(cd "$COMPAT_DIR/../.." && pwd)"
ADMIN_PW="admin-pw"
WORK="$(mktemp -d /tmp/yuzhu-auth.XXXXXX)"
AUTH_PORT_BASE="${AUTH_PORT_BASE:-7430}"
declare -A INST_KIND INST_PORT INST_DIR INST_PID INST_TIMEOUT
INST_NAMES=()
_next_port=0

cleanup_all() {
    local n
    for n in "${INST_NAMES[@]:-}"; do [ -n "$n" ] && inst_stop "$n" >/dev/null 2>&1; done
    if [ "${AUTH_KEEP:-0}" = 1 ]; then echo "作業ディレクトリを残した: $WORK" >&2; else rm -rf "$WORK"; fi
}
trap cleanup_all EXIT

# ---- 結果の記録 ----
RESULTS=()
FAIL=0
# rec <ID> <ok|FAIL|SKIP> <詳細>
rec() {
    RESULTS+=("$1|$2|$3")
    echo "RESULT $1 $2 $3"
    [ "$2" = FAIL ] && FAIL=1
    return 0
}
# expect_eq <ID> <説明> <期待> <実際>
expect_eq() {
    if [ "$3" = "$4" ]; then rec "$1" ok "$2"; else rec "$1" FAIL "$2: 期待「$3」実際「$4」"; fi
}

# ---- クライアント ----
build_client() {
    local tdir="${COMPAT_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-/tmp/yuzhu-compat-target}}"
    ( cd "$AUTH_DIR/client" && CARGO_TARGET_DIR="$tdir" cargo build --release --locked --quiet 2>/dev/null \
        || CARGO_TARGET_DIR="$tdir" cargo build --release --locked --offline --quiet ) || return 1
    AUTHC="$tdir/release/yuzhu-compat-authc"
    [ -x "$AUTHC" ]
}
# authc の最初の行（結果）だけ返す。<host> は接続先であり送信元アドレス（127.0.0.x に bind する）。
# try_connect <host> <port> <user> <db> [<password>] [<sql>]
try_connect() {
    local host="$1" port="$2" user="$3" db="$4" pw="${5:--}" sql="${6:-}" args=()
    [ "$pw" = "-" ] || args+=(--password "$pw")
    [ -z "$sql" ] || args+=(--sql "$sql")
    local out
    out="$("$AUTHC" connect "$host" "$port" "$user" "$db" --bind "$host" "${args[@]}")"
    printf '%s\n' "${out%%$'\n'*}"
}
# 結果の行 → 判定語（ok/<method> か SQLSTATE）
verdict() {
    case "$1" in
        OK\ *) echo "ok/${1#OK }" ;;
        ERR\ *) local r="${1#ERR }"; echo "${r%% *}" ;;
        *) echo "?${1}" ;;
    esac
}

# ---- サーバの起動 ----
with_user() {
    if getent passwd "$(id -u)" >/dev/null 2>&1; then "$@"; return; fi
    local lib passwd group rc=0
    lib="$(find /usr/lib -name libnss_wrapper.so 2>/dev/null | head -n 1)"
    [ -n "$lib" ] || { echo "libnss_wrapper.so not found" >&2; return 1; }
    passwd="$(mktemp)"; group="$(mktemp)"
    echo "postgres:x:$(id -u):$(id -g):PostgreSQL:/tmp:/bin/false" >"$passwd"
    echo "postgres:x:$(id -g):" >"$group"
    LD_PRELOAD="$lib" NSS_WRAPPER_PASSWD="$passwd" NSS_WRAPPER_GROUP="$group" "$@" || rc=$?
    rm -f "$passwd" "$group"
    return "$rc"
}

yuzhu_bin_dir() {
    local d
    for d in "${YUZHU_BIN_DIR:-}" "${CARGO_TARGET_DIR:+$CARGO_TARGET_DIR/release}" "$REPO_ROOT/impl/rust/target/release"; do
        [ -n "$d" ] && [ -x "$d/yuzhu-server" ] && [ -x "$d/yuzhu-initdb" ] || continue
        # 認証（08 章。--hba-file）に対応していないバイナリは使わない（古いビルドを拾わない）
        "$d/yuzhu-server" --help 2>/dev/null | grep -q -- "--hba-file" || continue
        echo "$d"; return 0
    done
    return 1
}

# kind_available <pg|yuzhu>
kind_available() {
    case "$1" in
        pg) command -v initdb >/dev/null 2>&1 && command -v pg_ctl >/dev/null 2>&1 ;;
        yuzhu) yuzhu_bin_dir >/dev/null ;;
    esac
}

_wait_port() {
    local port="$1" i
    for i in $(seq 1 120); do
        "$AUTHC" stall 127.0.0.1 "$port" silent 1 2>/dev/null | grep -q '^ALIVE' && return 0
        sleep 0.25
    done
    return 1
}

# inst_start <name> <pg|yuzhu> [<authentication_timeout 秒>]。起動後は trust で、postgres のパスワードは admin-pw に設定済み。
inst_start() {
    local name="$1" kind="$2" timeout="${3:-}" port dir
    port=$((AUTH_PORT_BASE + _next_port)); _next_port=$((_next_port + 1))
    dir="$WORK/$name"
    INST_KIND[$name]="$kind"; INST_PORT[$name]="$port"; INST_DIR[$name]="$dir"; INST_TIMEOUT[$name]="$timeout"
    INST_NAMES+=("$name")
    case "$kind" in
        pg)
            with_user initdb -D "$dir" -U postgres --auth=trust --locale=C --encoding=UTF8 >"$WORK/$name.initdb.log" 2>&1 \
                || { cat "$WORK/$name.initdb.log" >&2; return 1; }
            {
                echo "listen_addresses = '127.0.0.1,127.0.0.2'"
                echo "unix_socket_directories = ''"
                echo "port = $port"
                echo "max_connections = 50"
                [ -z "$timeout" ] || echo "authentication_timeout = ${timeout}s"
            } >>"$dir/postgresql.conf"
            ;;
        yuzhu)
            local bin; bin="$(yuzhu_bin_dir)" || { echo "yuzhu のバイナリが無い" >&2; return 1; }
            "$bin/yuzhu-initdb" -D "$dir" -U postgres --no-sync >"$WORK/$name.initdb.log" 2>&1 \
                || { cat "$WORK/$name.initdb.log" >&2; return 1; }
            ;;
    esac
    _inst_launch "$name" || return 1
    inst_admin "$name" -c "alter role postgres password '$ADMIN_PW'" >/dev/null || return 1
}

_inst_launch() {
    local name="$1" kind="${INST_KIND[$1]}" port="${INST_PORT[$1]}" dir="${INST_DIR[$1]}" timeout="${INST_TIMEOUT[$1]}"
    case "$kind" in
        pg)
            with_user pg_ctl -D "$dir" -l "$WORK/$name.log" -o "-p $port" -w -t 60 start >/dev/null 2>&1 \
                || { tail -n 20 "$WORK/$name.log" >&2; return 1; }
            ;;
        yuzhu)
            local bin args; bin="$(yuzhu_bin_dir)"
            args=(-D "$dir" --listen 0.0.0.0 --port "$port")
            [ -z "$timeout" ] || args+=(--authentication-timeout "$timeout")
            nohup "$bin/yuzhu-server" "${args[@]}" >>"$WORK/$name.log" 2>&1 &
            INST_PID[$name]=$!
            ;;
    esac
    _wait_port "$port" || { echo "$name が起動しない" >&2; tail -n 20 "$WORK/$name.log" >&2; return 1; }
}

# 止める（データは残す）。 inst_halt <name>
inst_halt() {
    local name="$1" kind="${INST_KIND[$1]:-}" pid="${INST_PID[$1]:-}" i
    case "$kind" in
        pg) with_user pg_ctl -D "${INST_DIR[$name]}" -m fast -w stop >/dev/null 2>&1 || true ;;
        yuzhu)
            [ -n "$pid" ] || return 0
            kill -INT "$pid" 2>/dev/null || true
            for i in $(seq 1 120); do kill -0 "$pid" 2>/dev/null || break; sleep 0.25; done
            kill -9 "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            INST_PID[$name]=""
            ;;
    esac
}
inst_stop() { inst_halt "$1"; }

# 設定ファイルを hba に置いて反映する（どちらも再起動。M5 の yuzhu は再読み込みが無い）。 inst_set_hba <name> <file>
inst_set_hba() {
    local name="$1" file="$2" kind="${INST_KIND[$1]}"
    inst_halt "$name"
    case "$kind" in
        pg) cp "$file" "${INST_DIR[$name]}/pg_hba.conf" ;;
        yuzhu) cp "$file" "${INST_DIR[$name]}/yuzhu_hba.conf" ;;
    esac
    _inst_launch "$name"
}

# hba を差し替えて起動できるか（解析エラーなら起動しない）。成功 0。
inst_try_set_hba() {
    local name="$1" file="$2" kind="${INST_KIND[$1]}"
    inst_halt "$name"
    case "$kind" in
        pg) cp "$file" "${INST_DIR[$name]}/pg_hba.conf" ;;
        yuzhu) cp "$file" "${INST_DIR[$name]}/yuzhu_hba.conf" ;;
    esac
    _inst_launch "$name" 2>/dev/null
}

# 標準の hba（127.0.0.0/8 を 1 つの方式にする）。 write_hba <ファイル> <方式>
write_hba() { printf '# TYPE DATABASE USER ADDRESS METHOD\nhost all all 127.0.0.0/8 %s\n' "$2" >"$1"; }

# 管理者（postgres）として実行する。trust のときも admin-pw を付けて通る。 inst_admin <name> <psql の引数...>
inst_admin() {
    local name="$1"; shift
    PGPASSWORD="$ADMIN_PW" psql -X -Atq -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "${INST_PORT[$name]}" -U postgres -d postgres "$@"
}

# ---- scram 構成の共有サーバ（A2・A5・A6・A8・A9）----
# ensure_sc: 初回だけ起動して、hba を `127.0.0.0/8 scram-sha-256` にする。postgres のパスワードは admin-pw。
SC_READY=0
ensure_sc() {
    [ "$SC_READY" = 1 ] && return 0
    inst_start sc "$TARGET" || return 1
    inst_set_hba_prepared_sc || return 1
    SC_READY=1
}
inst_set_hba_prepared_sc() {
    write_hba "$WORK/sc.hba" scram-sha-256
    inst_set_hba sc "$WORK/sc.hba"
}
# sc に対する接続の結果の行。 sc_connect <user> <db> <pw> [<sql>]
sc_connect() { try_connect 127.0.0.1 "${INST_PORT[sc]}" "$1" "$2" "$3" "${4:-}"; }
sc_admin() { inst_admin sc "$@"; }
# 全文一致で比べる（メッセージの検査）
expect_line() { # <ID> <説明> <期待の行> <実際の行>
    if [ "$3" = "$4" ]; then rec "$1" ok "$2"; else rec "$1" FAIL "$2: 期待「$3」実際「$4」"; fi
}
