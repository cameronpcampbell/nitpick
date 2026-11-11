# Nitpick

Rustfmt with configurable blank lines.

## Use

Run from your Cargo project:

```sh
nitpick                                  # Format the project.
nitpick --check                          # Check without changing files.
nitpick --edition 2024 src/main.rs        # Format a file and its modules.
nitpick --edition 2024 - < src/main.rs    # Format stdin to stdout.
```

At a workspace root, Nitpick formats every member. Inside a member, it formats
that package. Project mode reads editions from Cargo. For files and stdin, set
`--edition` or configure it in `rustfmt.toml`.

From this checkout, use `cargo run` or `cargo run -- --check` without installing.

- `--emit stdout` previews formatted files.
- `--backup` saves changed originals as `.bk` files.
- `--files-with-diff` lists changed paths.
- `--help` lists all options.

`--check` exits with 0 for a match, 1 for differences, or 2 for a Nitpick error.
Rustfmt failures keep their exit codes.

## Configure

Optional `nitpick.toml`, with all defaults shown:

```toml
[whitespace]
blank_lines_around_multiline = true       # Separate multiline code, fields, and match arms.
blank_line_at_end_of_if = true            # Before else when the body has 2+ lines.
blank_lines_between_trait_methods = true
blank_lines_between_node_types = true    # Separate different item/statement kinds.
```

Field docs and attributes count toward multiline spacing. Single-line fields
stay together. Compact branches, one-line bodies, and standalone `if`s stay unpadded.

Nitpick uses the nearest config above each file. Use `--nitpick-config PATH` to
select one, or `--whitespace on|off` to override all rules.

Keep rustfmt settings in `rustfmt.toml`. Use `--config max_width=100` for an override.

Use Nitpick in your editor and CI. Running `cargo fmt` directly can remove its spacing.
`nitpick rustfmt ...` runs plain rustfmt. `NITPICK_RUSTFMT` selects its executable.

See [module handling and limitations](docs/staging.md) for unsupported layouts.

## Install

From this checkout, with Rust and Cargo installed:

```sh
rustup component add rustfmt
cargo install --path .
```

Make sure `~/.cargo/bin` is on your `PATH`.
