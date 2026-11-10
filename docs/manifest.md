# Filesystem manifest contract (T07)

The `baukfs` crate provides a pure, in-memory manifest model. It performs no filesystem access, database queries, content reads, or symlink resolution. T08 persists these canonical encodings in schema version 3 and validates content references at the storage boundary (see `docs/storage.md`).

## Paths and symbolic links

`WorkspacePath::new(impl Into<Vec<u8>>)` validates workspace-relative Unix filename bytes. Empty paths, leading or trailing `/`, repeated `/`, NUL, and components equal to `.` or `..` are rejected. The root is implicit and cannot have an entry. `/` is the only separator; backslashes are ordinary bytes.

Accepted bytes are preserved exactly, including non-UTF-8 filenames, whitespace, control bytes other than NUL, and distinct Unicode representations. There is no trimming, case folding, Unicode normalization, filesystem lookup, or symlink resolution. Workspace identifiers remain separate metadata types and must not be used as paths.

`SymlinkTarget::new(impl Into<Vec<u8>>)` rejects only empty targets and NUL. Absolute targets, `..`, repeated separators, trailing separators, and non-UTF-8 bytes are preserved. A target is a distinct type from a workspace path. Representing a target does not authorize restoration through it; restoration needs its own containment and symlink traversal rules.

Both types have private byte storage and expose only `as_bytes()` for borrowing it. Validation failures use `ByteStringError`: `Empty`, `Nul { offset }`, `AbsolutePath`, `EmptyComponent`, or `DotComponent`. NUL offsets are byte offsets.

## Entries and structure

`ManifestEntry::new(WorkspacePath, EntryKind)` constructs an entry. Its fields are private, with `path()` and `kind()` read access. `EntryKind` supports exactly:

- `RegularFile { content: ContentId, executable: bool }`: a reference to immutable whole-file contents and an executable flag.
- `Directory`: an explicit directory, including an empty directory.
- `Symlink { target: SymlinkTarget }`: exact link target bytes.

No ownership, timestamps, ACLs, devices, sockets, FIFOs, or hard-link identity are represented. The executable flag does not encode a full permission mode.

`Manifest::new(Vec<ManifestEntry>)` accepts arbitrary input order and sorts paths lexicographically by unsigned raw bytes (a prefix sorts before its extensions). This is whole-path byte order, not a recursive directory traversal or locale order. An empty manifest is valid.

Every path is unique, even when duplicate entries have identical kinds and data. Every non-root parent must have its own directory entry. Files and symlinks cannot have descendants. Checking immediate parents for all entries enforces the complete ancestor chain. Missing directories are never synthesized and conflicting entries are never discarded.

The entry collection is private. `entries()` borrows its canonical slice; `lookup(&WorkspacePath)` performs binary search and returns a borrowed entry. No API permits mutation of stored paths, kinds, or entries.

Structural errors are `ManifestError::DuplicatePath(path)`, `MissingParent(child_path)`, or `NonDirectoryParent(parent_path)`. These are typed errors; no erased error-chain traversal is required.

## Canonical binary encoding, version 1

`Manifest::encode()` returns `Result<Vec<u8>, ManifestError>`. All integers are unsigned and little-endian. Counts and byte lengths are checked for conversion to u64. No native `usize`, text identifiers, native filesystem structs, debug output, or unordered collections are serialized.

The complete encoding is the following concatenation, with no padding:

| Field | Representation |
| --- | --- |
| Domain header | The 16 exact bytes `baukfs-manifest\0` (ASCII plus one NUL) |
| Format version | u32, value 1 (`01 00 00 00`) |
| Entry count | u64 |
| Entries | Exactly `count` entries in strictly increasing raw-byte path order |

Each entry begins with `path_length: u64`, followed by exactly that many path bytes, then a one-byte kind tag:

| Tag | Kind | Payload after tag |
| --- | --- | --- |
| `01` | Regular file | Exactly 32 raw ContentId digest bytes, then one executable byte: `00` false, `01` true |
| `02` | Directory | None |
| `03` | Symbolic link | `target_length: u64`, followed by exactly that many target bytes |

