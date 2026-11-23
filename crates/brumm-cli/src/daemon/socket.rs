//! The daemon's local endpoint: a Unix socket (mode 0600 in a 0700 directory, and only
//! connections from our own user) or, on Windows, a named pipe that refuses remote clients.
//! No TCP: the daemon is never reachable from the network.

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix {
    use std::{
        fs, io,
        os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
    };

    use tokio::net::{UnixListener, UnixStream};

    const SOCKET_MODE: u32 = 0o600;
    const DIR_MODE: u32 = 0o700;

    pub type ServerStream = UnixStream;
    pub type ClientStream = UnixStream;

    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
        owner: u32,
    }

    /// Call only while holding the daemon lock: an existing socket file is then stale.
    pub fn bind(path: &Path) -> io::Result<Listener> {
        if let Some(dir) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(dir)?;
        }
        match fs::remove_file(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
        let inner = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))?;
        let owner = fs::metadata(path)?.uid();
        Ok(Listener {
            inner,
            path: path.to_owned(),
            owner,
        })
    }

    impl Listener {
        /// The next connection from our own user (others are dropped).
        pub async fn accept(&self) -> io::Result<ServerStream> {
            loop {
                let (stream, _) = self.inner.accept().await?;
                match stream.peer_cred() {
                    Ok(peer) if peer.uid() == self.owner => return Ok(stream),
                    Ok(peer) => tracing::warn!(uid = peer.uid(), "refused a connection from another user"),
                    Err(err) => tracing::warn!("refused a connection without credentials: {err}"),
                }
            }
        }

        pub fn remove(&self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    pub async fn connect(path: &Path) -> io::Result<ClientStream> {
        UnixStream::connect(path).await
    }
}

#[cfg(windows)]
mod windows {
    use std::{io, path::Path, time::Duration};

    use tokio::{
        net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
        sync::Mutex,
    };

    /// `ERROR_PIPE_BUSY`: every instance is taken; try again shortly.
    const ERROR_PIPE_BUSY: i32 = 231;
    const BUSY_RETRY: Duration = Duration::from_millis(50);

    pub type ServerStream = NamedPipeServer;
    pub type ClientStream = NamedPipeClient;

    pub struct Listener {
        name: String,
        next: Mutex<NamedPipeServer>,
    }

    pub fn bind(path: &Path) -> io::Result<Listener> {
        let name = path.to_string_lossy().into_owned();
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)?;
        Ok(Listener {
            name,
            next: Mutex::new(first),
        })
    }

    impl Listener {
        pub async fn accept(&self) -> io::Result<ServerStream> {
            let mut next = self.next.lock().await;
            next.connect().await?;
            let fresh = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            Ok(std::mem::replace(&mut *next, fresh))
        }

        pub fn remove(&self) {}
    }

    pub async fn connect(path: &Path) -> io::Result<ClientStream> {
        let name = path.to_string_lossy().into_owned();
        loop {
            match ClientOptions::new().open(&name) {
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(BUSY_RETRY).await;
                }
                other => return other,
            }
        }
    }
}
