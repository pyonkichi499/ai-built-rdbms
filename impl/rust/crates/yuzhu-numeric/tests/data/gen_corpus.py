#!/usr/bin/env python3
"""Generate pg17_corpus.tsv: numeric test cases evaluated by a real PostgreSQL.

Usage (PostgreSQL 17 running in a Docker container):
    docker run -d --name yuzhu-numeric-pg17 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:17
    PGCONTAINER=yuzhu-numeric-pg17 python3 gen_corpus.py

Output format (PostgreSQL COPY text format, one case per line):
    op <TAB> a <TAB> b <TAB> result
where result is "OK:<text>", "OKH:<len>:<fnv1a64 hex>" (long results), or
"ERR:<sqlstate>:<message>[:<detail>]".
"""

import os
import random
import struct
import subprocess
import sys

random.seed(20261004)
HERE = os.path.dirname(os.path.abspath(__file__))
CONTAINER = os.environ.get("PGCONTAINER", "yuzhu-numeric-pg17")

cases = []


def add(op, a, b=""):
    cases.append((op, str(a), str(b)))


def rdigits(n, lead_nonzero=True):
    if n == 0:
        return ""
    s = "".join(random.choice("0123456789") for _ in range(n))
    if lead_nonzero and s[0] == "0":
        s = random.choice("123456789") + s[1:]
    return s


def rnum(maxint=25, maxfrac=25, allow_neg=True):
    """Random finite numeric literal in plain decimal form."""
    shape = random.random()
    if shape < 0.15:
        ip = rdigits(random.randint(1, 3))
        fp = rdigits(random.randint(0, 3), False)
    elif shape < 0.3:
        # small magnitude with leading zeros in the fraction
        ip = "0"
        fp = "0" * random.randint(0, 20) + rdigits(random.randint(1, 12))
    elif shape < 0.4:
        # digits that make lots of 9s / 5s (carry and rounding paths)
        ip = random.choice(["", "9" * random.randint(1, 12), "4" * random.randint(1, 3)]) or "0"
        fp = random.choice(["9", "5", "49", "50", "0"]) * random.randint(0, 8)
    else:
        ip = rdigits(random.randint(0, maxint)) or "0"
        fp = rdigits(random.randint(0, maxfrac), False)
    if random.random() < 0.2 and fp:
        fp += "0" * random.randint(1, 5)
    s = ip + ("." + fp if fp else "")
    if allow_neg and random.random() < 0.4:
        s = "-" + s
    return s


SPECIALS = ["NaN", "Infinity", "-Infinity"]
ZEROS = ["0", "0.0", "0.000", "-0", "0.0000000000"]
EDGE = [
    "1", "-1", "0.5", "-0.5", "1.5", "2.5", "-2.5", "9999", "10000", "0.0001", "0.00005",
    "99999999", "100000000", "0.9999", "1.0001", "12345.6789", "3", "7", "1e20", "1e-20",
    "9.999999999999999999", "0.1", "0.2", "0.3",
]


def rval():
    r = random.random()
    if r < 0.03:
        return random.choice(SPECIALS)
    if r < 0.06:
        return random.choice(ZEROS)
    if r < 0.12:
        return random.choice(EDGE)
    return rnum()


def typmod(p, s):
    return ((p << 16) | (s & 0x7FF)) + 4


