use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use baukfs::git_import::{ImportError, ImportOutcome, ObjectFormat, import_commit};
use baukfs::storage::{BusyTimeout, HistoryError, StorageError, UpgradeOutcome, WorkspaceStore};
use baukfs::{ContentId, EntryKind, Manifest, SessionId};

use crate::support::{TestDir, block_on, raw_connection, raw_execute, raw_values, workspace};

fn git(repo: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn oid(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap().trim().to_owned()
}
fn repo(dir: &TestDir, format: &str) -> std::path::PathBuf {
    let repo = dir.path().join("source");
    fs::create_dir(&repo).unwrap();
    git(
        &repo,
        &[
            "init",
            "--initial-branch=main",
            &format!("--object-format={format}"),
        ],
        b"",
    );
    repo
}
fn commit(repo: &Path, tree: &str, message: &str) -> String {
    oid(git(repo, &["commit-tree", tree], message.as_bytes()))
}
fn empty_tree(repo: &Path) -> String {
    oid(git(repo, &["mktree"], b""))
}
fn store(dir: &TestDir) -> WorkspaceStore {
    block_on(WorkspaceStore::create(
        &dir.database(),
        &workspace("ws"),
        BusyTimeout::IMMEDIATE,
    ))
    .unwrap()
    .0
}
fn session() -> SessionId {
    SessionId::new("owner").unwrap()
}
fn import(
    repo: &Path,
    selector: &str,
    store: &mut WorkspaceStore,
) -> Result<baukfs::git_import::ImportReceipt, ImportError> {
    block_on(import_commit(repo, selector, &session(), store))
}
fn assert_conflict(result: &Result<baukfs::git_import::ImportReceipt, ImportError>) {
    assert!(
        matches!(
            result,
            Err(ImportError::Storage(StorageError::History(
                HistoryError::GitOriginConflict | HistoryError::InitializationConflict
            )))
        ),
        "{result:?}"
    );
}

fn dirty_fixture(repo: &Path) -> String {
    fs::create_dir(repo.join("nested")).unwrap();
    fs::write(repo.join("nested/file"), b"committed\0\xff").unwrap();
    fs::write(repo.join("exec"), b"#!/bin/false\n").unwrap();
    fs::write(repo.join(".gitignore"), b"ignored\n").unwrap();
    fs::write(repo.join("ignored"), b"tracked despite ignore").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{PermissionsExt, symlink};
        fs::set_permissions(repo.join("exec"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(
            std::ffi::OsStr::from_bytes(b"../target\xff"),
            repo.join("link"),
        )
        .unwrap();
        fs::write(
            repo.join(std::ffi::OsStr::from_bytes(b"raw\xff\n\t")),
            b"raw",
        )
        .unwrap();
    }
    git(repo, &["add", "-f", "."], b"");
    git(repo, &["commit", "-m", "first"], b"");
    let selected = oid(git(repo, &["rev-parse", "HEAD"], b""));
    fs::write(repo.join("nested/file"), b"second").unwrap();
    git(repo, &["add", "."], b"");
    git(repo, &["commit", "-m", "second"], b"");
    fs::write(repo.join("nested/file"), b"staged").unwrap();
    git(repo, &["add", "."], b"");
    fs::write(repo.join("nested/file"), b"unstaged").unwrap();
    fs::write(repo.join("untracked"), b"untracked").unwrap();
    selected
}
fn assert_fixture_manifest(manifest: &Manifest) {
    assert!(
        manifest
            .entries()
            .iter()
            .any(|entry| entry.path().as_bytes() == b"nested"
                && entry.kind() == &EntryKind::Directory)
    );
    assert!(
        manifest
            .entries()
            .iter()
            .any(|entry| entry.path().as_bytes() == b"ignored")
    );
    assert!(
        !manifest
            .entries()
            .iter()
            .any(|entry| entry.path().as_bytes() == b"untracked")
    );
    let file = manifest
        .entries()
        .iter()
        .find(|entry| entry.path().as_bytes() == b"nested/file")
        .unwrap();
    assert_eq!(
        file.kind(),
        &EntryKind::RegularFile {
            content: ContentId::of(b"committed\0\xff"),
            executable: false
        }
    );
    #[cfg(unix)]
    {
        assert!(manifest.entries().iter().any(|entry| matches!(
            entry.kind(),
            EntryKind::RegularFile {
                executable: true,
                ..
            }
        ) && entry.path().as_bytes() == b"exec"));
        assert!(manifest.entries().iter().any(|entry| matches!(entry.kind(), EntryKind::Symlink { target } if target.as_bytes() == b"../target\xff")));
        assert!(
            manifest
                .entries()
                .iter()
                .any(|entry| entry.path().as_bytes() == b"raw\xff\n\t")
        );
    }
}

#[test]
fn committed_bytes_modes_dirty_checkout_reopen_and_retry() {
    for format in ["sha1", "sha256"] {
        let dir = TestDir::new(format);
        let repo = repo(&dir, format);
        let selected = dirty_fixture(&repo);
        let before = checkout_state(&repo);
        let mut store = store(&dir);
        let receipt = import(&repo, &selected, &mut store).unwrap();
        assert_eq!(receipt.outcome, ImportOutcome::Imported);
        assert_eq!(receipt.origin.commit(), selected);
        assert_eq!(
            receipt.origin.object_format(),
            if format == "sha1" {
                ObjectFormat::Sha1
            } else {
                ObjectFormat::Sha256
            }
        );
        assert_eq!(checkout_state(&repo), before);
        let manifest = block_on(store.read_manifest(&receipt.manifest))
            .unwrap()
            .unwrap();
        assert_fixture_manifest(&manifest);
        drop(store);
        let mut store = block_on(WorkspaceStore::open_existing(
            &dir.database(),
            &workspace("ws"),
            BusyTimeout::IMMEDIATE,
        ))
        .unwrap();
        assert_eq!(
            block_on(store.read_git_origin()).unwrap(),
            Some(receipt.origin.clone())
        );
        let workspace = block_on(store.read_workspace()).unwrap().unwrap();
        assert_eq!(workspace.head(), receipt.origin.root_revision());
        let revision = block_on(store.read_revision(workspace.head()))
            .unwrap()
            .unwrap();
        assert_eq!(revision.manifest(), &receipt.manifest);
        assert_eq!(revision.parent(), None);
        assert_eq!(
            block_on(store.read_content(&ContentId::of(b"committed\0\xff"))).unwrap(),
            Some(b"committed\0\xff".to_vec())
        );
        assert_eq!(
            block_on(store.read_manifest(&receipt.manifest)).unwrap(),
            Some(manifest)
        );
        assert_eq!(
            import(&repo, &selected, &mut store).unwrap().outcome,
            ImportOutcome::AlreadyImported
        );
        assert_conflict(&block_on(import_commit(
            &repo,
            &selected,
            &SessionId::new("other").unwrap(),
            &mut store,
        )));
        assert_conflict(&import(&repo, "HEAD", &mut store));
        block_on(store.advance_head(workspace.head(), &Manifest::new(vec![]).unwrap())).unwrap();
        assert_conflict(&import(&repo, &selected, &mut store));
    }
}
fn checkout_state(repo: &Path) -> [Vec<u8>; 5] {
    [
        git(repo, &["status", "--porcelain=v1", "-z"], b""),
        fs::read(repo.join(".git/index")).unwrap(),
        fs::read(repo.join(".git/HEAD")).unwrap(),
        git(repo, &["show-ref"], b""),
        fs::read(repo.join("nested/file")).unwrap(),
    ]
}

#[test]
fn empty_trees_identical_tree_conflicts_and_unrelated_initialization() {
    let dir = TestDir::new("empty");
    let repo = repo(&dir, "sha1");
    let tree = empty_tree(&repo);
    let first = commit(&repo, &tree, "first");
    let second = commit(&repo, &tree, "second");
    let mut store = store(&dir);
    let receipt = import(&repo, &first, &mut store).unwrap();
    assert_eq!(
        receipt.manifest,
        Manifest::new(vec![]).unwrap().id().unwrap()
    );
    assert_conflict(&import(&repo, &second, &mut store));
    let other = TestDir::new("plain-root");
    let mut other_store = self::store(&other);
    block_on(other_store.initialize_history(&session(), &Manifest::new(vec![]).unwrap())).unwrap();
    assert_conflict(&import(&repo, &first, &mut other_store));
}

#[test]
fn represented_empty_directory_and_gitlink_rejection() {
    let dir = TestDir::new("tree-modes");
    let repo = repo(&dir, "sha1");
    let empty = empty_tree(&repo);
    let tree = oid(git(
        &repo,
        &["mktree", "-z"],
        format!("040000 tree {empty}\tempty\0").as_bytes(),
    ));
    let selected = commit(&repo, &tree, "empty directory");
    let mut store = store(&dir);
    let receipt = import(&repo, &selected, &mut store).unwrap();
    let manifest = block_on(store.read_manifest(&receipt.manifest))
        .unwrap()
        .unwrap();
    assert_eq!(manifest.entries().len(), 1);
    assert_eq!(manifest.entries()[0].path().as_bytes(), b"empty");
    let tree = oid(git(
        &repo,
        &["mktree", "-z"],
        format!("160000 commit {selected}\tsub\0").as_bytes(),
    ));
    let gitlink = commit(&repo, &tree, "submodule");
    assert!(
        matches!(import(&repo, &gitlink, &mut store), Err(ImportError::Gitlink { path }) if path == b"sub")
    );
}

#[test]
fn invalid_noncommit_and_missing_local_objects() {
    let dir = TestDir::new("missing");
    let repo = repo(&dir, "sha1");
    let mut store = store(&dir);
    for selector in ["", "--all", "does-not-exist", "HEAD\0bad"] {
        let result = import(&repo, selector, &mut store);
        assert!(
            matches!(result, Err(ImportError::InvalidSelector { .. })),
            "{result:?}"
        );
    }
    let tree = empty_tree(&repo);
    assert!(matches!(
        import(&repo, &tree, &mut store),
        Err(ImportError::NonCommit { .. })
    ));
    let blob = oid(git(
        &repo,
        &["hash-object", "-w", "--stdin"],
        b"missing blob",
    ));
    let tree = oid(git(
        &repo,
        &["mktree"],
        format!("100644 blob {blob}\tfile\n").as_bytes(),
    ));
    let selected = commit(&repo, &tree, "missing");
    fs::remove_file(repo.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
    git(&repo, &["config", "remote.origin.promisor", "true"], b"");
    git(
        &repo,
        &[
            "config",
            "remote.origin.url",
            "https://invalid.invalid/repo",
        ],
        b"",
    );
    assert!(
        matches!(import(&repo, &selected, &mut store), Err(ImportError::MissingObject { id, .. }) if id == blob)
    );
    assert_eq!(block_on(store.read_workspace()).unwrap(), None);
    assert_eq!(block_on(store.read_git_origin()).unwrap(), None);
}

#[test]
fn malicious_configuration_and_replacements_do_not_change_source() {
    let dir = TestDir::new("config");
    let repo = repo(&dir, "sha1");
    let blob = oid(git(&repo, &["hash-object", "-w", "--stdin"], b"original"));
    let tree = oid(git(
        &repo,
        &["mktree"],
        format!("100644 blob {blob}\tfile\n").as_bytes(),
    ));
    let selected = commit(&repo, &tree, "original");
    let replacement_tree = empty_tree(&repo);
    let replacement = commit(&repo, &replacement_tree, "replacement");
    git(&repo, &["replace", &selected, &replacement], b"");
    let marker = dir.path().join("executed");
    let attack = format!("touch '{}'", marker.display());
    for key in [
        "core.fsmonitor",
        "filter.evil.clean",
        "filter.evil.smudge",
        "diff.external",
        "core.pager",
        "core.sshCommand",
    ] {
        git(&repo, &["config", key, &attack], b"");
    }
    git(&repo, &["config", "filter.evil.required", "true"], b"");
    fs::write(
        repo.join(".git/info/attributes"),
        b"* filter=evil diff=evil\n",
    )
    .unwrap();
    fs::write(
        repo.join(".git/hooks/post-checkout"),
        format!("#!/bin/sh\n{attack}\n"),
    )
    .unwrap();
    let mut store = store(&dir);
    let receipt = import(&repo, &selected, &mut store).unwrap();
    assert_eq!(receipt.origin.tree(), tree);
    assert_eq!(
        block_on(store.read_content(&ContentId::of(b"original"))).unwrap(),
        Some(b"original".to_vec())
    );
    assert!(!marker.exists());
}

#[test]
fn persistence_failures_roll_back_history_and_origin_together() {
    for table in [
        "baukfs_manifests",
        "baukfs_revisions",
        "baukfs_workspace",
        "baukfs_git_origin",
    ] {
        let dir = TestDir::new(table);
        let repo = repo(&dir, "sha1");
        let blob = oid(git(
            &repo,
            &["hash-object", "-w", "--stdin"],
            b"retained content",
        ));
        let tree = oid(git(
            &repo,
            &["mktree"],
            format!("100644 blob {blob}\tfile\n").as_bytes(),
        ));
        let selected = commit(&repo, &tree, "root");
        let mut store = store(&dir);
        raw_execute(
            &dir.database(),
            &format!(
                "CREATE TRIGGER fail_import AFTER INSERT ON {table} BEGIN SELECT RAISE(ABORT, 'injected import persistence failure'); END"
            ),
        );
        assert!(matches!(
            import(&repo, &selected, &mut store),
            Err(ImportError::Storage(StorageError::Driver(_)))
        ));
        let counts = raw_values(
            &raw_connection(&dir.database()),
            "SELECT (SELECT count(*) FROM baukfs_manifests), (SELECT count(*) FROM baukfs_revisions), (SELECT count(*) FROM baukfs_workspace), (SELECT count(*) FROM baukfs_git_origin)",
        );
        assert_eq!(
            counts,
            Vec::from([Vec::from(std::array::from_fn::<_, 4, _>(|_| {
                turso::Value::Integer(0)
            }))])
        );
        assert_eq!(block_on(store.read_workspace()).unwrap(), None);
        assert_eq!(block_on(store.read_git_origin()).unwrap(), None);
        assert_eq!(
            block_on(store.read_content(&ContentId::of(b"retained content"))).unwrap(),
            Some(b"retained content".to_vec())
        );
        raw_execute(&dir.database(), "DROP TRIGGER fail_import");
        assert_eq!(
            import(&repo, &selected, &mut store).unwrap().outcome,
            ImportOutcome::Imported
        );
    }
}

#[test]
fn version_three_upgrade_preserves_history_and_content() {
    let dir = TestDir::new("v3-upgrade");
    let mut store = store(&dir);
    let (content, _) = block_on(store.insert_content(b"preserved")).unwrap();
    let (root, _) =
        block_on(store.initialize_history(&session(), &Manifest::new(vec![]).unwrap())).unwrap();
    drop(store);
    raw_execute(
        &dir.database(),
        "DROP TRIGGER baukfs_git_origin_insert_once; DROP TRIGGER baukfs_git_origin_immutable; DROP TRIGGER baukfs_git_origin_undeletable; DROP TABLE baukfs_git_origin; UPDATE baukfs_metadata SET schema_version = 3",
    );
    assert!(matches!(
        block_on(WorkspaceStore::open_existing(
            &dir.database(),
            &workspace("ws"),
            BusyTimeout::IMMEDIATE
        )),
        Err(StorageError::UpgradeRequired { found: 3 })
    ));
    assert!(matches!(
        block_on(WorkspaceStore::create(
            &dir.database(),
            &workspace("ws"),
            BusyTimeout::IMMEDIATE
        )),
        Err(StorageError::UpgradeRequired { found: 3 })
    ));
    let (mut store, outcome) = block_on(WorkspaceStore::upgrade(
        &dir.database(),
        &workspace("ws"),
        BusyTimeout::IMMEDIATE,
    ))
    .unwrap();
    assert_eq!(outcome, UpgradeOutcome::Upgraded { from: 3 });
    assert_eq!(block_on(store.read_workspace()).unwrap(), Some(root));
    assert_eq!(
        block_on(store.read_content(&content)).unwrap(),
        Some(b"preserved".to_vec())
    );
    assert_eq!(block_on(store.read_git_origin()).unwrap(), None);
}

#[test]
fn unsupported_raw_modes_and_invalid_symlinks_are_rejected() {
    let dir = TestDir::new("invalid-tree");
    let repo = repo(&dir, "sha1");
    let blob = oid(git(&repo, &["hash-object", "-w", "--stdin"], b"bytes"));
    let digest: Vec<u8> = blob
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let mut store = store(&dir);
    for mode in [b"100600", b"100664", b"140000"] {
        let mut bytes = mode.to_vec();
        bytes.extend_from_slice(b" file\0");
        bytes.extend_from_slice(&digest);
        let tree = oid(git(
            &repo,
            &["hash-object", "--literally", "-w", "-t", "tree", "--stdin"],
            &bytes,
        ));
        let selected = commit(&repo, &tree, "unsupported mode");
        assert!(
            matches!(import(&repo, &selected, &mut store), Err(ImportError::UnsupportedMode { mode: actual, path }) if actual == mode && path == b"file")
        );
    }
    for target in [b"".as_slice(), b"bad\0target"] {
        let blob = oid(git(&repo, &["hash-object", "-w", "--stdin"], target));
        let tree = oid(git(
            &repo,
            &["mktree"],
            format!("120000 blob {blob}\tlink\n").as_bytes(),
        ));
        let selected = commit(&repo, &tree, "invalid link");
        assert!(matches!(
            import(&repo, &selected, &mut store),
            Err(ImportError::InvalidTarget(_))
        ));
    }
    assert_eq!(block_on(store.read_workspace()).unwrap(), None);
    assert_eq!(block_on(store.read_git_origin()).unwrap(), None);
}

#[test]
fn missing_root_tree_and_commit_are_typed_failures() {
    let dir = TestDir::new("missing-root");
    let repo = repo(&dir, "sha1");
    let blob = oid(git(&repo, &["hash-object", "-w", "--stdin"], b"root bytes"));
    let tree = oid(git(
        &repo,
        &["mktree"],
        format!("100644 blob {blob}\tfile\n").as_bytes(),
    ));
    let selected = commit(&repo, &tree, "root");
    let mut store = store(&dir);
    fs::remove_file(repo.join(".git/objects").join(&tree[..2]).join(&tree[2..])).unwrap();
    let result = import(&repo, &selected, &mut store);
    assert!(
        matches!(&result, Err(ImportError::MissingObject { id, .. }) if *id == tree),
        "{result:?}"
    );
    fs::remove_file(
        repo.join(".git/objects")
            .join(&selected[..2])
            .join(&selected[2..]),
    )
    .unwrap();
    assert!(
        matches!(import(&repo, &selected, &mut store), Err(ImportError::MissingObject { id, .. }) if id == selected)
    );
}

#[test]
fn environment_child() {
    let Some(path) = std::env::var_os("BAUKFS_IMPORT_TEST_REPO") else {
        return;
    };
    let dir = TestDir::new("environment-child");
    let mut store = store(&dir);
    let receipt = import(
        Path::new(&path),
        &std::env::var("BAUKFS_IMPORT_TEST_COMMIT").unwrap(),
        &mut store,
    )
    .unwrap();
    assert_eq!(
        receipt.origin.tree(),
        std::env::var("BAUKFS_IMPORT_TEST_TREE").unwrap()
    );
}

#[test]
fn inherited_git_environment_cannot_redirect_the_source() {
    let dir = TestDir::new("environment");
    let repo = repo(&dir, "sha1");
    let tree = empty_tree(&repo);
    let selected = commit(&repo, &tree, "root");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "git_import::environment_child", "--nocapture"])
        .env("BAUKFS_IMPORT_TEST_REPO", &repo)
        .env("BAUKFS_IMPORT_TEST_COMMIT", &selected)
        .env("BAUKFS_IMPORT_TEST_TREE", &tree)
        .env("GIT_DIR", "/missing-repository")
        .env("GIT_COMMON_DIR", "/missing-common-dir")
        .env("GIT_WORK_TREE", "/missing-worktree")
        .env("GIT_OBJECT_DIRECTORY", "/missing-objects")
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/missing-alternates")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "extensions.objectFormat")
        .env("GIT_CONFIG_VALUE_0", "invalid-format")
        .env("GIT_CONFIG_PARAMETERS", "invalid parameters")
        .env("GIT_INDEX_FILE", "/missing-index")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn provenance_is_immutable_and_reads_reject_malformed_or_unmatched_origins() {
    let dir = TestDir::new("origin-integrity");
    let repo = repo(&dir, "sha1");
    let tree = empty_tree(&repo);
    let selected = commit(&repo, &tree, "root");
    let mut store = store(&dir);
    let receipt = import(&repo, &selected, &mut store).unwrap();
    let connection = raw_connection(&dir.database());
    for sql in [
        "UPDATE baukfs_git_origin SET commit_id = 'changed'",
        "DELETE FROM baukfs_git_origin",
        "INSERT OR REPLACE INTO baukfs_git_origin SELECT * FROM baukfs_git_origin",
    ] {
        assert!(block_on(connection.execute(sql, ())).is_err());
        assert_eq!(
            block_on(store.read_git_origin()).unwrap(),
            Some(receipt.origin.clone())
        );
    }
    raw_execute(
        &dir.database(),
        "DROP TRIGGER baukfs_git_origin_immutable; UPDATE baukfs_git_origin SET root_revision = 'unmatched-root'",
    );
    assert!(matches!(
        block_on(store.read_git_origin()),
        Err(StorageError::History(HistoryError::MalformedGitOrigin))
    ));
    block_on(connection.execute(
        "UPDATE baukfs_git_origin SET root_revision = ?1, commit_id = 'not-an-object-id'",
        (receipt.origin.root_revision().as_str(),),
    ))
    .unwrap();
    assert!(matches!(
        block_on(store.read_git_origin()),
        Err(StorageError::History(HistoryError::MalformedGitOrigin))
    ));
}

#[test]
fn import_then_restore_exact_snapshot_without_database_changes() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let dir = TestDir::new("import-restore");
    let repo = repo(&dir, "sha1");
    let selected = dirty_fixture(&repo);
    let mut store = store(&dir);
    let imported = import(&repo, &selected, &mut store).unwrap();
    let revision = imported.origin.root_revision();
    let manifest = block_on(store.read_manifest(&imported.manifest))
        .unwrap()
        .unwrap();
    let connection = raw_connection(&dir.database());
    let tables = [
        "baukfs_metadata",
        "baukfs_content",
        "baukfs_manifests",
        "baukfs_revisions",
        "baukfs_workspace",
        "baukfs_git_origin",
    ];
    let before =
        tables.map(|table| raw_values(&connection, &format!("SELECT * FROM {table} ORDER BY 1")));
    let destination = dir.path().join("restored");
    let receipt = block_on(baukfs::restoration::restore_revision(
        &mut store,
        revision,
        &destination,
    ))
    .unwrap();
    assert_eq!(receipt.manifest, imported.manifest);
    for entry in manifest.entries() {
        let path = destination.join(std::ffi::OsStr::from_bytes(entry.path().as_bytes()));
        match entry.kind() {
            EntryKind::Directory => assert!(fs::symlink_metadata(path).unwrap().is_dir()),
            EntryKind::RegularFile {
                content,
                executable,
            } => {
                assert_eq!(
                    fs::read(&path).unwrap(),
                    block_on(store.read_content(content)).unwrap().unwrap()
                );
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                    if *executable { 0o755 } else { 0o644 }
                );
            }
            EntryKind::Symlink { target } => assert_eq!(
                fs::read_link(path).unwrap().as_os_str().as_bytes(),
                target.as_bytes()
            ),
        }
    }
    assert_eq!(
        before,
        tables.map(|table| raw_values(&connection, &format!("SELECT * FROM {table} ORDER BY 1")))
    );
}
