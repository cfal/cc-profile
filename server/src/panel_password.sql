CREATE TABLE panel_password (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  revision INTEGER NOT NULL,
  verifier TEXT,
  CHECK ((revision = 0 AND verifier IS NULL) OR (revision > 0 AND verifier IS NOT NULL))
);
INSERT INTO panel_password VALUES (1, 0, NULL);
PRAGMA user_version=2;
