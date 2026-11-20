PRAGMA user_version=1;
CREATE TABLE generations(id BLOB PRIMARY KEY CHECK(length(id)=8), sealed INTEGER NOT NULL, coverage INTEGER NOT NULL, revision INTEGER NOT NULL) STRICT, WITHOUT ROWID;
CREATE TABLE entries(
 generation BLOB NOT NULL REFERENCES generations(id), id BLOB NOT NULL CHECK(length(id)=8),
 parent BLOB NOT NULL CHECK(length(parent)=8), name BLOB NOT NULL,
 kind INTEGER NOT NULL, logical BLOB CHECK(length(logical)=8), allocated BLOB CHECK(length(allocated)=8),
 modified_s INTEGER, modified_ns INTEGER CHECK(modified_ns>=0 AND modified_ns<1000000000),
 attempt BLOB NOT NULL, presence INTEGER NOT NULL, metadata_state INTEGER NOT NULL,
 PRIMARY KEY(generation,id), UNIQUE(generation,parent,name)
) STRICT, WITHOUT ROWID;
CREATE INDEX entries_size ON entries(generation,logical,id);
CREATE TABLE tasks(generation BLOB NOT NULL, directory BLOB NOT NULL, owner BLOB NOT NULL, attempt BLOB NOT NULL, ack INTEGER NOT NULL, eof INTEGER NOT NULL, sweep BLOB, PRIMARY KEY(generation,directory)) STRICT, WITHOUT ROWID;
CREATE TABLE contributions(generation BLOB NOT NULL, id BLOB NOT NULL, parent BLOB NOT NULL, logical BLOB, validation BLOB NOT NULL, PRIMARY KEY(generation,id)) STRICT, WITHOUT ROWID;
