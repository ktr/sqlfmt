-- Deletes up to max_roots of a mailbox's old MSN tree roots, and the tree nodes only they
-- use. A root is old when its modseq is below the mailbox's newest root created before
-- until. That newest root and every root after it stay, along with all of their nodes.
--
-- Roots are deleted newest first. Older roots share nodes with the ones deleted, so a root
-- left behind for a later batch can be missing nodes. It also deletes those roots when
-- their root node goes, so no foreign key breaks. Keep calling it until a mailbox is done.
--
-- Returns how many roots it deleted, which is at least as many as it selected. Fewer than
-- max_roots means the mailbox has nothing left before until. Returns null, deleting
-- nothing, when the newest root's tree is already missing nodes.
--
-- The tree is read before the mailbox row is locked, so the lock that blocks new mail is
-- held only for the deletes. Roots added in the meantime are built from the kept root, so
-- they reach no node this deletes.
create or replace function msn_tree_cleanup_mailbox(
  target_mailbox_id mailbox.mailbox_id % type
  , until timestamptz
  , max_roots integer
) returns bigint
set enable_seqscan = off
as $$
declare
    latest mailbox_msn_tree_root%rowtype;
    keep_ids bigint[];
    old_modseqs bigint[];
    old_roots bigint[];
    delete_ids bigint[];
    deleted bigint;
BEGIN
    select * into latest
    from mailbox_msn_tree_root
    where mailbox_id = target_mailbox_id and created < until
    order by modseq desc
    limit 1;
    if latest.root_node is null then
        return 0;
    end if;

    with recursive tree(id) as (
        select latest.root_node
        union
        select child
        from tree
        join mailbox_msn_tree on mailbox_msn_tree_id = tree.id
        cross join unnest(children) as child
    )
    select array_agg(id) into keep_ids from tree;

    if exists (
        select 1
        from unnest(keep_ids) as keep(id)
        where not exists (select 1 from mailbox_msn_tree where mailbox_msn_tree_id = keep.id)
    ) then
        return null;
    end if;

    select array_agg(modseq), array_agg(root_node) into old_modseqs, old_roots
    from (
        select modseq, root_node
        from mailbox_msn_tree_root
        where mailbox_id = target_mailbox_id and modseq < latest.modseq
        order by modseq desc
        limit max_roots
    ) as batch;
    if old_modseqs is null then
        return 0;
    end if;

    -- Every child of a kept node is kept too, so the walk stops at kept nodes.
    with recursive
    keep as materialized (select unnest(keep_ids) as id),
    tree(id) as (
        select root
        from unnest(old_roots) as root
        where not exists (select 1 from keep where keep.id = root)
        union
        select child
        from tree
        join mailbox_msn_tree on mailbox_msn_tree_id = tree.id
        cross join unnest(children) as child
        where not exists (select 1 from keep where keep.id = child)
    )
    select coalesce(array_agg(id), array[]::bigint[]) into delete_ids from tree;

    perform 1 from mailbox where mailbox_id = target_mailbox_id for update;
    if not found then
        return 0;
    end if;

    delete from mailbox_msn_tree_root
    where mailbox_id = target_mailbox_id
      and (modseq = any(old_modseqs) or root_node = any(delete_ids));
    get diagnostics deleted = row_count;

    delete from mailbox_msn_tree where mailbox_msn_tree_id = any(delete_ids);

    return deleted;
end;
$$ language plpgsql;

