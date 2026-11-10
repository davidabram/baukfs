use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use crate::support::{TestDir, block_on, raw_connection, raw_execute, raw_values, workspace};
use baukfs::restoration::{RestoreFailure, restore_revision};
use baukfs::storage::{BusyTimeout, WorkspaceStore};
use baukfs::{
    EntryKind, Manifest, ManifestEntry, RevisionId, SessionId, SymlinkTarget, WorkspacePath,
};

fn entry(path: &[u8], kind: EntryKind) -> ManifestEntry {
    ManifestEntry::new(WorkspacePath::new(path.to_vec()).unwrap(), kind)
}
fn snapshot(dir: &TestDir) -> Vec<Vec<Vec<turso::Value>>> {
    let connection = raw_connection(&dir.database());
    [
        "baukfs_metadata",
        "baukfs_content",
        "baukfs_manifests",
        "baukfs_revisions",
        "baukfs_workspace",
        "baukfs_git_origin",
    ]
    .map(|table| raw_values(&connection, &format!("SELECT * FROM {table} ORDER BY 1")))
    .to_vec()
}

fn fixture_manifest(content: baukfs::ContentId, sentinel: &std::path::Path) -> Manifest {
    Manifest::new(Vec::from([
        entry(b"empty", EntryKind::Directory),
        entry(b"nested", EntryKind::Directory),
        entry(
            b"nested/file\xff",
            EntryKind::RegularFile {
                content,
                executable: false,
            },
        ),
        entry(
            b"exec",
            EntryKind::RegularFile {
                content,
                executable: true,
            },
        ),
        entry(
            b"raw-link\xff",
            EntryKind::Symlink {
                target: SymlinkTarget::new(b"../sentinel\xff".to_vec()).unwrap(),
            },
        ),
        entry(
            b"parent-link",
            EntryKind::Symlink {
                target: SymlinkTarget::new(b"../sentinel".to_vec()).unwrap(),
            },
        ),
        entry(
            b"absolute",
            EntryKind::Symlink {
                target: SymlinkTarget::new(sentinel.as_os_str().as_bytes().to_vec()).unwrap(),
            },
        ),
    ]))
    .unwrap()
}

