use std::ffi::OsString;
use std::path::PathBuf;

use clap::error::{ContextKind, ContextValue};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "nitpick",
    version,
    about = "Format Rust code with rustfmt and configurable whitespace rules",
    after_help = "For additional rustfmt options, use `nitpick rustfmt ...`.",
    args_conflicts_with_subcommands = true,
    args_override_self = true,
    disable_help_subcommand = true
)]
pub struct Options {
    /// Select a single Nitpick configuration file.
    #[arg(long, value_name = "PATH")]
    pub nitpick_config: Option<PathBuf>,

    /// Override all four whitespace rules for this invocation.
    #[arg(long, value_parser = ["on", "off"])]
    pub whitespace: Option<String>,

    /// Check formatting without changing files.
    #[arg(long)]
    pub check: bool,

    /// Choose where Nitpick writes the final source.
    #[arg(long, value_name = "MODE", value_parser = ["files", "stdout"])]
    pub emit: Vec<String>,

    /// Select the Rust language edition.
    #[arg(long, value_name = "EDITION", value_parser = ["2015", "2018", "2021", "2024"])]
    pub edition: Vec<String>,

    /// Select the rustfmt style edition.
    #[arg(long, value_name = "EDITION", value_parser = ["2015", "2018", "2021", "2024"])]
    pub style_edition: Vec<String>,

    /// Use a rustfmt configuration file or directory.
    #[arg(long, value_name = "PATH")]
    pub config_path: Vec<PathBuf>,

    /// Pass inline rustfmt settings, such as max_width=100.
    #[arg(long, value_name = "KEY=VALUE,...")]
    pub config: Vec<OsString>,

    /// Back up modified files.
    #[arg(long)]
    pub backup: bool,

    /// Print the names of files with formatting differences.
    #[arg(short = 'l', long)]
    pub files_with_diff: bool,

    /// Choose when rustfmt uses color.
    #[arg(long, value_name = "WHEN", value_parser = ["always", "never", "auto"])]
    pub color: Vec<String>,

    /// Print verbose rustfmt output.
    #[arg(short, long)]
    pub verbose: bool,

    /// Print less rustfmt output.
    #[arg(short, long)]
    pub quiet: bool,

    /// Files to format. Read stdin when omitted.
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Forward all following arguments directly to rustfmt.
    #[command(disable_help_flag = true, disable_version_flag = true)]
    Rustfmt {
        #[arg(
            value_name = "ARG",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        arguments: Vec<OsString>,
    },
}

pub enum Invocation {
    Rustfmt(Vec<OsString>),
    Format(Box<Options>),
}

