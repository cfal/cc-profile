CREATE TABLE sessions_v15 (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, tool TEXT NOT NULL, native_id TEXT NOT NULL,
  owner INTEGER NOT NULL, evidence TEXT NOT NULL CHECK(
    evidence IN ('assigned','observed') OR (tool IN ('agy','codex') AND evidence='saved_file')
  ),
  workspace TEXT, bound_ms INTEGER NOT NULL, last_routed_ms INTEGER, tui_workspace TEXT,
  saved_file_seen_ms INTEGER CHECK(saved_file_seen_ms IS NULL OR tool IN ('agy','codex')),
  CHECK(evidence='saved_file' OR (workspace IS NOT NULL AND last_routed_ms IS NOT NULL)),
  UNIQUE(tool,native_id), FOREIGN KEY(tool,owner) REFERENCES configs(tool,id)
);
INSERT INTO sessions_v15 SELECT seq,tool,native_id,owner,evidence,workspace,bound_ms,
    last_routed_ms,tui_workspace,saved_file_seen_ms FROM sessions;
UPDATE sqlite_sequence SET seq=max(seq,coalesce((SELECT seq FROM sqlite_sequence WHERE name='sessions'),0))
  WHERE name='sessions_v15';
INSERT INTO sqlite_sequence(name,seq)
  SELECT 'sessions_v15',seq FROM sqlite_sequence WHERE name='sessions'
  AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name='sessions_v15');
DROP TABLE sessions;
ALTER TABLE sessions_v15 RENAME TO sessions;
CREATE TRIGGER immutable_owner BEFORE UPDATE OF tool,native_id,owner ON sessions
BEGIN SELECT RAISE(ABORT,'immutable session binding'); END;
CREATE INDEX sessions_page ON sessions(tool,owner,seq DESC);
CREATE INDEX sessions_tool ON sessions(tool,seq DESC);
CREATE INDEX sessions_config ON sessions(owner,seq DESC);

-- A directory stamp is published only after every matching name has been indexed.
-- These are lookup hints, not ownership: every resume rechecks file metadata.
CREATE TABLE IF NOT EXISTS rollout_dirs (
  owner INTEGER NOT NULL REFERENCES configs(id), dir TEXT NOT NULL, stamp TEXT NOT NULL,
  PRIMARY KEY(owner,dir)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rollout_paths (
  owner INTEGER NOT NULL REFERENCES configs(id), dir TEXT NOT NULL, name TEXT NOT NULL,
  native_id TEXT NOT NULL, needs_check INTEGER NOT NULL DEFAULT 1, UNIQUE(owner,dir,name)
);
CREATE INDEX IF NOT EXISTS rollout_by_id ON rollout_paths(native_id);
CREATE INDEX IF NOT EXISTS rollout_deferred ON rollout_paths(needs_check) WHERE needs_check=1;
UPDATE launches SET state='unobserved',reason='router_upgraded' WHERE tool='codex' AND state='pending';
PRAGMA user_version=15;
