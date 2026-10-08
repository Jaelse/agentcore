-- Building an organisation from templates: which template a department came
-- from, and the company profile and growth path (blueprint) chosen.
ALTER TABLE departments ADD COLUMN template text;

ALTER TABLE org_settings ADD COLUMN company_name text NOT NULL DEFAULT '';
ALTER TABLE org_settings ADD COLUMN company_about text NOT NULL DEFAULT '';
ALTER TABLE org_settings ADD COLUMN blueprint text;
