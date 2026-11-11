# Staging prototype

The prototype tests live beside the implementation in `src/staging.rs`. They
compare staged file output with direct rustfmt output on a temporary module tree.
The installed reference was `rustfmt 1.9.0-stable`, commit `ac68faa20c`, dated
2026-05-25.

## File discovery

Nitpick parses each explicit root using the effective Rust language edition.
It walks module items, carries the module lookup directory through inline
modules, and resolves either `name.rs`, `name/mod.rs`, or a relative `#[path]`.
Ordinary `name.rs` modules defer the `name` directory until resolving children.
Explicit path modules resolve subsequent modules beside the selected file.
Relative path attributes inside inline modules use that inline lookup directory.

The graph records source files and directories needed to resolve paths, including
intermediate directories used by `..`. It copies only these sources, never a
recursive directory scan. Skipped external modules stay outside the output set.
Their files are mirrored when rustfmt must read an inner skip attribute. Duplicate
inputs covered by another root's graph are removed regardless of operand order.
Cycles, ambiguous module files, symlinks, and unsupported module attributes fail
before running the file transformation.

The mirror contains the original absolute directory hierarchy under a temporary
root. A source's relative module paths therefore still resolve inside staging.
Paths that climb above the original filesystem root are rejected. Absolute module
paths and conditional module paths cannot bypass the mirror because preflight
rejects them. The raw proxy remains available for those inputs.

## Configuration and editions

For each root, Nitpick asks the installed child for `--print-config current` with
the original input path and the supplied rustfmt settings. The effective language
edition comes from that response, rather than Nitpick's own Cargo edition.

The prototype verifies that language edition defaults to 2015, that style edition
2024 leaves it at 2015, and that inline `edition=2018` overrides `--edition 2024`.
The fixture uses an ancestor config with edition 2021 and hard tabs. A child has
its own conflicting rustfmt config. Both direct rustfmt and staging use the root's
config for that child, including its async syntax and tab indentation.

Staging copies `.rustfmt.toml` and `rustfmt.toml` at the applicable ancestor
locations, plus explicit config files or directory lookup contexts. The mirror's
root config uses the user's fallback rustfmt config when present, or an empty
config otherwise. This prevents temporary-directory ancestors from introducing
settings. Nonempty path filters cannot retain their meaning under a different
absolute root and are rejected. Child diagnostics have the mirror prefix removed.

Nitpick configuration is independent of the root's rustfmt settings. The custom
pass discovers a nearest `nitpick.toml` separately for each affected file.

## Output behavior checked against upstream

Rustfmt's file stdout mode labels even a single file with its absolute path,
followed by a colon and a blank line. Nitpick emits final buffers in that form.
It never parses multi-file stdout to recover source. Inline rustfmt `emit_mode`
can override even upstream's `--emit` flag. The combined command therefore sets
`emit_mode=Files,make_backup=false` last for staging, and forces stdout capture for
stdin. Nitpick's own `--emit` and `--backup` select the final output operation.

The installed child rejects a `--config-path` directory without a config directly
in that directory, including when a parent contains one. The directory-search
test compares failures as well as successful explicit selections with upstream.

Upstream rejects `--check --emit files`, `--check --emit stdout`, and
`--verbose --quiet` with status 1. The combined command keeps these rejections.
Upstream stdout output takes precedence over `--files-with-diff` and ignores
`--backup`. The combined command follows those interactions.

Upstream stdin with `--check --files-with-diff` reports `<stdin>` but returned
status 0 for differences in this version. Combined check mode deliberately
returns 1 whenever the final result differs. It never asks the child to check.

The pipeline tests cover repeated full formatting and a successful combined check,
each rule disabled separately, width and indentation options, CRLF output, stdin
and file equivalence, outside working directories, distinct configs, duplicate
module inputs, protected source, failed children, backup failures, and source
changes during formatting. Raw proxy tests still compare direct arguments,
streams, and exit statuses with upstream.