# ---------------------------------------------------------------- input
INVALID = [
    "", " ", "abc", "1.2.3", "1e", "1e+", "1e-", "--1", "+-1", "-+1", "- 1", "1 2", ".", "+.", "-.",
    "1_", "_1", "1__2", "1._5", "1_.5", "1e5_", "1e_5", "1e5__0", "0x", "0xg", "0x_", "0x1_", "0b2",
    "0o8", "0b", "0o", "NaNx", "-NaN", "+NaN", "nan", "NAN", "nAn", " NaN ", "inf", "-inf", "+inf",
    "INF", "Infinity", "-Infinity", "+Infinity", "infinity", "INFINITY", "infinityx", "infinit",
    "infx", " -Infinity ", "\t-inf\n", "1e2147483647", "1e1073741824", "1e1073741823",
    "1e-1073741823", "1e-1073741824", "\u0661", "1,5", "$1", "1e-16383", "1e-16384", "1e131071",
    "9e131071", "1e131072", "9.9999e131071", "1e-16380", "0.1e-16382", "0x1F", "0X1f", "-0x80",
    "0x_1F", "0x1_F", "0xFFFFFFFFFFFFFFFFFFFFFFFF", "0o17", "0O17", "0b101", "0B101", "0x0", "-0x0",
    "0x1F.5", "0x1Fe5", " 0x10 ", "-0b1111111111111111111111111111111111111111111111111111111111111111",
    "0x7FFFFFFFFFFFFFFF", "0x8000000000000000", "0o_7", "0b_1_0", "1_000", "1_000.000_1",
    "1_000e1_0", ".5", "5.", "-.5", "+5.", "1.e5", ".e5", "1e+05", "1E-05", "  1.230 ",
    "\t1\n", "\r\n 7 \x0b\x0c", "1\x00", "00001.1000", "-000.000", "+0", "1e0", "1e-0",
    "123456789012345678901234567890.123456789012345678901234567890", "1 e5", "1e 5", "e5",
    "0.0e10", "5e-1", "5e-5", "12345e-3", "12345e-10", "1.5e1", "1.55e1", "0x" + "F" * 40,
    "0b" + "1" * 70, "0o" + "7" * 30, "1x", "1.2e3.4", "nana", "infinityinfinity",
    "1" * 200, "0." + "0" * 300 + "1",
]
for s in INVALID:
    add("in", s, -1)
for _ in range(1200):
    s = rnum(30, 30)
    r = random.random()
    if r < 0.15:
        s = s + "e" + random.choice(["", "+", "-"]) + str(random.randint(0, 40))
    elif r < 0.2:
        s = random.choice([" ", "  ", "\t", "\n"]) + s + random.choice(["", " ", "\n "])
    elif r < 0.25:
        s = s.replace("-", "+") if s.startswith("-") else "+" + s
    add("in", s, -1)
# typmod application at input
TYPMODS = [(1, 0), (2, 0), (3, 1), (4, 2), (5, -1), (2, -2), (3, -5), (5, 5), (2, 5), (10, 2),
           (18, 6), (38, 10), (1000, 500), (1, -3), (6, 0), (20, 20), (4, 1), (7, 3)]
for _ in range(1500):
    p, s = random.choice(TYPMODS) if random.random() < 0.6 else (random.randint(1, 40), random.randint(-10, 45))
    v = random.choice([rnum(12, 12), rnum(4, 8), random.choice(EDGE + ZEROS + SPECIALS)])
    add("in", v, typmod(p, s))
    add("cast", v, typmod(p, s))
for v in ["0.5", "-0.5", "9.5", "-9.5", "99.95", "99.949", "0.05", "0.0049", "1.23", "123", "150",
          "149", "-150", "999.5", "1e-30"]:
    for p, s in [(1, 0), (2, 0), (3, 1), (5, -1), (2, -2), (1, -2), (3, 5), (2, 5), (4, 3)]:
        add("in", v, typmod(p, s))
        add("cast", v, typmod(p, s))

# ------------------------------------------------------------ typmodin
for mods in ["10,2", "10", "1", "0", "1000", "1001", "-1", "5,-1000", "5,1000", "5,1001",
             "5,-1001", "1,2,3", "38,-5", "1000,1000", "1000,-1000", "3,0", "2147483647"]:
    add("typmod", mods)

# ----------------------------------------------------------- arithmetic
for op in ["add", "sub", "mul"]:
    for _ in range(1500):
        add(op, rval(), rval())
for op in ["add", "sub", "mul", "div", "mod", "divtrunc", "cmp"]:
    for a in SPECIALS + ["0", "1.5", "-2"]:
        for b in SPECIALS + ["0", "0.00", "1.5", "-2"]:
            add(op, a, b)
for _ in range(4000):
    a, b = rval(), rval()
    if random.random() < 0.3:
        a = rnum(60, 40)
        b = rnum(40, 60)
    add("div", a, b)
for _ in range(400):
    a = rdigits(random.randint(1, 6)) + "." + rdigits(random.randint(0, 3), False)
    b = rdigits(random.randint(1, 6))
    add("div", a, b)
    add("div", "1", b)
for _ in range(200):
    add("div", rnum(200, 200), rnum(150, 100))
    add("mul", rnum(150, 150), rnum(150, 150))
for _ in range(1500):
    add("mod", rval(), rval())
for _ in range(600):
    add("divtrunc", rval(), rval())
