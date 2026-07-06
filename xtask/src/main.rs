// SPDX-License-Identifier: MIT
//! `cargo xtask <cmd>` — repo automation entrypoint.

#![allow(missing_docs, clippy::doc_markdown)]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use toml_edit::{value, DocumentMut};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "TaskFast SDK repo automation.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Normalize the OpenAPI spec and write the result next to the input.
    ///
    /// Reads `spec/openapi.yaml`, folds structurally-identical error schemas
    /// (see `taskfast_codegen::ERROR_ALIASES`) into `#/components/schemas/Error`, and
    /// writes the result to `spec/openapi.normalized.yaml`. The on-disk
    /// authoritative spec is not modified.
    SyncSpec {
        /// Path to the input spec (default: `spec/openapi.yaml` relative to cwd).
        #[arg(long, default_value = "spec/openapi.yaml")]
        input: PathBuf,
        /// Path to write the normalized output (default: `spec/openapi.normalized.yaml`).
        #[arg(long, default_value = "spec/openapi.normalized.yaml")]
        output: PathBuf,
        /// Don't write output; just report what would change. Exit 0.
        #[arg(long)]
        dry_run: bool,
    },
    /// Bump every version group (see `version_groups()`) by `level`, then
    /// refresh `Cargo.lock`.
    ///
    /// Bumps the workspace version (`taskfast-cli` + `taskfast-agent` + `xtask`,
    /// all `version.workspace = true`) and, on its own version line,
    /// `taskfast-client` — its public API is consumed by `taskfast-cli`, so it
    /// must bump in lockstep or the crates.io publish fails (gh#85). Other
    /// independent crates (`taskfast-chains`, `taskfast-codegen`) are not bumped;
    /// add them to `version_groups()` if they ever gain consumed public API.
    Bump {
        /// Which semver component to increment.
        level: BumpLevel,
        /// Skip running `cargo check` to refresh `Cargo.lock`.
        #[arg(long)]
        no_lock: bool,
        /// Print what would change without writing any file.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BumpLevel {
    Major,
    Minor,
    Patch,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::SyncSpec {
            input,
            output,
            dry_run,
        } => run_sync_spec(&input, &output, dry_run),
        Cmd::Bump {
            level,
            no_lock,
            dry_run,
        } => run_bump(level, no_lock, dry_run),
    }
}

fn run_sync_spec(input: &std::path::Path, output: &std::path::Path, dry_run: bool) -> Result<()> {
    let src = std::fs::read_to_string(input)
        .with_context(|| format!("read spec from {}", input.display()))?;

    let (normalized, report) =
        taskfast_codegen::normalize_spec_with_report(&src).context("normalize spec in-memory")?;

    eprintln!(
        "sync-spec: folded {} alias(es), rewrote {} $ref(s), stripped {} multipart op(s), collapsed {} multi-media request body(ies), dropped {} non-2xx response(s), stripped {} null-type variant(s)",
        report.folded_aliases.len(),
        report.refs_rewritten,
        report.stripped_operations.len(),
        report.request_media_collapsed,
        report.error_responses_stripped,
        report.null_variants_stripped,
    );
    if !report.folded_aliases.is_empty() {
        eprintln!("  folded:    {}", report.folded_aliases.join(", "));
    }
    if !report.stripped_operations.is_empty() {
        eprintln!("  stripped:  {}", report.stripped_operations.join(", "));
    }

    if dry_run {
        eprintln!(
            "sync-spec: --dry-run, skipping write to {}",
            output.display()
        );
    } else {
        std::fs::write(output, &normalized)
            .with_context(|| format!("write normalized spec to {}", output.display()))?;
        eprintln!(
            "sync-spec: wrote {} ({} bytes)",
            output.display(),
            normalized.len()
        );
    }
    Ok(())
}

/// A Cargo.toml key whose value must track the workspace version — because
/// the dep target uses `version.workspace = true` and Cargo enforces that
/// `path + version` inline deps match the target's declared version.
///
/// Keep this list exhaustive. A missed site will be caught by `cargo check`
/// the next bump, but failing loud *before* writing is friendlier.
struct SyncedSite {
    /// Path relative to workspace root.
    file: &'static str,
    /// Dotted TOML key path, e.g. `workspace.package.version`.
    toml_path: &'static [&'static str],
}

