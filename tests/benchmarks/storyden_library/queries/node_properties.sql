with
  sibling_properties as (
    select
      ps.id         schema_id,
      min(psf.id)   field_id,
      min(psf.name) name,
      min(psf.type) type,
      min(psf.sort) sort,
      'sibling' as source
    from
      nodes n
      left join nodes sn on sn.parent_node_id = n.parent_node_id
      inner join property_schemas ps on ps.id = sn.property_schema_id
      or ps.id = n.property_schema_id
      inner join property_schema_fields psf on psf.schema_id = ps.id
    where
      n.id = $1
    group by ps.id, psf.id
  ),
  child_properties as (
    select
      ps.id         schema_id,
      min(psf.id)   field_id,
      min(psf.name) name,
      min(psf.type) type,
      min(psf.sort) sort,
      'child' as source
    from
      nodes n
      inner join nodes cn on cn.parent_node_id = n.id
      inner join property_schemas ps on ps.id = cn.property_schema_id
      inner join property_schema_fields psf on psf.schema_id = ps.id
    where
      n.id = $2
    group by ps.id, psf.id
  )
select
  *
from
  sibling_properties
union all
select
  *
from
  child_properties
order by source desc, sort asc
