SELECT 'proc' AS kind, p.oid::text AS id, p.proname::text AS name, p.proargtypes::text AS args, p.prorettype::text AS ret
  FROM pg_proc p
 WHERE p.prorettype = @OID@ OR p.proargtypes::text ~ '(^| )@OID@( |$)'
UNION ALL
SELECT 'oper', o.oid::text, o.oprname::text, o.oprleft::text || ' ' || o.oprright::text, o.oprresult::text
  FROM pg_operator o WHERE @OID@ IN (o.oprleft, o.oprright, o.oprresult)
UNION ALL
SELECT 'cast', c.castsource::text || '>' || c.casttarget::text, c.castcontext::text || c.castmethod::text, c.castfunc::text, ''
  FROM pg_cast c WHERE @OID@ IN (c.castsource, c.casttarget)
UNION ALL
SELECT 'amop', a.amopfamily::text || ':' || a.amopstrategy::text, a.amopopr::text, a.amoplefttype::text || ' ' || a.amoprighttype::text, ''
  FROM pg_amop a WHERE a.amoplefttype = @OID@ AND a.amoprighttype = @OID@
ORDER BY 1, 2
