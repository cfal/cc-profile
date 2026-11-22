CREATE TABLE IF NOT EXISTS reset_grants (
  handle TEXT PRIMARY KEY NOT NULL,
  config_id INTEGER NOT NULL REFERENCES configs(id),
  profile TEXT NOT NULL, account TEXT NOT NULL, credit TEXT NOT NULL, details TEXT NOT NULL,
  seen_ms INTEGER NOT NULL, attempt TEXT,
  outcome TEXT CHECK(outcome IN ('reset','alreadyRedeemed','nothingToReset','noCredit')),
  CHECK(outcome IS NULL OR attempt IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS reset_credit ON reset_grants(credit);
PRAGMA user_version=12;
