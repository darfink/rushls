## Crates
- Use `derive-more` to derive common traits (e.g., `From`, `Into`, `Display`, etc.) where appropriate, instead of hand-writing impls.

## Code Style & Comments Rule
- **Move shared test code setups to fixtures.rs for duplicated code:** If test code is shared between files, move common code to fixtures.rs
- **Preserve Existing Comments:** NEVER strip, remove, or alter existing inline/docstring comments unless specifically instructed to delete them.
- **Add Informative Comments:** Include concise, meaningful inline comments for non-trivial logic, type signatures, edge cases, and public interface methods.
- **Explain 'Why', Not 'What':** Avoid trivial self-explanatory comments (e.g., `i += 1 // increment i`), but document complex business logic, architectural constraints, or math optimizations.