-- Schema 1. Archive ELBETA01 remains unchanged; all references are transactional.
CREATE TABLE identity(network TEXT NOT NULL, genesis BLOB NOT NULL CHECK(length(genesis)=32));
CREATE TABLE state(id INTEGER PRIMARY KEY CHECK(id=1), transactions INTEGER NOT NULL CHECK(transactions>=0), utxos INTEGER NOT NULL CHECK(utxos>=0), history INTEGER NOT NULL CHECK(history>=0));
INSERT INTO state VALUES(1,0,0,0);
CREATE TABLE blocks(
    height INTEGER PRIMARY KEY CHECK(height>=0), hash BLOB NOT NULL UNIQUE CHECK(length(hash)=32),
    segment INTEGER NOT NULL CHECK(segment>=0), offset INTEGER NOT NULL CHECK(offset>=0), length INTEGER NOT NULL CHECK(length>0), header BLOB CHECK(length(header)=80)
);
CREATE TABLE occurrences(
    height INTEGER NOT NULL REFERENCES blocks(height) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
    position INTEGER NOT NULL CHECK(position>=0), txid BLOB NOT NULL CHECK(length(txid)=32),
    PRIMARY KEY(height,position)
) WITHOUT ROWID;
CREATE INDEX occurrences_txid ON occurrences(txid,height,position);
CREATE TABLE history(
    script BLOB NOT NULL CHECK(length(script)=32), height INTEGER NOT NULL, position INTEGER NOT NULL,
    PRIMARY KEY(script,height,position),
    FOREIGN KEY(height,position) REFERENCES occurrences(height,position) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
) WITHOUT ROWID;
CREATE TABLE utxos(
    outpoint BLOB PRIMARY KEY CHECK(length(outpoint)=36), script BLOB NOT NULL CHECK(length(script)=32),
    value INTEGER NOT NULL CHECK(value>=0), height INTEGER NOT NULL CHECK(height>=0), coinbase INTEGER NOT NULL CHECK(coinbase IN(0,1))
) WITHOUT ROWID;
CREATE INDEX utxos_script ON utxos(script,outpoint);
CREATE TABLE undo_blocks(height INTEGER PRIMARY KEY REFERENCES blocks(height) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE undo(
    height INTEGER NOT NULL REFERENCES undo_blocks(height) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
    outpoint BLOB NOT NULL CHECK(length(outpoint)=36), old_script BLOB, old_value INTEGER, old_height INTEGER, old_coinbase INTEGER,
    PRIMARY KEY(height,outpoint),
    CHECK((old_value IS NULL AND old_script IS NULL AND old_height IS NULL AND old_coinbase IS NULL) OR
          (old_value IS NOT NULL AND old_script IS NOT NULL AND old_height IS NOT NULL AND old_coinbase IS NOT NULL AND old_value>=0 AND length(old_script)=32 AND old_height>=0 AND old_coinbase IN(0,1)))
) WITHOUT ROWID;
PRAGMA application_id=0x454C4954;
CREATE TABLE archive_cursor(id INTEGER PRIMARY KEY CHECK(id=1), segment INTEGER NOT NULL, offset INTEGER NOT NULL, length INTEGER NOT NULL);
CREATE TABLE prune_audit(id INTEGER PRIMARY KEY, requested INTEGER NOT NULL, actual INTEGER, tip_height INTEGER NOT NULL, tip_hash BLOB NOT NULL, state TEXT NOT NULL);
PRAGMA user_version=2;