#[test]
fn raw_entries_permissions_links_independent_files_and_earlier_revision() {
    block_on(async {
        let dir = TestDir::new("restore-entries");
        let (mut store, _) =
            WorkspaceStore::create(&dir.database(), &workspace("ws"), BusyTimeout::IMMEDIATE)
                .await
                .unwrap();
        let (content, _) = store.insert_content(b"exact\0\xff").await.unwrap();
        let sentinel = dir.path().join("sentinel");
        fs::write(&sentinel, b"outside").unwrap();
        let manifest = fixture_manifest(content, &sentinel);
        let (initial, _) = store
            .initialize_history(&SessionId::new("session").unwrap(), &manifest)
            .await
            .unwrap();
        let empty = Manifest::new(Vec::new()).unwrap();
        let (advanced, _) = store.advance_head(initial.head(), &empty).await.unwrap();
        let before = snapshot(&dir);
        let destination = dir.path().join("restored");
        let receipt = restore_revision(&mut store, initial.head(), &destination)
            .await
            .unwrap();
        assert_eq!(receipt.revision, *initial.head());
        assert_eq!(receipt.manifest, manifest.id().unwrap());
        assert_eq!(
            (receipt.directories, receipt.regular_files, receipt.symlinks),
            (2, 2, 3)
        );
        let file = destination.join(OsStr::from_bytes(b"nested/file\xff"));
        assert_eq!(fs::read(&file).unwrap(), b"exact\0\xff");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert_eq!(
            fs::metadata(destination.join("exec"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        for path in [
            &destination,
            &destination.join("empty"),
            &destination.join("nested"),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                0o755
            );
        }
        assert_eq!(fs::read_dir(destination.join("empty")).unwrap().count(), 0);
        assert_ne!(
            fs::metadata(&file).unwrap().ino(),
            fs::metadata(destination.join("exec")).unwrap().ino()
        );
        fs::write(&file, b"changed").unwrap();
        assert_eq!(fs::read(destination.join("exec")).unwrap(), b"exact\0\xff");
        for (name, target) in [
            (b"raw-link\xff".as_slice(), b"../sentinel\xff".as_slice()),
            (b"parent-link", b"../sentinel"),
            (b"absolute", sentinel.as_os_str().as_bytes()),
        ] {
            assert_eq!(
                fs::read_link(destination.join(OsStr::from_bytes(name)))
                    .unwrap()
                    .as_os_str()
                    .as_bytes(),
                target
            );
        }
        assert_eq!(fs::read(sentinel).unwrap(), b"outside");
        let empty_destination = dir.path().join("empty-result");
        let receipt = restore_revision(&mut store, advanced.head(), &empty_destination)
            .await
            .unwrap();
        assert_eq!(
            (receipt.directories, receipt.regular_files, receipt.symlinks),
            (0, 0, 0)
        );
        assert_eq!(fs::read_dir(empty_destination).unwrap().count(), 0);
        assert_eq!(before, snapshot(&dir));
    });
}

#[test]
fn rejects_all_existing_destination_kinds_and_missing_parent() {
    block_on(async {
        let dir = TestDir::new("restore-existing");
        let (mut store, _) =
            WorkspaceStore::create(&dir.database(), &workspace("ws"), BusyTimeout::IMMEDIATE)
                .await
                .unwrap();
        let (initial, _) = store
            .initialize_history(
                &SessionId::new("session").unwrap(),
                &Manifest::new(Vec::new()).unwrap(),
            )
            .await
            .unwrap();
        fs::write(dir.path().join("file"), b"keep").unwrap();
        fs::create_dir(dir.path().join("directory")).unwrap();
        fs::write(dir.path().join("directory/keep"), b"keep").unwrap();
        symlink("file", dir.path().join("symlink")).unwrap();
        symlink("missing", dir.path().join("dangling")).unwrap();
        for name in ["file", "directory", "symlink", "dangling"] {
            let result = restore_revision(&mut store, initial.head(), &dir.path().join(name))
                .await
                .unwrap_err();
            assert!(matches!(
                result.failure,
                RestoreFailure::DestinationExists(_)
            ));
        }
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"keep");
        assert_eq!(
            fs::read(dir.path().join("directory/keep")).unwrap(),
            b"keep"
        );
        assert_eq!(
            fs::read_link(dir.path().join("symlink")).unwrap(),
            std::path::Path::new("file")
        );
        assert_eq!(
            fs::read_link(dir.path().join("dangling")).unwrap(),
            std::path::Path::new("missing")
        );
        assert!(
            restore_revision(
                &mut store,
                initial.head(),
                &dir.path().join("missing/result")
            )
            .await
            .is_err()
        );
        assert!(!dir.path().join("missing").exists());
    });
}

#[test]
fn corrupt_or_missing_references_never_publish() {
    for damage in [
        "missing-revision",
        "missing-content",
        "content",
        "manifest",
        "history",
    ] {
        block_on(async {
            let dir = TestDir::new(damage);
            let (mut store, _) =
                WorkspaceStore::create(&dir.database(), &workspace("ws"), BusyTimeout::IMMEDIATE)
                    .await
                    .unwrap();
            let (content, _) = store.insert_content(b"original").await.unwrap();
            let manifest = Manifest::new(Vec::from([entry(
                b"file",
                EntryKind::RegularFile {
                    content,
                    executable: false,
                },
            )]))
            .unwrap();
            let (initial, _) = store
                .initialize_history(&SessionId::new("session").unwrap(), &manifest)
                .await
                .unwrap();
            match damage {
                "missing-content" => raw_execute(&dir.database(), "DELETE FROM baukfs_content"),
                "content" => raw_execute(
                    &dir.database(),
                    "DROP TRIGGER baukfs_content_immutable; UPDATE baukfs_content SET contents = X'00'",
                ),
                "manifest" => raw_execute(
                    &dir.database(),
                    "DROP TRIGGER baukfs_manifests_immutable; UPDATE baukfs_manifests SET encoding = X'00'",
                ),
                "history" => raw_execute(
                    &dir.database(),
                    "DROP TRIGGER baukfs_revisions_immutable; UPDATE baukfs_revisions SET parent = 'missing'",
                ),
                _ => {}
            }
            let revision = if damage == "missing-revision" {
                RevisionId::new("missing").unwrap()
            } else {
                initial.head().clone()
            };
            let before = dir.entries();
            let destination = dir.path().join("result");
            let error = restore_revision(&mut store, &revision, &destination)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error.failure,
                    RestoreFailure::Storage(_) | RestoreFailure::MissingRevision(_)
                ),
                "{error:?}"
            );
            assert!(!destination.exists());
            assert_eq!(before, dir.entries());
        });
    }
}

#[test]
fn filesystem_aliases_are_exclusive() {
    for names in [
        [b"Case".as_slice(), b"case".as_slice()],
        ["\u{e9}".as_bytes(), "e\u{301}".as_bytes()],
    ] {
        block_on(async {
            let dir = TestDir::new("restore-aliases");
            let (mut store, _) =
                WorkspaceStore::create(&dir.database(), &workspace("ws"), BusyTimeout::IMMEDIATE)
                    .await
                    .unwrap();
            let (content, _) = store.insert_content(b"bytes").await.unwrap();
            let manifest = Manifest::new(
                names
                    .map(|name| {
                        entry(
                            name,
                            EntryKind::RegularFile {
                                content,
                                executable: false,
                            },
                        )
                    })
                    .to_vec(),
            )
            .unwrap();
            let (initial, _) = store
                .initialize_history(&SessionId::new("session").unwrap(), &manifest)
                .await
                .unwrap();
            let destination = dir.path().join("result");
            match restore_revision(&mut store, initial.head(), &destination).await {
                Ok(_) => {
                    assert_eq!(fs::read_dir(&destination).unwrap().count(), 2);
                    assert_ne!(
                        fs::metadata(destination.join(OsStr::from_bytes(names[0])))
                            .unwrap()
                            .ino(),
                        fs::metadata(destination.join(OsStr::from_bytes(names[1])))
                            .unwrap()
                            .ino()
                    );
                }
                Err(error) => {
                    assert!(
                        matches!(error.failure, RestoreFailure::Io { error, .. } if error.kind() == std::io::ErrorKind::AlreadyExists)
                    );
                    assert!(!destination.exists());
                }
            }
        });
    }
}