for _ in range(1500):
    a = rval()
    b = a if random.random() < 0.1 else rval()
    if random.random() < 0.1 and a not in SPECIALS:
        b = a + ("0" if "." in a else ".00")
    add("cmp", a, b)
# result-scale overflow paths
add("mul", "1e-9000", "1e-9000")
add("mul", "0.5e-9000", "1e-8000")
add("add", "9e131071", "9e131071")
add("mul", "1e100000", "1e100000")
add("div", "1e131000", "1e-1000")
add("div", "1", "1e-16383")
add("div", "1e-16383", "7")

# ------------------------------------------------------------ rounding
for op in ["round", "trunc"]:
    for _ in range(1500):
        add(op, rval(), random.randint(-12, 30))
    for v in ["2.5", "-2.5", "0.5", "-0.5", "1.45", "1.55", "9999.5", "99999.99", "0.00049999",
              "555.555", "4999", "5000", "-5000", "12345", "NaN", "Infinity", "-Infinity", "0.000"]:
        for s in [-6, -5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 2000, 2001, -2001, 16383, 16384, -131072, -131073, 2147483647, -2147483648]:
            add(op, v, s)
for op in ["round", "trunc"]:
    for s in [-131071, -131072, -131073, -131074, -131075, -2001, 0]:
        add(op, "9e131071", s)
        add(op, "4e131071", s)
for op in ["ceil", "floor", "abs", "neg", "sign"]:
    for _ in range(400):
        add(op, rval())
    for v in SPECIALS + ZEROS + ["0.1", "-0.1", "1", "-1", "9999.0001", "-9999.0001", "2.000", "-2.000"]:
        add(op, v)

# --------------------------------------------------------------- casts
INT_EDGES = {
    "toi2": [32767, -32768],
    "toi4": [2147483647, -2147483648],
    "toi8": [9223372036854775807, -9223372036854775808],
}
for op, edges in INT_EDGES.items():
    for e in edges:
        for delta in ["", ".4", ".49999", ".5", ".5000001", ".9"]:
            for adj in [-1, 0, 1]:
                v = e + adj
                s = str(v) + delta if v >= 0 else str(v) + delta
                add(op, s)
    for _ in range(1000):
        r = random.random()
        if r < 0.3:
            v = str(random.randint(-(10 ** random.randint(1, 20)), 10 ** random.randint(1, 20)))
            if random.random() < 0.5:
                v += random.choice([".5", ".4999", ".5001", ".0", ".99"])
        else:
            v = rval()
        add(op, v)
for op in ["tof8", "tof4"]:
    for _ in range(1200):
        add(op, rval())
    for v in ["1e308", "1.7976931348623157e308", "1.7976931348623159e308", "1e309", "1e-307",
              "1e-320", "2e-324", "3e-324", "1e-400", "3.4028234663852886e38", "3.4028235e38",
              "3.4028236e38", "3.5e38", "1e39", "1e-38", "1e-45", "7e-46", "1e-46", "1.4e-45",
              "0.1", "0.30000000000000004", "123456789012345678901234567890",
              "0." + "0" * 340 + "1", "9" * 400, "16777217", "9007199254740993",
              "0.5000000000000000555111512312578270211815834045410156250000001"]:
        add(op, v)
        add(op, "-" + v)


def rand_double():
    r = random.random()
    if r < 0.4:
        bits = random.getrandbits(64)
        x = struct.unpack("<d", struct.pack("<Q", bits))[0]
    elif r < 0.6:
        x = random.uniform(-1e6, 1e6)
    elif r < 0.8:
        # exact binary fractions (ties in decimal rounding)
        x = random.randint(-(2 ** 40), 2 ** 40) / (2 ** random.randint(0, 60))
    else:
        x = random.choice([0.1, 0.2, 0.3, 1 / 3, 2 / 3, 1e15, 1e16, 1e-5, 1.5e-5, 123456789012345678.0,
                           0.5, 2.5, 1e22, 1e23, 5e-324, 1.7976931348623157e308, -0.0, 0.0,
                           0.30000000000000004, 1234.5, 100.0, 999999999999999.9, 9.5e-5])
    return x


for _ in range(2500):
    x = rand_double()
    if x != x or x in (float("inf"), float("-inf")):
        continue
    add("fromf8", repr(x))
