//! A drag's files, over a data connection to the worker: received into a staging folder
//! (a drag from the host), or sent from where they are (a drag out to the host).

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddrV4, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use agent_proto::{Chunk, Frame, MAX_DATA, Message, Parser, path};

/// Where data connections go, and the token they show (the worker's [`Message::Config`]).
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub address: SocketAddrV4,
    pub token: [u8; 16],
}

/// The transfer the host called off last.
pub static CANCELLED: AtomicU32 = AtomicU32::new(0);

/// A data connection for transfer `transfer`.
pub fn connect(config: &Config, transfer: u32) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(&config.address.into(), Duration::from_secs(5))?;
    stream.set_nodelay(true)?;
    stream.write_all(
        &Message::Attach {
            token: config.token,
            transfer,
        }
        .frame(),
    )?;
    Ok(stream)
}

/// The folder drags from the host are staged in: `~/.cache/vmmc`.
pub fn staging_root() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    cache.join("vmmc")
}

/// Removes staged drags: all of them, or those older than an hour (a program may still
/// be copying from a recent one).
pub fn clean_staging(all: bool) {
    let Ok(entries) = fs::read_dir(staging_root()) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        if (all || old) && entry.file_name().to_string_lossy().starts_with("drop-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Messages off a data connection.
struct Incoming {
    stream: TcpStream,
    parser: Parser,
    pending: VecDeque<Message>,
}

impl Incoming {
    fn next(&mut self) -> Option<Message> {
        let mut buffer = vec![0u8; 64 << 10];
        loop {
            if let Some(message) = self.pending.pop_front() {
                return Some(message);
            }
            let read = self.stream.read(&mut buffer).ok().filter(|&n| n > 0)?;
            for frame in self.parser.feed(&buffer[..read]) {
                if let Frame::Message(message) = frame {
                    self.pending.push_back(message);
                }
            }
        }
    }
}

/// Receives transfer `transfer`'s files into `staging`, then tells the worker how that
/// went. Returns how many files and bytes came.
pub fn receive(stream: TcpStream, transfer: u32, staging: &Path) -> Result<(u64, u64), String> {
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut incoming = Incoming {
        stream,
        parser: Parser::default(),
        pending: VecDeque::new(),
    };
    let mut current: Option<(File, u64, u64)> = None;
    let (mut files, mut bytes) = (0, 0);
    let result = loop {
        let Some(message) = incoming.next() else {
            break Err("the connection closed early".to_owned());
        };
        let Message::Chunk {
            transfer: id,
            chunk,
        } = message
        else {
            continue;
        };
        if id != transfer {
            continue;
        }
        let step = (|| -> Result<bool, String> {
            match chunk {
                Chunk::Total { .. } => {}
                Chunk::Begin {
                    path: relative,
                    dir,
                    size,
                } => {
                    current = None;
                    let names = path::check(&relative)?;
                    let target = names
                        .iter()
                        .fold(staging.to_owned(), |path, name| path.join(name));
                    if dir {
                        fs::create_dir_all(&target)
                            .map_err(|e| format!("can't make {relative}: {e}"))?;
                    } else {
                        let file = File::create(&target)
                            .map_err(|e| format!("can't make {relative}: {e}"))?;
                        current = Some((file, size, 0));
                        files += 1;
                    }
                }
                Chunk::Data(data) => {
                    let (file, size, written) =
                        current.as_mut().ok_or("file data before its file")?;
                    *written += data.len() as u64;
                    if *written > *size {
                        return Err("more of a file came than the host said".into());
                    }
                    file.write_all(&data)
                        .map_err(|e| format!("can't write a file: {e}"))?;
                    bytes += data.len() as u64;
                }
                Chunk::End => current = None,
                Chunk::Finish { error } => {
                    return match error {
                        Some(error) => Err(error),
                        None => Ok(true),
                    };
                }
            }
            Ok(false)
        })();
        match step {
            Ok(true) => break Ok((files, bytes)),
            Ok(false) => {}
            Err(e) => break Err(e),
        }
    };
    drop(current);
    let receipt = Message::Received {
        transfer,
        error: result.as_ref().err().cloned(),
    };
    let _ = writer.write_all(&receipt.frame());
    let _ = writer.shutdown(Shutdown::Write);
    // Until the worker closes, so the receipt gets there.
    let mut drain = [0u8; 256];
    while matches!(writer.read(&mut drain), Ok(n) if n > 0) {}
    result
}

/// A file or folder to send, and its path in the transfer.
struct Entry {
    path: PathBuf,
    relative: String,
    dir: bool,
    size: u64,
}

/// `path` and what's in it, folders before their contents, with names Windows can hold.
/// Links to folders inside aren't followed (they could loop).
fn walk(path: &Path, relative: String, entries: &mut Vec<Entry>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let dir = metadata.is_dir();
    if !dir && !metadata.is_file() {
        // Devices, sockets and pipes stay.
        return;
    }
    entries.push(Entry {
        path: path.to_owned(),
        relative: relative.clone(),
        dir,
        size: if dir { 0 } else { metadata.size() },
    });
    if !dir {
        return;
    }
    let Ok(children) = fs::read_dir(path) else {
        return;
    };
    let mut children: Vec<PathBuf> = children.flatten().map(|entry| entry.path()).collect();
    children.sort();
    for child in children {
        let linked = fs::symlink_metadata(&child).is_ok_and(|m| m.file_type().is_symlink());
        if linked && child.is_dir() {
            continue;
        }
        let name = child
            .file_name()
            .map(|name| path::fix_name(&name.to_string_lossy()))
            .unwrap_or_default();
        walk(&child, format!("{relative}/{name}"), entries);
    }
}

/// Sends `paths` (and what's in the folders) as transfer `transfer`. Returns how many
/// files and bytes went.
pub fn send(mut stream: TcpStream, transfer: u32, paths: &[PathBuf]) -> Result<(u64, u64), String> {
    let mut entries = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map(|name| path::fix_name(&name.to_string_lossy()))
            .unwrap_or_else(|| "drop".into());
        walk(path, name, &mut entries);
    }
    let files = entries.iter().filter(|e| !e.dir).count() as u64;
    let bytes: u64 = entries.iter().map(|e| e.size).sum();
    let mut send = |chunk: Chunk| -> Result<(), String> {
        if CANCELLED.load(Ordering::Relaxed) == transfer {
            return Err("the host called it off".into());
        }
        stream
            .write_all(&Message::Chunk { transfer, chunk }.frame())
            .map_err(|e| format!("the connection failed: {e}"))
    };
    send(Chunk::Total { files, bytes })?;
    let mut buffer = vec![0u8; MAX_DATA];
    for entry in &entries {
        let mut file = if entry.dir {
            None
        } else {
            match File::open(&entry.path) {
                Ok(file) => Some(file),
                // Unreadable: the rest still goes.
                Err(_) => continue,
            }
        };
        send(Chunk::Begin {
            path: entry.relative.clone(),
            dir: entry.dir,
            size: entry.size,
        })?;
        if let Some(file) = &mut file {
            let mut left = entry.size;
            while left > 0 {
                let want = (left as usize).min(MAX_DATA);
                let read = match file.read(&mut buffer[..want]) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                send(Chunk::Data(buffer[..read].to_vec()))?;
                left -= read as u64;
            }
            send(Chunk::End)?;
        }
    }
    send(Chunk::Finish { error: None })?;
    let _ = stream.shutdown(Shutdown::Write);
    let mut drain = [0u8; 256];
    while matches!(stream.read(&mut drain), Ok(n) if n > 0) {}
    Ok((files, bytes))
}

/// The paths a `text/uri-list` names: its `file://` URIs, decoded.
pub fn uri_paths(list: &str) -> Vec<PathBuf> {
    list.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|uri| uri.strip_prefix("file://"))
        // file://host/path: the path starts at the next slash.
        .filter_map(|rest| rest.find('/').map(|at| &rest[at..]))
        .map(|path| {
            let bytes = path.as_bytes();
            let mut decoded = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%'
                    && i + 2 < bytes.len()
                    && let Ok(byte) = u8::from_str_radix(&path[i + 1..i + 3], 16)
                {
                    decoded.push(byte);
                    i += 3;
                    continue;
                }
                decoded.push(bytes[i]);
                i += 1;
            }
            PathBuf::from(
                <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(decoded),
            )
        })
        .collect()
}

/// `paths` as a `text/uri-list`.
pub fn uri_list(paths: &[PathBuf]) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut list = String::new();
    for path in paths {
        list.push_str("file://");
        for &byte in path.as_os_str().as_bytes() {
            if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
                list.push(byte as char);
            } else {
                list.push_str(&format!("%{byte:02X}"));
            }
        }
        list.push_str("\r\n");
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_round_trip() {
        let paths = vec![
            PathBuf::from("/home/vmmc/a b/ärger.txt"),
            PathBuf::from("/tmp/x%y"),
        ];
        let list = uri_list(&paths);
        assert_eq!(uri_paths(&list), paths);
        assert_eq!(
            uri_paths("# comment\r\nfile:///etc/hosts\r\nfile://host/srv/a\r\nhttp://x/y\r\n"),
            [PathBuf::from("/etc/hosts"), PathBuf::from("/srv/a")]
        );
    }
}
