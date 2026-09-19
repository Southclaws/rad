with recursive children (parent, id, sort, depth) as (
    select
        parent_node_id,
        id,
        sort,
        0
    from
        nodes
    where id = cast($1 as text)
union
    select
        d.parent,
        s.id,
        s.sort,
        d.depth + 1
    from
        children d
        join nodes parent_node on parent_node.id = d.id
        join nodes s on d.id = s.parent_node_id
    where
        parent_node.hide_child_tree = false
)
select
    distinct n.id       node_id,
    n.account_id        node_account_id,
    n.visibility        node_visibility,
    n.sort              node_sort_key,
    depth
from
    children
    inner join nodes n on n.id = children.id
    inner join accounts a on a.id = n.account_id
where
    n.account_id = $2 or n.visibility in ($3, $4)
order by
    depth, node_sort_key