/// Independently-versioned crate groups. Each inner slice is a set of sites
/// that share ONE version and bump together by the requested level; the groups
/// themselves are on separate version lines and bump independently.
///
/// `taskfast-client` is its own group because it lives on a 0.x line distinct
/// from the workspace version — but its public API is consumed by
/// `taskfast-cli`, so it MUST bump on every release. If it is left stale, a
/// crates.io publish of `taskfast-cli` fails verifying its tarball against the
/// last *published* `taskfast-client` (which lacks any newly-added API). Local
/// CI and `cargo-semver-checks` use path deps and cannot catch this; only the
/// registry publish does. See the gh#85 / v0.9.0 postmortem.
fn version_groups() -> &'static [&'static [SyncedSite]] {
    &[
        // Workspace version: taskfast-cli + taskfast-agent (`version.workspace`),
        // plus the inline taskfast-agent dep requirement.
        &[
            SyncedSite {
                file: "Cargo.toml",
                toml_path: &["workspace", "package", "version"],
            },
            SyncedSite {
                file: "Cargo.toml",
                toml_path: &["workspace", "dependencies", "taskfast-agent", "version"],
            },
        ],
        // taskfast-client: its own package version + the workspace dep
        // requirement that pins it. Both must stay in lockstep so taskfast-cli
        // always requires the freshly-published taskfast-client.
        &[
            SyncedSite {
                file: "crates/taskfast-client/Cargo.toml",
                toml_path: &["package", "version"],
            },
            SyncedSite {
                file: "Cargo.toml",
                toml_path: &["workspace", "dependencies", "taskfast-client", "version"],
            },
        ],
    ]
}

fn run_bump(level: BumpLevel, no_lock: bool, dry_run: bool) -> Result<()> {
    let workspace_root = find_workspace_root().context("locate workspace root")?;
    let groups = version_groups();

    // Load each distinct file once across ALL groups. Multiple sites in the
    // same file must share one DocumentMut so sequential edits don't clobber
    // each other on write.
    let mut docs: Vec<(PathBuf, DocumentMut)> = Vec::new();
    for site in groups.iter().flat_map(|g| g.iter()) {
        let path = workspace_root.join(site.file);
        if docs.iter().any(|(p, _)| p == &path) {
            continue;
        }
        let src =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let doc: DocumentMut = src
            .parse()
            .with_context(|| format!("parse {}", path.display()))?;
        docs.push((path, doc));
    }

    let doc_for = |file: &str, docs: &[(PathBuf, DocumentMut)]| -> usize {
        let target = workspace_root.join(file);
        docs.iter()
            .position(|(p, _)| p == &target)
            .expect("file preloaded")
    };

    // For each group, validate intra-group sync and compute current -> next.
    // Groups version independently, so each reads its own authoritative value.
    let mut plans: Vec<(&'static [SyncedSite], String)> = Vec::new();
    for group in groups {
        let current = read_toml_string(&docs[doc_for(group[0].file, &docs)].1, group[0].toml_path)
            .with_context(|| format!("{} @ {}", group[0].file, group[0].toml_path.join(".")))?
            .to_owned();
        for site in &group[1..] {
            let v = read_toml_string(&docs[doc_for(site.file, &docs)].1, site.toml_path)
                .with_context(|| format!("{} @ {}", site.file, site.toml_path.join(".")))?;
            if v != current {
                bail!(
                    "synced site {}@{} = {v} but group version = {current}; \
                     fix manually before bumping",
                    site.file,
                    site.toml_path.join("."),
                );
            }
        }
        let next = bump_semver(&current, level)?;
        eprintln!("bump: {current} -> {next} ({level:?})");
        for site in *group {
            eprintln!("  touched: {} @ {}", site.file, site.toml_path.join("."));
        }
        plans.push((group, next));
    }

    // Roll the changelog against the release (workspace / taskfast-cli) version.
    // groups[0] — and thus plans[0] — is the workspace version line by
    // construction of `version_groups()`; that is the version the release tag
    // `taskfast-cli-vX.Y.Z` carries and the section cargo-dist matches.
    // Compute (and fail-early-validate) BEFORE any file is written.
    let changelog_path = workspace_root.join("CHANGELOG.md");
    let changelog_src = std::fs::read_to_string(&changelog_path)
        .with_context(|| format!("read {}", changelog_path.display()))?;
    let release_version = plans[0].1.clone();
    let date = today_utc();
    let rolled = roll_changelog(&changelog_src, &release_version, &date)?;
    eprintln!("bump: changelog: ## Unreleased -> ## [{release_version}] - {date}");
    if rolled.body_empty {
        eprintln!(
            "bump: warning: `## Unreleased` had no entries — rolling an empty \
             {release_version} section (pure chore release?)"
        );
    }

    if dry_run {
        eprintln!("bump: --dry-run, no files written");
        return Ok(());
    }

    // Apply all edits in-memory, then flush each distinct doc once.
    for (group, next) in &plans {
        for site in *group {
            let idx = doc_for(site.file, &docs);
            write_toml_string(&mut docs[idx].1, site.toml_path, next);
        }
    }
    for (path, doc) in &docs {
        std::fs::write(path, doc.to_string())
            .with_context(|| format!("write {}", path.display()))?;
        eprintln!("bump: wrote {}", path.display());
    }
    std::fs::write(&changelog_path, &rolled.content)
        .with_context(|| format!("write {}", changelog_path.display()))?;
    eprintln!("bump: wrote {}", changelog_path.display());

    if no_lock {
        eprintln!("bump: --no-lock, skipping `cargo check`");
    } else {
        refresh_lockfile(&workspace_root).context("refresh Cargo.lock via `cargo check`")?;
    }

    eprintln!("bump: done. review: git diff");
    Ok(())
}

fn read_toml_string<'a>(doc: &'a DocumentMut, path: &[&str]) -> Result<&'a str> {
    let mut item = doc.as_item();
    for key in path {
        item = item
            .get(key)
            .with_context(|| format!("key `{key}` missing"))?;
    }
    item.as_str().context("expected string value")
}

