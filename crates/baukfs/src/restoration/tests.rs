use super::*;
use crate::{ManifestEntry, SessionId, WorkspaceId, WorkspacePath};
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            return value;
        }
        std::thread::yield_now();
    }
}

#[test]
fn injected_failure_and_publication_race_clean_only_owned_staging() {
    block_on(async {
        let mut parent = Staging::create(&std::env::temp_dir()).unwrap();
        let parent_path = parent.0.as_ref().unwrap().clone();
        let (mut store, _) = WorkspaceStore::create(
            &parent_path.join("db"),
            &WorkspaceId::new("test").unwrap(),
            crate::storage::BusyTimeout::IMMEDIATE,
        )
        .await
        .unwrap();
        let manifest = Manifest::new(Vec::from([
            ManifestEntry::new(
                WorkspacePath::new(b"empty".to_vec()).unwrap(),
                EntryKind::Directory,
            ),
            ManifestEntry::new(
                WorkspacePath::new(b"second".to_vec()).unwrap(),
                EntryKind::Directory,
            ),
        ]))
        .unwrap();
        let (workspace, _) = store
            .initialize_history(&SessionId::new("test").unwrap(), &manifest)
            .await
            .unwrap();
        let destination = parent_path.join("result");
        let mut before: Vec<_> = fs::read_dir(&parent_path)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        before.sort();
        let mut entries = 0;
        let error = restore_with(&mut store, workspace.head(), &destination, |phase, root| {
            assert_eq!(fs::metadata(root)?.permissions().mode() & 0o777, 0o700);
            if matches!(phase, Phase::Entry) {
                entries += 1;
            }
            if entries == 2 {
                assert!(root.join("empty").is_dir());
                return Err(io::Error::other("injected materialization failure"));
            }
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(matches!(
            error.failure,
            RestoreFailure::Io {
                operation: "before entry",
                ..
            }
        ));
        assert!(error.cleanup.is_none());
        assert!(!destination.exists());
        let mut after: Vec<_> = fs::read_dir(&parent_path)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        after.sort();
        assert_eq!(before, after);
        let unrelated = parent_path.join(".baukfs-restore-unowned");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("keep"), b"keep").unwrap();
        let error = restore_with(&mut store, workspace.head(), &destination, |phase, _| {
            if matches!(phase, Phase::Publish) {
                fs::create_dir(&destination)?;
                fs::write(destination.join("winner"), b"winner")?;
            }
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(matches!(
            error.failure,
            RestoreFailure::DestinationExists(_)
        ));
        assert!(error.cleanup.is_none());
        assert_eq!(fs::read(destination.join("winner")).unwrap(), b"winner");
        assert_eq!(fs::read(unrelated.join("keep")).unwrap(), b"keep");
        assert_eq!(
            fs::read_dir(&parent_path).unwrap().count(),
            before.len() + 2
        );
        drop(store);
        assert!(parent.cleanup().is_none());
    });
}
