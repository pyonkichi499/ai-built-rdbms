#!/usr/bin/env python3
"""Generate pg17_regress.tsv: hand-picked edge cases found by adversarial
review, evaluated by a real PostgreSQL 17. Same file format and same `ev`
function as gen_corpus.py (whose FUNC is reused).

Usage:
    docker run -d --name yuzhu-numeric-pg17 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:17
    PGCONTAINER=yuzhu-numeric-pg17 python3 gen_regress.py
"""

import os
import random
import re
import subprocess
import sys

random.seed(4242)
HERE = os.path.dirname(os.path.abspath(__file__))
CONTAINER = os.environ.get("PGCONTAINER", "yuzhu-numeric-pg17")

_src = open(os.path.join(HERE, "gen_corpus.py"), encoding="utf-8").read()
FUNC = re.search(r'FUNC = r"""(.*?)"""', _src, re.S).group(1)

cases = []


def add(op, a, b=""):
    cases.append((op, str(a), str(b)))


def typmod(p, s):
    return ((p << 16) | (s & 0x7FF)) + 4


# ------------------------------------------------ float -> numeric ties
# %.15g / %.6g round exact decimal ties half to even.
for v in ["1000000000000000.5", "1000000000000005.0", "1000000000000015.0",
          "1000000000000025.0", "123456789012345.5", "123456789012344.5",
          "2.5e15", "4503599627370495.5", "0.5", "1.5", "2.5",
          "9007199254740993", "999999999999999.5", "999999999999998.5"]:
    add("fromf8", v)