fn write_toml_string(doc: &mut DocumentMut, path: &[&str], new: &str) {
    let (last, prefix) = path.split_last().expect("non-empty path");
    let mut item = doc.as_item_mut();
    for key in prefix {
        item = &mut item[*key];
    }
    item[*last] = value(new);
}

/// Result of rolling `## Unreleased` into a dated release section.
#[derive(Debug)]
struct RolledChangelog {
    /// The rewritten changelog text.
    content: String,
    /// True if the rolled `Unreleased` section had no entries (blank body until
    /// the next `## ` heading or EOF). A pure `chore` release is legitimate, so
    /// this only warns rather than fails.
    body_empty: bool,
}

/// Roll the `## Unreleased` section into a dated `## [version] - date` section,
/// leaving a fresh empty `## Unreleased` stub above it.
///
/// Fails loud if there is no `## Unreleased` heading — the caller runs this
/// *before* writing any file, so a missing heading aborts the whole bump with
/// no side effects (same fail-early philosophy as the version-site checks).
fn roll_changelog(content: &str, version: &str, date: &str) -> Result<RolledChangelog> {
    const HEADING: &str = "## Unreleased";
    let idx = content
        .lines()
        .position(|l| l.trim_end() == HEADING)
        .with_context(|| {
            format!(
                "CHANGELOG.md has no `{HEADING}` heading — cannot roll it into `## [{version}]`. \
                 Add an `{HEADING}` section (Keep a Changelog format) before bumping."
            )
        })?;

    // Empty = only blank lines from just after the heading up to the next
    // `## ` heading (or EOF).
    let body_empty = content
        .lines()
        .skip(idx + 1)
        .take_while(|l| !l.starts_with("## "))
        .all(|l| l.trim().is_empty());

    // ponytail: the heading line is unique in this file, so a substring
    // `replacen(.., 1)` targets it precisely and preserves the rest byte-for-byte
    // (avoids re-normalizing line endings that a lines().join() rebuild would).
    let replacement = format!("{HEADING}\n\n## [{version}] - {date}");
    let content = content.replacen(HEADING, &replacement, 1);
    Ok(RolledChangelog {
        content,
        body_empty,
    })
}

