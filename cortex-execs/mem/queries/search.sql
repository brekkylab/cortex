select item.id, item_fts.body, item.written_at
from item_fts
join item on item.rowid = item_fts.rowid
where item_fts match ?1
order by bm25(item_fts), item_fts.rowid
limit ?2;
