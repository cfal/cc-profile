-- File discovery supplies neither a launch time nor a workspace.
CREATE TABLE sessions_v13 (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, tool TEXT NOT NULL, native_id TEXT NOT NULL,
  owner INTEGER NOT NULL, evidence TEXT NOT NULL CHECK(
    evidence IN ('assigned','observed') OR (tool='agy' AND evidence='saved_file')
  ),
  workspace TEXT, bound_ms INTEGER NOT NULL, last_routed_ms INTEGER, tui_workspace TEXT,
  saved_file_seen_ms INTEGER CHECK(saved_file_seen_ms IS NULL OR tool='agy'),
  CHECK(evidence='saved_file' OR (workspace IS NOT NULL AND last_routed_ms IS NOT NULL)),
  UNIQUE(tool,native_id), FOREIGN KEY(tool,owner) REFERENCES configs(tool,id)
);
INSERT INTO sessions_v13
  SELECT seq,tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms,tui_workspace,
         CASE WHEN evidence='saved_file' THEN bound_ms END FROM sessions;
UPDATE sqlite_sequence SET seq=max(seq,coalesce((SELECT seq FROM sqlite_sequence WHERE name='sessions'),0))
  WHERE name='sessions_v13';
-- Also preserve the high-water mark if every row was removed before the upgrade.
INSERT INTO sqlite_sequence(name,seq)
  SELECT 'sessions_v13',seq FROM sqlite_sequence WHERE name='sessions'
  AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name='sessions_v13');
DROP TABLE sessions;
ALTER TABLE sessions_v13 RENAME TO sessions;
CREATE TRIGGER immutable_owner BEFORE UPDATE OF tool,native_id,owner ON sessions
BEGIN SELECT RAISE(ABORT,'immutable session binding'); END;
CREATE INDEX sessions_page ON sessions(tool,owner,seq DESC);
CREATE INDEX sessions_tool ON sessions(tool,seq DESC);
CREATE INDEX sessions_config ON sessions(owner,seq DESC);
PRAGMA user_version=13;
