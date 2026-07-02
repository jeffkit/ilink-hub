-- v12: add description to clients table for MCP list_agents.
ALTER TABLE clients ADD COLUMN description TEXT NOT NULL DEFAULT '';
