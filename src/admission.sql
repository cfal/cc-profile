ALTER TABLE configs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0
  CHECK(typeof(priority)='integer' AND priority BETWEEN 0 AND 100);
CREATE TABLE settings (
  id INTEGER PRIMARY KEY CHECK(id=1),
  routing_mode TEXT NOT NULL CHECK(routing_mode IN
    ('round_robin','most_remaining','least_remaining','priority'))
);
INSERT INTO settings VALUES(1,'round_robin');
PRAGMA user_version=8;
