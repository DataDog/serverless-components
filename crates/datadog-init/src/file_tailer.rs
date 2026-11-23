// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Reads the lines appended to the files that a path or a glob pattern names, as the Go logs
//! agent's file launcher does for `DD_SERVERLESS_LOG_PATH`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::debug;

/// How often the files are read and the pattern is matched again: the Go launcher's tailer sleep
/// and its default `logs_config.file_scan_period`, both one second.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The size of each read, as Go's file tailer reads.
const READ_SIZE: usize = 4096;

/// The most bytes read from one file in one poll, so a large file is read over several polls
/// instead of into memory at once.
const READ_BUDGET: usize = 4 * 1024 * 1024;

/// The longest message: Go's default `logs_config.max_message_size_bytes`.
const MAX_MESSAGE_SIZE: usize = 900_000;

/// The marker Go puts on each piece of a split line.
const TRUNCATED_FLAG: &[u8] = b"...TRUNCATED...";

/// Splits a file's content into messages as Go's file framer does: one message for each line,
/// without its newline, and an unfinished line waits for the rest. A line longer than the limit is
/// split into pieces of the limit; each piece but the last ends with `TRUNCATED_FLAG`, and each
/// piece but the first starts with it.
struct Framer {
    pending: Vec<u8>,
    limit: usize,
    last_was_truncated: bool,
}

impl Framer {
    fn new(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            limit,
            last_was_truncated: false,
        }
    }

    /// Adds `chunk` and appends the messages it completes. `pending` stays shorter than the limit.
    fn push(&mut self, mut chunk: &[u8], messages: &mut Vec<Vec<u8>>) {
        loop {
            let room = self.limit - self.pending.len();
            match chunk.iter().position(|byte| *byte == b'\n') {
                Some(newline) if newline <= room => {
                    self.pending.extend_from_slice(&chunk[..newline]);
                    self.emit(false, messages);
                    chunk = &chunk[newline + 1..];
                }
                _ if chunk.len() >= room => {
                    self.pending.extend_from_slice(&chunk[..room]);
                    self.emit(true, messages);
                    chunk = &chunk[room..];
                }
                _ => {
                    self.pending.extend_from_slice(chunk);
                    return;
                }
            }
        }
    }

    fn emit(&mut self, truncated: bool, messages: &mut Vec<Vec<u8>>) {
        let mut message = Vec::new();
        if self.last_was_truncated {
            message.extend_from_slice(TRUNCATED_FLAG);
        }
        message.append(&mut self.pending);
        if truncated {
            message.extend_from_slice(TRUNCATED_FLAG);
        }
        self.last_was_truncated = truncated;
        messages.push(message);
    }
}

/// One file being read.
struct TailedFile {
    file: File,
    /// The device and inode of the open file, which change when the path is rotated.
    identity: (u64, u64),
    /// How far the file has been read.
    offset: u64,
    framer: Framer,
}

impl TailedFile {
    /// Opens `path` at its end, or at its beginning when `from_beginning`.
    fn open(path: &Path, from_beginning: bool) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let offset = if from_beginning {
            0
        } else {
            file.seek(SeekFrom::End(0))?
        };
        Ok(Self {
            file,
            identity: (metadata.dev(), metadata.ino()),
            offset,
            framer: Framer::new(MAX_MESSAGE_SIZE),
        })
    }

    /// Reads up to `budget` bytes, from the beginning again when the file was truncated, and
    /// appends the messages they complete. Returns whether the end of the file was reached.
    fn read_messages(
        &mut self,
        budget: usize,
        messages: &mut Vec<Vec<u8>>,
    ) -> std::io::Result<bool> {
        if self.file.metadata()?.len() < self.offset {
            self.file.seek(SeekFrom::Start(0))?;
            self.offset = 0;
            self.framer = Framer::new(self.framer.limit);
        }
        let mut buffer = [0; READ_SIZE];
        let mut remaining = budget;
        while remaining > 0 {
            let read = self.file.read(&mut buffer[..READ_SIZE.min(remaining)])?;
            if read == 0 {
                return Ok(true);
            }
            self.offset += read as u64;
            remaining -= read;
            self.framer.push(&buffer[..read], messages);
        }
        Ok(false)
    }
}