/// Today's date as `YYYY-MM-DD` in UTC, for the rolled changelog heading.
fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

fn bump_semver(current: &str, level: BumpLevel) -> Result<String> {
    // Reject pre-release / build metadata — keep scope tight. Revisit if needed.
    if current.contains('-') || current.contains('+') {
        bail!(
            "pre-release/build-metadata version `{current}` not supported; \
             bump manually or extend xtask",
        );
    }
    let parts: Vec<&str> = current.split('.').collect();
    if parts.len() != 3 {
        bail!("version `{current}` is not MAJOR.MINOR.PATCH");
    }
    let parse = |s: &str, field: &str| -> Result<u64> {
        s.parse::<u64>()
            .with_context(|| format!("parse {field} of `{current}`"))
    };
    let (major, minor, patch) = (
        parse(parts[0], "major")?,
        parse(parts[1], "minor")?,
        parse(parts[2], "patch")?,
    );
    let (m, n, p) = match level {
        BumpLevel::Major => (major + 1, 0, 0),
        BumpLevel::Minor => (major, minor + 1, 0),
        BumpLevel::Patch => (major, minor, patch + 1),
    };
    Ok(format!("{m}.{n}.{p}"))
}

fn refresh_lockfile(workspace_root: &Path) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    eprintln!("bump: running `cargo check --workspace` to refresh Cargo.lock");
    let status = Command::new(&cargo)
        .arg("check")
        .arg("--workspace")
        .current_dir(workspace_root)
        .status()
        .with_context(|| format!("spawn {}", cargo.to_string_lossy()))?;
    if !status.success() {
        bail!("`cargo check --workspace` failed with {status}");
    }
    Ok(())
}

