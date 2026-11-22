CREATE TABLE configs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tool TEXT NOT NULL CHECK(tool IN ('claude','codex')),
  label TEXT NOT NULL, home TEXT NOT NULL UNIQUE, dev INTEGER NOT NULL, ino INTEGER NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
  current INTEGER NOT NULL DEFAULT 0 CHECK(current IN (0,1) AND current <= enabled),
  revision INTEGER NOT NULL DEFAULT 1,
  proxy_mode TEXT NOT NULL DEFAULT 'inherit' CHECK(proxy_mode IN ('inherit','direct','custom')),
  proxy_url TEXT,
  observer_stamp TEXT,
  UNIQUE(tool,label), UNIQUE(tool,id), UNIQUE(dev,ino)
);
CREATE UNIQUE INDEX one_current ON configs(tool) WHERE current=1;
CREATE TABLE tools (
  tool TEXT PRIMARY KEY, cursor INTEGER NOT NULL DEFAULT 0,
  link_root TEXT, outer_path TEXT, projected TEXT
);
INSERT INTO tools(tool) VALUES ('claude'),('codex');
CREATE TABLE sessions (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, tool TEXT NOT NULL, native_id TEXT NOT NULL,
  owner INTEGER NOT NULL, evidence TEXT NOT NULL CHECK(evidence IN ('assigned','observed')),
  workspace TEXT NOT NULL, bound_ms INTEGER NOT NULL, last_routed_ms INTEGER NOT NULL,
  UNIQUE(tool,native_id), FOREIGN KEY(tool,owner) REFERENCES configs(tool,id)
);
CREATE TRIGGER immutable_owner BEFORE UPDATE OF tool,native_id,owner ON sessions
BEGIN SELECT RAISE(ABORT,'immutable session binding'); END;
CREATE INDEX sessions_page ON sessions(tool,owner,seq DESC);
CREATE INDEX sessions_tool ON sessions(tool,seq DESC);
CREATE INDEX sessions_config ON sessions(owner,seq DESC);
CREATE TABLE launches (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL UNIQUE,
  tool TEXT NOT NULL, intent TEXT NOT NULL, state TEXT NOT NULL, reason TEXT NOT NULL,
  native_id TEXT, config_id INTEGER, revision INTEGER, workspace TEXT,
  digest BLOB NOT NULL, ticket_digest BLOB NOT NULL, created_ms INTEGER NOT NULL, observed_ms INTEGER,
  FOREIGN KEY(tool,config_id) REFERENCES configs(tool,id)
);
CREATE INDEX startups_page ON launches(tool,state,seq DESC);
CREATE INDEX startups_config ON launches(config_id,seq DESC);
CREATE INDEX startups_session ON launches(tool,native_id,seq DESC);
CREATE TABLE quota_cache (
  config_id INTEGER PRIMARY KEY REFERENCES configs(id), revision INTEGER NOT NULL,
  attempt_ms INTEGER NOT NULL, success_ms INTEGER, retry_ms INTEGER NOT NULL,
  failures INTEGER NOT NULL, status TEXT NOT NULL, stamp TEXT NOT NULL, snapshot TEXT
);
PRAGMA user_version=1;
