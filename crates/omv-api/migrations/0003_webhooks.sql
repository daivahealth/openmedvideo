-- Per-client webhook registration (design §5.3 point 5): study lifecycle
-- events are POSTed to each client's endpoint, HMAC-signed with a secret
-- unique to that client.
ALTER TABLE clients ADD COLUMN IF NOT EXISTS webhook_url TEXT;
ALTER TABLE clients ADD COLUMN IF NOT EXISTS webhook_secret TEXT;
