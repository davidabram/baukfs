```
            (    )
             \  /
      /\   .-'``'-.   /\
     /  \ /  _  _  \ /  \
    / /\ \| (o)(o) |/ /\ \
    \/  \/ \  /\  / \/  \/
            \ \/\/ /
       ___   '.__.'   ___
      /   \__/ || \__/   \
     | /\  /  _||_  \  /\ |
     |/  \|  /    \  |/  \|
          | |  ><  | |
          | |      | |
         /__\      /__\
        (_/\_)    (_/\_)
             \    /
              \__/~~~>
```

# BaukFS

BaukFS is a Rust library with a thin CLI. The intended MVP gives agents native working directories on Linux and macOS, with local Turso storage for durable file contents, immutable revisions, and lineage.

## Status

Implemented:

- Cargo workspace with the `baukfs` library and the `baukfs-cli` binary, pinned to Rust 1.95.0 (edition 2024).
- `baukfs-cli help` (also `-h`, `--help`).
- In-memory workspace domain model: identifiers, lifecycle transitions, and typed errors (see `docs/domain-contract.md`).
- Architecture rules (see `AGENTS.md`) enforced by `architecture/check.sh`, its fixture self-test, and `clippy.toml`.
- Independent Nix checks for build, formatting, Clippy, tests, and architecture.
- Turso driver compatibility spike in `crates/baukfs-turso-spike` (not shipped; see `docs/turso-compatibility.md`).
- Workspace storage foundation in `baukfs::storage`: one local Turso database per workspace copy, always opened in multiprocess WAL mode. Covers explicit creation, existing-only opening, read-only inspection, and workspace identity (see `docs/storage.md`).
- Content identity: `ContentId`, the BLAKE3 digest of the exact bytes, written as `b3:` plus 64 lowercase hex digits.
- Immutable, content-addressed storage of whole-file bytes (schema version 2): idempotent, cross-process-safe insertion that never overwrites, and reads that verify length and digest before returning bytes, with typed integrity errors.
- Explicit, atomic schema upgrade from version 1 (`WorkspaceStore::upgrade`). Inspection and normal opening never migrate.

Planned, not implemented:

- Filesystem scanning and Turso-backed manifests.
- Immutable revisions and lineage, and their persistence.
- Working-directory cloning, runners, confinement, and Git publication.

## Supported systems

- `aarch64-linux`
- `x86_64-linux`
- `aarch64-darwin` (Apple Silicon only; Intel macOS is not supported)

## Development

Enter the development shell, which provides the pinned toolchain, ripgrep, and Bash 5:

```sh
nix develop
```

Run CLI help:

```sh
cargo run -p baukfs-cli -- --help
nix run . -- --help
```

The architecture scripts require Bash 5 or newer. Run them with the Nix-provided Bash, not the system Bash (macOS ships Bash 3.2):

```sh
nix develop -c bash architecture/self-test.sh
nix develop -c bash architecture/check.sh .
```

`self-test.sh` runs the checker against the fixtures in `architecture/fixtures/`; `check.sh .` scans the workspace source. Exit status 0 means clean, 1 means a violation, and 2 means the scan could not complete.

## Validation

Each Nix check runs independently, so a small change only needs the relevant one:

```sh
nix build -L .#checks.$(nix config show system).architecture
nix build -L .#checks.$(nix config show system).fmt
nix build -L .#checks.$(nix config show system).clippy
nix build -L .#checks.$(nix config show system).test
nix build -L .#checks.$(nix config show system).build
```

Full validation for the current system:

```sh
nix flake check -L
```

New files must be added to Git (`git add`) before the flake can see them.
