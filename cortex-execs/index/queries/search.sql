select item.path,
       item.title,
       -bm25(item_fts),
       snippet(item_fts, 1, '', '', '…', 32)
from item_fts
join item on item.rowid = item_fts.rowid
where item_fts match ?1
order by bm25(item_fts), item.rowid
limit ?2;
