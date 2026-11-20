-- VERSION 3
CREATE TABLE retention(singleton INTEGER PRIMARY KEY CHECK(singleton=1), semantic INTEGER NOT NULL, enabled INTEGER NOT NULL, days INTEGER NOT NULL, pending_purge INTEGER NOT NULL DEFAULT 0) STRICT;
INSERT INTO retention VALUES(1,1,0,30,0);
CREATE TABLE measurements(generation BLOB PRIMARY KEY REFERENCES generations(id), root BLOB NOT NULL, started INTEGER NOT NULL, measured INTEGER NOT NULL) STRICT, WITHOUT ROWID;
PRAGMA user_version=3;

-- VERSION 4
CREATE TABLE entry_evidence(generation BLOB NOT NULL,id BLOB NOT NULL,proof BLOB,PRIMARY KEY(generation,id),FOREIGN KEY(generation,id) REFERENCES entries(generation,id)) STRICT, WITHOUT ROWID; PRAGMA user_version=4;

-- VERSION 5
CREATE TABLE acquisition_records(generation BLOB PRIMARY KEY REFERENCES generations(id),root BLOB NOT NULL,started INTEGER NOT NULL) STRICT, WITHOUT ROWID; CREATE TABLE history_receipt(singleton INTEGER PRIMARY KEY CHECK(singleton=1),generation BLOB NOT NULL REFERENCES generations(id)) STRICT; PRAGMA user_version=5;

-- VERSION 6
CREATE TABLE entry_properties(generation BLOB NOT NULL,id BLOB NOT NULL,allocated_state INTEGER NOT NULL,accessed_state INTEGER NOT NULL,accessed_s INTEGER,accessed_ns INTEGER,owner BLOB NOT NULL,PRIMARY KEY(generation,id),FOREIGN KEY(generation,id) REFERENCES entries(generation,id),CHECK(allocated_state BETWEEN 0 AND 5),CHECK(accessed_state BETWEEN 0 AND 5),CHECK(length(owner) BETWEEN 1 AND 70),CHECK((accessed_s IS NULL)=(accessed_ns IS NULL)),CHECK(accessed_ns IS NULL OR accessed_ns BETWEEN 0 AND 999999999)) STRICT, WITHOUT ROWID;
CREATE TABLE collection_options(generation BLOB PRIMARY KEY REFERENCES generations(id),allocation INTEGER NOT NULL CHECK(allocation IN (0,1)),owners INTEGER NOT NULL CHECK(owners IN (0,1))) STRICT, WITHOUT ROWID;
CREATE TRIGGER immutable_properties_insert BEFORE INSERT ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=new.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_properties_update BEFORE UPDATE ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_properties_delete BEFORE DELETE ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_options_insert BEFORE INSERT ON collection_options WHEN (SELECT sealed FROM generations WHERE id=new.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_options_update BEFORE UPDATE ON collection_options BEGIN SELECT RAISE(ABORT,'immutable collection options'); END;
CREATE TRIGGER immutable_options_delete BEFORE DELETE ON collection_options WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
PRAGMA user_version=6;
