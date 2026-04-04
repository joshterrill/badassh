use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedConnection {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthMethod {
    Password,
    KeyFile(String),
    DefaultKey,
}

impl SavedConnection {
    pub fn new(
        name: String,
        host: String,
        port: u16,
        username: String,
        auth_method: AuthMethod,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            host,
            port,
            username,
            auth_method,
            created_at: Utc::now(),
            last_used_at: None,
        }
    }
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn new() -> Result<Self> {
        let db_path = Self::get_db_path()?;
        
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        let conn = Connection::open(&db_path)?;
        let db = Self { conn };
        db.init_tables()?;
        Ok(db)
    }
    
    fn get_db_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?;
        Ok(config_dir.join("rust-sftp").join("connections.db"))
    }
    
    fn init_tables(&self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS connections (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                host TEXT NOT NULL,
                port INTEGER NOT NULL,
                username TEXT NOT NULL,
                auth_method TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_used_at TEXT
            )",
            [],
        )?;
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            [],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM settings WHERE key = ?1")?;
        let mut rows = stmt.query_map([key], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
    
    pub fn save_connection(&self, conn: &SavedConnection) -> Result<()> {
        let auth_method_json = serde_json::to_string(&conn.auth_method)?;
        
        self.conn.execute(
            "INSERT OR REPLACE INTO connections 
             (id, name, host, port, username, auth_method, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                conn.id,
                conn.name,
                conn.host,
                conn.port,
                conn.username,
                auth_method_json,
                conn.created_at.to_rfc3339(),
                conn.last_used_at.map(|dt| dt.to_rfc3339()),
            ],
        )?;
        Ok(())
    }
    
    pub fn get_all_connections(&self) -> Result<Vec<SavedConnection>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, host, port, username, auth_method, created_at, last_used_at 
             FROM connections ORDER BY name"
        )?;
        
        let connections = stmt.query_map([], |row| {
            let auth_method_json: String = row.get(5)?;
            let auth_method: AuthMethod = serde_json::from_str(&auth_method_json)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                    5, rusqlite::types::Type::Text, Box::new(e)
                ))?;
            
            let created_at_str: String = row.get(6)?;
            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                    6, rusqlite::types::Type::Text, Box::new(e)
                ))?
                .with_timezone(&Utc);
            
            let last_used_at: Option<String> = row.get(7)?;
            let last_used_at = last_used_at.and_then(|s| {
                DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))
            });
            
            Ok(SavedConnection {
                id: row.get(0)?,
                name: row.get(1)?,
                host: row.get(2)?,
                port: row.get(3)?,
                username: row.get(4)?,
                auth_method,
                created_at,
                last_used_at,
            })
        })?;
        
        connections.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
    
    pub fn get_recent_connections(&self, limit: usize) -> Result<Vec<SavedConnection>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, host, port, username, auth_method, created_at, last_used_at 
             FROM connections 
             WHERE last_used_at IS NOT NULL
             ORDER BY last_used_at DESC
             LIMIT ?1"
        )?;
        
        let connections = stmt.query_map([limit], |row| {
            let auth_method_json: String = row.get(5)?;
            let auth_method: AuthMethod = serde_json::from_str(&auth_method_json)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                    5, rusqlite::types::Type::Text, Box::new(e)
                ))?;
            
            let created_at_str: String = row.get(6)?;
            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                    6, rusqlite::types::Type::Text, Box::new(e)
                ))?
                .with_timezone(&Utc);
            
            let last_used_at: Option<String> = row.get(7)?;
            let last_used_at = last_used_at.and_then(|s| {
                DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))
            });
            
            Ok(SavedConnection {
                id: row.get(0)?,
                name: row.get(1)?,
                host: row.get(2)?,
                port: row.get(3)?,
                username: row.get(4)?,
                auth_method,
                created_at,
                last_used_at,
            })
        })?;
        
        connections.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
    
    pub fn update_last_used(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE connections SET last_used_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }
    
    #[allow(dead_code)]
    pub fn delete_connection(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM connections WHERE id = ?1", [id])?;
        Ok(())
    }
}
