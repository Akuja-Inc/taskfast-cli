## Summary

<!-- One-line description of what this PR does and why. -->

## Type

<!-- Delete all but one. -->

- [ ] `feat` — new feature
- [ ] `fix` — bug fix
- [ ] `refactor` — code restructuring, no behavior change
- [ ] `perf` — performance improvement
- [ ] `docs` — documentation only
- [ ] `chore` — tooling, CI, dependencies
- [ ] `test` — adding or updating tests
- [ ] `ci` — CI/CD changes
- [ ] `build` — build system changes

## Checklist

- [ ] Commits follow [Conventional Commits](https://www.conventionalcommits.org/) (`type(scope): summary`)
- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --all-targets --all-features --workspace --locked -- -D warnings` clean
- [ ] `cargo test --workspace --locked` green
- [ ] New public items have rustdoc
- [ ] Breaking change? If yes, commit includes `!` or `BREAKING CHANGE:` footer

## Test plan

<!-- How did you verify this change? Commands run, manual testing, etc. -->
