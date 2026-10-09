#!/usr/bin/env perl
# Generate the PostgreSQL 17 differential corpus for the `time` type of
# yuzhu-datetime (kinds time_*, tests/fixtures/pg17_time_corpus.tsv).
#
# Usage:  perl gen_time_corpus.pl <host> <port>
#
# Needs a PostgreSQL 17 server reachable as user postgres with `psql` on
# PATH. Each output line is
#     kind TAB TimeZone TAB DateStyle TAB IntervalStyle TAB a TAB b TAB expected
# where expected is the text result, NULL, or `ERROR <sqlstate> <message>`.
use strict;
use warnings;
use File::Basename qw(dirname);
use IPC::Open3;
use Symbol qw(gensym);

my ($host, $port) = @ARGV;
die "usage: perl gen_time_corpus.pl <host> <port>\n" unless defined $port;
srand(20261007);

my %T = (
    time_in        => 'SELECT %1$L::time::text',
    time_typmod    => 'SELECT %1$L::time(%2$s)::text',
    time_pl_iv     => 'SELECT (%1$L::time + %2$L::interval)::text',
    time_mi_iv     => 'SELECT (%1$L::time - %2$L::interval)::text',
    time_mi_time   => 'SELECT (%1$L::time - %2$L::time)::text',
    time_to_iv     => 'SELECT %1$L::time::interval::text',
    iv_to_time     => 'SELECT %1$L::interval::time::text',
    time_cmp       => 'SELECT CASE WHEN %1$L::time < %2$L::time THEN -1 WHEN %1$L::time = %2$L::time THEN 0 ELSE 1 END::text',
    make_time      => 'SELECT make_time(%1$s)::text',
    ts_to_time     => 'SELECT %1$L::timestamp::time::text',
    tstz_to_time   => 'SELECT %1$L::timestamptz::time::text',
    date_pl_time   => 'SELECT (%1$L::date + %2$L::time)::text',
    extract_time   => 'SELECT extract(%2$L from %1$L::time)::text',
    part_time      => 'SELECT date_part(%2$L, %1$L::time)::text',
    time_roundtrip => 'SELECT %1$L::time::text::time::text',
);

