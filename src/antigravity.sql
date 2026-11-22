-- SQLite cannot extend the provider CHECK constraint in place.
CREATE TABLE configs_v7 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tool TEXT NOT NULL CHECK(tool IN ('claude','codex','agy')),
  label TEXT NOT NULL, home TEXT NOT NULL UNIQUE, dev INTEGER NOT NULL, ino INTEGER NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
  current INTEGER NOT NULL DEFAULT 0 CHECK(current IN (0,1) AND current <= enabled),
  revision INTEGER NOT NULL DEFAULT 1,
  proxy_mode TEXT NOT NULL DEFAULT 'inherit' CHECK(proxy_mode IN ('inherit','direct','custom')),
  proxy_url TEXT, birth_ns INTEGER,
  UNIQUE(tool,label), UNIQUE(tool,id)
);
INSERT INTO configs_v7 SELECT id,tool,label,home,dev,ino,enabled,current,revision,
  proxy_mode,proxy_url,birth_ns FROM configs;
UPDATE sqlite_sequence SET seq=max(seq,coalesce((SELECT seq FROM sqlite_sequence WHERE name='configs'),0))
WHERE name='configs_v7';
DROP TABLE configs;
ALTER TABLE configs_v7 RENAME TO configs;
CREATE UNIQUE INDEX config_identity ON configs(dev,ino,birth_ns);
CREATE UNIQUE INDEX one_current ON configs(tool) WHERE current=1;
INSERT OR IGNORE INTO tools(tool) VALUES('agy');
PRAGMA user_version=7;
