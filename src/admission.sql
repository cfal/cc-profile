ALTER TABLE configs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0
  CHECK(typeof(priority)='integer' AND priority BETWEEN 0 AND 100);
CREATE TABLE settings (
  id INTEGER PRIMARY KEY CHECK(id=1),
  routing_mode TEXT NOT NULL CHECK(routing_mode IN
    ('round_robin','most_remaining','least_remaining','priority'))
);
-- v17 seeds the current default when there was no prior setting.
PRAGMA user_version=8;
