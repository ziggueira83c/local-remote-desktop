//! Persistência local em SQLite.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../../../migrations/0001_init.sql"))];

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub id: String,
    pub hostname: String,
    pub display_name: String,
    pub os_type: String,
    pub fingerprint: String,
    pub last_ip: Option<String>,
    pub last_port: Option<u16>,
    pub favorite: bool,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    /// Há uma senha deste dispositivo lembrada no cofre do sistema (preenchido pela sessão).
    #[serde(default)]
    pub saved_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: String,
    pub remote_device_id: Option<String>,
    pub remote_name: Option<String>,
    pub remote_address: Option<String>,
    pub direction: String,
    pub status: String,
    pub permissions: Vec<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub error_code: Option<String>,
}

pub struct NewDevice<'a> {
    pub id: &'a str,
    pub hostname: &'a str,
    pub display_name: &'a str,
    pub os_type: &'a str,
    pub cert_der: &'a [u8],
    pub fingerprint: &'a str,
    pub last_ip: Option<&'a str>,
    pub last_port: Option<u16>,
}

pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("abrindo {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;
        let current: i64 = conn
            .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| r.get(0))?;
        for (version, sql) in MIGRATIONS {
            if *version > current {
                conn.execute_batch(sql).with_context(|| format!("migração {version}"))?;
                conn.execute("INSERT INTO schema_version (version) VALUES (?1)", params![version])?;
            }
        }
        // Sessões que ficaram abertas por um encerramento abrupto do app.
        conn.execute(
            "UPDATE sessions SET status = 'failed', ended_at = ?1, error_code = 'app_terminated'
             WHERE status NOT IN ('closed','failed')",
            params![now()],
        )?;
        Ok(Storage { conn: Mutex::new(conn) })
    }

    // ---------- dispositivos ----------

    pub fn get_device(&self, id: &str) -> Result<Option<DeviceRecord>> {
        let conn = self.conn.lock().unwrap();
        let rec = conn
            .query_row(
                "SELECT id, hostname, display_name, os_type, fingerprint, last_ip, last_port, favorite,
                        last_seen_at, created_at FROM devices WHERE id = ?1",
                params![id],
                row_to_device,
            )
            .optional()?;
        Ok(rec)
    }

    /// Insere ou atualiza um dispositivo conhecido. O fingerprint só é gravado na criação:
    /// uma troca de fingerprint deve ser detectada e recusada antes de chegar aqui.
    pub fn upsert_device(&self, d: &NewDevice<'_>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let ts = now();
        conn.execute(
            "INSERT INTO devices (id, hostname, display_name, os_type, public_key, fingerprint,
                                  last_ip, last_port, favorite, last_seen_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?9)
             ON CONFLICT(id) DO UPDATE SET
                hostname = excluded.hostname,
                display_name = excluded.display_name,
                os_type = excluded.os_type,
                last_ip = COALESCE(excluded.last_ip, devices.last_ip),
                last_port = COALESCE(excluded.last_port, devices.last_port),
                last_seen_at = excluded.last_seen_at",
            params![
                d.id,
                d.hostname,
                d.display_name,
                normalize_os(d.os_type),
                d.cert_der,
                d.fingerprint,
                d.last_ip,
                d.last_port,
                ts
            ],
        )?;
        Ok(())
    }

    pub fn list_devices(&self) -> Result<Vec<DeviceRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, hostname, display_name, os_type, fingerprint, last_ip, last_port, favorite,
                    last_seen_at, created_at FROM devices
             ORDER BY favorite DESC, last_seen_at DESC LIMIT 100",
        )?;
        let rows = stmt.query_map([], row_to_device)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set_favorite(&self, id: &str, favorite: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE devices SET favorite = ?2 WHERE id = ?1", params![id, favorite as i64])?;
        Ok(())
    }

    pub fn delete_device(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM devices WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ---------- sessões ----------

    pub fn insert_session(
        &self,
        id: &str,
        direction: &str,
        status: &str,
        remote_name: Option<&str>,
        remote_address: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id, direction, status, remote_name, remote_address, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, direction, status, remote_name, remote_address, now()],
        )?;
        Ok(())
    }

    pub fn session_activated(&self, id: &str, remote_device_id: &str, permissions: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET status = 'active', remote_device_id = ?2, permissions_json = ?3 WHERE id = ?1",
            params![id, remote_device_id, serde_json::to_string(permissions)?],
        )?;
        Ok(())
    }

    pub fn session_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE sessions SET status = ?2 WHERE id = ?1", params![id, status])?;
        Ok(())
    }

    pub fn session_ended(&self, id: &str, status: &str, error_code: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET status = ?2, ended_at = ?3, error_code = ?4
             WHERE id = ?1 AND status NOT IN ('closed','failed')",
            params![id, status, now(), error_code],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self, limit: u32) -> Result<Vec<SessionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, remote_device_id, remote_name, remote_address, direction, status,
                    permissions_json, started_at, ended_at, error_code
             FROM sessions ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                let perms: Option<String> = r.get(6)?;
                Ok(SessionRecord {
                    id: r.get(0)?,
                    remote_device_id: r.get(1)?,
                    remote_name: r.get(2)?,
                    remote_address: r.get(3)?,
                    direction: r.get(4)?,
                    status: r.get(5)?,
                    permissions: perms.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default(),
                    started_at: r.get(7)?,
                    ended_at: r.get(8)?,
                    error_code: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn clear_history(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE status IN ('closed','failed')", [])?;
        Ok(())
    }

    // ---------- auditoria ----------

    pub fn audit(&self, session_id: Option<&str>, event_type: &str, details: serde_json::Value) {
        let conn = self.conn.lock().unwrap();
        if let Err(e) = conn.execute(
            "INSERT INTO audit_events (session_id, event_type, details_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, event_type, details.to_string(), now()],
        ) {
            tracing::warn!("falha ao gravar auditoria: {e}");
        }
    }

    // ---------- configurações ----------

    pub fn get_setting<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row("SELECT value_json FROM settings WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?;
        Ok(match raw {
            Some(s) => serde_json::from_str(&s).ok(),
            None => None,
        })
    }

    pub fn put_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value_json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
            params![key, serde_json::to_string(value)?, now()],
        )?;
        Ok(())
    }
}