for _ in range(300):
    add("fromf8", random.randrange(10 ** 15, 9 * 10 ** 15) // 10 * 10 + 5)
for _ in range(300):
    add("fromf8", f"{random.randrange(10 ** 14, 10 ** 15)}.5")
for _ in range(300):
    add("fromf8", f"{random.randrange(10 ** 13, 10 ** 14)}.25")
for v in ["1234565", "1234575", "123456.5", "123455.5", "9999995", "0.5", "2.5",
          "16777215", "1000005", "65536.5"]:
    add("fromf4", v)
for _ in range(300):
    add("fromf4", random.randrange(10 ** 6, 16 * 10 ** 6) // 10 * 10 + 5)
for _ in range(300):
    add("fromf4", f"{random.randrange(10 ** 5, 10 ** 6)}.5")
for _ in range(200):
    add("fromf4", f"{random.randrange(10 ** 4, 10 ** 5)}.25")

# ------------------------------------------------ numeric -> float edges
for v in ["1e-400", "2.4703282292062327e-324", "2.4703282292062328e-324",
          "4.9406564584124654e-324", "1.7976931348623157e308", "1.7976931348623158e308",
          "1.79769313486231580793728971405301e308", "1.7976931348623159e308", "1e308",
          "-1e-400", "0.000", "1e-320", "-2.2250738585072011e-308"]:
    add("tof8", v)
for v in ["1e-46", "7e-46", "7.006492321624085e-46", "1.401298464324817e-45",
          "3.4028234663852886e38", "3.4028235677973366e38", "3.4028235677973367e38",
          "3.5e38", "-3.5e38", "1e-40", "1.1754943508222875e-38", "0.000"]:
    add("tof4", v)

# ------------------------------------------------ numeric -> int edges
for v in ["9223372036854775807.4999", "9223372036854775807.5", "-9223372036854775808.4999",
          "-9223372036854775808.5", "99999999999999999999.5", "1e19", "-1e19",
          "2147483647.49999999999", "2147483647.5", "-2147483648.5", "32767.4999", "32767.5",
          "-32768.5", "-32768.49", "0.49999999999999999999", "-0.5", "1e-16383"]:
    for op in ("toi8", "toi4", "toi2"):
        add(op, v)

# ------------------------------------------------ text input edges
INPUTS = [
    "0x_1", "0x1_", "0x_", "-0x", "0X1F", "-0x1F", "+0x1f", "0b102", "0o8", "0o17", "0B1_0",
    "0x1.5", "0x1e5", "0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF", "0x" + "f" * 200,
    "0b" + "1" * 300, "0o" + "7" * 120, " 0x10 ", "0x 10", "00x10", "0_0x1",
    "1e+_5", "1e-_5", "1e_", "1.e5", ".5e-3", "5.e", "5.e+", "1.5_5", ".5_5", "1_000.000_1",
    "1e00000000000000000005", "1e0000000000000000000000000000000000",
    "1e2147483647", "1e1073741823", "1e1073741824", "1e-1073741823", "1e-1073741824",
    "0e-20000", "0e20000", "0.0e-16383", "0e-16383", "0e-16384", "1e-16383", "1e-16384",
    "1e131071", "1e131072", "9.999e131071", "99999e131067", "0.00001e131076",
    "-1e-16383", "1e-16382", "1e-16383_0",
    " NaN ", "+NaN", "-NaN", "nan", "NAN", "nanx", "NaN1", "\u00a01", "1\u00a0",
    "infinity", "INFINITY", "-inf", "+Inf", "infinit", "infinityy", "inf inity", "- inf",
    "Infinity ", " -Infinity\t", "\v1\f", "\r\n1\r\n", "1\x00",
    "1" + "0" * 131071, "1" + "0" * 131072, "0." + "0" * 16382 + "1", "0." + "0" * 16383 + "1",
    "." + "0" * 16383, "." + "0" * 16384, "00000000000000000000001.10000000000000000000",
    "-0", "-0.000", "-0e10", "-0x0", "+0", "+.0", "-.0e-5",
]
for s in INPUTS:
    add("in", s, -1)
for s in ["1e-20000", "1e100000", "1e200000", "0e-20000", "1e131072", "Infinity", "NaN",
          "0x10000", "-0x1F", "99.995", "-99.995", "0.5e1"]:
    for t in [typmod(5, 2), typmod(1, -1000), typmod(1000, 1000), typmod(1, 0), typmod(3, 5),
              typmod(1000, -1000)]:
        add("in", s, t)

# ------------------------------------------------ typmod casts
for s in ["5e999", "4.9e999", "1e999", "9.5e1000", "1e1001", "-5e999", "0.5", "-0.5",
          "0.000005", "0.0000049", "0.00000500"]:
    for t in [typmod(1, -1000), typmod(1000, -1000), typmod(1000, 1000), typmod(1, 1000),
              typmod(3, 5), typmod(2, -3), typmod(1, -1)]:
        add("cast", s, t)
for mods in ["1000,-1000", "1000,1000", "1000,1001", "1000,-1001", "1,-1", "0,0", "1001",
             "-1", "1,2,3", "2147483647", "5,-2147483648"]:
    add("typmod", mods)

# ------------------------------------------------ arithmetic at the limits
BIG = ["1e131071", "9.9999e131071", "1e100000", "1e65536", "1e-16383", "1e-10000", "1e-8192",
       "5e-16383", "0.5", "3", "7", "1e-1000", "-1e131071", "1e-1001", "0"]
for a in BIG:
    for b in BIG:
        for op in ("add", "sub", "mul", "div", "mod", "divtrunc", "cmp"):
            add(op, a, b)
for a, b in [("1", "3e-16383"), ("1e131071", "0.1"), ("1e131071", "0.5"), ("1e131071", "2"),
             ("1", "1e131071"), ("-1", "1e131071"), ("1e-16383", "1e-16383"),
             ("1e-16383", "3"), ("2e-16383", "3"), ("1e-16383", "2"), ("1e-16383", "-2"),
             ("0.000", "-5"), ("-0.000", "5.00"), ("-1", "3"), ("1e1000", "3e-1000"),
             ("99999999999999999999", "0.00000000000000000001"),
             ("12345678901234567890.123456789", "-0.000000000000000000000001")]:
    for op in ("add", "sub", "mul", "div", "mod", "divtrunc"):
        add(op, a, b)

# ------------------------------------------------ rounding at the limits
for v in ["9e131071", "4.9e131071", "5e131071", "1e131071", "-9.9e131071", "12345.6789",
          "9999.5", "0.5", "1e-16383", "5e-16383", "4.9999e-16383"]:
    for s in [-131074, -131073, -131072, -131071, -131068, -131067, -16383, -4, -1, 0,
              16382, 16383, 16384, 2147483647, -2147483648]:
        add("round", v, s)
        add("trunc", v, s)
    add("ceil", v)
    add("floor", v)

# ------------------------------------------------ send
for v in ["1e131071", "-1e-16383", "0.00000", "-0.0", "1e-16383", "9.9999e131071",
          "0." + "0" * 16382 + "1"]:
    add("send", v)

# ---------------------------------------------------------------- run


def copy_escape(s):
    return (s.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
            .replace("\r", "\\r").replace("\x0b", "\\v").replace("\x0c", "\\f"))


cases = [c for c in cases if "\x00" not in c[1]]
data = "".join(f"{i}\t{copy_escape(op)}\t{copy_escape(a)}\t{copy_escape(b)}\n"
               for i, (op, a, b) in enumerate(cases))
script = (
    "set extra_float_digits = 1;\n" + FUNC +
    "create temp table c(id int, op text, a text, b text);\n"
    "copy c from stdin;\n" + data + "\\.\n"
    "copy (select op, a, b, ev(op, a, b) from c order by id) to stdout;\n"
)
out = subprocess.run(
    ["docker", "exec", "-i", CONTAINER, "psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-U", "postgres"],
    input=script.encode(), capture_output=True, check=False,
)
if out.returncode != 0:
    sys.stderr.write(out.stderr.decode())
    sys.exit(1)


def fnv64(s):
    h = 0xCBF29CE484222325
    for byte in s.encode():
        h ^= byte
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def shorten(s):
    """Long operands/results are kept readable in the fixture."""
    if len(s) > 100 and s.startswith("OK:"):
        body = s[3:]
        return f"OKH:{len(body)}:{fnv64(body):016x}"
    return s


rows = []
for line in out.stdout.decode().splitlines():
    op, a, b, res = line.split("\t")
    rows.append("\t".join((op, a, b, shorten(res))) + "\n")
# ------------------------------------------------ binary receive
# numeric_recv cannot be called from SQL; it is driven through
# COPY ... (FORMAT binary) into a column of the wanted typmod.
import struct  # noqa: E402


def mk(nd,w,sign,ds,digs,extra=b""):
    b=struct.pack(">HhHH",nd,w,sign,ds)
    for d in digs: b+=struct.pack(">h",d) if d<32768 else struct.pack(">H",d)
    return b+extra
RECV_CASES = [
 (mk(1,0,0x4000,0,[1]),-1),(mk(0,0,0x4000,0,[]),-1),(mk(3,2,0,0,[0,0,5]),-1),
 (mk(2,0,0,2,[1234,5678]),-1),(mk(0,0,0,0x4000,[]),-1),(mk(1,0,0,0,[10000]),-1),
 (mk(1,0,0,0,[0xFFFF]),-1),(mk(0,0,0x8000,0,[]),-1),(mk(0,0,0xD000,0,[]),typmod(5,2)),
 (mk(0,0,0xC000,0,[]),typmod(5,2)),(mk(1,0,0,0,[1],b"\x00"),-1),(mk(2,0,0,0,[1]),-1),
 (mk(2,32767,0,0,[1,0]),-1),(mk(2,32767,0,0,[0,1]),-1),(mk(1,-32768,0,16383,[1]),-1),
 (mk(1,32767,0,0,[1]),typmod(5,2)),(mk(0,0,0xD000,32,[]),-1),(mk(1,0,0xF000,0,[7]),-1),
 (mk(1,0,0xC000,0,[10000]),-1),(mk(1,0,0x4000,3,[0]),-1),(mk(1,1,0,5,[1]),typmod(3,1)),
 (mk(3,-1,0,4,[1,2,3]),-1),(mk(1,-1,0,0,[5000]),typmod(1,0)),(mk(1,-1,0,4,[5000]),typmod(1,0)),
 (mk(1,-1,0x4000,4,[5000]),typmod(1,0)),(mk(1,-1,0,4,[9500]),typmod(2,1)),(b"\x00\x01\x00\x00\x90\x00",-1),
 (b"\x00",-1),(b"",-1),(mk(1,0,0,0x3FFF,[1]),-1),(mk(1,-4097,0,0x3FFF,[1]),-1),
 (mk(1,-32768,0,0,[1]),-1),(mk(2,32767,0,0,[1,2]),typmod(1000,0)),(mk(1,0,0x0001,0,[1]),-1),
 (mk(1,0,0xE000,0,[1]),-1),(mk(1,4,0,0,[9]),typmod(20,0)),(mk(4,0,0,12,[1,9999,9999,9999]),typmod(5,2)),
]


def run_recv(b, t):
    col = "numeric" if t == -1 else f"numeric({(t - 4) >> 16},{((((t - 4) & 0x7FF) ^ 1024) - 1024)})"
    data = (b"PGCOPY\n\xff\r\n\x00" + struct.pack(">ii", 0, 0) + struct.pack(">hi", 1, len(b)) + b
            + struct.pack(">h", -1))
    args = ["docker", "exec", "-i", CONTAINER, "psql", "-X", "-q", "-At", "-U", "postgres",
            "-v", "ON_ERROR_STOP=1", "-v", "VERBOSITY=verbose"]
    for sql in ["drop table if exists yuzhu_recv", f"create table yuzhu_recv(x {col})",
                "copy yuzhu_recv from stdin (format binary)", "select x::text from yuzhu_recv"]:
        args += ["-c", sql]
    r = subprocess.run(args, input=data, capture_output=True, check=False)
    if r.returncode == 0:
        return shorten("OK:" + r.stdout.decode().strip())
    res = None
    for line in r.stderr.decode().splitlines():
        m = re.match(r"ERROR:  (\w{5}): (.*)", line)
        if m:
            res = f"ERR:{m.group(1)}:{m.group(2)}"
        m = re.match(r"DETAIL:  (.*)", line)
        if m:
            res += ":" + m.group(1)
    return res


for b, t in RECV_CASES:
    rows.append(f"recv\t{b.hex()}\t{t}\t{run_recv(b, t)}\n")
with open(os.path.join(HERE, "pg17_regress.tsv"), "w", encoding="utf-8") as f:
    f.write("".join(rows))
print(f"{len(rows)} cases written")
