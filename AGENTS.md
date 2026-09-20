## Crates
- Use `derive-more` to derive common traits (e.g., `From`, `Into`, `Display`, etc.) where appropriate, instead of hand-writing impls.
- Use the `nz` crate macros (`nz::u32!`, `nz::u64!`, `nz::usize!`, etc.) for compile-time nonzero constants instead of `NonZero*::new(...).expect(...)`. Keep `NonZero*::new(...)` only for runtime values that cannot be expressed as literals.

## Visibility
- Prefer plain `pub` or private items. Use restricted visibility like `pub(crate)` or `pub(super)` only when a boundary genuinely needs it and the simpler choice would leak an internal API.

## Logging
- Avoid `eprintln!` outside the event consumer; emit observe events instead.

## Refactoring
- Do not care about backwards compatibility unless explicitly asked to.

## Code Style & Comments Rule
- **Move shared test code setups to fixtures.rs for duplicated code:** If test code is shared between files, move common code to fixtures.rs
- Return `Result<(), Error>` from tests and use `?` for fallible setup; keep `expect`/`unwrap` for intentional failure assertions or invariants.
- **Preserve Existing Comments:** Do not strip, remove, or alter existing inline/docstring comments unless they're no longer of relevance.
- **Add Informative Comments:** Include concise, meaningful inline comments for non-trivial logic, type signatures, edge cases, and public interface methods.
- **Explain 'Why', Not 'What':** Avoid trivial self-explanatory comments (e.g., `i += 1 // increment i`), but document complex business logic, architectural constraints, or math optimizations.
