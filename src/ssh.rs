use anyhow::{Context, Result};
use log::{debug, error, info};
use ssh2::{Session, Sftp};
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;

pub struct SshConnection {
    session: Session,
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

impl SshConnection {
    pub fn connect(params: &ConnectionParams) -> Result<Self> {
        let addr = format!("{}:{}", params.host, params.port);
        info!("Connecting to SSH: {}@{}", params.username, addr);

        let tcp =
            TcpStream::connect(&addr).with_context(|| format!("Failed to connect to {}", addr))?;

        debug!("TCP connection established");

        let mut session = Session::new().context("Failed to create SSH session")?;

        session.set_tcp_stream(tcp);
        session.handshake().context("SSH handshake failed")?;

        debug!("SSH handshake completed");

        // Try authentication methods in order of preference
        if let Some(ref password) = params.password {
            debug!("Attempting password authentication");
            session
                .userauth_password(&params.username, password)
                .context("Password authentication failed")?;
        } else if let Some(ref key_path) = params.key_path {
            debug!("Attempting key file authentication: {}", key_path);
            let key_path = PathBuf::from(key_path);
            Self::try_key_file(&mut session, &params.username, &key_path)?;
        } else {
            debug!("Attempting auto authentication (agent + default keys)");
            Self::try_auto_auth(&mut session, &params.username)?;
        }

        if !session.authenticated() {
            error!(
                "SSH authentication failed for {}@{}",
                params.username, params.host
            );
            anyhow::bail!("Authentication failed");
        }

        info!(
            "SSH connected successfully to {}@{}",
            params.username, params.host
        );

        Ok(Self {
            session,
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
                }
            }
        } else {
            errors.push("SSH agent: SSH_AUTH_SOCK not set".to_string());
        }

        // Try default key files
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
        let ssh_dir = home.join(".ssh");

        let key_names = ["id_ed25519", "id_rsa", "id_ecdsa", "id_dsa"];

        for key_name in &key_names {
            let key_path = ssh_dir.join(key_name);
            if key_path.exists() {
                match session.userauth_pubkey_file(username, None, &key_path, None) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        errors.push(format!("{}: {}", key_path.display(), e));
                    }
                }
            }
        }

        // Check if we found any keys at all
        let keys_found: Vec<_> = key_names
            .iter()
            .filter(|k| ssh_dir.join(k).exists())
            .collect();

        if keys_found.is_empty() {
            errors.push(format!("No SSH keys found in {}", ssh_dir.display()));
        }

        // Build detailed error message
        let error_details = errors.join("\n  - ");
        anyhow::bail!(
            "Authentication failed. Tried methods:\n  - {}\n\nHint: Your keys may require a passphrase. Try running 'ssh-add' first to add your key to the agent.",
            error_details
        )
    }

    pub fn exec(&self, command: &str) -> Result<String> {
        let mut channel = self
            .session
            .channel_session()
            .context("Failed to open channel")?;

        channel.exec(command).context("Failed to execute command")?;

        // Read both stdout and stderr
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

        // Combine stdout and stderr for full output
        if stderr.is_empty() {
            Ok(stdout)
        } else if stdout.is_empty() {
            Ok(stderr)
        } else {
            Ok(format!("{}\n{}", stdout, stderr))
        }
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
        self.session.authenticated()
    }

    #[allow(dead_code)]
    pub fn disconnect(self) {
        let _ = self.session.disconnect(None, "User disconnected", None);
    }

    pub fn sftp(&self) -> Result<Sftp> {
        self.session
            .sftp()
            .context("Failed to create SFTP subsystem")
    }
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        let _ = self.session.disconnect(None, "Connection closed", None);
    }
}
