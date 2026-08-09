// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The file-backed [`Persistence`] adapter for a replicated map.
//!
//! This module holds the half of persistence that touches the outside world: [`FileSnapshot`], a
//! durable backend that writes a whole [`PersistedState`] to one file as
//! `magic || version || bincode(state)`, atomically. The port it implements
//! ([`lww_register::persistence::Persistence`]), the snapshot value type, and the non-durable
//! [`InMemoryPersistence`](lww_register::persistence::InMemoryPersistence) default live in the
//! infrastructure-free `lww-register` crate — the domain owns the contract, this adapter owns the
//! filesystem and the codec.
//!
//! It briefly lived in a standalone `snapshot` crate during the workspace split and was folded back
//! into `reconcile` afterwards: one type with no standalone reuse value did not earn a published
//! identity of its own (ARCHITECTURE.md §2). [`FileSnapshot`] is re-exported from
//! [`crate::persistence`] and from the crate root, which is where consumers have always seen it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use lww_register::persistence::{PersistedState, Persistence};

/// On-disk snapshot header: a 4-byte magic followed by a little-endian `u32` format version.
///
/// The body is bincode (not self-describing), so a format change — e.g. the `Entry` / `State`
/// domain-type migration, which changed how each cell encodes — would otherwise be **silently
/// misinterpreted** on load rather than detected. The header lets [`FileSnapshot::load`] reject any
/// snapshot whose magic or version does not match this build with a descriptive error instead of
/// decoding stale bytes into a plausible but wrong state (which would drop or corrupt tombstones and
/// re-enable resurrection, F4). A pre-header (0.2.x) snapshot has no such prefix and is rejected the
/// same way.
const SNAPSHOT_MAGIC: [u8; 4] = *b"RCNL";
/// Current on-disk snapshot format version. Bump whenever the serialized shape of
/// [`PersistedState`] changes. Version 1 is the `Entry<Timestamp, V>` / `State<V>` layout.
const SNAPSHOT_FORMAT_VERSION: u32 = 1;
/// Length of the header written ahead of the bincode body: magic (4) + version (4).
const SNAPSHOT_HEADER_LEN: usize = 8;

/// Serialize a snapshot as `magic || version(LE u32) || bincode(state)`.
fn encode_snapshot<K, V>(state: &PersistedState<K, V>) -> bincode::Result<Vec<u8>>
where
    K: Serialize,
    V: Serialize,
{
    let body = bincode::serialize(state)?;
    let mut out = Vec::with_capacity(SNAPSHOT_HEADER_LEN + body.len());
    out.extend_from_slice(&SNAPSHOT_MAGIC);
    out.extend_from_slice(&SNAPSHOT_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Validate the snapshot header, then bincode-decode the body. Every failure — too short, wrong
/// magic, unsupported version, or a body that does not decode — maps to an `io::Error` of kind
/// `InvalidData` with a descriptive message, so a stale-format snapshot (e.g. a pre-`Entry`/`State`
/// file) is rejected cleanly rather than silently misread into a plausible-but-wrong state.
fn decode_snapshot<K, V>(bytes: &[u8]) -> io::Result<PersistedState<K, V>>
where
    K: DeserializeOwned + Eq + std::hash::Hash,
    V: DeserializeOwned,
{
    if bytes.len() < SNAPSHOT_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "snapshot is {} bytes, shorter than the {SNAPSHOT_HEADER_LEN}-byte format header \
                 (truncated, or a pre-Entry/State snapshot without the versioned header)",
                bytes.len()
            ),
        ));
    }
    let (header, body) = bytes.split_at(SNAPSHOT_HEADER_LEN);
    if header[..4] != SNAPSHOT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "snapshot magic {:02x?} does not match {SNAPSHOT_MAGIC:02x?}; the file is not a \
                 reconcile snapshot, or predates the versioned format",
                &header[..4]
            ),
        ));
    }
    let version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "snapshot format version {version} is not supported by this build (expected \
                 {SNAPSHOT_FORMAT_VERSION}); it was written by a different reconcile version and \
                 must be migrated or discarded"
            ),
        ));
    }
    bincode::deserialize::<PersistedState<K, V>>(body)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// A durable, file-based [`Persistence`] backend storing a single bincode-encoded snapshot.
///
/// Saves are **atomic**: the snapshot is written to a sibling `*.tmp` file, flushed, then renamed
/// over the target path, so a crash mid-write never corrupts a previously good snapshot.
#[derive(Clone, Debug)]
pub struct FileSnapshot {
    path: PathBuf,
}

impl FileSnapshot {
    /// Create a backend that reads from and writes to `path`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        FileSnapshot {
            path: path.as_ref().to_path_buf(),
        }
    }

    fn tmp_path(&self) -> PathBuf {
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        PathBuf::from(tmp)
    }
}

