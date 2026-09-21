create table meta (key text primary key, value text not null);

create table item (
    rowid      integer primary key,
    id         text unique,
    path       text unique,
    title      text not null default '',
    mtime      integer not null default 0,
    len        integer not null default 0,
    written_at text not null
);

create virtual table item_fts using fts5(
    title,
    body,
    tokenize='porter unicode61 remove_diacritics 2'
);

create table chunk (
    rowid       integer primary key,
    item_id     integer not null references item(rowid) on delete cascade,
    ordinal     integer not null,
    start_byte  integer not null,
    end_byte    integer not null,
    embedding   blob,
    unique(item_id, ordinal)
);

create index chunk_of_item on chunk(item_id);
