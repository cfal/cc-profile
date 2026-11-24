CREATE TABLE settings_v17 (
  id INTEGER PRIMARY KEY CHECK(id=1),
  routing_mode TEXT NOT NULL CHECK(routing_mode IN
    ('round_robin','most_remaining','least_remaining',
     'earliest_expiry_most_remaining','earliest_expiry_least_remaining','priority'))
);
INSERT INTO settings_v17 (id,routing_mode)
  SELECT id, CASE routing_mode WHEN 'earliest_expiry' THEN 'earliest_expiry_most_remaining'
                               ELSE routing_mode END FROM settings;
INSERT INTO settings_v17 SELECT 1,'earliest_expiry_most_remaining'
  WHERE NOT EXISTS (SELECT 1 FROM settings_v17);
DROP TABLE settings;
ALTER TABLE settings_v17 RENAME TO settings;
PRAGMA user_version=17;
