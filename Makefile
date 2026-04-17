.PHONY: hooks fmt fmt-check clippy test doc ci bump-patch bump-minor bump-major

hooks:
	./.githooks/install.sh

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

test:
	cargo test --workspace --locked

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# Same gate the pre-push hook runs. Handy for manual verification.
ci: fmt-check clippy test doc

# Bump workspace version (taskfast-cli + taskfast-agent). See CONTRIBUTING.md.
bump-patch:
	cargo xtask bump patch

bump-minor:
	cargo xtask bump minor

bump-major:
	cargo xtask bump major
