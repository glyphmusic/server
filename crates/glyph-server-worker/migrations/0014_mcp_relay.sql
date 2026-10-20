-- Short-lived mailbox for the MCP relay: tool calls waiting for a user's app,
-- their results, OAuth clients / authorizations / codes and MCP tokens.
-- `scope` is a user id or an internal namespace ("_client", "_token", ...).
CREATE TABLE IF NOT EXISTS mcp_mailbox (
 scope TEXT NOT NULL,
 key TEXT NOT NULL,
 value TEXT NOT NULL,
 expires INTEGER NOT NULL,
 PRIMARY KEY (scope, key)
);
CREATE INDEX IF NOT EXISTS mcp_mailbox_expiry ON mcp_mailbox(expires);
