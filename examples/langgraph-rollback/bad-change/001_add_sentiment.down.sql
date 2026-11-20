-- Reverse of 001_add_sentiment.up.sql. deploy-bad-change.sh copies it to
-- .agentic/schema/; `agentic rollback` runs it (with the matching
-- agentic_migrations delete) in one transaction before restoring memory.
-- No BEGIN/COMMIT here: the daemon owns the transaction.

ALTER TABLE episodes DROP COLUMN sentiment;