for s in ["NaN", "Infinity", "-Infinity"]:
    add("fromf8", s)
    add("fromf4", s)
for _ in range(2500):
    r = random.random()
    if r < 0.5:
        bits = random.getrandbits(32)
        x = struct.unpack("<f", struct.pack("<I", bits))[0]
        if x != x or x in (float("inf"), float("-inf")):
            continue
    elif r < 0.75:
        x = random.randint(-(2 ** 20), 2 ** 20) / (2 ** random.randint(0, 30))
    else:
        x = random.uniform(-1e5, 1e5)
    # shortest repr of the float4 value
    f = struct.unpack("<f", struct.pack("<f", x))[0]
    s = repr(f)
    add("fromf4", s)
for v in [0, 1, -1, 9999, 10000, -10000, 2 ** 31, -(2 ** 31), 2 ** 63 - 1, -(2 ** 63), 123456789,
          100000000, 99990000]:
    add("fromi8", v)
for _ in range(200):
    add("fromi8", random.randint(-(2 ** 63), 2 ** 63 - 1))

# ---------------------------------------------------------- binary send
for _ in range(600):
    add("send", rval())
for v in SPECIALS + ZEROS + ["1e131071", "1e-16383", "0.00001", "123456789.123456789"]:
    add("send", v)

# ---------------------------------------------------------------- run
FUNC = r"""
create or replace function ev(op text, a text, b text) returns text language plpgsql as $f$
declare r text; st text; msg text; det text;
begin
  case op
    when 'in' then r := numeric_in(a::cstring, 0, b::int)::text;
    when 'cast' then r := pg_catalog."numeric"(a::numeric, b::int)::text;
    when 'typmod' then r := numerictypmodin(string_to_array(a, ',')::cstring[])::text;
    when 'add' then r := (a::numeric + b::numeric)::text;
    when 'sub' then r := (a::numeric - b::numeric)::text;
    when 'mul' then r := (a::numeric * b::numeric)::text;
    when 'div' then r := (a::numeric / b::numeric)::text;
    when 'mod' then r := (a::numeric % b::numeric)::text;
    when 'divtrunc' then r := div(a::numeric, b::numeric)::text;
    when 'cmp' then r := numeric_cmp(a::numeric, b::numeric)::text;
    when 'round' then r := round(a::numeric, b::int)::text;
    when 'trunc' then r := trunc(a::numeric, b::int)::text;
    when 'ceil' then r := ceil(a::numeric)::text;
    when 'floor' then r := floor(a::numeric)::text;
    when 'abs' then r := abs(a::numeric)::text;
    when 'neg' then r := (- a::numeric)::text;
    when 'sign' then r := sign(a::numeric)::text;
    when 'toi2' then r := a::numeric::int2::text;
    when 'toi4' then r := a::numeric::int4::text;
    when 'toi8' then r := a::numeric::int8::text;
    when 'tof4' then r := a::numeric::float4::text;
    when 'tof8' then r := a::numeric::float8::text;
    when 'fromf4' then r := a::float4::numeric::text;
    when 'fromf8' then r := a::float8::numeric::text;
    when 'fromi8' then r := a::int8::numeric::text;
    when 'send' then r := encode(numeric_send(a::numeric), 'hex');
  end case;
  return 'OK:' || coalesce(r, 'NULL');
exception when others then
  get stacked diagnostics st = returned_sqlstate, msg = message_text, det = pg_exception_detail;
  return 'ERR:' || st || ':' || msg || coalesce(':' || nullif(det, ''), '');
end
$f$;
"""


def copy_escape(s):
    return (s.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
            .replace("\r", "\\r").replace("\x0b", "\\v").replace("\x0c", "\\f"))


# NUL cannot be sent to PostgreSQL text; drop such cases.
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


# Very long results are stored as length + FNV-1a 64 hash of the text.
rows = []
for line in out.stdout.decode().splitlines():
    op, a, b, res = line.split("\t")
    if len(res) > 100 and res.startswith("OK:"):
        body = res[3:]
        res = f"OKH:{len(body)}:{fnv64(body):016x}"
    rows.append("\t".join((op, a, b, res)) + "\n")
lines = "".join(rows)
with open(os.path.join(HERE, "pg17_corpus.tsv"), "w", encoding="utf-8") as f:
    f.write(lines)
print(f"{len(lines.splitlines())} cases written")
