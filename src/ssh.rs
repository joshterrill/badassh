use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use ssh2::{Session, Sftp};
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const SSH_KEEPALIVE_INTERVAL_SECS: u32 = 30;

pub struct SshConnection {
    session: Mutex<Session>,
    params: ConnectionParams,
    #[allow(dead_code)]
    pub host: String,
    #[allow(dead_code)]
    pub port: u16,
    #[allow(dead_code)]
    pub username: String,
}

#[derive(Debug, Clone)]
pub struct ConnectionParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub key_path: Option<String>,
}

pub(crate) fn open_ssh_session(params: &ConnectionParams, blocking: bool) -> Result<Session> {
    let addr = format!("{}:{}", params.host, params.port);
    let tcp =
        TcpStream::connect(&addr).with_context(|| format!("Failed to connect to {}", addr))?;

    debug!("TCP connection established to {}", addr);

    let mut session = Session::new().context("Failed to create SSH session")?;
    session.set_tcp_stream(tcp);
    session.handshake().context("SSH handshake failed")?;
    session.set_keepalive(false, SSH_KEEPALIVE_INTERVAL_SECS);

    debug!("SSH handshake completed");

    if let Some(ref password) = params.password {
        debug!("Attempting password authentication");
        session
            .userauth_password(&params.username, password)
            .context("Password authentication failed")?;
    } else if let Some(ref key_path) = params.key_path {
        debug!("Attempting key file authentication: {}", key_path);
        let key_path = PathBuf::from(key_path);
        SshConnection::try_key_file(&mut session, &params.username, &key_path)?;
    } else {
        debug!("Attempting auto authentication (agent + default keys)");
        SshConnection::try_auto_auth(&mut session, &params.username)?;
    }

    if !session.authenticated() {
        error!(
            "SSH authentication failed for {}@{}",
            params.username, params.host
        );
        anyhow::bail!("Authentication failed");
    }

    session.set_blocking(blocking);
    Ok(session)
}

impl SshConnection {
    pub fn connect(params: &ConnectionParams) -> Result<Self> {
        let addr = format!("{}:{}", params.host, params.port);
        info!("Connecting to SSH: {}@{}", params.username, addr);

        let session = open_ssh_session(params, true)?;

        info!(
            "SSH connected successfully to {}@{}",
            params.username, params.host
        );

        Ok(Self {
            session: Mutex::new(session),
            params: params.clone(),
            host: params.host.clone(),
            port: params.port,
            username: params.username.clone(),
        })
    }

    fn try_key_file(session: &mut Session, username: &str, key_path: &PathBuf) -> Result<()> {
        // Try without passphrase first
        if session
            .userauth_pubkey_file(username, None, key_path, None)
            .is_ok()
        {
            return Ok(());
        }

        // Key might be encrypted - we can't prompt for passphrase in TUI yet
        anyhow::bail!(
            "Key authentication failed for {}. The key may require a passphrase.",
            key_path.display()
        )
    }

