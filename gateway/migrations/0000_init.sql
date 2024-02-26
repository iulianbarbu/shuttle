CREATE TABLE IF NOT EXISTS projects (
  project_id TEXT PRIMARY KEY,
  project_name TEXT NOT NULL UNIQUE,
  account_name TEXT NOT NULL,
  initial_key TEXT NOT NULL,
  project_state JSON NOT NULL,
  created_at INT
);

CREATE TABLE IF NOT EXISTS custom_domains (
  fqdn TEXT PRIMARY KEY,
  project_name TEXT NOT NULL REFERENCES projects (project_name),
  project_id TEXT NOT NULL REFERENCES projects (project_id),
  certificate TEXT NOT NULL,
  private_key TEXT NOT NULL
);
