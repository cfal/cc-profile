-- Rebuild the v3 table to replace its inline UNIQUE(dev,ino) constraint.
CREATE TABLE configs_v4 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tool TEXT NOT NULL CHECK(tool IN ('claude','codex')),
  label TEXT NOT NULL, home TEXT NOT NULL UNIQUE, dev INTEGER NOT NULL, ino INTEGER NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
  current INTEGER NOT NULL DEFAULT 0 CHECK(current IN (0,1) AND current <= enabled),
  revision INTEGER NOT NULL DEFAULT 1,
  proxy_mode TEXT NOT NULL DEFAULT 'inherit' CHECK(proxy_mode IN ('inherit','direct','custom')),
  proxy_url TEXT, observer_stamp TEXT, birth_ns INTEGER,
  UNIQUE(tool,label), UNIQUE(tool,id)
);
INSERT INTO configs_v4 SELECT id,tool,label,home,dev,ino,enabled,current,revision,
  proxy_mode,proxy_url,observer_stamp,birth_ns FROM configs;
DROP TABLE configs;
ALTER TABLE configs_v4 RENAME TO configs;
CREATE UNIQUE INDEX config_identity ON configs(dev,ino,birth_ns);
CREATE UNIQUE INDEX one_current ON configs(tool) WHERE current=1;
PRAGMA user_version=4;
