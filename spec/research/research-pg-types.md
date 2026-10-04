# yuzhu 型システム調査: PostgreSQL の型・キャスト・演算子解決・SQLSTATE

調査日: 2026-10-04
一次資料（REL_17_STABLE ブランチを raw.githubusercontent.com から取得して確認したもの）:

- `src/include/catalog/pg_type.dat`, `pg_cast.dat`, `pg_operator.dat`, `pg_proc.dat`, `pg_proc.h`
- `src/backend/parser/parse_oper.c`（`binary_oper_exact` L262, `oper_select_candidate` L312, `oper` L370）
- `src/backend/parser/parse_func.c`（`func_match_argtypes` L978, `func_select_candidate` L1063, `func_get_detail` L1450）
- `src/backend/parser/parse_coerce.c`（`coerce_to_target_type` L78, `coerce_type` L157, `can_coerce_type` L556, `coerce_type_typmod` L757, `select_common_type` L1348, `TypeCategory` L2982, `IsPreferredType` L3001, `find_coercion_pathway` L3159）
- `src/backend/parser/parse_node.c`（`make_const`）
- `src/backend/utils/adt/numeric.c`（`make_numeric_typmod`）
- `src/backend/utils/errcodes.txt`
- 公式ドキュメント: 第10章「Type Conversion」(https://www.postgresql.org/docs/current/typeconv.html)、§10.2 Operators、§10.3 Functions、§10.4 Value Storage、§10.5 UNION/CASE and Related Constructs、§53 System Catalogs（pg_type / pg_cast / pg_operator / pg_proc）、Appendix A「PostgreSQL Error Codes」

> 注: 下表の値は `.dat` から機械的に抜き出したもの。行番号は REL_17_STABLE 時点。

---

## 1. 初期型の OID / typlen / typbyval / typcategory / typispreferred

出典: `pg_type.dat`

| 型 | OID | 配列型 OID | typlen | typbyval | typcategory | typispreferred | typalign | typmodin | typcollation |
|---|---|---|---|---|---|---|---|---|---|
| bool | 16 | 1000 | 1 | t | B | **t** | c | - | - |
| bytea（参考） | 17 | 1001 | -1 | f | U | f | i | - | - |
| "char"（参考） | 18 | 1002 | 1 | t | Z | f | c | - | - |
| name | 19 | 1003 | NAMEDATALEN(=64) | f | S | f | c | - | C |
| int8 | 20 | 1016 | 8 | FLOAT8PASSBYVAL（64bit ビルドでは t） | N | f | d | - | - |
| int2 | 21 | 1005 | 2 | t | N | f | s | - | - |
| int4 | 23 | 1007 | 4 | t | N | f | i | - | - |
| text | 25 | 1009 | -1 | f | S | **t** | i | - | default |
| oid | 26 | 1028 | 4 | t | N | **t** | i | - | - |
| float4 | 700 | 1021 | 4 | t | N | f | i | - | - |
| float8 | 701 | 1022 | 8 | FLOAT8PASSBYVAL | N | **t** | d | - | - |
| unknown | 705 | なし | **-2**（cstring 形式） | f | X | f | c | - | - |
| bpchar | 1042 | 1014 | -1 | f | S | f | i | bpchartypmodin | default |
| varchar | 1043 | 1015 | -1 | f | S | f | i | varchartypmodin | default |
| date | 1082 | 1182 | 4 | t | D | f | i | - | - |
| time（参考） | 1083 | 1183 | 8 | FLOAT8PASSBYVAL | D | f | d | timetypmodin | - |
| timestamp | 1114 | 1115 | 8 | FLOAT8PASSBYVAL | D | f | d | timestamptypmodin | - |
| timestamptz | 1184 | 1185 | 8 | FLOAT8PASSBYVAL | D | **t** | d | timestamptztypmodin | - |
| interval（参考） | 1186 | 1187 | 16 | f | T | **t** | d | intervaltypmodin | - |
| numeric | 1700 | 1231 | -1 | f | N | f | i | numerictypmodin | - |
| regclass | 2205 | 2210 | 4 | t | N | f | i | - | - |
| regtype | 2206 | 2211 | 4 | t | N | f | i | - | - |
| void | 2278 | なし | 4 | t | P | f | i | - | - |

重要な観察:

- **N（数値）カテゴリの preferred は float8 だけでなく oid も t**。実装時に見落としやすい。
- S（文字列）カテゴリの preferred は text。D は timestamptz、T は interval、B は bool。
- unknown は typlen = -2（NUL 終端文字列）で、カテゴリは X（unknown）。
- regclass / regtype は oid と同じ 4 byte 値で、カテゴリは N。
- typcategory の文字一覧（docs §53 pg_type 表 53.65）: A=array, B=boolean, C=composite, D=date/time, E=enum, G=geometric, I=network, N=numeric, P=pseudo, R=range, S=string, T=timespan, U=user-defined, V=bit-string, X=unknown, Z=internal。
- ユーザー定義オブジェクトの OID は `FirstNormalObjectId = 16384` から始まる（`access/transam.h`）。yuzhu でも 16384 未満は組み込み用に予約しておくべき。

### typmod の意味

typmod は int32 で、**-1 は「指定なし」**。エンコードは型ごとに異なる。

| 型 | SQL 表記 | typmod | 根拠 |
|---|---|---|---|
| varchar(n) | `varchar(10)` | `n + VARHDRSZ` = n + 4（例: 14） | varchartypmodin |
| bpchar(n) | `char(10)` | n + 4。DDL で長さを省略した `char` は char(1) = 5。`bpchar` と書いた場合は -1（長さ無制限） | bpchartypmodin / gram.y |
| numeric(p,s) | `numeric(10,2)` | `((p << 16) \| (s & 0x7ff)) + 4`。例: (10<<16 \| 2) + 4 = 655366。p は 1..1000。PG15 以降の s は -1000..1000（11bit で負数も表現する） | numeric.c `make_numeric_typmod` |
| numeric(p) | | s = 0 として上と同じ式 | |
| timestamp(p) / timestamptz(p) / time(p) | | **p そのもの**（VARHDRSZ のオフセットは付かない）。0..6。省略時は -1（マイクロ秒精度） | timestamptypmodin |
| 型名（`format_type`） | | `character varying(10)`, `numeric(10,2)`, `timestamp(3) without time zone` | format_type.c |

typmod を強制するのは「長さ強制キャスト（sizing cast）」で、pg_cast に**同じ型から同じ型への**エントリとして登録されている（後述）。

---

## 2. カタログの構造と、最小実装で必要な列

### 2.1 pg_type

PG17 の全列: `oid, typname, typnamespace, typowner, typlen, typbyval, typtype, typcategory, typispreferred, typisdefined, typdelim, typrelid, typsubscript, typelem, typarray, typinput, typoutput, typreceive, typsend, typmodin, typmodout, typanalyze, typalign, typstorage, typnotnull, typbasetype, typtypmod, typndims, typcollation, typdefaultbin, typdefault, typacl`

最小構成（M2 で SQL から問い合わせられるようにする列）:

- 必須: `oid, typname, typnamespace(=11 pg_catalog), typlen, typbyval, typtype('b' base / 'p' pseudo), typcategory, typispreferred, typelem, typarray, typinput, typoutput, typmodin, typmodout, typalign, typcollation`
- ほぼ固定値でよい（psql / ドライバ互換のため列だけは用意する）: `typowner(10), typisdefined(t), typdelim(','), typrelid(0), typstorage, typnotnull(f), typbasetype(0), typtypmod(-1), typndims(0), typdefault(NULL), typreceive, typsend`
- JDBC / psycopg / pgx などのクライアントは起動時に `pg_type` の `oid, typname, typtype, typelem, typarray, typbasetype, typrelid, typdelim` を参照することが多い。

### 2.2 pg_cast

列: `oid, castsource, casttarget, castfunc, castcontext, castmethod`

- `castcontext`: `'e'` は明示キャストのみ（`CAST(x AS t)` / `x::t`）、`'a'` は代入（INSERT/UPDATE の代入、および明示）、`'i'` は暗黙（式中どこでも）。`find_coercion_pathway` は enum の大小 `COERCION_IMPLICIT < COERCION_ASSIGNMENT < COERCION_EXPLICIT` を比べ、`ccontext >= castcontext` なら使ってよいと判定する。
- `castmethod`: `'f'` は castfunc を呼ぶ、`'b'` はバイナリ互換（RelabelType で再ラベルするだけ）、`'i'` は I/O 変換（出力関数のあと入力関数を通す）。
- **pg_cast にエントリが無い場合の自動 I/O キャスト**（`find_coercion_pathway` の末尾）: 変換先が文字列カテゴリ（S）なら assignment 以上で CoerceViaIO を許し、変換元が S なら explicit で CoerceViaIO を許す。つまり `int4 -> text` は pg_cast に無いが代入なら通り、`text -> int4` は明示キャストでしか通らない。
- 同じ型同士なら常に RELABEL 扱い（domain は基底型に落としてから比較する）。

対象型の間にある pg_cast エントリ（`pg_cast.dat` から抽出）:

```
-- 整数・浮動小数・numeric
int2->int4 i f   int2->int8 i f   int2->float4 i f  int2->float8 i f  int2->numeric i f
int4->int8 i f   int4->int2 a f   int4->float4 i f  int4->float8 i f  int4->numeric i f
int8->int2 a f   int8->int4 a f   int8->float4 i f  int8->float8 i f  int8->numeric i f
float4->int2/int4/int8 a f   float4->float8 i f   float4->numeric a f
float8->int2/int4/int8 a f   float8->float4 a f   float8->numeric a f
numeric->int2/int4/int8 a f  numeric->float4 i f  numeric->float8 i f
-- bool
int4->bool e f   bool->int4 e f   bool->text/varchar/bpchar a f
-- oid / reg*
int2->oid i f    int4->oid i b    int8->oid i f    oid->int4 a b    oid->int8 a f
oid<->regclass i b   oid<->regtype i b   int4->regclass/regtype i b  int8->regclass/regtype i f
regclass/regtype->int4 a b   regclass/regtype->int8 a f
text->regclass i f   varchar->regclass i f
-- 文字列
text->varchar i b   text->bpchar i b   varchar->text i b   varchar->bpchar i b
bpchar->text i f (rtrim)   bpchar->varchar i f
text/bpchar/varchar->name i f   name->text i f   name->varchar a f   name->bpchar a f
-- 日時
date->timestamp i f   date->timestamptz i f   timestamp->timestamptz i f
timestamp->date a f   timestamptz->date a f   timestamptz->timestamp a f
-- 長さ強制（sizing cast、typmod を適用する）
bpchar->bpchar i f bpchar(bpchar,int4,bool)
varchar->varchar i f varchar(varchar,int4,bool)
numeric->numeric i f numeric(numeric,int4)
timestamp->timestamp i f timestamp(timestamp,int4)
timestamptz->timestamptz i f timestamptz(timestamptz,int4)
```

整数は暗黙に拡大し、縮小は代入キャスト、数値から文字列は代入キャスト（I/O 経由）、文字列から数値は明示キャストのみ、という原則が表からわかる。**int と bool の間は明示キャストのみ**。

### 2.3 pg_operator

列: `oid, oprname, oprnamespace, oprowner, oprkind('b' 二項 / 'l' 前置), oprcanmerge, oprcanhash, oprleft, oprright, oprresult, oprcom, oprnegate, oprcode(regproc), oprrest, oprjoin`

- 最小構成: `oid, oprname, oprnamespace, oprkind, oprleft(前置演算子では 0), oprright, oprresult, oprcode, oprcom, oprnegate`。`oprcanhash` / `oprcanmerge` はハッシュ結合やマージ結合を実装する段階（M3〜M4）で使う。`oprrest` / `oprjoin` は選択率推定用なので後回しでよい（列だけ置いて 0 を入れる）。
- **後置演算子は PG14 で廃止**されたので、oprkind は 'b' と 'l' の二種類だけ実装すればよい。
- 代表的な OID: `int4 = int4`→96 (int4eq), `int4 < int4`→97, `int4 + int4`→551 (int4pl), `int4 - int4`→555, `int4 * int4`→514, `int4 / int4`→528, `int4 % int4`→530, `int8 = int8`→410, `text = text`→98, `text || text`→654, `numeric + numeric`→1758, `float8 + float8`→591, `bool = bool`→91, `date = date`→1093。
- **整数は異なる幅の組み合わせごとに演算子が用意されている**（int24pl 552, int48pl 692, int84eq 416, int28eq 1862 など）。一方で **numeric と整数の混合演算子は無く**、float4/float8 の混合（float48pl など）はある。
- **varchar の演算子は存在しない**。`varchar = varchar` は binary-coercible な `text = text` に解決される（§3 の例を参照）。bpchar には専用の比較演算子（bpchareq など。末尾空白を無視して比較する）がある。
- `||` は `text || text`(654) のほか、`text || anynonarray`(2779) と `anynonarray || text`(2780) がある。`'a' || 1` が動くのは後者のおかげ。

### 2.4 pg_proc

列（`pg_proc.h`）: `oid, proname, pronamespace, proowner, prolang, procost, prorows, provariadic, prosupport, prokind('f'/'a'/'w'/'p'), prosecdef, proleakproof, proisstrict, proretset, provolatile('i'/'s'/'v'), proparallel, pronargs, pronargdefaults, prorettype, proargtypes(oidvector), proallargtypes, proargmodes, proargnames, proargdefaults, protrftypes, prosrc, probin, prosqlbody, proconfig, proacl`

- 最小構成: `oid, proname, pronamespace, prokind, proisstrict, proretset, provolatile, pronargs, prorettype, proargtypes, prosrc`。prosrc には組み込み関数の識別子（Rust 側の関数テーブルのキー）を入れる。
- `proisstrict = t` は「引数に NULL が一つでもあれば呼び出さずに NULL を返す」という意味。組み込み関数の大半は strict。
- `provolatile` は定数畳み込みとインデックス利用の可否を決める。`now()` は s（stable）、`random()` は v（volatile）。
- 代表的な OID: int4in 42, int4out 43, int4eq 65, int4lt 66, int4pl 177, textcat 1258, length(text) 1317, length(bpchar) 1318。

---

## 3. 解決アルゴリズム

### 3.0 共通の基本処理

- `TypeCategory(t)` は pg_type.typcategory、`IsPreferredType(cat, t)` は「t のカテゴリが cat に等しく、かつ typispreferred」（cat が INVALID ならカテゴリは問わない）。
- `can_coerce_type(in[], target[], ctx)` は引数ごとに次を判定する: 同じ型なら OK、target が any なら OK、target が多相型なら保留、**入力が unknown なら OK（何にでも変換可能と仮定する）**、それ以外は `find_coercion_pathway(target, in, ctx) != NONE` なら OK。
- `find_coercion_pathway` は 2.2 で述べたとおり、pg_cast を引き、無ければ配列要素の変換を試し、それも無ければ I/O 変換の規則を適用する。

### 3.1 演算子解決（docs §10.2、`parse_oper.c: oper()`）

入力: 演算子名と (ltype, rtype)。前置演算子では ltype = 0。

1. **候補の収集**: search_path 上で名前と種別（二項 / 前置）が一致する演算子をすべて集める。修飾名 `OPERATOR(schema.op)` なら、そのスキーマだけを見る。
2. **完全一致（`binary_oper_exact`）**:
   - 片方が unknown で、もう片方が既知なら、unknown 側を既知側と同じ型とみなして `(T, T)` を検索する。例: `int4col = '5'` は `int4 = int4` に一致する。
   - `(ltype, rtype)` で完全一致すれば決定。
   - unknown を置き換えた型が domain なら、基底型でも一度試す。
3. **最良一致（`oper_select_candidate`）**:
   - a. `func_match_argtypes`: `can_coerce_type(入力, 候補の引数, IMPLICIT)` を満たさない候補を捨てる（unknown は常に通る）。0 件なら「operator does not exist」(42883)、1 件ならそれで決定。
   - b.〜f. 残りは `func_select_candidate` に任せる（3.2 と共通）。決まらなければ「operator is not unique」(**42725** ambiguous_function)。

`func_select_candidate` の手順（パラメータ化して実装できる粒度で書く）:

```
入力: input[i]（domain は基底型に置換、unknown はそのまま）、候補リスト C
nunknowns = unknown の個数

(c) 完全一致数でふるい分け:
    score(c) = #{ i | input[i] != unknown && c.arg[i] == input[i] }
    最高スコアの候補だけを残す（全候補が 0 点なら全部残す）。1件なら決定。

(d) 完全一致数と preferred 一致数でふるい分け:
    cat[i] = TypeCategory(input[i])
    score(c) = #{ i | input[i] != unknown &&
                     (c.arg[i] == input[i] || IsPreferredType(cat[i], c.arg[i])) }
    最高スコアの候補だけを残す。1件なら決定。
    ※ preferred として数えるのは入力と同じカテゴリの型だけ（7.4 以降）

    nunknowns == 0 ならここで失敗（ambiguous）

(e) unknown 位置のカテゴリを推定する:
    unknown の各位置 i について、残っている候補の arg[i] のカテゴリを調べる:
      - どれか一つでも S（STRING）があれば slotcat = S（文字列を優先する）
      - そうでなく、全候補のカテゴリが一致していればそのカテゴリ
      - それ以外は推定失敗（e の処理全体を飛ばす）
      slot_pref[i] = 候補のうち slotcat かつ preferred な型を取るものが存在するか
    推定に成功したら:
      unknown 位置で arg のカテゴリ != slotcat の候補を捨てる
      slot_pref[i] が真なのに arg[i] が preferred でない候補も捨てる
      （結果が0件になる場合は捨てない）。1件なら決定。

(f) 最後の手段:
    既知の入力型がすべて同じ型 T なら、unknown をすべて T とみなし、
    can_coerce_type(IMPLICIT) を満たす候補がちょうど1件ならそれに決定。

それでも決まらなければ ambiguous。
```

**検算例**（上の pg_cast / pg_operator の表を使う）:

- `int4 + numeric`: 完全一致なし。暗黙変換で通る候補は numeric+numeric、float4+float4、float8+float8 など（numeric→int8 は 'a' なので int8 系は落ちる）。(c) で numeric+numeric だけが右辺で 1 点を取り、**numeric** に決まる。
- `varchar = varchar`: 完全一致なし。暗黙変換可能な候補は text=text、name=name、bpchar=bpchar、text=name、name=text など。(c) では全候補 0 点。(d) では text が S カテゴリの preferred なので text=text だけが 2 点となり、**text = text** に決まる。
- `float8 = numeric`: numeric→float8 が 'i'、float8→numeric が 'a' なので、通る候補は float8=float8 だけになり、**float8 で比較される**（numeric を float8 に落とす）。
- `'1' + '2'`（unknown + unknown）: (c)(d) では 0 点のまま絞れない。(e) では `+` の候補の引数カテゴリが N（数値）、D（date + int4）、T（interval）、G（point）、I（inet）などに分かれ、S を取る候補も無いので推定に失敗する。既知の型も無いので (f) も使えず、**42725 `operator is not unique: unknown + unknown`** になる（PG の実際の挙動と一致する）。一方、`'abc' || 'def'` は S カテゴリの候補があるので (e) で S に決まり、preferred な text を取る `text || text` に解決される（docs §10.2 の例）。
- `'abc' || 1`: 完全一致なし。通る候補は `text || anynonarray`（unknown→text、int4→anynonarray）など。結果は text。
- `text = int4`: 暗黙キャストが無い（int4→text は 'a' 相当の I/O）ので通る候補が無く、**42883** `operator does not exist: text = integer` となり、HINT が付く。

### 3.2 関数呼び出し（docs §10.3、`parse_func.c: func_get_detail`）

1. 名前と引数の数が一致する関数を pg_proc から集める（VARIADIC 展開と DEFAULT 補完も考慮する。最小実装では無視してよい）。
2. **完全一致**: 引数型の配列が完全に一致する候補があれば決定（一つしかありえない）。
3. **関数形式のキャスト**: 引数が一つで、関数名が型名に一致する場合（`FuncNameAsType`）、
   - 引数が unknown 型の Const なら、常にキャストとして扱う（`int4('12')`）。
   - それ以外は `find_coercion_pathway(target, src, EXPLICIT)` が RELABEL か COERCEVIAIO のときだけキャストとして扱う（FUNC なら同名のキャスト関数が通常の検索で見つかるはず）。
4. **最良一致**: `func_match_argtypes`（暗黙変換で通るものだけ残す）を行い、1 件ならそれで決定、複数なら `func_select_candidate`（3.1 と同じ c〜f）を行う。
5. 失敗した場合は、0 件なら **42883** `function f(integer) does not exist`、複数なら **42725** `function f(unknown) is not unique`。
6. 決定した関数の引数型に合わせて、各実引数に暗黙キャストを挿入する（`make_fn_arguments`）。unknown 定数は入力関数で直接その型に変換する。

### 3.3 UNION / CASE / VALUES / ARRAY / COALESCE / GREATEST の共通型（docs §10.5、`select_common_type`）

```
入力: 式のリスト（それぞれの型）
1. 全入力が同じ型で、それが unknown でなければその型（domain もそのまま保持する）。
2. 以降は domain を基底型として扱う。
3. 全入力が unknown なら text。unknown 以外が一つでもあれば unknown は無視する。
4. 非 unknown の入力のカテゴリが一つでも違えば失敗:
   42804 "UNION types integer and text cannot be matched"
   （CASE なら "CASE types ..."、VALUES なら "VALUES types ..."）
5. 最初の非 unknown 型を候補 P とする。残りの非 unknown 型 N を左から順に見て、
     P が preferred でなく、
     かつ P→N が暗黙変換可能、
     かつ N→P が暗黙変換不能
   の三条件を満たすなら P := N とする。
6. 全入力を P へ暗黙変換する。できなければ 42846 "... could not convert type X to Y"。
   （PG12 以降は verify_common_type で事前に検査する経路もある）
```

例: `SELECT 1 UNION SELECT 2.5` は int4→numeric が暗黙で、逆は代入のみなので numeric になる。`CASE WHEN c THEN 1 ELSE 2.0::float8 END` は float8 になる。`VALUES (1),('x')` は 'x' が unknown なので int4 に決まり、'x' の int4 入力で **22P02** になる。

補足:

- **UNION / INTERSECT / EXCEPT は二項ずつ左から順に解決する**。`SELECT NULL UNION SELECT NULL UNION SELECT 1` は、最初の対で text に決まった後に int4 と組み合わされるので失敗する（docs §10.5 の例）。VALUES は一列ごとに全行を一度に解決する。
- CASE は THEN/ELSE の全分岐をまとめて解決する。COALESCE / GREATEST / LEAST / ARRAY[...] も同じアルゴリズムを使う。
- `IN (list)` は、まず全要素で共通型を試し、だめなら `=` を要素ごとに解決する。

### 3.4 値の格納（INSERT/UPDATE、docs §10.4、`transformAssignedExpr` → `coerce_to_target_type(..., COERCION_ASSIGNMENT, ...)`）

1. 式の型が列の型と完全一致すれば、型変換はしない。
2. 一致しなければ **COERCION_ASSIGNMENT** で変換を試みる。使えるのは pg_cast の 'i' と 'a'、文字列カテゴリへの I/O 変換、そして unknown 定数（列型の入力関数で直接パースする。text を経由しない）。
   - 変換できなければ **42804** `column "c" is of type integer but expression is of type text`。HINT は「You will need to rewrite or cast the expression.」
3. **長さ強制**: 列の typmod が -1 でなく、かつ（式の typmod が異なるか、型変換が入った）場合に sizing cast（同じ型→同じ型の pg_cast エントリ）を `isExplicit = false` で適用する（`coerce_type_typmod`）。
   - varchar(n)/char(n) に入れる値が長すぎるとエラー **22001** `value too long for type character varying(n)`。ただし超過分が空白だけなら切り詰めて受け入れる。**明示キャスト `'abcdef'::varchar(3)` はエラーにせず黙って切り詰める**（isExplicit = true のため）。
   - numeric(p,s) は s 桁に丸めたうえで整数部が p-s 桁を超えると **22003** `numeric field overflow`。
   - timestamp(p) は秒の小数部を p 桁に丸める。
4. 代入キャストで起こる典型的なエラー: `INSERT INTO t(i int4) VALUES (3000000000)` は int8→int4 の代入キャストで **22003** `integer out of range`。`VALUES ('abc')` は int4 入力関数で **22P02**。

### 3.5 その他の文脈

- `WHERE` / `HAVING` / `JOIN ON` / `CHECK` は bool を要求する。unknown なら bool に強制し、それ以外の型は暗黙変換で bool にする（`coerce_to_boolean`）。できなければ **42804** `argument of WHERE must be type boolean, not type integer`。
- `LIMIT` / `OFFSET` は int8 に強制する。
- **SELECT の出力列に残った unknown は text に解決する**（PG10 以降の `resolveTargetListUnknowns`）。`SELECT 'a'` の列型は text。
- 明示キャスト `x::t` / `CAST(x AS t)` は COERCION_EXPLICIT で解決し、型変換のあと typmod を isExplicit = true で適用する。不可能なら **42846** `cannot cast type X to Y`。

---

## 4. リテラルの型付け（`gram.y` / `scan.l` / `parse_node.c: make_const`）

| リテラル | 字句 | 型 |
|---|---|---|
| `'123'`, `'abc'`, `E'..'`, `$$..$$` | SCONST | **unknown**（705、typmod -1）。使われる文脈で型が決まる |
| `123` | ICONST（int32 に収まる整数） | **int4** |
| `3000000000` | 整数だが int32 に収まらないため、scanner が FCONST として扱う | make_const が int64 として解析できれば **int8** |
| `99999999999999999999` | FCONST | int64 にも収まらないので **numeric** |
| `1.5`, `1e10`, `.5` | FCONST | **numeric**（float8 ではない点に注意） |
| `-2147483648` | 単項マイナスは gram.y の `doNegate` で定数に畳み込まれ FCONST "-2147483648" になり、int32 に収まる | **int4** |
| `TRUE` / `FALSE` | キーワード | **bool** |
| `NULL` | | **unknown** 型の Const（constisnull = true）。文脈で型が決まり、最後まで決まらなければ text |
| `B'101'`, `X'1F'` | BCONST/XCONST | bit |
| `0x1F`, `0o17`, `0b101`, `1_000_000` | PG16 以降 | 値に応じて int4 / int8 / numeric |
| `int4 '123'`, `'123'::int4`, `CAST('123' AS int4)` | 型付きリテラル | int4。値は入力関数で即座にパースされ、不正なら 22P02 |

要点:

- 数値リテラルの型は「int4 → int8 → numeric」の順に、値が収まる最小の型になる。**小数リテラルは numeric**。そのため `1.5 + 1` は numeric になり、`1.5 + 1.0::float8` は float8 になる。
- unknown の扱いは型解決の中心にある。演算子の片方が unknown なら他方の型に合わせ（3.1 の手順 2）、関数解決では文字列を優先し（3.1 の e）、共通型解決では無視し（3.3）、代入では列型の入力関数でパースする（3.4）。
- 文字列リテラルを型へ変換するのはパース時（定数のまま）なので、不正値のエラー（22P02）は実行前に出る。

---

## 5. 数値演算の挙動と、M1〜M4 で必要な SQLSTATE

### 5.1 数値演算の挙動

| ケース | 挙動 | SQLSTATE / メッセージ |
|---|---|---|
| int2/int4/int8 の加減乗算のオーバーフロー | エラー（ラップアラウンドはしない）。`int2 + int2` の結果型は int2 なので 32767 を超えた時点でエラー | **22003** `smallint out of range` / `integer out of range` / `bigint out of range` |
| 整数の除算 | 0 方向へ切り捨て（`7 / -2 = -3`）。`%` の符号は被除数に従う | |
| `INT_MIN / -1`, `-INT_MIN`, `abs(INT_MIN)` | エラー | **22003** |
| 整数・numeric・float の 0 除算（`/` と `%`） | エラー（float も Inf を返さない） | **22012** `division by zero` |
| float4/float8 の演算結果のオーバーフローとアンダーフロー | 入力が有限で結果が Inf になればエラー、0 でない入力から 0 になってもエラー | **22003** `value out of range: overflow` / `underflow` |
| float の NaN / Infinity | `'NaN'::float8`, `'Infinity'` は入力として受け付ける。**比較では NaN を最大値とし、NaN = NaN は真**（ソートとインデックスのため） | |
| numeric | 任意精度（小数点以下は最大 16383 桁）。NaN と、PG14 以降は ±Infinity を持つ。除算結果の scale は規則で決まる（最低 16 桁の有効桁数） | 0 除算は 22012 |
| 文字列から整数への変換で書式が不正（`'abc'::int4`, `'1.5'::int4`） | 前後の空白は許す | **22P02** `invalid input syntax for type integer: "abc"` |
| 文字列から整数への変換で範囲外（`'3000000000'::int4`） | | **22003** `value "3000000000" is out of range for type integer` |
| numeric から整数への変換 | **0 から遠い方へ丸める**（`2.5::numeric::int4 = 3`） | 範囲外なら 22003 |
| float から整数への変換 | `rint()` による**偶数丸め**（`2.5::float8::int4 = 2`） | 範囲外・NaN なら 22003 |
| numeric(p,s) への代入時の桁あふれ | | **22003** `numeric field overflow` |
| `sqrt(-1)`, `ln(0)`, `power(0,-1)` | | 2201F / 2201E |

### 5.2 M1〜M4 で必要な SQLSTATE（`errcodes.txt` で確認済み）

**構文・意味解析（クラス 42）**

| コード | 名前 | 主な発生箇所 |
|---|---|---|
| 42601 | syntax_error | パーサ |
| 42P01 | undefined_table | `relation "t" does not exist` |
| 42703 | undefined_column | `column "x" does not exist` |
| 42702 | ambiguous_column | `column reference "id" is ambiguous` |
| 42883 | undefined_function | 演算子・関数が見つからない（`operator does not exist: ...`） |
| 42725 | ambiguous_function | 演算子・関数の候補が複数残って絞れない |
| 42804 | datatype_mismatch | 代入型の不一致、UNION/CASE の型不一致、WHERE が bool でない |
| 42846 | cannot_coerce | `cannot cast type X to Y` |
| 42P18 | indeterminate_datatype | `could not determine data type of parameter $1` |
| 42704 | undefined_object | 型名が見つからない（`type "foo" does not exist`） |
| 42P07 | duplicate_table | CREATE TABLE で名前が重複 |
| 42701 | duplicate_column | 同じ列名の重複（errcodes にあるが上の抽出では省略） |
| 42710 | duplicate_object | 制約やインデックスの重複など |
| 42712 | duplicate_alias | FROM 句での別名の重複 |
| 42803 | grouping_error | GROUP BY に無い列の参照、集約のネスト |
| 42P10 | invalid_column_reference | ORDER BY に存在しない位置番号、DISTINCT の ORDER BY |
| 42611 | invalid_column_definition | 列定義の不正 |
| 42809 | wrong_object_type | テーブルでないものへの操作 |
| 42622 | name_too_long | 識別子が 63 バイトを超える（PG は NOTICE を出して切り詰めるのでエラーではない） |
| 42501 | insufficient_privilege | 権限（後半のマイルストーン） |
| 42P20 | windowing_error | ウィンドウ関数（実装する場合） |

**データ例外（クラス 22）**

| コード | 名前 |
|---|---|
| 22003 | numeric_value_out_of_range |
| 22012 | division_by_zero |
| 22P02 | invalid_text_representation |
| 22001 | string_data_right_truncation |
| 22007 | invalid_datetime_format |
| 22008 | datetime_field_overflow |
| 22023 | invalid_parameter_value |
| 22004 | null_value_not_allowed |
| 22021 | character_not_in_repertoire（不正な UTF-8） |
| 2201E / 2201F | invalid_argument_for_logarithm / power_function |

**制約違反（クラス 23）**: 23502 not_null_violation, 23505 unique_violation, 23514 check_violation, 23503 foreign_key_violation（FK を実装する場合）

**トランザクション（クラス 25 / 40）**: 25P02 in_failed_sql_transaction（`current transaction is aborted, commands ignored until end of transaction block`）、25001 active_sql_transaction（BEGIN の中での BEGIN は WARNING。トランザクションブロック内で実行できないコマンドはエラー）、25P01 no_active_sql_transaction（WARNING として使われる）、25006 read_only_sql_transaction、40001 serialization_failure、40P01 deadlock_detected、55P03 lock_not_available

**その他**: 0A000 feature_not_supported（未実装構文の明示的な拒否に多用する）、21000 cardinality_violation（スカラーサブクエリが複数行を返した）、08P01 protocol_violation、26000 invalid_sql_statement_name（プリペアドステートメント）、34000 invalid_cursor_name、3D000 invalid_catalog_name（存在しないデータベース）、3F000 invalid_schema_name、28000 / 28P01（認証）、53100 disk_full、53200 out_of_memory、54000 program_limit_exceeded、54001 statement_too_complex、57014 query_canceled、XX000 internal_error、XX001 data_corrupted

---

## 6. 推奨: yuzhu の型システムの最小データモデル（Rust）

### 6.1 型の識別

```rust
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct Oid(pub u32);

pub mod oid {             // pg_type.dat と同じ値（生成コードにするのが理想）
    pub const BOOL: Oid = Oid(16);   pub const NAME: Oid = Oid(19);
    pub const INT8: Oid = Oid(20);   pub const INT2: Oid = Oid(21);
    pub const INT4: Oid = Oid(23);   pub const TEXT: Oid = Oid(25);
    pub const OID: Oid = Oid(26);    pub const FLOAT4: Oid = Oid(700);
    pub const FLOAT8: Oid = Oid(701);pub const UNKNOWN: Oid = Oid(705);
    pub const BPCHAR: Oid = Oid(1042); pub const VARCHAR: Oid = Oid(1043);
    pub const DATE: Oid = Oid(1082); pub const TIMESTAMP: Oid = Oid(1114);
    pub const TIMESTAMPTZ: Oid = Oid(1184); pub const NUMERIC: Oid = Oid(1700);
    pub const REGCLASS: Oid = Oid(2205); pub const REGTYPE: Oid = Oid(2206);
    pub const VOID: Oid = Oid(2278);
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SqlType { pub oid: Oid, pub typmod: i32 }   // typmod: -1 = 指定なし
```

- 式ノードには必ず `(type_oid, typmod)` を持たせる（PG の `exprType` / `exprTypmod` に相当）。typmod は列定義、CAST、sizing cast、UNION の結果（全入力の typmod が一致する場合だけ保持し、それ以外は -1）などで伝播する。
- `enum DataType { Int4, Text, ... }` を**型の正体として使わない**。enum は「組み込み型の実装ディスパッチ」にだけ使い、型の識別と比較は常に Oid で行う。そうしておけば、ユーザー定義型・domain・配列（typelem/typarray）を後から追加しても形が変わらない。

### 6.2 値（Datum）

```rust
pub enum Datum {
    Null,
    Bool(bool), Int2(i16), Int4(i32), Int8(i64),
    Float4(f32), Float8(f64),
    Numeric(Numeric),              // 自前実装、または rust_decimal ではなく PG 互換の BCD/base-10000（NaN/Inf/scale を保持する）
    Text(Box<str>),               // text / varchar / bpchar / name / unknown 共通
    Date(i32),                    // 2000-01-01 からの日数（PG と同じ epoch）
    Timestamp(i64),               // 2000-01-01 00:00:00 からのマイクロ秒
    TimestampTz(i64),             // UTC のマイクロ秒
    Oid(u32),                     // oid / regclass / regtype
}
```

- Datum そのものは型を名乗らない（PG と同じく型は式やスキーマ側が持つ）。デバッグ用の assert のためにバリアントを分けるのは構わない。
- `numeric` を f64 や i128 で代用しないこと。scale の保持、`1.50` の表示、NaN、除算時の scale 規則が互換性に直結する。
- 日時は PG と同じ epoch（2000-01-01）と単位（マイクロ秒）で保持する。将来バイナリプロトコル（COPY BINARY / extended query の binary format）に対応するとき、そのまま使える。

### 6.3 静的な組み込みカタログ（M1 は Rust の static、M2 でテーブル化する）

```rust
pub struct TypeDef { oid: Oid, name: &'static str, len: i16, byval: bool,
    typtype: u8, category: u8, preferred: bool, elem: Oid, array: Oid,
    input: ProcId, output: ProcId, typmodin: Option<ProcId>, align: u8, collation: Oid }

pub enum CastContext { Implicit = 0, Assignment = 1, Explicit = 2 }  // 順序が意味を持つ
pub enum CastMethod { Function(ProcId), Binary, InOut }
pub struct CastDef { source: Oid, target: Oid, context: CastContext, method: CastMethod }

pub struct OperDef { oid: Oid, name: &'static str, kind: u8 /*b|l*/,
    left: Oid /*前置なら0*/, right: Oid, result: Oid, proc: ProcId,
    commutator: Oid, negator: Oid, can_hash: bool, can_merge: bool }

pub struct ProcDef { oid: Oid, name: &'static str, args: &'static [Oid], ret: Oid,
    strict: bool, volatility: u8, retset: bool, kind: u8, imp: BuiltinFn }
```

- **OID は PG と同じ値を使う**（int4pl = 177、`int4 + int4` = 551 など）。pg_*.dat から build.rs やスクリプトで必要な行だけ生成すれば、転記ミスが無く、後から行を足すのも容易になる。
- 解決器（`find_coercion_pathway`、`func_select_candidate`、`select_common_type`）は **trait `Catalog` 越しに**型・キャスト・演算子・関数を引くように書く。M1 では static 配列を実装として渡し、M2 では pg_type / pg_cast / pg_operator / pg_proc テーブル（キャッシュ付き）を実装として渡す。解決アルゴリズムのコードは変えずに済む。
- 演算子の実装も「演算子 → oprcode(ProcId) → 関数テーブルの関数ポインタ」の二段階にする。演算子は関数の別名にすぎない、という PG のモデルに合わせると、比較演算子からソート、ハッシュ、インデックス（opclass / opfamily）へ拡張しやすい。

### 6.4 エラー

```rust
pub struct PgError { pub sqlstate: [u8; 5], pub severity: Severity, pub message: String,
    pub detail: Option<String>, pub hint: Option<String>, pub position: Option<u32>,
    pub schema/table/column/constraint/datatype: Option<String> }
```

- SQLSTATE 定数は `errcodes.txt` から生成する。メッセージ文言も PG に合わせる（sqllogictest や PG の regression 出力と比較しやすくなる）。
- `position`（クエリ文字列内の 1 始まりの文字位置）を最初から持たせる。パーサのトークン位置を AST に保持しておく必要がある。

### 6.5 後から互換性を足しやすくするために避けるべき落とし穴

1. **unknown を text として扱わない**。リテラルは unknown のまま残し、演算子・関数・共通型・代入の各解決で文脈に応じて型を決める。早い段階で text に決めると `int4col = '5'` が 42883 になる。
2. **暗黙変換を if 文で直書きしない**（「int なら float に上げる」など）。必ず cast テーブルと `castcontext` の三段階（i/a/e）で判定する。代入（a）と暗黙（i）を区別しないと、`int8col` から `int4col` への INSERT が通るのに `int4 + int8` の解決が狂う、といった問題が起きる。
3. **preferred とカテゴリを持たせる**。oid も N の preferred であることを忘れない。`varchar = varchar` が `text = text` に、`'a' || 'b'` が `text || text` に解決されるのはこの仕組みのため。
4. **結果型は演算子定義から取る**。`int2 + int2` は int2、`int4 / int4` は整数除算、`1.5 + 1` は numeric。「大きい型に揃える」という独自規則を作らない。
5. **varchar には演算子を定義しない**。text への binary-coercible（RelabelType）で解決する。そのため式木に「型だけを付け替える」ノード（Relabel）を用意しておく。bpchar は末尾空白の意味論（比較で無視、text への変換で除去）を持つ別物として扱う。
6. **typmod の適用を型変換から分離する**（sizing cast）。暗黙・代入では長さ超過をエラーにし、明示キャストでは切り詰める。typmod -1 を「無制限」として扱う。timestamp の typmod にはオフセットが無いのに varchar/numeric には +4 がある点に注意。
7. **Oid の名前空間を確保しておく**。組み込みは 16384 未満、ユーザーオブジェクトは 16384 以上。pg_class / pg_attribute / pg_namespace（pg_catalog = 11、public = 2200）も PG の OID を流用すると、`::regclass` や psql の `\d` 系クエリに対応しやすい。
8. **NaN の比較規則**（NaN = NaN が真で、NaN が最大）と **-0.0 の扱い**を、ソート・ハッシュ・等値で一貫させる。
9. **整数演算は checked_* で 22003 を返す**。Rust の wrapping や debug 時の panic に頼らない。`i32::MIN / -1` と `i32::MIN % -1` も明示的に処理する（PG では `%` の方は 0 を返す）。
10. **出力形式を PG に合わせる**。float は PG12 以降、最短で往復可能な表現（`extra_float_digits` = 1 が既定）。bool は `t` / `f`、numeric は scale を保持、timestamp は ISO 形式（`DateStyle` = ISO, MDY）。
11. **name は 63 バイトで切り詰め、照合順序は C**。識別子はクォートしなければ小文字に変換する。
12. **多相型（anyelement, anynonarray, anycompatible など）は M1 で実装しなくてよいが、ProcDef.args に入れられる形にしておく**。`text || anynonarray` のような定義を後から足したときに、解決器の `can_coerce_type` に「target が多相型なら保留」という分岐を足すだけで済む。
13. 演算子・関数の検索は最初から **search_path（pg_catalog が暗黙で先頭）** に従う。M1 では pg_catalog 固定でも、`Catalog::lookup_operators(name, kind)` の戻り値を候補リストにしておく。

### 6.6 マイルストーンへの割り当て案

- **M1**: static カタログ（上記 19 型、§2.2 のキャスト、比較・算術・`||`・LIKE 程度の演算子、I/O 関数、length/upper/lower/abs/coalesce など）、3.1〜3.5 の解決器、Datum、PgError（SQLSTATE 付き）。
- **M2**: pg_namespace / pg_class / pg_attribute / pg_type / pg_cast / pg_operator / pg_proc を実テーブルとして生成し、`Catalog` trait の実装を差し替える。regclass / regtype の入出力（名前と OID の変換）。
- **M3〜M4**: 制約（23xxx）、トランザクション（25P02 / 40P01 / 40001）、oprcanhash / oprcanmerge と opclass によるインデックスとハッシュ結合、拡張プロトコル（42P18 の型推論、パラメータの unknown 扱い）。
