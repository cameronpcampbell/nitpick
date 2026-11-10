use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::process::Command;

#[derive(Debug)]
pub struct LaunchError {
    executable: OsString,
    source: io::Error,
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not run rustfmt executable {:?}: {}",
            self.executable, self.source
        )
    }
}

pub fn run(arguments: &[OsString]) -> Result<i32, LaunchError> {
    let executable = env::var_os("NITPICK_RUSTFMT").unwrap_or_else(|| "rustfmt".into());
    let status = Command::new(&executable)
        .args(arguments)
        .status()
        .map_err(|source| LaunchError { executable, source })?;

    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Output, Stdio};

    use tempfile::tempdir;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const UNFORMATTED: &str = "fn main(){let answer=42;println!(\"{answer}\");}\n";
    const FORMATTED: &str = "fn main() {\n    let answer = 42;\n    println!(\"{answer}\");\n}\n";

    fn binary() -> PathBuf {
        if let Some(binary) = option_env!("CARGO_BIN_EXE_nitpick") {
            return binary.into();
        }

        // Cargo also checks the binary's test target under --all-targets.
        let test_binary = env::current_exe().unwrap();
        let profile_directory = test_binary.parent().unwrap().parent().unwrap();

        profile_directory.join(format!("nitpick{}", env::consts::EXE_SUFFIX))
    }

    fn installed_rustfmt() -> OsString {
        env::var_os("NITPICK_RUSTFMT").unwrap_or_else(|| "rustfmt".into())
    }

    fn wrapper(directory: &Path) -> Command {
        let mut command = Command::new(binary());
        command
            .current_dir(directory)
            .env("NITPICK_RUSTFMT", installed_rustfmt());

        command
    }

    fn capture(mut command: Command, arguments: &[&str], input: &str) -> Output {
        let mut child = command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).unwrap();
        drop(stdin);

        child.wait_with_output().unwrap()
    }

    fn compare(direct: &Output, wrapped: &Output) {
        assert_eq!(wrapped.status.code(), direct.status.code());
        assert_eq!(wrapped.stdout, direct.stdout);
        assert_eq!(wrapped.stderr, direct.stderr);
    }

    #[test]
    fn stdin_matches_rustfmt_for_formatting_and_check_mode() {
        let directory = tempdir().unwrap();
        let cases: &[(&[&str], &str)] = &[
            (&[], UNFORMATTED),
            (&[], FORMATTED),
            (&["--emit", "stdout"], UNFORMATTED),
            (&["--emit", "stdout"], FORMATTED),
            (&["--check"], FORMATTED),
            (&["--check"], UNFORMATTED),
        ];

        for (arguments, input) in cases {
            let mut direct = Command::new(installed_rustfmt());
            direct.current_dir(directory.path());
            let direct = capture(direct, arguments, input);
            let wrapped = capture(wrapper(directory.path()), arguments, input);

            compare(&direct, &wrapped);
        }
    }

    #[test]
    fn files_match_rustfmt_for_in_place_stdout_and_check_modes() {
        let directory = tempdir().unwrap();
        let direct_dir = directory.path().join("direct");
        let wrapper_dir = directory.path().join("wrapper");
        fs::create_dir(&direct_dir).unwrap();
        fs::create_dir(&wrapper_dir).unwrap();
        let cases: &[(&[&str], &str, i32)] = &[
            (&[], UNFORMATTED, 0),
            (&[], FORMATTED, 0),
            (&["--emit", "files"], UNFORMATTED, 0),
            (&["--emit", "files"], FORMATTED, 0),
            (&["--emit", "stdout"], UNFORMATTED, 0),
            (&["--emit", "stdout"], FORMATTED, 0),
            (&["--check"], FORMATTED, 0),
            (&["--check"], UNFORMATTED, 1),
            (&["--files-with-diff", "--check"], UNFORMATTED, 1),
        ];

        for (options, input, code) in cases {
            fs::write(direct_dir.join("source with spaces.rs"), input).unwrap();
            fs::write(wrapper_dir.join("source with spaces.rs"), input).unwrap();
            let arguments = [*options, &["source with spaces.rs"]].concat();
            let direct = Command::new(installed_rustfmt())
                .args(&arguments)
                .current_dir(&direct_dir)
                .output()
                .unwrap();
            let wrapped = wrapper(&wrapper_dir).args(&arguments).output().unwrap();

            assert_eq!(direct.status.code(), Some(*code));
            // Rustfmt includes absolute paths in file diffs and stdout headers.
            assert_eq!(
                String::from_utf8_lossy(&wrapped.stdout)
                    .replace(wrapper_dir.to_str().unwrap(), "SOURCE"),
                String::from_utf8_lossy(&direct.stdout)
                    .replace(direct_dir.to_str().unwrap(), "SOURCE"),
            );
            assert_eq!(wrapped.status.code(), direct.status.code());
            assert_eq!(wrapped.stderr, direct.stderr);
            assert_eq!(
                fs::read(wrapper_dir.join("source with spaces.rs")).unwrap(),
                fs::read(direct_dir.join("source with spaces.rs")).unwrap(),
            );
        }
    }

    #[test]
    fn rustfmt_diagnostics_and_failure_status_reach_the_caller() {
        let directory = tempdir().unwrap();
        let invalid = "fn main( {";
        let mut direct = Command::new(installed_rustfmt());
        direct.current_dir(directory.path());
        let direct = capture(direct, &[], invalid);
        let wrapped = capture(wrapper(directory.path()), &[], invalid);

        assert!(!direct.status.success());
        assert!(!direct.stderr.is_empty());
        compare(&direct, &wrapped);
    }

    #[test]
    fn configuration_multiple_files_and_backups_match_rustfmt() {
        let directory = tempdir().unwrap();
        let direct_dir = directory.path().join("direct");
        let wrapper_dir = directory.path().join("wrapper");
        let filenames = ["source with spaces.rs", "--edition.rs"];

        for target in [&direct_dir, &wrapper_dir] {
            fs::create_dir_all(target.join("config dir")).unwrap();
            fs::write(
                target.join("config dir/rustfmt.toml"),
                "hard_tabs = false\n",
            )
            .unwrap();

            for filename in filenames {
                fs::write(target.join(filename), UNFORMATTED).unwrap();
            }
        }

        let arguments = [
            "--config-path",
            "config dir/rustfmt.toml",
            "--config",
            "max_width=80,hard_tabs=false",
            "--config",
            "hard_tabs=true",
            "--edition=2024",
            "--style-edition=2024",
            "--backup",
            "--quiet",
            "--emit=files",
            "--",
            filenames[0],
            filenames[1],
        ];
        let direct = Command::new(installed_rustfmt())
            .current_dir(&direct_dir)
            .args(arguments)
            .output()
            .unwrap();
        let wrapped = wrapper(&wrapper_dir).args(arguments).output().unwrap();

        assert!(
            direct.status.success(),
            "{}",
            String::from_utf8_lossy(&direct.stderr)
        );
        compare(&direct, &wrapped);

        for filename in filenames {
            assert_eq!(
                fs::read(wrapper_dir.join(filename)).unwrap(),
                fs::read(direct_dir.join(filename)).unwrap(),
            );
            assert_eq!(
                fs::read(wrapper_dir.join(filename).with_extension("bk")).unwrap(),
                fs::read(direct_dir.join(filename).with_extension("bk")).unwrap(),
            );
        }
    }

    #[test]
    fn missing_executable_is_reported_but_wrapper_help_and_version_still_work() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("missing rustfmt");
        let output = wrapper(directory.path())
            .env("NITPICK_RUSTFMT", &missing)
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(missing.to_str().unwrap()));

        for argument in ["--help", "--version"] {
            let output = wrapper(directory.path())
                .env("NITPICK_RUSTFMT", &missing)
                .arg(argument)
                .output()
                .unwrap();

            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            assert!(String::from_utf8_lossy(&output.stdout).contains("nitpick"));
        }

        let invalid = wrapper(directory.path())
            .env("NITPICK_RUSTFMT", &missing)
            .arg("--unknown-option")
            .output()
            .unwrap();

        assert_eq!(invalid.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("nitpick rustfmt ..."));
        assert!(!String::from_utf8_lossy(&invalid.stderr).contains("could not run"));
    }

    #[test]
    fn raw_help_version_and_config_queries_reach_rustfmt() {
        let directory = tempdir().unwrap();

        for arguments in [
            vec!["--help"],
            vec!["--version"],
            vec!["--print-config", "default"],
        ] {
            let direct = Command::new(installed_rustfmt())
                .current_dir(directory.path())
                .args(&arguments)
                .output()
                .unwrap();
            let wrapped = wrapper(directory.path())
                .arg("rustfmt")
                .args(&arguments)
                .output()
                .unwrap();

            assert!(direct.status.success());
            compare(&direct, &wrapped);
        }
    }

    #[cfg(unix)]
    fn executable(directory: &Path, contents: &str) -> PathBuf {
        let path = directory.join("custom rustfmt");
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        path
    }

    #[cfg(unix)]
    #[test]
    fn custom_child_inherits_streams_environment_directory_and_arguments() {
        let directory = tempdir().unwrap();
        let child = executable(
            directory.path(),
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$SHIM_TEST_VALUE\" \"$RUSTFMT\" \"$@\"\ncat\nprintf 'child diagnostic\\n' >&2\nexit 37\n",
        );
        let path = OsString::from_vec(b"source \xff.rs".to_vec());
        let cases: &[&[&str]] = &[
            &["rustfmt", "--unknown", "--"],
            &["--check", "--config=max_width=80,hard_tabs=true", "--"],
        ];

        for arguments in cases {
            let mut command = wrapper(directory.path());
            command
                .env("NITPICK_RUSTFMT", &child)
                .env("SHIM_TEST_VALUE", "inherited environment")
                .env("RUSTFMT", binary())
                .args(*arguments)
                .arg(&path);
            let output = capture(command, &[], "stdin contents\n");
            let mut expected = format!(
                "{}\ninherited environment\n{}\n",
                directory.path().canonicalize().unwrap().display(),
                binary().display()
            )
            .into_bytes();
            let forwarded = if arguments[0] == "rustfmt" {
                &arguments[1..]
            } else {
                arguments
            };

            for argument in forwarded {
                expected.extend_from_slice(argument.as_bytes());
                expected.push(b'\n');
            }

            expected.extend(path.clone().into_vec());
            expected.extend_from_slice(b"\nstdin contents\n");

            assert_eq!(output.status.code(), Some(37));
            assert_eq!(output.stdout, expected);
            assert_eq!(output.stderr, b"child diagnostic\n");
        }
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_ignores_cargos_rustfmt_override() {
        let directory = tempdir().unwrap();
        let child = executable(
            directory.path(),
            "#!/bin/sh\nprintf 'rustfmt from PATH\\n'\n",
        );
        fs::rename(child, directory.path().join("rustfmt")).unwrap();
        let output = wrapper(directory.path())
            .env_remove("NITPICK_RUSTFMT")
            .env("PATH", directory.path())
            .env("RUSTFMT", binary())
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"rustfmt from PATH\n");
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_child_terminated_by_a_signal_is_a_failure() {
        let directory = tempdir().unwrap();
        let child = executable(directory.path(), "#!/bin/sh\nkill -TERM $$\n");
        let output = wrapper(directory.path())
            .env("NITPICK_RUSTFMT", child)
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}
