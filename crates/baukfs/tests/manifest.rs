use baukfs::{
    ByteStringError, ContentId, EntryKind, Manifest, ManifestEntry, ManifestError, ManifestId,
    SymlinkTarget, WorkspacePath,
};

fn path(bytes: &[u8]) -> WorkspacePath {
    WorkspacePath::new(bytes).unwrap()
}

fn directory(bytes: &[u8]) -> ManifestEntry {
    ManifestEntry::new(path(bytes), EntryKind::Directory)
}

fn file(bytes: &[u8], contents: &[u8], executable: bool) -> ManifestEntry {
    ManifestEntry::new(
        path(bytes),
        EntryKind::RegularFile {
            content: ContentId::of(contents),
            executable,
        },
    )
}

fn link(bytes: &[u8], target: &[u8]) -> ManifestEntry {
    ManifestEntry::new(
        path(bytes),
        EntryKind::Symlink {
            target: SymlinkTarget::new(target).unwrap(),
        },
    )
}

fn manifest(entries: Vec<ManifestEntry>) -> Manifest {
    Manifest::new(entries).unwrap()
}

#[test]
fn paths_validate_raw_bytes_without_normalization() {
    let cases: &[(&[u8], ByteStringError)] = &[
        (b"", ByteStringError::Empty),
        (b"/", ByteStringError::AbsolutePath),
        (b"/a", ByteStringError::AbsolutePath),
        (b"a/", ByteStringError::EmptyComponent),
        (b"a//b", ByteStringError::EmptyComponent),
        (b".", ByteStringError::DotComponent),
        (b"..", ByteStringError::DotComponent),
        (b"./a", ByteStringError::DotComponent),
        (b"a/../b", ByteStringError::DotComponent),
        (b"a/.", ByteStringError::DotComponent),
        (b"a/..", ByteStringError::DotComponent),
        (b"a\0b", ByteStringError::Nul { offset: 1 }),
    ];
    for (bytes, expected) in cases {
        assert_eq!(WorkspacePath::new(*bytes), Err(*expected), "{bytes:?}");
    }
    for bytes in [
        b" a ".as_slice(),
        b"a\\b",
        b"\xff",
        b"A",
        b"a",
        b"e\xcc\x81",
        b"\xc3\xa9",
        b"a/\n",
    ] {
        assert_eq!(path(bytes).as_bytes(), bytes);
    }
    assert_ne!(path(b"A"), path(b"a"));
    assert_ne!(path(b"e\xcc\x81"), path(b"\xc3\xa9"));
}

#[test]
fn targets_are_exact_nonempty_nul_free_bytes() {
    assert_eq!(SymlinkTarget::new(b""), Err(ByteStringError::Empty));
    assert_eq!(
        SymlinkTarget::new(b"a\0"),
        Err(ByteStringError::Nul { offset: 1 })
    );
    for bytes in [
        b"/absolute".as_slice(),
        b"..",
        b"../a//./",
        b"\\",
        b" \xff ",
    ] {
        assert_eq!(SymlinkTarget::new(bytes).unwrap().as_bytes(), bytes);
    }
}

#[test]
fn input_order_does_not_affect_encoding_identity_or_lookup() {
    // Raw byte order differs from a tree traversal: a- precedes a/b.
    let entries = Vec::from([
        directory(b"a"),
        directory(b"a-"),
        file(b"a/b", b"data", true),
        link(b"z", b"../a"),
    ]);
    let expected = manifest(entries.clone());
    for rotation in 0..entries.len() {
        let mut reordered = entries.clone();
        reordered.rotate_left(rotation);
        reordered.reverse();
        let actual = manifest(reordered);
        assert_eq!(actual, expected);
        assert_eq!(actual.encode(), expected.encode());
        assert_eq!(actual.id(), expected.id());
    }
    assert_eq!(expected.entries()[1].path().as_bytes(), b"a-");
    assert!(matches!(
        expected.lookup(&path(b"a/b")).unwrap().kind(),
        EntryKind::RegularFile {
            executable: true,
            ..
        }
    ));
    assert_eq!(expected.lookup(&path(b"missing")), None);
    let encoded = expected.encode().unwrap();
    assert_eq!(Manifest::decode(&encoded), Ok(expected));
}