pub fn invocation(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Invocation, clap::Error> {
    let arguments: Vec<_> = arguments.into_iter().collect();
    let options = parse_options(&arguments)?;

    if options.command.is_some() {
        return Ok(Invocation::Rustfmt(arguments.into_iter().skip(2).collect()));

    }

    Ok(Invocation::Format(Box::new(options)))
}

impl Options {
    pub fn rustfmt_arguments(&self) -> Vec<OsString> {
        let mut arguments = Vec::new();

        for (flag, values) in [
            ("--edition", &self.edition),
            ("--style-edition", &self.style_edition),
            ("--color", &self.color),
        ] {
            for value in values {
                arguments.extend([OsString::from(flag), value.into()]);
            }
        }

        for path in &self.config_path {
            arguments.extend([OsString::from("--config-path"), path.as_os_str().into()]);
        }

        for value in &self.config {
            arguments.extend([OsString::from("--config"), value.clone()]);
        }

        arguments
    }
}

fn parse_options(arguments: &[OsString]) -> Result<Options, clap::Error> {
    Options::try_parse_from(arguments).map_err(|mut error| {
        error.insert(
            ContextKind::Suggested,
            ContextValue::StyledStrs(vec![
                "For additional rustfmt options, use `nitpick rustfmt ...`.".into(),
            ]),
        );

        error
    })
}

#[cfg(test)]
fn parse_from(arguments: impl IntoIterator<Item = OsString>) -> Result<Vec<OsString>, clap::Error> {
    let arguments: Vec<_> = arguments.into_iter().collect();
    let parsed = parse_options(&arguments)?;

    let prefix_length = if parsed.command.is_some() {
        2

    } else {
        1
    };

    // Retain the original spelling, ordering, and delimiters after validation.
    Ok(arguments.into_iter().skip(prefix_length).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::error::ErrorKind;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    fn parse(arguments: &[&str]) -> Result<Vec<OsString>, clap::Error> {
        parse_from(arguments.iter().map(OsString::from))
    }

    #[test]
    fn common_arguments_keep_their_original_order_and_spelling() {
        let cases: &[&[&str]] = &[
            &[],
            &["src/main.rs", "src/lib.rs"],
            &["src/main.rs", "rustfmt"],
            &["--check", "rustfmt"],
            &["--emit", "files"],
            &[
                "--check",
                "--emit=stdout",
                "--edition",
                "2024",
                "src/main.rs",
                "--style-edition=2021",
                "--config-path",
                "config with spaces/rustfmt.toml",
                "--config",
                "max_width=100,hard_tabs=true",
                "--backup",
                "--files-with-diff",
                "--color=always",
                "--verbose",
                "--quiet",
                "src/lib.rs",
            ],
            &[
                "-lvq",
                "-v",
                "--check",
                "--check",
                "--edition=2018",
                "--edition",
                "2024",
                "--config=max_width=80",
                "source with spaces.rs",
                "--config",
                "max_width=100,hard_tabs=false",
                "--config-path=first.toml",
                "--config-path",
                "second.toml",
                "--emit=files",
                "--emit=stdout",
                "--style-edition=2015",
                "--style-edition=2024",
                "--color=never",
                "--color=auto",
            ],
            &["--", "--check", "rustfmt", "file with spaces.rs"],
            &["src/main.rs", "--", "--help"],
        ];

        for case in cases {
            let arguments = [&["nitpick"][..], case].concat();
            let expected: Vec<_> = case.iter().map(OsString::from).collect();

            assert_eq!(parse(&arguments).unwrap(), expected);
        }
    }

    #[test]
    fn nitpick_options_are_separate_from_child_settings() {
        let invocation = invocation(
            [
                "nitpick",
                "--nitpick-config",
                "custom.toml",
                "--whitespace",
                "off",
                "--config-path",
                "rustfmt.toml",
                "--config",
                "max_width=80",
                "source.rs",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .unwrap();

        let Invocation::Format(options) = invocation else {
            panic!("combined invocation");
        };

        assert_eq!(options.nitpick_config, Some(PathBuf::from("custom.toml")));
        assert_eq!(options.whitespace.as_deref(), Some("off"));

        assert_eq!(
            options.rustfmt_arguments(),
            ["--config-path", "rustfmt.toml", "--config", "max_width=80"].map(OsString::from)
        );

        for arguments in [
            vec!["nitpick", "--whitespace", "maybe"],
            vec!["nitpick", "--nitpick-config"],
        ] {
            assert!(parse(&arguments).is_err());
        }
    }

    #[test]
    fn raw_proxy_preserves_every_argument() {
        let cases: &[&[&str]] = &[
            &[],
            &["--help"],
            &["--version"],
            &["--help=config"],
            &["--print-config", "default", "rustfmt.toml"],
            &["--"],
            &["--", "--help", "rustfmt"],
            &["--unknown-option", "--", "file with spaces.rs", "--version"],
        ];

        for case in cases {
            let arguments = [&["nitpick", "rustfmt"][..], case].concat();
            let expected: Vec<_> = case.iter().map(OsString::from).collect();

            assert_eq!(parse(&arguments).unwrap(), expected);
        }
    }

    #[test]
    fn default_validation_explains_the_raw_proxy() {
        let cases: &[&[&str]] = &[
            &["--unknown-option"],
            &["--emit", "json"],
            &["--edition", "2030"],
            &["--style-edition", "2030"],
            &["--color", "sometimes"],
            &["--config-path"],
            &["--config"],
        ];

        for case in cases {
            let arguments = [&["nitpick"][..], case].concat();
            let error = parse(&arguments).unwrap_err();

            assert_eq!(error.exit_code(), 2);
            assert!(error.to_string().contains("nitpick rustfmt ..."));
        }
    }

    #[test]
    fn wrapper_help_and_version_belong_to_clap() {
        let help = parse(&["nitpick", "--help"]).unwrap_err();
        let version = parse(&["nitpick", "--version"]).unwrap_err();

        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("[FILE]..."));
        assert!(help.to_string().contains("nitpick rustfmt ..."));
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);

        assert_eq!(
            version.to_string().trim(),
            concat!("nitpick ", env!("CARGO_PKG_VERSION"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_paths_and_values_are_preserved() {
        let path = OsString::from_vec(b"source \xff.rs".to_vec());
        let setting = OsString::from_vec(b"ignore=\xff".to_vec());

        let cases = [
            vec![
                "--config-path".into(),
                path.clone(),
                "--config".into(),
                setting,
                path.clone(),
            ],
            vec!["--".into(), path.clone()],
            vec!["rustfmt".into(), "--upstream-option".into(), path],
        ];

        for case in cases {
            let mut arguments = vec!["nitpick".into()];

            arguments.extend(case.clone());

            let expected = if case[0] == "rustfmt" {
                &case[1..]

            } else {
                &case
            };

            assert_eq!(parse_from(arguments).unwrap(), expected);
        }
    }
}
