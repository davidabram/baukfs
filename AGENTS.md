## Comments in code

Prefer clear names and structure over comments. Add comments only to explain non-obvious constraints, invariants, or behavior that the code cannot express clearly. Keep broader design rationale and implementation history in SCE context. Never use code comments to narrate changes, describe previous implementations, or maintain change logs.

## Architecture rules

These rules are mandatory for all first-party Rust code: library and binary crates, integration tests, examples, benchmarks, and build scripts. There are no discretionary exceptions.

- No `anyhow` usage, and no direct `anyhow` dependency declarations, including renamed dependencies.
- No `dyn` trait objects.
- No authored `Box` usage.
- No boxed futures, boxed closures, or macros that introduce type-erased application abstractions (for example `async_trait`).
- Use concrete types, generics, enums, and compile-time platform selection (`cfg`).
- Use explicit typed errors with meaningful variants.
- Do not traverse errors through `Error::source` or equivalent erased error-chain APIs.

`architecture/check.sh` and `clippy.toml` enforce these rules on first-party source and manifests. Do not weaken them or add exclusions. Third-party implementation internals require separate dependency review; the source checker does not prove their dispatch behavior.
