-- Forward migration the bad change ships: a NOT NULL `sentiment` column
-- on the agent's memory table. A code-only `git revert` cannot undo it.
-- The agentic_migrations row is what `agentic_schema_version()` reads,
-- so the next `agentic commit` records this version in the Commit.
-- Applied by deploy-bad-change.sh under `psql -1` (one transaction).

ALTER TABLE episodes ADD COLUMN sentiment text NOT NULL DEFAULT 'neutral';
INSERT INTO agentic_migrations (name) VALUES ('001_add_sentiment');
