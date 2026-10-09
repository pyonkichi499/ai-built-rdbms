#!/usr/bin/env bash
# pgJDBC のシナリオを流す（JDK 17+ が要る。ドライバの jar は版と SHA-1 を固定して取得する。ネットワークが要る）。
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

PGJDBC_VERSION="42.7.4"
PGJDBC_SHA1="264310fd7b2cd76738787dc0b9f7ea2e3b11adc1"
CACHE="${YUZHU_COMPAT_CACHE:-${TMPDIR:-/tmp}/yuzhu-compat-cache}"
JAR="$CACHE/postgresql-$PGJDBC_VERSION.jar"

command -v java >/dev/null 2>&1 || { echo "RESULT setup FAIL java が見つからない（JDK 17 以上が必要）"; exit 1; }

if [ ! -f "$JAR" ]; then
    mkdir -p "$CACHE"
    tmp="$JAR.$$"
    curl -fsSL -o "$tmp" "https://repo1.maven.org/maven2/org/postgresql/postgresql/$PGJDBC_VERSION/postgresql-$PGJDBC_VERSION.jar" \
        || { rm -f "$tmp"; echo "RESULT setup FAIL pgjdbc の取得に失敗"; exit 1; }
    sum="$(sha1sum "$tmp" | cut -d' ' -f1)"
    [ "$sum" = "$PGJDBC_SHA1" ] || { rm -f "$tmp"; echo "RESULT setup FAIL pgjdbc の SHA-1 が一致しない ($sum)"; exit 1; }
    mv "$tmp" "$JAR"
fi

# JVM のタイムゾーンを UTC 以外にして、timestamptz の往復（setTimestamp / getTimestamp）を踏む
exec java -Dfile.encoding=UTF-8 -Dstdout.encoding=UTF-8 -Duser.timezone=Asia/Tokyo -cp "$JAR" Main.java
