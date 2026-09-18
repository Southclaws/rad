select
  n.id id
from
  nodes n
  left join properties p on n.id = p.node_id
  inner join property_schema_fields f on p.field_id = f.id and f.name = $1
where
  n.id in ({{IDS}})
order by
  case f.type when 'text'      then p.value                    end asc,
  case f.type when 'number'    then cast(p.value as numeric)   end asc,
  case f.type when 'timestamp' then cast(p.value as timestamp) end asc,
  case f.type when 'boolean'   then cast(p.value as boolean)   end asc,
  p.value asc
limit 100
offset 0
