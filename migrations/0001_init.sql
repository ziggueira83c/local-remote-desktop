-- LRD: esquema local (cada computador tem seu próprio banco, sem sincronização central).
-- Nunca gravar frames, teclas, senhas ou chaves privadas aqui.
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY,
    hostname TEXT NOT NULL,
    display_name TEXT NOT NULL,
    os_type TEXT NOT NULL CHECK(os_type IN ('windows','macos','other')),
    public_key BLOB NOT NULL,          -- certificado DER do par
    fingerprint TEXT NOT NULL,         -- SHA-256 do certificado (hex)
    last_ip TEXT,
    last_port INTEGER CHECK(last_port BETWEEN 1 AND 65535),
    favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1)),
    last_seen_at TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS trusted_peers (
    id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    public_key BLOB NOT NULL,
    fingerprint TEXT NOT NULL,
    permissions_json TEXT NOT NULL,
    revoked_at TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY(device_id) REFERENCES devices(id) ON DELETE CASCADE,
    UNIQUE(device_id, fingerprint)
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    remote_device_id TEXT,
    remote_name TEXT,
    remote_address TEXT,
    direction TEXT NOT NULL CHECK(direction IN ('inbound','outbound')),
    status TEXT NOT NULL CHECK(status IN (
        'connecting','authenticating','awaiting_approval',
        'active','reconnecting','closed','failed')),
    permissions_json TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    error_code TEXT,
    FOREIGN KEY(remote_device_id) REFERENCES devices(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS transfers (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    filename TEXT NOT NULL,
    total_bytes INTEGER NOT NULL CHECK(total_bytes >= 0),
    transferred_bytes INTEGER NOT NULL DEFAULT 0 CHECK(transferred_bytes >= 0),
    sha256 TEXT,
    state TEXT NOT NULL CHECK(state IN ('pending','active','completed','failed','cancelled')),
    created_at TEXT NOT NULL,
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT,
    event_type TEXT NOT NULL,
    details_json TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_devices_last_seen ON devices(last_seen_at);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_transfers_session ON transfers(session_id);
CREATE INDEX IF NOT EXISTS idx_audit_session_time ON audit_events(session_id, created_at);
