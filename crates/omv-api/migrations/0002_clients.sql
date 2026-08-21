-- Registered client apps (design §5.3 point 1). Adding a new doctor/nurse
-- app is a row here, not a code change.
CREATE TABLE IF NOT EXISTS clients (
    client_id          TEXT PRIMARY KEY,
    client_secret_hash TEXT NOT NULL,            -- sha256 hex of the secret
    name               TEXT NOT NULL,
    scopes             TEXT NOT NULL DEFAULT 'imaging.read',
    -- The identity provider this app's users authenticate against.
    -- Subject tokens presented in token exchange must come from it.
    idp_issuer         TEXT,
    idp_audience       TEXT,
    idp_jwks_url       TEXT,                     -- RS256 (production IdPs)
    idp_hs256_secret   TEXT,                     -- HS256 (dev/test IdPs)
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