/// Reads the files that match a pattern. A file that matches at the first poll is read from its
/// end, and one that matches later from its beginning, as Go does with the `end` tailing mode.
/// A file that is truncated, or replaced at its path, is read again from its beginning; a replaced
/// file's old content is still read to its end.
pub struct FileTailer {
    pattern: String,
    read_budget: usize,
    started: bool,
    files: BTreeMap<PathBuf, TailedFile>,
    /// Files replaced at their path whose old content is not read to its end yet.
    draining: Vec<(PathBuf, TailedFile)>,
}

impl FileTailer {
    /// Opens nothing yet: the first poll finds the files.
    pub fn new(pattern: String) -> Self {
        Self::with_read_budget(pattern, READ_BUDGET)
    }

    fn with_read_budget(pattern: String, read_budget: usize) -> Self {
        Self {
            pattern,
            read_budget,
            started: false,
            files: BTreeMap::new(),
            draining: Vec::new(),
        }
    }

    /// The regular files that match the pattern now.
    fn matching_paths(&self) -> Vec<PathBuf> {
        match glob::glob(&self.pattern) {
            Ok(paths) => paths
                .filter_map(Result::ok)
                .filter(|path| path.is_file())
                .collect(),
            Err(error) => {
                debug!("Invalid log path pattern {}: {error}", self.pattern);
                Vec::new()
            }
        }
    }

    /// Starts reading the files that newly match and stops reading those that no longer match.
    fn scan(&mut self, from_beginning: bool) {
        let paths = self.matching_paths();
        self.files.retain(|path, _| paths.contains(path));
        for path in paths {
            if self.files.contains_key(&path) {
                continue;
            }
            match TailedFile::open(&path, from_beginning) {
                Ok(tailed) => {
                    self.files.insert(path, tailed);
                }
                Err(error) => debug!("Failed to open {}: {error}", path.display()),
            }
        }
    }

    /// Returns the messages read from each file since the last poll, by file.
    pub fn poll(&mut self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut messages = Vec::new();
        if !self.started {
            self.started = true;
            self.scan(false);
            return messages;
        }
        let budget = self.read_budget;
        self.draining
            .retain_mut(|(path, tailed)| read(path, tailed, budget, &mut messages) == Some(false));
        let mut not_at_end = Vec::new();
        for (path, tailed) in &mut self.files {
            if read(path, tailed, budget, &mut messages) == Some(false) {
                not_at_end.push(path.clone());
            }
        }
        for (path, tailed) in std::mem::take(&mut self.files) {
            let replaced = std::fs::metadata(&path).map_or(true, |metadata| {
                (metadata.dev(), metadata.ino()) != tailed.identity
            });
            if !replaced {
                self.files.insert(path, tailed);
            } else if not_at_end.contains(&path) {
                self.draining.push((path, tailed));
            }
        }
        let known: Vec<PathBuf> = self.files.keys().cloned().collect();
        self.scan(true);
        for (path, tailed) in &mut self.files {
            if !known.contains(path) {
                read(path, tailed, budget, &mut messages);
            }
        }
        messages
    }

    /// Polls on a blocking thread, so file reads never hold an async worker. Returns `None` when
    /// the poll panicked.
    pub async fn poll_in_background(mut self) -> Option<(Self, Vec<(PathBuf, Vec<u8>)>)> {
        tokio::task::spawn_blocking(move || {
            let messages = self.poll();
            (self, messages)
        })
        .await
        .ok()
    }
}