my @cases;
sub add {
    my ($kind, $a, $b, $tz, $ds, $ist) = @_;
    $b //= ''; $tz //= 'UTC'; $ds //= 'ISO, MDY'; $ist //= 'postgres';
    return if $a =~ /\bnow\b/i;
    for ($a, $b) { die "bad char in $_" if /[\t\n\\]/ }
    push @cases, [$kind, $tz, $ds, $ist, $a, $b];
}
sub ri { my ($lo, $hi) = @_; return $lo + int(rand($hi - $lo + 1)) }
sub pick { return $_[ri(0, $#_)] }
sub frac {
    return '' if rand() < 0.4;
    return '.' . join('', map { ri(0, 9) } 1 .. ri(1, 9));
}
sub rand_time {
    my $h = ri(0, 23); my $m = ri(0, 59); my $s = ri(0, 59);
    my $st = ri(0, 9);
    return sprintf('%02d:%02d', $h, $m) if $st == 0;
    return sprintf('%02d%02d%02d', $h, $m, $s) if $st == 1;
    return sprintf('%d:%02d:%02d%s PM', ri(1, 12), $m, $s, frac()) if $st == 2;
    return sprintf('%d:%02d AM', ri(1, 12), $m) if $st == 3;
    return sprintf('%02d%02d', $h, $m) if $st == 4;
    return sprintf('%02d%02d%02d%s', $h, $m, $s, frac()) if $st == 5;
    return sprintf('%02dh%02dm%02ds', $h, $m, $s) if $st == 6;
    return sprintf('%02d:%02d:%02d%s', $h, $m, $s, frac());
}
my @TZS = ('', '', '', 'Z', '+09', '-05', '+05:30', '-03:30', '+0530', ' UTC', ' PST', ' PDT',
           ' JST', ' America/New_York', ' Asia/Tokyo', '+14', '-15:59', ' EST', ' CEST');
sub rand_time_text {
    my $t = rand_time();
    my $r = rand();
    my $s = $t;
    if ($r < 0.12) { $s = sprintf('%04d-%02d-%02d %s', ri(1900, 2100), ri(1, 12), ri(1, 28), $t) }
    elsif ($r < 0.18) { $s = sprintf('%04d-%02d-%02dT%s', ri(1900, 2100), ri(1, 12), ri(1, 28), $t) }
    elsif ($r < 0.22) { $s = sprintf('%d/%d/%d %s', ri(1, 12), ri(1, 28), ri(1900, 2100), $t) }
    elsif ($r < 0.25) { $s = 'T' . $t }
    elsif ($r < 0.28) { $s = sprintf('%s %d-%s-%d', $t, ri(1, 28), pick(qw(Jan Feb Mar Aug Dec)), ri(1900, 2100)) }
    return $s . pick(@TZS) if rand() < 0.4;
    return $s;
}
sub rand_iv {
    my @p;
    for my $u (['year', 'years'], ['mon', 'mons'], ['day', 'days'], ['hour', 'hours'],
               ['min', 'mins'], ['sec', 'secs']) {
        next if rand() < 0.5;
        my $v = ri(-30, 30);
        $v = ri(-100000, 100000) if rand() < 0.15;
        push @p, "$v $u->[0]";
    }
    push @p, sprintf('%02d:%02d:%02d%s', ri(0, 99), ri(0, 59), ri(0, 59), frac()) if rand() < 0.3;
    return @p ? join(' ', @p) : '1 hour';
}

my @FIXED = (
    '04:05:06.789', '04:05:06', '04:05', '4:05', '040506', '04:05:06.789-8', '04:05:06-08:00',
    '04:05-08:00', '04:05:06 PST', '2003-04-12 04:05:06 America/New_York', '04:05 AM', '04:05 PM',
    '12:00 AM', '12:00 PM', '12:30:45 AM', '12:30:45 PM', '00:00 AM', '00:00 PM', '13:00 PM',
    '0:00 AM', '1:00 PM', '11:59:59.999999 PM', '24:00', '24:00:00', '24:00:00.000000',
    '24:00:00.1', '24:00:01', '24:01', '25:00', '23:59:59', '23:59:60', '23:59:60.5', '23:59:61',
    '00:00:00', '00:00:00.000001', '00:60', '00:59:59.9999995', '23:59:59.9999995',
    '23:59:59.9999994', 'allballs', 'ALLBALLS', 'now', 'today', 'tomorrow', 'yesterday', 'epoch',
    'infinity', '-infinity', 'zulu', 'z', 'T04:05:06', 'T0405', 't040506', '04:05:06Z',
    '04:05:06 z', '04:05:06 zulu', '1:2:3', '1:2', '12:3', '1.5', '12', '123', '1234', '12345',
    '123456', '1234567', '12345.5', '123456.789', '12:34:56.789012345', '12:34:56.', '12:34.5',
    '12:34:56:78', '12:', ':12', '12::', '', ' ', 'garbage', '12:34 garbage', '12:34 UTC UTC',
    '12:34:56 +09', '12:34:56+09:30:15', '12:34:56 -2147483648', '12:34 EDT', '12:34 DST',
    '12:34 MET DST', '12:34 PST DST', '12:34 America/New_York', '12:34 Asia/Tokyo', '12:34 foo/bar',
    '12:34 +15:00', '12:34 -15:59', '12:34 +16:00', '12:34 +99', '2020-02-30 12:34', '2020-02-29 12:34',
    '2020-13-01 12:34', '2019-02-29 12:34', '2020-01-01', '2020-01-01 12', 'Jan 8 12:34',
    'January 8, 1999 04:05', '04:05 January 8, 1999', '04:05 Jan 8 1999 BC', '04:05 BC',
    '04:05 AD', '04:05 AM PM', '04:05 PM AM', 'AM 04:05', '04:05 AM 05:06', '04:05 05:06',
    '04:05 2020-01-01 05:06', 'J2451187 04:05', '2451187 J 04:05', 'J2451187', 'j2451187.5',
    '04:05:06 J2451187', '2020.100 04:05', '2020-100 04:05', '99999-01-01 04:05',
    '04:05 monday', 'mon 04:05', '04:05 Mon Tue', '04:05 at', '04:05 on', '04:05 t', '04:05T06:07',
    '04:05 isodow', '04:05 doy', '04:05 julian', '04:05 week', '04:05 year', '04:05 j',
    '0405 06', '04 05 06', '04-05-06', '04.05.06', '04/05/06', '4/5/06 7:08', '040506.5', '0405.5',
    '04:05:06.5 ', ' 04:05:06.5', '+04:05', '-04:05', '04:-05', '04:05:-06',
    '04:05:06.-5', '04:05:06.+5', '04:05:06e1', '04:05:06.5e1', '1e2', '04:05:06.1234567',
    '04:05:06.9999995', '04:05:06.0000005', '04:05:06.0000004', '4:5:6.7', '04:5', '4:05:6',
    '00:00:60', '23:59:59.9999999', '23:59:60.0000001', '24:00:00.0000001', '240000', '2400',
    '240000.5', '236000', '006000', '000060', '120000 PM', '1200 AM', '12:00:00.123456789 AM',
    '12:00 a.m.', '12:00 A', '12:00 P', '12:00am', '12:00pm', '12am', '3pm', '3 pm', '15 pm',
    '0 am', '12 am', '13 am', '3:04:05.678 PM EST', '3:04:05.678 pm est',
);
my @IVS = (
    '1 hour', '-1 hour', '24 hours', '-24 hours', '25 hours', '48 hours', '1 day', '1 month',
    '1 year 2 mons 3 days 04:05:06', '-1 day -1 hour', '0', '1 microsecond', '-1 microsecond',
    '23:59:59.999999', '24:00:00', '-24:00:00', '100000 hours', '-100000 hours',
    '2562047788:00:54.775807', '-2562047788:00:54.775808', '9223372036854775807 microseconds',
    'infinity', '-infinity', '30 seconds', '1.5 hours', '0.000001 seconds', '12:34:56.789',
    '-12:34:56.789', '1 day 25:00:00', '2147483647 days', '2147483647 months',
);
my @UNITS = (
    qw(microseconds milliseconds second minute hour day month year quarter week dow doy epoch
       isodow isoyear julian decade century millennium timezone timezone_hour timezone_minute
       now infinity foo SECOND Hour EPOCH MILLISECOND mS usec msec sec min hr),
    '', ' hour', 'hour ', 'us', 'seconds', 'minutes', 'hours', 'days', 'dayofweek',
);

# time_in: fixed, random, token soup.
my @STYLES = (['UTC', 'ISO, MDY'], ['America/New_York', 'ISO, DMY'], ['Asia/Tokyo', 'ISO, YMD'],
              ['Europe/London', 'Postgres, DMY'], ['UTC', 'SQL, MDY'], ['UTC', 'German, DMY']);
for my $s (@FIXED) {
    for my $st (@STYLES) { add('time_in', $s, '', @$st) }
    add('time_roundtrip', $s);
}
for (1 .. 3000) {
    my $s = rand_time_text();
    my $st = pick(@STYLES);
    add('time_in', $s, '', @$st);
    add('time_roundtrip', $s, '', @$st) if rand() < 0.3;
}
my @words = qw(jan february bc ad pm am utc z j t dst est pdt mon day days hour ago at on y m d h
               s mm julian doy week years msk jst Asia/Tokyo america/new_york foo isodow sat
               september P PT W Y allballs now epoch infinity today);
my @chars = split //, '0123456789-/.:+ T,_';
for (1 .. 2500) {
    my @parts;
    for (1 .. ri(1, 6)) {
        my $r = rand();
        if ($r < 0.45) { push @parts, ri(0, 1) ? ri(0, 99) : ri(0, 99999999) }
        elsif ($r < 0.6) { push @parts, sprintf('%02d:%02d', ri(0, 25), ri(0, 61)) }
        elsif ($r < 0.8) { push @parts, pick(@words) }
        else { push @parts, join('', map { pick(@chars) } 1 .. ri(1, 4)) }
    }
    my $sep = pick('', ' ', '-', ':', '/', '.');
    my $s = join($sep, @parts);
    $s =~ s/^\s+|\s+$//g;
    add('time_in', $s, '', 'UTC', pick('ISO, MDY', 'ISO, DMY', 'ISO, YMD'));
}

# Typmods.
for my $s ('04:05:06.789', '04:05:06.5', '04:05:06.4999995', '23:59:59.9999995', '23:59:59.5',
           '24:00:00', '00:00:00.0000005', '12:00:00.123456', '12:00:00.1', '04:05:06.999999') {
    for my $p (0 .. 6) { add('time_typmod', $s, $p) }
}
for (1 .. 300) { add('time_typmod', rand_time() . frac(), ri(0, 6)) }
for my $s ('04:05', 'garbage', '25:00') { add('time_typmod', $s, 2) }

# Arithmetic.
for (1 .. 600) {
    my $t = rand_time() . frac();
    my $iv = rand() < 0.5 ? pick(@IVS) : rand_iv();
    add('time_pl_iv', $t, $iv);
    add('time_mi_iv', $t, $iv);
}
for my $t ('00:00', '12:00', '23:59:59.999999', '24:00:00') {
    for my $iv (@IVS) { add('time_pl_iv', $t, $iv); add('time_mi_iv', $t, $iv) }
}
for (1 .. 400) { add('time_mi_time', rand_time() . frac(), rand_time() . frac()) }
for my $x ('00:00', '24:00', '12:00') { for my $y ('00:00', '24:00', '12:00') { add('time_mi_time', $x, $y) } }
for (1 .. 150) { add('time_cmp', rand_time(), rand_time()) }
for my $x ('00:00', '24:00', '23:59:59.999999') { for my $y ('00:00', '24:00', '23:59:59.999999') { add('time_cmp', $x, $y) } }
for my $st ('postgres', 'postgres_verbose', 'sql_standard', 'iso_8601') {
    for my $t ('00:00', '04:05:06.789', '24:00:00', '23:59:59.999999') {
        add('time_to_iv', $t, '', 'UTC', 'ISO, MDY', $st);
    }
}
for (1 .. 300) { add('iv_to_time', rand() < 0.5 ? pick(@IVS) : rand_iv()) }
for my $iv (@IVS) { add('iv_to_time', $iv) }

# make_time.
for my $a ('8, 15, 23.5', '0, 0, 0', '24, 0, 0', '24, 0, 0.5', '24, 1, 0', '23, 59, 60', '23, 59, 59.9999999',
           '23, 59, 60.5', '23, 60, 0', '-1, 0, 0', '0, -1, 0', '0, 0, -0.5', '25, 0, 0', '0, 0, 61',
           '8, 15, 0.0000005', '8, 15, 0.0000015', '8, 15, 1e-7', '8, 15, 5', '8, 15, 5.25', '8, 15, 10.5',
           '8, 15, 100000', '8, 15, 1234567', '8, 15, 12345678', '8, 15, 0.1234567', '8, 15, 1e10',
           "8, 15, 'NaN'::float8", "8, 15, 'Infinity'::float8", "8, 15, '-Infinity'::float8", '2147483647, 0, 0',
           '-2147483648, 0, 0', '8, 15, -0.0', '8, 15, 0.000001', '0, 0, 59.9999995', '1, 2, 3.4567891') {
    add('make_time', $a);
}
for (1 .. 200) {
    add('make_time', sprintf('%d, %d, %s', ri(-2, 26), ri(-2, 61), ri(-3, 62) . frac()));
}

# Conversions.
for (1 .. 150) {
    my $ts = sprintf('%04d-%02d-%02d %s', ri(1, 9999), ri(1, 12), ri(1, 28), rand_time() . frac());
    add('ts_to_time', $ts);
    add('tstz_to_time', $ts, '', pick('UTC', 'America/New_York', 'Asia/Tokyo', 'Australia/Sydney', '+05:30', 'EST5EDT'));
    add('tstz_to_time', $ts . '+00', '', pick('UTC', 'America/New_York', 'Asia/Tokyo', 'Europe/London'));
}
for my $s ('infinity', '-infinity', '294276-12-31 23:59:59.999999', '4714-11-24 00:00:00 BC',
           '2024-03-10 02:30:00', '2024-11-03 01:30:00', '2024-01-01 00:00:00') {
    add('ts_to_time', $s);
    add('tstz_to_time', $s, '', 'America/New_York');
    add('tstz_to_time', $s, '', 'UTC');
}
for (1 .. 200) {
    my $d = sprintf('%04d-%02d-%02d', ri(1, 9999), ri(1, 12), ri(1, 28));
    add('date_pl_time', $d, rand_time() . frac());
}
for my $d ('infinity', '-infinity', '5874897-12-31', '294276-12-31', '294277-01-01', '4714-11-24 BC', '2024-01-01') {
    for my $t ('00:00', '24:00', '12:00:00.5', '23:59:59.999999') { add('date_pl_time', $d, $t) }
}

# extract / date_part.
for my $t ('00:00:00', '04:05:06.789', '12:30:45.123456', '23:59:59.999999', '24:00:00',
           '00:00:00.000001', '01:02:03.4') {
    for my $u (@UNITS) { add('extract_time', $t, $u); add('part_time', $t, $u) }
}
for (1 .. 80) {
    my $t = rand_time() . frac();
    my $u = pick(qw(microseconds milliseconds second minute hour epoch));
    add('extract_time', $t, $u);
    add('part_time', $t, $u);
}

# Evaluate.
sub esc { my $s = shift; $s =~ s/\\/\\\\/g; $s =~ s/\t/\\t/g; $s =~ s/\n/\\n/g; return $s }
my $i = 0;
my $copy = join("\n", map {
    my ($k, $tz, $ds, $ist, $a, $b) = @$_;
    join("\t", $i++, $k, $T{$k}, $tz, $ds, $ist, esc($a), esc($b));
} @cases);
my $script = <<'SQL' . $copy . "\n\\.\n" . <<'SQL2';
\set ON_ERROR_STOP 1
SET client_min_messages = warning;
CREATE OR REPLACE FUNCTION pg_temp.yz_eval(tz text, ds text, ist text, tmpl text, a text, b text)
RETURNS text LANGUAGE plpgsql AS $f$
DECLARE r text;
BEGIN
  PERFORM set_config('TimeZone', tz, false);
  PERFORM set_config('DateStyle', ds, false);
  PERFORM set_config('IntervalStyle', ist, false);
  EXECUTE format(tmpl, a, b) INTO r;
  RETURN coalesce(r, 'NULL');
EXCEPTION WHEN others THEN
  RETURN 'ERROR ' || SQLSTATE || ' ' || SQLERRM;
END
$f$;
CREATE TEMP TABLE cases(id int, kind text, tmpl text, tz text, ds text, ist text, a text, b text);
COPY cases FROM STDIN;
SQL
COPY (SELECT id, pg_temp.yz_eval(tz, ds, ist, tmpl, a, b) FROM cases ORDER BY id) TO STDOUT;
SQL2
my $err = gensym;
my $pid = open3(my $in, my $out, $err, 'psql', '-h', $host, '-p', $port, '-U', 'postgres', '-X', '-q', '-At');
my $pid2 = fork();
if (!$pid2) { print $in $script; close $in; exit 0 }
close $in;
my %res;
while (my $l = <$out>) {
    chomp $l;
    next unless length $l;
    my ($idx, $r) = split /\t/, $l, 2;
    $res{$idx} = $r;
}
waitpid($pid, 0); waitpid($pid2, 0);
die "psql failed: " . join('', <$err>) unless keys %res == @cases;
my $path = dirname($0) . '/pg17_time_corpus.tsv';
open my $fh, '>', $path or die $!;
print $fh "# Generated by gen_time_corpus.pl against PostgreSQL 17.\n";
$i = 0;
for my $c (@cases) {
    print $fh join("\t", @$c, $res{$i++}), "\n";
}
close $fh;
print "wrote ", scalar(@cases), " cases\n";