fn find_workspace_root() -> Result<PathBuf> {
    let start = std::env::current_dir().context("cwd")?;
    for dir in start.ancestors() {
        let candidate = dir.join("Cargo.toml");
        if !candidate.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&candidate)
            .with_context(|| format!("read {}", candidate.display()))?;
        if text.contains("[workspace]") {
            return Ok(dir.to_path_buf());
        }
    }
    bail!(
        "no ancestor Cargo.toml with [workspace] found from {}",
        start.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_patch_basic() {
        assert_eq!(bump_semver("0.2.1", BumpLevel::Patch).unwrap(), "0.2.2");
    }

    #[test]
    fn bump_minor_resets_patch() {
        assert_eq!(bump_semver("0.2.5", BumpLevel::Minor).unwrap(), "0.3.0");
    }

    #[test]
    fn bump_major_resets_minor_and_patch() {
        assert_eq!(bump_semver("1.4.7", BumpLevel::Major).unwrap(), "2.0.0");
    }

    #[test]
    fn bump_handles_zero_versions() {
        assert_eq!(bump_semver("0.0.0", BumpLevel::Patch).unwrap(), "0.0.1");
        assert_eq!(bump_semver("0.0.0", BumpLevel::Minor).unwrap(), "0.1.0");
        assert_eq!(bump_semver("0.0.0", BumpLevel::Major).unwrap(), "1.0.0");
    }

    #[test]
    fn bump_rejects_prerelease() {
        let err = bump_semver("0.2.1-alpha", BumpLevel::Patch).unwrap_err();
        assert!(
            err.to_string().contains("pre-release"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bump_rejects_build_metadata() {
        let err = bump_semver("0.2.1+build.7", BumpLevel::Patch).unwrap_err();
        assert!(
            err.to_string().contains("pre-release"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bump_rejects_two_segment_version() {
        let err = bump_semver("1.2", BumpLevel::Patch).unwrap_err();
        assert!(
            err.to_string().contains("MAJOR.MINOR.PATCH"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bump_rejects_non_numeric_segment() {
        let err = bump_semver("0.x.0", BumpLevel::Patch).unwrap_err();
        assert!(
            err.to_string().contains("parse minor"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn taskfast_client_is_an_independent_bump_group() {
        // Regression guard for the gh#85 / v0.9.0 release: taskfast-client must
        // bump alongside every release so its published version always carries
        // the API that taskfast-cli depends on.
        let groups = version_groups();
        assert_eq!(groups.len(), 2, "workspace group + taskfast-client group");
        let client = groups
            .iter()
            .find(|g| {
                g.iter()
                    .any(|s| s.file == "crates/taskfast-client/Cargo.toml")
            })
            .expect("a taskfast-client version group exists");
        // It pins both the crate's own version and the workspace dep requirement.
        assert!(client
            .iter()
            .any(|s| s.file == "crates/taskfast-client/Cargo.toml"
                && s.toml_path == ["package", "version"].as_slice()));
        assert!(client.iter().any(|s| s.file == "Cargo.toml"
            && s.toml_path
                == ["workspace", "dependencies", "taskfast-client", "version"].as_slice()));
    }

    #[test]
    fn roll_inserts_dated_section_and_keeps_unreleased_stub() {
        let src = "# Changelog\n\n## Unreleased\n\n### Fixed\n\n- a bug\n";
        let rolled = roll_changelog(src, "1.2.3", "2026-07-06").unwrap();
        assert!(
            rolled
                .content
                .contains("## Unreleased\n\n## [1.2.3] - 2026-07-06\n"),
            "unexpected roll:\n{}",
            rolled.content
        );
        // Prior entries are carried into the dated section, not lost.
        assert!(rolled.content.contains("- a bug"));
        assert!(!rolled.body_empty);
        // Fresh stub sits above the dated section.
        let stub = rolled.content.find("## Unreleased").unwrap();
        let dated = rolled.content.find("## [1.2.3]").unwrap();
        assert!(stub < dated, "stub must precede dated section");
    }

    #[test]
    fn roll_fails_when_unreleased_missing() {
        let src = "# Changelog\n\n## [0.1.0] - 2020-01-01\n\n- old\n";
        let err = roll_changelog(src, "0.2.0", "2026-07-06").unwrap_err();
        assert!(
            err.to_string().contains("Unreleased"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn roll_flags_empty_unreleased_but_succeeds() {
        // Blank body up to the next section heading.
        let src = "# Changelog\n\n## Unreleased\n\n## [0.1.0] - 2020-01-01\n\n- old\n";
        let rolled = roll_changelog(src, "0.2.0", "2026-07-06").unwrap();
        assert!(rolled.body_empty, "empty Unreleased body should be flagged");
        assert!(rolled.content.contains("## [0.2.0] - 2026-07-06"));
    }

    #[test]
    fn roll_flags_empty_unreleased_at_eof() {
        let src = "# Changelog\n\n## Unreleased\n";
        let rolled = roll_changelog(src, "0.2.0", "2026-07-06").unwrap();
        assert!(rolled.body_empty);
        assert!(rolled.content.contains("## [0.2.0] - 2026-07-06"));
    }

    #[test]
    fn today_utc_is_iso_date() {
        let d = today_utc();
        // YYYY-MM-DD
        assert_eq!(d.len(), 10, "got {d}");
        let parts: Vec<&str> = d.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].len() == 4 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
    }

    #[test]
    fn toml_roundtrip_preserves_formatting() {
        // Note: toml_edit's `value()` replaces trailing decor on the value
        // itself, so inline comments on the bumped line are not preserved.
        // Structural comments (section banners, free-floating) survive, which
        // is what actually matters for diff ergonomics.
        let src = r#"# top comment
[workspace.package]
version = "0.2.1"

# between sections
[workspace.dependencies]
taskfast-agent = { path = "crates/taskfast-agent", version = "0.2.1" }
"#;
        let mut doc: DocumentMut = src.parse().unwrap();
        assert_eq!(
            read_toml_string(&doc, &["workspace", "package", "version"]).unwrap(),
            "0.2.1"
        );
        write_toml_string(&mut doc, &["workspace", "package", "version"], "0.3.0");
        write_toml_string(
            &mut doc,
            &["workspace", "dependencies", "taskfast-agent", "version"],
            "0.3.0",
        );
        let out = doc.to_string();
        assert!(out.contains("# top comment"), "lost top comment");
        assert!(out.contains("# between sections"), "lost section comment");
        assert!(out.contains("version = \"0.3.0\""));
        assert!(!out.contains("\"0.2.1\""));
    }
}