#[test]
fn every_supported_field_contributes_to_identity() {
    let variants = [
        manifest(Vec::from([])),
        manifest(Vec::from([directory(b"a")])),
        manifest(Vec::from([directory(b"b")])),
        manifest(Vec::from([file(b"a", b"one", false)])),
        manifest(Vec::from([file(b"a", b"two", false)])),
        manifest(Vec::from([file(b"a", b"one", true)])),
        manifest(Vec::from([link(b"a", b"one")])),
        manifest(Vec::from([link(b"a", b"two")])),
    ];
    for (index, value) in variants.iter().enumerate() {
        for other in &variants[index + 1..] {
            assert_ne!(value.id(), other.id());
        }
        assert_eq!(
            Manifest::decode(&value.encode().unwrap()),
            Ok(value.clone())
        );
    }
}

#[test]
fn non_utf8_paths_and_targets_round_trip() {
    let expected = manifest(Vec::from([
        directory(b"\xff"),
        file(b"\xff/\xfe", b"", false),
        link(b"\x80", b"/../\xfe\xff"),
    ]));
    let actual = Manifest::decode(&expected.encode().unwrap()).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        actual.lookup(&path(b"\x80")).unwrap().kind(),
        &EntryKind::Symlink {
            target: SymlinkTarget::new(b"/../\xfe\xff").unwrap()
        }
    );
}

#[test]
fn duplicate_and_invalid_parent_structures_are_rejected() {
    for second in [directory(b"a"), file(b"a", b"", false), link(b"a", b"x")] {
        assert_eq!(
            Manifest::new(Vec::from([directory(b"a"), second])),
            Err(ManifestError::DuplicatePath(path(b"a")))
        );
    }
    for ancestor in [file(b"a", b"", false), link(b"a", b"x")] {
        assert_eq!(
            Manifest::new(Vec::from([directory(b"a/b"), ancestor])),
            Err(ManifestError::NonDirectoryParent(path(b"a")))
        );
    }
    for entries in [
        Vec::from([directory(b"a/b")]),
        Vec::from([directory(b"a/b"), directory(b"a/b/c")]),
        Vec::from([directory(b"a"), directory(b"a/b/c")]),
    ] {
        assert!(matches!(
            Manifest::new(entries),
            Err(ManifestError::MissingParent(_))
        ));
    }
    assert!(
        Manifest::new(Vec::from([
            directory(b"a"),
            directory(b"a/b"),
            directory(b"a/b/c")
        ]))
        .is_ok()
    );
}

fn header(count: u64) -> Vec<u8> {
    let mut bytes = b"baukfs-manifest\0\x01\x00\x00\x00".to_vec();
    bytes.extend(count.to_le_bytes());
    bytes
}

fn raw_entry(path: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut bytes = u64::try_from(path.len()).unwrap().to_le_bytes().to_vec();
    bytes.extend(path);
    bytes.extend(payload);
    bytes
}

fn raw_manifest(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = header(u64::try_from(entries.len()).unwrap());
    for entry in entries {
        bytes.extend(entry);
    }
    bytes
}

#[test]
fn golden_encoding_fixes_header_lengths_tags_digest_and_flags() {
    let value = manifest(Vec::from([
        directory(b"d"),
        file(b"f", b"", true),
        link(b"s", b".."),
    ]));
    let mut golden = b"baukfs-manifest\0\x01\x00\x00\x00\x03\x00\x00\x00\x00\x00\x00\x00".to_vec();
    golden.extend_from_slice(b"\x01\x00\x00\x00\x00\x00\x00\x00d\x02");
    golden.extend_from_slice(b"\x01\x00\x00\x00\x00\x00\x00\x00f\x01");
    golden.extend_from_slice(&[
        0xaf, 0x13, 0x49, 0xb9, 0xf5, 0xf9, 0xa1, 0xa6, 0xa0, 0x40, 0x4d, 0xea, 0x36, 0xdc, 0xc9,
        0x49, 0x9b, 0xcb, 0x25, 0xc9, 0xad, 0xc1, 0x12, 0xb7, 0xcc, 0x9a, 0x93, 0xca, 0xe4, 0x1f,
        0x32, 0x62,
    ]);
    golden.extend_from_slice(
        b"\x01\x01\x00\x00\x00\x00\x00\x00\x00s\x03\x02\x00\x00\x00\x00\x00\x00\x00..",
    );
    assert_eq!(golden.len(), 101);
    assert_eq!(value.encode().unwrap(), golden);
    assert_eq!(Manifest::decode(&golden), Ok(value.clone()));
    assert_eq!(
        value.id().unwrap().digest(),
        blake3::hash(&golden).as_bytes()
    );
    assert_ne!(value.id().unwrap().digest(), ContentId::of(b"").digest());
    assert_eq!(manifest(Vec::from([])).encode().unwrap(), header(0));
}

