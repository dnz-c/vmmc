//! A drag's files, between this host's disk and a running VM's guest, through the
//! worker's pipe (see `ipc`'s drag requests). Each transfer wants a connection of its
//! own: its requests wait for room or for data.
//!
//! - [`send`] walks the dragged files and folders and pushes them into a transfer.
//! - [`receive`] pulls a transfer's files into a folder, checking each name before it
//!   makes anything: they come from the guest.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use agent_proto::{Chunk, Item, MAX_DATA, path};
use ipc::{Request, Response};

use crate::Connection;

/// How much one [`Request::SendChunks`] carries: it has to fit a pipe message.
const BATCH: usize = 768 << 10;

/// How a transfer is going.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub files: u64,
    pub bytes: u64,
    /// What the whole transfer holds, once that's known.
    pub total: Option<(u64, u64)>,
}

/// The items a drag of `paths` carries: each one's name, and whether it's a folder.
pub fn items(paths: &[PathBuf]) -> Vec<Item> {
    paths
        .iter()
        .map(|path| Item {
            name: name_of(path),
            dir: path.is_dir(),
        })
        .collect()
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// A file or folder to send, and its path in the transfer.
struct Entry {
    path: PathBuf,
    relative: String,
    dir: bool,
    size: u64,
}

/// `path` and what's in it, folders before their contents. Links to folders inside
/// aren't followed (they could loop); links to files are.
fn walk(path: &Path, relative: String, entries: &mut Vec<Entry>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let dir = metadata.is_dir();
    entries.push(Entry {
        path: path.to_owned(),
        relative: relative.clone(),
        dir,
        size: if dir { 0 } else { metadata.len() },
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
        let is_link = fs::symlink_metadata(&child).is_ok_and(|m| m.file_type().is_symlink());
        if is_link && child.is_dir() {
            continue;
        }
        walk(&child, format!("{relative}/{}", name_of(&child)), entries);
    }
}

/// Pushes the files and folders `paths` name (and what's in the folders) into transfer
/// `transfer`. Calls `progress` as it goes; stops if `cancelled` is set.
pub fn send(
    connection: &mut Connection,
    transfer: u32,
    paths: &[PathBuf],
    cancelled: &AtomicBool,
    mut progress: impl FnMut(&Progress),
) -> Result<(), String> {
    let mut entries = Vec::new();
    for path in paths {
        walk(path, name_of(path), &mut entries);
    }
    let files = entries.iter().filter(|e| !e.dir).count() as u64;
    let bytes = entries.iter().map(|e| e.size).sum();
    let mut done = Progress {
        total: Some((files, bytes)),
        ..Progress::default()
    };
    let mut batch = Batch {
        connection,
        transfer,
        chunks: vec![Chunk::Total { files, bytes }],
        cost: 0,
    };
    let result = (|| {
        for entry in &entries {
            if cancelled.load(Ordering::Relaxed) {
                return Err("called off".to_owned());
            }
            let mut file = if entry.dir {
                None
            } else {
                match File::open(&entry.path) {
                    Ok(file) => Some(file),
                    // Gone or locked since: the rest still goes.
                    Err(_) => continue,
                }
            };
            batch.push(Chunk::Begin {
                path: entry.relative.clone(),
                dir: entry.dir,
                size: entry.size,
            })?;
            if let Some(file) = &mut file {
                let mut left = entry.size;
                let mut buffer = vec![0u8; MAX_DATA];
                while left > 0 {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err("called off".to_owned());
                    }
                    let want = (left as usize).min(MAX_DATA);
                    let read = file
                        .read(&mut buffer[..want])
                        .map_err(|e| format!("can't read {}: {e}", entry.path.display()))?;
                    if read == 0 {
                        // It shrank since: what there was is what goes.
                        break;
                    }
                    batch.push(Chunk::Data(buffer[..read].to_vec()))?;
                    left -= read as u64;
                    done.bytes += read as u64;
                    progress(&done);
                }
                batch.push(Chunk::End)?;
                done.files += 1;
                progress(&done);
            }
        }
        batch.push(Chunk::Finish { error: None })?;
        batch.flush()
    })();
    if result.is_err() {
        let _ = batch.connection.call(&Request::CancelTransfer { transfer });
    }
    result
}

/// Chunks on their way into a transfer, sent a pipe message's worth at a time.
struct Batch<'a> {
    connection: &'a mut Connection,
    transfer: u32,
    chunks: Vec<Chunk>,
    cost: usize,
}

impl Batch<'_> {
    fn push(&mut self, chunk: Chunk) -> Result<(), String> {
        if self.cost + chunk.cost() > BATCH {
            self.flush()?;
        }
        self.cost += chunk.cost();
        self.chunks.push(chunk);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        if self.chunks.is_empty() {
            return Ok(());
        }
        let chunks = std::mem::take(&mut self.chunks);
        self.cost = 0;
        match self.connection.call(&Request::SendChunks {
            transfer: self.transfer,
            chunks,
        })? {
            Response::Done => Ok(()),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }
}

/// Pulls transfer `transfer`'s files into the folder `into`, which must exist. Calls
/// `progress` as it goes; stops if `cancelled` is set.
pub fn receive(
    connection: &mut Connection,
    transfer: u32,
    into: &Path,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(&Progress),
) -> Result<(), String> {
    let mut done = Progress::default();
    let mut current: Option<(File, u64, u64)> = None;
    let result = (|| {
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err("called off".to_owned());
            }
            let chunks = match connection.call(&Request::ReceiveChunks { transfer })? {
                Response::Chunks(chunks) => chunks,
                other => return Err(format!("unexpected answer {other:?}")),
            };
            for chunk in chunks {
                match chunk {
                    Chunk::Total { files, bytes } => done.total = Some((files, bytes)),
                    Chunk::Begin {
                        path: relative,
                        dir,
                        size,
                    } => {
                        current = None;
                        let names = path::check(&relative)?;
                        let target = names
                            .iter()
                            .fold(into.to_owned(), |path, name| path.join(name));
                        if dir {
                            fs::create_dir_all(&target)
                                .map_err(|e| format!("can't make {}: {e}", target.display()))?;
                        } else {
                            if let Some(parent) = target.parent() {
                                fs::create_dir_all(parent)
                                    .map_err(|e| format!("can't make {}: {e}", parent.display()))?;
                            }
                            let file = File::create(&target)
                                .map_err(|e| format!("can't make {}: {e}", target.display()))?;
                            current = Some((file, size, 0));
                        }
                    }
                    Chunk::Data(data) => {
                        let (file, size, written) =
                            current.as_mut().ok_or("file data before its file")?;
                        *written += data.len() as u64;
                        if *written > *size {
                            return Err("the guest sent more of a file than it said".into());
                        }
                        file.write_all(&data)
                            .map_err(|e| format!("can't write a file: {e}"))?;
                        done.bytes += data.len() as u64;
                        progress(&done);
                    }
                    Chunk::End => {
                        current = None;
                        done.files += 1;
                        progress(&done);
                    }
                    Chunk::Finish { error } => {
                        return match error {
                            Some(error) => Err(error),
                            None => Ok(()),
                        };
                    }
                }
            }
        }
    })();
    if result.is_err() {
        let _ = connection.call(&Request::CancelTransfer { transfer });
    }
    result
}
