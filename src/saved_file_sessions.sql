-- SQLite cannot extend the evidence CHECK in place.
CREATE TABLE sessions_v10 (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, tool TEXT NOT NULL, native_id TEXT NOT NULL,
  owner INTEGER NOT NULL, evidence TEXT NOT NULL CHECK(
    evidence IN ('assigned','observed') OR (tool='agy' AND evidence='saved_file')
  ),
  workspace TEXT NOT NULL, bound_ms INTEGER NOT NULL, last_routed_ms INTEGER NOT NULL,
  UNIQUE(tool,native_id), FOREIGN KEY(tool,owner) REFERENCES configs(tool,id)
);
INSERT INTO sessions_v10(seq,tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms)
  SELECT seq,tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms FROM sessions;
UPDATE sqlite_sequence SET seq=max(seq,coalesce((SELECT seq FROM sqlite_sequence WHERE name='sessions'),0))
  WHERE name='sessions_v10';
DROP TABLE sessions;
ALTER TABLE sessions_v10 RENAME TO sessions;
CREATE TRIGGER immutable_owner BEFORE UPDATE OF tool,native_id,owner ON sessions
BEGIN SELECT RAISE(ABORT,'immutable session binding'); END;
CREATE INDEX sessions_page ON sessions(tool,owner,seq DESC);
CREATE INDEX sessions_tool ON sessions(tool,seq DESC);
CREATE INDEX sessions_config ON sessions(owner,seq DESC);
PRAGMA user_version=10;
