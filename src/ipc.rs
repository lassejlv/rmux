use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::protocol::{ClientRequest, ServerResponse};

pub struct ServerClient {
    stream: UnixStream,
}

impl ServerClient {
    pub fn request(&mut self, request: ClientRequest) -> Result<ServerResponse> {
        write_json(&mut self.stream, &request)?;
        read_json(&mut self.stream)
    }

    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.stream
            .set_read_timeout(timeout)
            .context("set read timeout")
    }
}

pub fn connect_session(session: &str) -> Result<ServerClient> {
    let stream = UnixStream::connect(socket_path(session))
        .with_context(|| format!("connect rmux session {session}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .context("set default read timeout")?;
    Ok(ServerClient { stream })
}

pub fn list_session_names() -> Result<Vec<String>> {
    let dir = socket_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("sock")
            && let Some(name) = path.file_stem().and_then(|name| name.to_str())
        {
            if UnixStream::connect(&path).is_ok() {
                names.push(name.to_string());
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
    names.sort();
    Ok(names)
}

pub fn socket_path(session: &str) -> PathBuf {
    socket_dir().join(format!("{}.sock", sanitized_session_name(session)))
}

pub fn prepare_socket_path(session: &str) -> Result<PathBuf> {
    let socket = socket_path(session);
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).context("create rmux socket dir")?;
        let metadata = fs::symlink_metadata(parent).context("inspect rmux socket dir")?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(anyhow!(
                "rmux socket path is not a secure directory: {}",
                parent.display()
            ));
        }
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .context("secure rmux socket dir")?;
    }
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            return Err(anyhow!("rmux session {session} is already running"));
        }
        fs::remove_file(&socket).context("remove stale rmux socket")?;
    }
    Ok(socket)
}

pub fn secure_socket_path(socket: &Path) -> Result<()> {
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .context("secure rmux server socket")
}

pub fn socket_dir() -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from("/tmp").join(format!("rmux-{user}"))
}

pub fn write_json<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).context("encode rmux message")?;
    let len: u32 = bytes
        .len()
        .try_into()
        .map_err(|_| anyhow!("rmux message too large"))?;
    stream
        .write_all(&len.to_be_bytes())
        .context("write rmux message length")?;
    stream
        .write_all(&bytes)
        .context("write rmux message body")?;
    Ok(())
}

pub fn read_json<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T> {
    const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
    let mut len = [0; 4];
    stream
        .read_exact(&mut len)
        .context("read rmux message length")?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(anyhow!("rmux message too large: {len} bytes"));
    }
    let mut bytes = vec![0; len];
    stream
        .read_exact(&mut bytes)
        .context("read rmux message body")?;
    serde_json::from_slice(&bytes).context("decode rmux message")
}

pub fn sanitized_session_name(session: &str) -> String {
    let sanitized = session
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn sanitizes_session_names_for_socket_files() {
        assert_eq!(sanitized_session_name("work"), "work");
        assert_eq!(sanitized_session_name("team/work 1"), "team_work_1");
        assert_eq!(sanitized_session_name(""), "default");
    }

    #[test]
    fn prepare_socket_path_refuses_live_session() {
        let session = format!("rmux-test-{}", std::process::id());
        let socket = prepare_socket_path(&session).expect("prepare first socket");
        let dir_mode = fs::metadata(socket.parent().expect("socket parent"))
            .expect("socket dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);

        let listener = UnixListener::bind(&socket).expect("bind live test socket");
        secure_socket_path(&socket).expect("secure live test socket");
        let socket_mode = fs::metadata(&socket)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(socket_mode, 0o600);

        let error = prepare_socket_path(&session).expect_err("live socket should be refused");
        assert!(error.to_string().contains("already running"));

        drop(listener);
        let _ = fs::remove_file(socket);
    }
}
