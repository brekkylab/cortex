select memory.text
from memory_fts_index
join memory on memory.rowid = memory_fts_index.rowid
where memory_fts_index match ?1
order by bm25(memory_fts_index), memory.rowid
limit ?2;
