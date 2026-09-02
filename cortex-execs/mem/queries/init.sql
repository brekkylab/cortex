create table meta (key text primary key, value text not null);

create table memory (
    rowid      integer primary key,
    id         text not null unique,
    text       text not null,
    written_at text not null
);

create virtual table memory_fts_index using fts5(
    terms,
    content='',
    contentless_delete=1,
    tokenize='ascii'
);
