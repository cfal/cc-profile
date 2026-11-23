CREATE TABLE settings_v14 (
  id INTEGER PRIMARY KEY CHECK(id=1),
  routing_mode TEXT NOT NULL CHECK(routing_mode IN
    ('round_robin','most_remaining','least_remaining','earliest_expiry','priority'))
);
INSERT INTO settings_v14 (id,routing_mode) SELECT id,routing_mode FROM settings;
DROP TABLE settings;
ALTER TABLE settings_v14 RENAME TO settings;
PRAGMA user_version=14;
