-- A member started only so a person can talk with it: it answers, does not
-- take up its normal work, and is stopped again once the conversation ends.
ALTER TABLE org_agents ADD COLUMN talk_only boolean NOT NULL DEFAULT false;
