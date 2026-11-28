what does this change?

why is it needed? link the issue if there is one.

how did you verify it? include the commands you actually ran.

```sh

```

if this touches transcription workers, downloads, model caching, exports or
terminal rendering, explain the relevant failure mode and trade-off.

checklist:

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --locked --all-targets --all-features` passes
- [ ] documentation is updated when behaviour changes
- [ ] no unrelated files or generated artifacts are included