/// Reads one file into `messages`, by file. Returns whether its end was reached, or `None` after
/// logging a failure.
fn read(
    path: &Path,
    tailed: &mut TailedFile,
    budget: usize,
    messages: &mut Vec<(PathBuf, Vec<u8>)>,
) -> Option<bool> {
    let mut read = Vec::new();
    let result = tailed.read_messages(budget, &mut read);
    messages.extend(
        read.into_iter()
            .map(|message| (path.to_path_buf(), message)),
    );
    result
        .inspect_err(|error| debug!("Failed to read {}: {error}", path.display()))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    /// A change to a file in the directory, by its name.
    enum Change {
        Append(&'static str, &'static str),
        Truncate(&'static str, &'static str),
        /// Renames the file to `<name>.1` and writes a new file of that name.
        Rotate(&'static str, &'static str),
    }

    fn append(path: &Path, content: &str) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open");
        file.write_all(content.as_bytes()).expect("write");
    }

    #[test]
    fn poll_table() {
        type Lines = &'static [(&'static str, &'static str)];
        let cases: &[(&str, Lines, &[Change], Lines)] = &[
            (
                "app.log",
                &[("app.log", "old\n")],
                &[Change::Append("app.log", "new\npartial")],
                &[("app.log", "new")],
            ),
            (
                "app.log",
                &[],
                &[Change::Append("app.log", "one\ntwo\n")],
                &[("app.log", "one"), ("app.log", "two")],
            ),
            (
                "app.log",
                &[("app.log", "old line\n")],
                &[Change::Truncate("app.log", "short\n")],
                &[("app.log", "short")],
            ),
            (
                "app.log",
                &[("app.log", "old\n")],
                &[
                    Change::Append("app.log", "last\n"),
                    Change::Rotate("app.log", "fresh\n"),
                ],
                &[("app.log", "last"), ("app.log", "fresh")],
            ),
            (
                "*.log",
                &[("a.log", "old\n")],
                &[
                    Change::Append("a.log", "a\n"),
                    Change::Append("b.log", "b\n"),
                    Change::Append("c.txt", "c\n"),
                ],
                &[("a.log", "a"), ("b.log", "b")],
            ),
        ];
        for (index, (pattern, before, changes, expected)) in cases.iter().enumerate() {
            let directory = tempfile::tempdir().expect("tempdir");
            for (name, content) in *before {
                append(&directory.path().join(name), content);
            }
            let mut tailer = FileTailer::new(directory.path().join(pattern).display().to_string());
            assert_eq!(
                tailer.poll(),
                Vec::new(),
                "case {index}: before the changes"
            );
            for change in *changes {
                match change {
                    Change::Append(name, content) => append(&directory.path().join(name), content),
                    Change::Truncate(name, content) => {
                        fs::write(directory.path().join(name), content).expect("truncate")
                    }
                    Change::Rotate(name, content) => {
                        let path = directory.path().join(name);
                        fs::rename(&path, directory.path().join(format!("{name}.1")))
                            .expect("rename");
                        append(&path, content);
                    }
                }
            }
            let expected: Vec<(PathBuf, Vec<u8>)> = expected
                .iter()
                .map(|(name, line)| (directory.path().join(name), line.as_bytes().to_vec()))
                .collect();
            assert_eq!(tailer.poll(), expected, "case {index}");
        }
    }

    #[test]
    fn framer_table() {
        type Chunks = &'static [&'static [u8]];
        let cases: &[(Chunks, Chunks)] = &[
            (&[b"one\ntwo\n"], &[b"one", b"two"]),
            (&[b"pa", b"rt\n", b"un"], &[b"part"]),
            (&[b"four\n"], &[b"four"]),
            (
                &[b"abcdefghij\n"],
                &[
                    b"abcd...TRUNCATED...",
                    b"...TRUNCATED...efgh...TRUNCATED...",
                    b"...TRUNCATED...ij",
                ],
            ),
            (
                &[b"abcdef", b"gh"],
                &[
                    b"abcd...TRUNCATED...",
                    b"...TRUNCATED...efgh...TRUNCATED...",
                ],
            ),
            (
                &[b"abcde\nok\n"],
                &[b"abcd...TRUNCATED...", b"...TRUNCATED...e", b"ok"],
            ),
        ];
        for (chunks, expected) in cases {
            let mut framer = Framer::new(4);
            let mut messages = Vec::new();
            for chunk in *chunks {
                framer.push(chunk, &mut messages);
            }
            assert_eq!(messages, *expected, "{chunks:?}");
            assert!(framer.pending.len() < 4, "{chunks:?}");
        }
    }

    /// A large file found after the first poll is read from its beginning over several polls, about
    /// the read budget each, and every line arrives in order.
    #[test]
    fn large_file_is_read_over_several_polls() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("app.log");
        let mut tailer = FileTailer::with_read_budget(path.display().to_string(), 10_000);
        assert_eq!(tailer.poll(), Vec::new());
        let content: String = (0..10_000)
            .map(|number| format!("line {number:05}\n"))
            .collect();
        append(&path, &content);
        let mut lines = Vec::new();
        let mut polls = 0;
        while lines.len() < 10_000 && polls < 100 {
            let messages = tailer.poll();
            let bytes: usize = messages.iter().map(|(_, line)| line.len() + 1).sum();
            // The budget, and the start of a line that the previous poll read.
            assert!(bytes <= 10_011, "poll {polls} sent {bytes} bytes");
            lines.extend(messages.into_iter().map(|(_, line)| line));
            polls += 1;
        }
        assert_eq!(polls, 11);
        assert_eq!(lines.len(), 10_000);
        assert_eq!(lines[0], b"line 00000");
        assert_eq!(lines[9_999], b"line 09999");
        assert!(lines.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
