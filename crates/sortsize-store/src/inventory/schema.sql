PRAGMA application_id=1112758837;
PRAGMA user_version=2;
CREATE TABLE identity(inventory BLOB NOT NULL CHECK(length(inventory)=16),source BLOB NOT NULL CHECK(length(source)=16), latest BLOB, latest_complete BLOB) STRICT;
CREATE TABLE generations(
 id BLOB PRIMARY KEY CHECK(length(id)=8), context BLOB NOT NULL, sealed INTEGER NOT NULL DEFAULT 0,
 revision BLOB NOT NULL, next_entry BLOB NOT NULL, aggregate_cursor BLOB,
 aggregate_sum BLOB NOT NULL, aggregate_unknown BLOB NOT NULL, aggregate_overflow INTEGER NOT NULL DEFAULT 0,
 aggregate_done INTEGER NOT NULL DEFAULT 0, coverage INTEGER NOT NULL DEFAULT 0
) STRICT, WITHOUT ROWID;
CREATE TABLE entries(
 generation BLOB NOT NULL REFERENCES generations(id), id BLOB NOT NULL,
 parent BLOB, name BLOB NOT NULL, kind INTEGER NOT NULL CHECK(kind BETWEEN 0 AND 4),
 logical BLOB CHECK(logical IS NULL OR length(logical)=8), allocated BLOB CHECK(allocated IS NULL OR length(allocated)=8),
 modified_s INTEGER, modified_ns INTEGER CHECK(modified_ns IS NULL OR modified_ns BETWEEN 0 AND 999999999),
 attempt BLOB NOT NULL, present INTEGER NOT NULL DEFAULT 1, metadata_known INTEGER NOT NULL,
 PRIMARY KEY(generation,id), UNIQUE(generation,parent,name),
 CHECK(length(generation)=8 AND length(id)=8), CHECK(parent IS NULL OR length(parent)=8),
 CHECK((modified_s IS NULL)=(modified_ns IS NULL))
) STRICT, WITHOUT ROWID;
CREATE INDEX entries_parent_id ON entries(generation,parent,id);
CREATE INDEX entries_size ON entries(generation,logical,id);
CREATE TABLE tasks(
 generation BLOB NOT NULL, directory BLOB NOT NULL, kind INTEGER NOT NULL DEFAULT 0,
 key BLOB, attempt BLOB, owner BLOB, branch BLOB, next_sequence BLOB NOT NULL, parent_attempt BLOB NOT NULL,
 listing INTEGER NOT NULL DEFAULT 0, sweep_cursor BLOB, reconciled INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(generation,directory,kind), FOREIGN KEY(generation,directory) REFERENCES entries(generation,id)
) STRICT, WITHOUT ROWID;
CREATE TABLE contributions(
 generation BLOB NOT NULL, id BLOB NOT NULL, parent BLOB NOT NULL, attempt BLOB NOT NULL,
 logical BLOB, metadata_known INTEGER NOT NULL, kind INTEGER NOT NULL,
 PRIMARY KEY(generation,id), FOREIGN KEY(generation,id) REFERENCES entries(generation,id)
) STRICT, WITHOUT ROWID;
CREATE TRIGGER immutable_entries_update BEFORE UPDATE ON entries WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_entries_insert BEFORE INSERT ON entries WHEN (SELECT sealed FROM generations WHERE id=new.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_entries_delete BEFORE DELETE ON entries WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;

CREATE TABLE recovery_guard(generation BLOB PRIMARY KEY REFERENCES generations(id), proof BLOB NOT NULL) STRICT, WITHOUT ROWID;
