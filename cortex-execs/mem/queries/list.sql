select item.id, item_fts.body, item.written_at
from item
join item_fts on item_fts.rowid = item.rowid
order by item.written_at desc, item.rowid desc
limit ?1;