The content digest is the raw BLAKE3 digest in its existing byte order; it is not hexadecimal text or an integer. There is no checksum, terminator, optional field, or trailing extension area. An empty manifest is exactly 28 bytes (header, version, zero count).

`Manifest::decode(&[u8])` accepts only this canonical form. It rejects a wrong domain header, unknown versions or tags, flags other than 0/1, truncated fields, trailing bytes, invalid paths or targets, duplicate paths, out-of-order entries, and all structural violations. It does not sort, deduplicate, or otherwise repair encoded input. Count/length conversions are checked; the count is bounded against remaining input before entry collection, and each byte length against remaining input before copying/allocating its value. The decoder does not reserve memory based on unchecked encoded counts. Length/count arithmetic does not wrap. Errors include `InvalidDomain`, `UnknownVersion`, `UnknownTag`, `InvalidExecutableFlag`, `Truncated`, `TrailingBytes`, `LengthOverflow`, `NoncanonicalOrder`, `InvalidPath`, and `InvalidTarget`, plus the structural errors above. For multiply malformed input, no precedence among defects is promised.

## Manifest identity

`Manifest::id()` returns `Result<ManifestId, ManifestError>` by computing the ordinary 32-byte BLAKE3 hash of the **entire canonical encoding above**, including domain header and version. Thus the identity preimage is domain separated from unframed file contents and versioned. The header is part of both the serialized bytes and the hashed bytes; there is no additional hidden hash prefix or BLAKE3 derive-key context. Paths, kinds, digests, executable flags, symlink targets, and explicit empty directories all contribute to identity. Input order does not.

`ManifestId` is a distinct type with private storage, separate from `ContentId`, `RevisionId`, and workspace identifiers. It exposes `digest()`, `Display`, `parse()`, and `FromStr`. There are no public conversions between identifier kinds. It follows ContentId's canonical text convention: `b3:` followed by exactly 64 lowercase hexadecimal digits. Parsing rejects uppercase, wrong lengths, non-hex characters, and whitespace. `ManifestIdError::kind()` exposes the reused `ContentIdError` hexadecimal validation classification. Sharing a text convention does not make the identifier types interchangeable, and parsing an ID does not prove any manifest exists.

`encode()` and `id()` are fallible for a count or byte length that cannot be represented as u64 (`LengthOverflow`). On the supported 64-bit platforms an in-memory length fits, but the wire format does not rely on native pointer width.

## Golden contract test

`crates/baukfs/tests/manifest.rs` hand-specifies a 101-byte encoding for three entries: directory `d`, executable regular file `f` referencing the empty content's BLAKE3 digest, and symlink `s` with target `..`. It fixes the header, version, count, ordering, all tags, lengths, digest bytes, and executable flag without calling the encoder to build its expected bytes. It checks exact encoding, decoding, and BLAKE3 identity over that preimage. The empty 28-byte encoding is checked separately.

Other pure deterministic tests cover input reordering, every identity-bearing field, exact non-UTF-8 round trips, invalid byte strings and structure, malformed fields, every truncated prefix of a mixed-kind manifest, huge counts and lengths, and noncanonical ordering and duplicates. No filesystem fixtures or property-testing framework are needed.

## Deferred responsibilities

- **Content-reference validation:** ContentIds are references. Pure construction and decoding neither check database existence nor read content bytes. T08 storage verifies all distinct regular-file references before publishing a revision and on reads.
- **Persistence and revisions (T08):** provided by `baukfs::storage`, separately from this pure module. ManifestId is not RevisionId.
- **Capture:** no scanning, race detection, metadata capture policy, Git import, or symlink following is implemented.
- **Restoration and cloning:** no disk writes, containment enforcement, platform filename compatibility, executable-mode policy, or restoration through symlinks is implemented. Valid Unix byte strings need not be usable on every filesystem.
- **Resource policy:** the model holds entries and encoded output in memory. Input-bounded decoding prevents trusting declared lengths, but does not impose an application-specific size quota or promise recovery from allocator exhaustion.
- **Integration:** no runner, CLI command, generic serialization framework, or backend abstraction is added.