#[test]
fn malformed_binary_fields_are_rejected() {
    let good = manifest(Vec::from([
        directory(b"d"),
        file(b"f", b"x", false),
        link(b"s", b"t"),
    ]))
    .encode()
    .unwrap();
    for length in 0..good.len() {
        assert!(
            Manifest::decode(&good[..length]).is_err(),
            "prefix {length}"
        );
    }
    let mut bad_domain = header(0);
    bad_domain[0] = b'X';
    assert_eq!(
        Manifest::decode(&bad_domain),
        Err(ManifestError::InvalidDomain)
    );
    let mut bad_version = header(0);
    bad_version[16..20].copy_from_slice(&2_u32.to_le_bytes());
    assert_eq!(
        Manifest::decode(&bad_version),
        Err(ManifestError::UnknownVersion(2))
    );
    let mut trailing = header(0);
    trailing.push(0);
    assert_eq!(
        Manifest::decode(&trailing),
        Err(ManifestError::TrailingBytes)
    );
    assert!(Manifest::decode(&header(u64::MAX)).is_err());
    for tag in [0, 4, 255] {
        assert_eq!(
            Manifest::decode(&raw_manifest(&[raw_entry(b"a", &[tag])])),
            Err(ManifestError::UnknownTag(tag))
        );
    }
    for flag in [2, 255] {
        let mut payload = Vec::from([1]);
        payload.extend([0; 32]);
        payload.push(flag);
        assert_eq!(
            Manifest::decode(&raw_manifest(&[raw_entry(b"a", &payload)])),
            Err(ManifestError::InvalidExecutableFlag(flag))
        );
    }
    for bytes in [
        b"".as_slice(),
        b"/a",
        b"a/",
        b"a//b",
        b"a/.",
        b"a/..",
        b"a\0",
    ] {
        assert!(matches!(
            Manifest::decode(&raw_manifest(&[raw_entry(bytes, &[2])])),
            Err(ManifestError::InvalidPath(_))
        ));
    }
    for target in [b"".as_slice(), b"\0"] {
        let mut payload = Vec::from([3]);
        payload.extend(u64::try_from(target.len()).unwrap().to_le_bytes());
        payload.extend(target);
        assert!(matches!(
            Manifest::decode(&raw_manifest(&[raw_entry(b"a", &payload)])),
            Err(ManifestError::InvalidTarget(_))
        ));
    }
    let mut huge_path = header(1);
    huge_path.extend(u64::MAX.to_le_bytes());
    huge_path.extend([2; 10]);
    assert!(Manifest::decode(&huge_path).is_err());
    let mut huge_target = Vec::from([3]);
    huge_target.extend(u64::MAX.to_le_bytes());
    assert!(Manifest::decode(&raw_manifest(&[raw_entry(b"a", &huge_target)])).is_err());
}

#[test]
fn decoder_does_not_sort_deduplicate_or_repair_structure() {
    let dir = raw_entry(b"a", &[2]);
    assert_eq!(
        Manifest::decode(&raw_manifest(&[raw_entry(b"b", &[2]), dir.clone()])),
        Err(ManifestError::NoncanonicalOrder)
    );
    assert_eq!(
        Manifest::decode(&raw_manifest(&[dir.clone(), dir])),
        Err(ManifestError::DuplicatePath(path(b"a")))
    );
    assert_eq!(
        Manifest::decode(&raw_manifest(&[raw_entry(b"a/b", &[2])])),
        Err(ManifestError::MissingParent(path(b"a/b")))
    );
    let mut regular = Vec::from([1]);
    regular.extend([0; 33]);
    let symlink = [3, 1, 0, 0, 0, 0, 0, 0, 0, b'x'];
    for payload in [regular.as_slice(), &symlink] {
        assert_eq!(
            Manifest::decode(&raw_manifest(&[
                raw_entry(b"a", payload),
                raw_entry(b"a/b", &[2])
            ])),
            Err(ManifestError::NonDirectoryParent(path(b"a")))
        );
    }
}

#[test]
fn manifest_ids_use_canonical_hex_with_a_distinct_type() {
    let id = manifest(Vec::from([])).id().unwrap();
    let text = id.to_string();
    assert_eq!(text.len(), 67);
    assert_eq!(ManifestId::parse(&text), Ok(id));
    assert_eq!(text.parse::<ManifestId>(), Ok(id));
    assert_eq!(format!("{id:?}"), format!("ManifestId({text})"));
    for malformed in [
        String::new(),
        text.to_uppercase(),
        format!("{text}0"),
        format!("{}g", &text[..66]),
        format!("{text} "),
    ] {
        assert!(ManifestId::parse(&malformed).is_err());
    }
}