impl<K, V> Persistence<K, V> for FileSnapshot
where
    K: Serialize + DeserializeOwned + Eq + std::hash::Hash + Send + Sync + 'static,
    V: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let state = decode_snapshot(&bytes)?;
        Ok(Some(state))
    }

    fn save(&self, state: &PersistedState<K, V>) -> io::Result<()> {
        let bytes = encode_snapshot(state)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let tmp = self.tmp_path();
        // Write to a temporary file, flush it, then atomically rename over the target so a crash
        // mid-write cannot corrupt a previously good snapshot.
        {
            use std::io::Write;
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use lww_register::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
    use lww_register::entry::Entry;

    use super::*;

    fn sample_state() -> PersistedState<i32, String> {
        let mut members = HashSet::new();
        members.insert("127.0.0.1".parse().unwrap());
        members.insert("127.0.0.2".parse().unwrap());

        let mut acks = HashMap::new();
        let mut key_acks = HashMap::new();
        key_acks.insert("127.0.0.1".parse().unwrap(), 42u64);
        acks.insert(7, key_acks);

        PersistedState {
            entries: vec![
                (
                    1,
                    Entry::present(
                        Timestamp::new(
                            Hlc::new(PhysicalTime::from_millis(1_000), LogicalCounter::new(0)),
                            NodeId::new(7),
                        ),
                        "alive".to_string(),
                    ),
                ),
                (
                    2,
                    Entry::tombstone(Timestamp::new(
                        Hlc::new(PhysicalTime::from_millis(2_000), LogicalCounter::new(1)),
                        NodeId::new(7),
                    )),
                ), // tombstone
            ],
            members,
            tombstone_acks: acks,
        }
    }

    fn assert_states_eq(a: &PersistedState<i32, String>, b: &PersistedState<i32, String>) {
        assert_eq!(a.entries, b.entries);
        assert_eq!(a.members, b.members);
        assert_eq!(a.tombstone_acks, b.tombstone_acks);
    }

    #[test]
    fn persisted_state_bincode_roundtrip() {
        let state = sample_state();
        let bytes = bincode::serialize(&state).unwrap();
        let back: PersistedState<i32, String> = bincode::deserialize(&bytes).unwrap();
        assert_states_eq(&back, &state);
    }

    #[test]
    fn file_snapshot_save_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileSnapshot::new(dir.path().join("snapshot.bin"));

        // Nothing saved yet.
        assert!(Persistence::<i32, String>::load(&backend)
            .unwrap()
            .is_none());

        let state = sample_state();
        Persistence::<i32, String>::save(&backend, &state).unwrap();

        let loaded = Persistence::<i32, String>::load(&backend)
            .unwrap()
            .expect("a snapshot was saved");
        assert_states_eq(&loaded, &state);
    }

    #[test]
    fn file_snapshot_save_is_atomic_replace() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileSnapshot::new(dir.path().join("snapshot.bin"));

        let mut first = sample_state();
        first.entries = vec![(
            1,
            Entry::present(
                Timestamp::new(
                    Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                    NodeId::new(0),
                ),
                "first".to_string(),
            ),
        )];
        Persistence::<i32, String>::save(&backend, &first).unwrap();

        let mut second = sample_state();
        second.entries = vec![(
            1,
            Entry::present(
                Timestamp::new(
                    Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
                    NodeId::new(0),
                ),
                "second".to_string(),
            ),
        )];
        Persistence::<i32, String>::save(&backend, &second).unwrap();

        // No leftover temporary file, and the latest snapshot wins.
        assert!(!dir.path().join("snapshot.bin.tmp").exists());
        let loaded = Persistence::<i32, String>::load(&backend).unwrap().unwrap();
        assert_eq!(loaded.entries[0].1.value(), Some(&"second".to_string()));
    }

    /// A **pre-header** snapshot — valid bincode of a `PersistedState`, but written without the
    /// magic/version prefix, exactly as a pre-`Entry`/`State` node would have produced — must be
    /// rejected as an `InvalidData` error, never silently misread. This is the resurrection-safety
    /// guard for the `Entry`/`State` format break.
    #[test]
    fn headerless_legacy_snapshot_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");
        // Raw bincode body with no header prefix — the historical on-disk shape.
        let body = bincode::serialize(&sample_state()).unwrap();
        fs::write(&path, &body).unwrap();
        let backend = FileSnapshot::new(&path);
        let err = Persistence::<i32, String>::load(&backend).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// A snapshot carrying the right magic but a **future/unknown format version** must be rejected
    /// rather than decoded with this build's layout.
    #[test]
    fn unknown_format_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SNAPSHOT_MAGIC);
        bytes.extend_from_slice(&(SNAPSHOT_FORMAT_VERSION + 1).to_le_bytes());
        bytes.extend_from_slice(&bincode::serialize(&sample_state()).unwrap());
        fs::write(&path, &bytes).unwrap();
        let backend = FileSnapshot::new(&path);
        let err = Persistence::<i32, String>::load(&backend).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("format version"),
            "error should name the version mismatch, got: {err}"
        );
    }

    /// A snapshot truncated below the header length must be rejected, not panic on the
    /// out-of-bounds header slice.
    #[test]
    fn truncated_snapshot_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");
        fs::write(&path, [0xAB; 3]).unwrap();
        let backend = FileSnapshot::new(&path);
        let err = Persistence::<i32, String>::load(&backend).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