    fn try_auto_auth(session: &mut Session, username: &str) -> Result<()> {
        let mut errors = Vec::new();
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
        let ssh_dir = home.join(".ssh");
        let key_names = ["id_ed25519", "id_rsa", "id_ecdsa", "id_dsa"];
        let default_keys: Vec<PathBuf> = key_names
            .iter()
            .map(|name| ssh_dir.join(name))
            .filter(|path| path.exists())
            .collect();

        // Try SSH agent first - this is what usually works in terminal
        // because the agent has your unlocked keys loaded
        if let Some(auth_sock) = std::env::var_os("SSH_AUTH_SOCK") {
            match session.userauth_agent(username) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    errors.push(format!(
                        "SSH agent ({}): {}",
                        auth_sock.to_string_lossy(),
                        e
                    ));
                    #[cfg(target_os = "macos")]
                    {
                        match Self::try_load_macos_keychain_identities(&default_keys) {
                            Ok(true) => match session.userauth_agent(username) {
                                Ok(()) => return Ok(()),
                                Err(retry_err) => errors.push(format!(
                                    "SSH agent after macOS keychain load: {}",
                                    retry_err
                                )),
                            },
                            Ok(false) => {}
                            Err(load_err) => {
                                errors.push(format!("macOS keychain agent load: {}", load_err))
                            }
                        }
                    }
                }
            }
        } else {
            errors.push("SSH agent: SSH_AUTH_SOCK not set".to_string());
        }

        // Try default key files
        for key_path in &default_keys {
            match session.userauth_pubkey_file(username, None, key_path, None) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    errors.push(format!("{}: {}", key_path.display(), e));
                }
            }
        }

        if default_keys.is_empty() {
            errors.push(format!("No SSH keys found in {}", ssh_dir.display()));
        }

        // Build detailed error message
        let error_details = errors.join("\n  - ");
        anyhow::bail!(
            "Authentication failed. Tried methods:\n  - {}\n\nHint: Your keys may require a passphrase. Try running 'ssh-add' first to add your key to the agent.",
            error_details
        )
    }

    #[cfg(target_os = "macos")]
    fn try_load_macos_keychain_identities(key_paths: &[PathBuf]) -> Result<bool> {
        if key_paths.is_empty() {
            return Ok(false);
        }

        debug!("Attempting to load SSH identities from macOS keychain");
        let status = Command::new("/usr/bin/ssh-add")
            .arg("--apple-load-keychain")
            .args(key_paths)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("Failed to invoke ssh-add for macOS keychain load")?;

        Ok(status.success())
    }

    fn reconnect(&self) -> Result<()> {
        info!(
            "Reconnecting SSH session for {}@{}:{}",
            self.params.username, self.params.host, self.params.port
        );
        let replacement = open_ssh_session(&self.params, true)?;
        let mut session = self.session.lock();
        let _ = session.disconnect(None, "Reconnecting", None);
        *session = replacement;
        Ok(())
    }

    fn with_reconnect<T, F>(&self, operation_name: &str, mut operation: F) -> Result<T>
    where
        F: FnMut(&Session) -> Result<T>,
    {
        {
            let session = self.session.lock();
            match operation(&session) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    warn!(
                        "{} failed for {}@{}:{}; attempting reconnect: {}",
                        operation_name,
                        self.params.username,
                        self.params.host,
                        self.params.port,
                        err
                    );
                }
            }
        }

        self.reconnect()
            .with_context(|| format!("{} failed and reconnect was unsuccessful", operation_name))?;

        let session = self.session.lock();
        operation(&session).with_context(|| format!("{} failed after reconnect", operation_name))
    }

    pub fn exec(&self, command: &str) -> Result<String> {
        self.with_reconnect("SSH command execution", |session| {
            let mut channel = session
                .channel_session()
                .context("Failed to open channel")?;

            channel.exec(command).context("Failed to execute command")?;

            let mut stdout = String::new();
            channel
                .read_to_string(&mut stdout)
                .context("Failed to read command stdout")?;

            let mut stderr = String::new();
            channel
                .stderr()
                .read_to_string(&mut stderr)
                .context("Failed to read command stderr")?;

            channel.wait_close()?;

            if stderr.is_empty() {
                Ok(stdout)
            } else if stdout.is_empty() {
                Ok(stderr)
            } else {
                Ok(format!("{}\n{}", stdout, stderr))
            }
        })
    }

    pub fn get_remote_pwd(&self) -> Result<String> {
        self.exec("pwd").map(|s| s.trim().to_string())
    }

    #[allow(dead_code)]
    pub fn list_directory(&self, path: &str) -> Result<String> {
        self.exec(&format!("ls -la {}", path))
    }

    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        self.session.lock().authenticated()
    }

    #[allow(dead_code)]
    pub fn disconnect(self) {
        let _ = self
            .session
            .lock()
            .disconnect(None, "User disconnected", None);
    }

    pub fn sftp(&self) -> Result<Sftp> {
        self.with_reconnect("SFTP session creation", |session| {
            session.sftp().context("Failed to create SFTP subsystem")
        })
    }
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        let _ = self
            .session
            .lock()
            .disconnect(None, "Connection closed", None);
    }
}
