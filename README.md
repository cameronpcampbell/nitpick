# Nitpick

Nitpick wraps the installed `rustfmt`. Install a Rust toolchain with rustfmt,
then build Nitpick with `cargo build --release`.

Format files in place, or read Rust source from stdin and write it to stdout:

```sh
nitpick src/main.rs src/lib.rs
cat src/main.rs | nitpick
```

Check formatting without changing files. The exit status comes from rustfmt:

```sh
nitpick --check src/main.rs
```

Select stdout output, an edition, or rustfmt settings:

```sh
nitpick --emit stdout --edition 2024 src/main.rs
nitpick --config-path rustfmt.toml --config max_width=100 src/main.rs
nitpick -- --filename-starting-with-a-dash.rs
```

Run `nitpick --help` for the common options. Use the raw proxy for any other
rustfmt arguments, including upstream help, version, and configuration queries:

```sh
nitpick rustfmt --help
nitpick rustfmt --version
nitpick rustfmt --print-config default rustfmt.toml
```

Nitpick adds no rustfmt settings by default. Option support and formatting
behavior depend on the installed rustfmt version and toolchain. The wrapper
inherits the working directory, environment, and standard streams.

Nitpick finds `rustfmt` through `PATH`. Set `NITPICK_RUSTFMT` to override the
executable path. Cargo's `RUSTFMT` variable does not select the child executable.
Argument and launch errors exit with code 2.