fn normalize_os(os: &str) -> &'static str {
    match os {
        "windows" => "windows",
        "macos" => "macos",
        _ => "other",
    }
}

fn row_to_device(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRecord> {
    let port: Option<i64> = r.get(6)?;
    let fav: i64 = r.get(7)?;
    Ok(DeviceRecord {
        id: r.get(0)?,
        hostname: r.get(1)?,
        display_name: r.get(2)?,
        os_type: r.get(3)?,
        fingerprint: r.get(4)?,
        last_ip: r.get(5)?,
        last_port: port.and_then(|p| u16::try_from(p).ok()),
        favorite: fav != 0,
        last_seen_at: r.get(8)?,
        created_at: r.get(9)?,
        saved_password: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_and_session_flow() {
        let s = Storage::open_in_memory().unwrap();
        s.upsert_device(&NewDevice {
            id: "d1",
            hostname: "h",
            display_name: "PC",
            os_type: "windows",
            cert_der: &[1, 2, 3],
            fingerprint: "ab",
            last_ip: Some("192.168.0.2"),
            last_port: Some(47321),
        })
        .unwrap();
        assert_eq!(s.get_device("d1").unwrap().unwrap().fingerprint, "ab");
        s.insert_session("s1", "outbound", "connecting", Some("PC"), Some("192.168.0.2")).unwrap();
        s.session_activated("s1", "d1", &["view_screen".into()]).unwrap();
        s.session_ended("s1", "closed", None).unwrap();
        let list = s.list_sessions(10).unwrap();
        assert_eq!(list[0].status, "closed");
        s.put_setting("k", &42).unwrap();
        assert_eq!(s.get_setting::<i32>("k").unwrap(), Some(42));
    }
}
