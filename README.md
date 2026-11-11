# Nitpick

Nitpick runs the installed `rustfmt`, then adds configurable blank lines. Build
it with `cargo build --release`. A Rust toolchain with rustfmt must be installed.

```sh
nitpick
nitpick --check
nitpick src/main.rs src/lib.rs
cat src/main.rs | nitpick
nitpick - < src/main.rs
nitpick --emit stdout --edition 2024 src/main.rs
```

Run `nitpick` from a Cargo project to format its targets and their modules.
Running from the workspace root includes every workspace member. Running from
a member package, or one of its subdirectories, selects that package. Discovery
uses `cargo metadata --no-deps --offline` and does not build the project or include
external dependencies. Binary, library, test, example, benchmark, and build script
targets are included. Unrelated Rust files are left alone.

Nonempty piped input selects stdin formatting instead of project discovery.
Use `-` to explicitly read stdin, including empty input. With no file arguments
and empty redirected input, Nitpick formats the Cargo project. Explicit file
paths select only those files and their modules.

The whitespace pass has four rules, all enabled by default:

- Separate multiline declarations, blocks, assignments, and expressions from
  neighboring items or statements with a blank line. Separate multiline field
  declarations from neighboring fields too, including their doc comments and
  attributes. Consecutive single-line fields stay together.
- Add a blank line before `else` when the preceding `if` or `else if` body has
  more than one nonblank line. The braces do not count. Compact branches,
  bodies with one line, and branches without an `else` stay unpadded.
- Put a blank line between trait methods, including methods without bodies.
- Put a blank line between neighboring nodes of different kinds. Consecutive
  single-line variable declarations can stay together. A following call needs
  a blank line.

Comments and attributes stay with their code. The custom pass preserves literal
contents, macro token bodies, and `rustfmt::skip` regions.

## Configuration

Put Nitpick settings in `nitpick.toml`, separately from rustfmt settings:

```toml
[whitespace]
blank_lines_around_multiline = true
blank_line_at_end_of_if = true
blank_lines_between_trait_methods = true
blank_lines_between_node_types = true
```

Nitpick loads the nearest `nitpick.toml` above each affected source file. For
stdin it searches from the working directory. It does not merge ancestor files.
Omitted settings default to `true`. Unknown keys, invalid values, malformed TOML,
and missing explicitly selected files are errors.

```sh
nitpick --nitpick-config config/nitpick.toml src/main.rs
nitpick --whitespace off src/main.rs
nitpick --whitespace on src/main.rs
nitpick --config-path rustfmt.toml --config max_width=100 src/main.rs
nitpick -- --filename-starting-with-a-dash.rs
```

`--nitpick-config` selects one Nitpick config for the invocation. `--whitespace
on` enables all four rules regardless of config values. `off` skips the custom
pass. Disabling a rule stops its spacing requests. It does not undo another
rule's requests or erase existing whitespace in the custom pass. Configuration
errors still count when an override is supplied.

`--config-path` and `--config` belong to rustfmt. Nitpick never sends its own
settings to rustfmt. Project discovery supplies each Cargo target's language
edition. Explicit file and stdin invocations follow rustfmt's effective settings,
which default to Rust 2015. `--edition` and inline configuration can override the
language edition. `--style-edition` selects style rather than language.
This repository sets its language edition in `rustfmt.toml`.

## Output and checking

File invocations write changed final source in place. Stdin prints only final
source to stdout. `--emit stdout` prints the final file buffers with rustfmt's
absolute file labels and leaves source files unchanged. Verbose child output goes
to stderr when Nitpick handles source output.

`--check` compares original bytes with the result of both passes. It prints a
unified diff and writes nothing. Exit code 0 means the final style matches, 1
means formatting differs, and 2 means a wrapper failure. A failing rustfmt keeps
its exit code and diagnostics. Its partial output never reaches the custom pass.

`--files-with-diff` reports original absolute file paths whose final output differs.
With `--check`, it replaces the diff. With file output, it reports changed files
after writing. With stdout output, the source and file labels take precedence.
For stdin, `--check --files-with-diff` reports `<stdin>` when formatting differs.
Nitpick owns output and backup selection. Use `--emit` and `--backup` for these
operations. Rustfmt's internal `emit_mode` and `make_backup` settings cannot change
the staging operation or publish intermediate output. `disable_all_formatting`
disables rustfmt's edits. Use `--whitespace off` to disable Nitpick's edits too.

`--backup` with file output saves the original bytes in the source path with its
extension replaced by `.bk`. Unchanged files do not get new backups. Check and
stdout modes do not create backups. Like upstream, Nitpick rejects `--check`
with any explicit `--emit`, and rejects `--verbose` with `--quiet`. For combined
stdin checks, Nitpick consistently returns 1 for differences, including when
`--files-with-diff` is present.

Nitpick validates every final buffer before writing. It replaces changed files
through temporary siblings, preserves permissions, and checks for concurrent
source changes. A replacement is atomic for one file. Several replacements are
not a transaction, so a later write or backup failure can leave earlier files
formatted.

## Modules and the raw proxy

Nitpick formats ordinary out-of-line modules, nested modules, inline modules
containing out-of-line modules, and relative `#[path]` attributes. It deduplicates
explicit files that also belong to another input's module graph. Child modules
use their root input's rustfmt settings, as in rustfmt. Each child independently
loads its nearest Nitpick configuration. Unrelated `.rs` files are not scanned.
With no source files or piped input, Cargo supplies the target entry points.
Workspace targets with different language editions run in separate groups,
as with `cargo fmt`. A failure in a later group can leave earlier groups formatted.
Explicit Cargo package selection flags are not implemented.

File formatting runs in an isolated mirror of the module graph and rustfmt
configuration lookup context. Absolute module paths, symlinked source paths,
conditional module attributes such as `cfg_attr`, and modules inside `cfg_if!`
or `cfg_select!` are unsupported. Shared modules with conflicting root language
editions, nonempty rustfmt `ignore`, and line-selection settings also require
the raw proxy. Nitpick reports these cases before formatting originals. Standard
macOS `/var`, `/tmp`, and `/etc` aliases are resolved before staging.

Use the raw proxy for upstream-only operations, help, and configuration queries:

```sh
nitpick rustfmt --help
nitpick rustfmt --version
nitpick rustfmt --print-config default rustfmt.toml
nitpick rustfmt --check src/main.rs
```

The proxy forwards arguments and standard streams unchanged. It bypasses all
Nitpick configuration and whitespace rules. Option availability depends on the
installed rustfmt. See [rustfmt's documentation](https://github.com/rust-lang/rustfmt#running)
for its module handling and direct invocation behavior.

Direct rustfmt can remove Nitpick's extra spacing. Configure editors and CI to
invoke Nitpick when using this style. For this repository:

```sh
cargo run
cargo run -- --check
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
```

Both commands find `rustfmt` through `PATH`. Set `NITPICK_RUSTFMT` to override the
executable. Cargo's `RUSTFMT` variable does not select Nitpick's child executable.

The [staging prototype notes](docs/staging.md) record file discovery, configuration
handling, and upstream behavior checked during implementation.
