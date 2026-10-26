-- Users are created on first SSO login. external_id is "entra:<tid>:<oid>" (or "dev:<email>" for dev login).
CREATE TABLE users (
    id            UUID PRIMARY KEY,
    external_id   TEXT NOT NULL UNIQUE,
    email         TEXT NOT NULL,
    name          TEXT NOT NULL,
    groups        TEXT[] NOT NULL DEFAULT '{}',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_login_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX users_email_idx ON users (lower(email));

-- Portal browser sessions. Only the SHA-256 of the cookie value is stored.
CREATE TABLE sessions (
    token_hash BYTEA PRIMARY KEY,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);

-- Bearer tokens: personal access tokens and OAuth access/refresh tokens. Only hashes are stored.
CREATE TABLE tokens (
    id           UUID PRIMARY KEY,
    token_hash   BYTEA NOT NULL UNIQUE,
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('pat', 'oauth_access', 'oauth_refresh')),
    name         TEXT NOT NULL,
    client_id    TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX tokens_user_idx ON tokens (user_id, kind);

-- OAuth 2.1 clients registered through dynamic client registration (RFC 7591), e.g. Claude connectors.
CREATE TABLE oauth_clients (
    client_id     TEXT PRIMARY KEY,
    client_name   TEXT NOT NULL,
    redirect_uris TEXT[] NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE oauth_codes (
    code_hash      BYTEA PRIMARY KEY,
    client_id      TEXT NOT NULL REFERENCES oauth_clients(client_id) ON DELETE CASCADE,
    user_id        UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    redirect_uri   TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    scope          TEXT NOT NULL,
    expires_at     TIMESTAMPTZ NOT NULL
);

CREATE TABLE artifacts (
    id              TEXT PRIMARY KEY,
    owner_id        UUID NOT NULL REFERENCES users(id),
    title           TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    visibility      TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private', 'org')),
    current_version INT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX artifacts_owner_idx ON artifacts (owner_id, updated_at DESC);
CREATE INDEX artifacts_visibility_idx ON artifacts (visibility, updated_at DESC);

-- Each version is a manifest of path -> content-addressed blob (artifacts/<id>/blobs/<sha256> in storage).
CREATE TABLE artifact_versions (
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    version     INT NOT NULL,
    files       JSONB NOT NULL,
    label       TEXT,
    created_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (artifact_id, version)
);

-- principal is a lower-cased email (principal_type 'user') or an Entra group object id ('group').
CREATE TABLE artifact_shares (
    artifact_id    TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    principal_type TEXT NOT NULL CHECK (principal_type IN ('user', 'group')),
    principal      TEXT NOT NULL,
    role           TEXT NOT NULL CHECK (role IN ('viewer', 'editor')),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (artifact_id, principal_type, principal)
);
CREATE INDEX artifact_shares_principal_idx ON artifact_shares (principal_type, principal);

-- Shared state for artifact pages: JSON documents grouped into collections, per artifact.
CREATE TABLE app_data (
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    collection  TEXT NOT NULL,
    doc_id      TEXT NOT NULL,
    data        JSONB NOT NULL,
    version     BIGINT NOT NULL DEFAULT 1,
    created_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    updated_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (artifact_id, collection, doc_id)
);
CREATE INDEX app_data_order_idx ON app_data (artifact_id, collection, created_at, doc_id);
