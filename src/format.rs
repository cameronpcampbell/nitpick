//! Run rustfmt in isolation, then compare or publish the final whitespace pass.

#[path = "staging.rs"]
mod staging;

#[path = "project.rs"]
mod project;

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;

use nitpick::{cli::Options, config, whitespace};
use similar::TextDiff;
use tempfile::NamedTempFile;

use staging::{Snapshot, Stage};
use whitespace::Edition;

type Result<Value> = std::result::Result<Value, Failure>;

#[derive(Debug)]
pub enum Failure {
    Wrapper(String),
    Child(i32),
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wrapper(message) => formatter.write_str(message),
            Self::Child(code) => write!(formatter, "rustfmt exited with {code}"),
        }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Wrapper(message)
    }
}

fn at_path(path: &Path, error: impl fmt::Display) -> Failure {
    Failure::Wrapper(format!("{}: {error}", path.display()))
}

fn command() -> Command {
    Command::new(env::var_os("NITPICK_RUSTFMT").unwrap_or_else(|| "rustfmt".into()))
}

fn capture(arguments: &[OsString], input: Option<&[u8]>) -> Result<Output> {
    let mut child = command()
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            Failure::Wrapper(format!(
                "could not run rustfmt executable {:?}: {error}",
                command().get_program()
            ))
        })?;

    let writer = input.map(|input| {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let input = input.to_owned();

        thread::spawn(move || stdin.write_all(&input))
    });

    let output = child
        .wait_with_output()
        .map_err(|error| at_path(Path::new("rustfmt"), error))?;

    if let Some(writer) = writer {
        let result = writer
            .join()
            .map_err(|_panic| Failure::Wrapper("stdin writer panicked".into()))?;

        if output.status.success() {
            result.map_err(|error| at_path(Path::new("<stdin>"), error))?;
        }
    }

    Ok(output)
}

fn check_child(output: &Output, stage: Option<&Stage>, source_stdout: bool) -> Result<()> {
    let normalize = |bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes);

        stage.map_or_else(|| text.to_string(), |stage| stage.diagnostics(&text))
    };

    eprint!("{}", normalize(&output.stderr));

    if !source_stdout {
        eprint!("{}", normalize(&output.stdout));
    }

    if !output.status.success() {
        return Err(Failure::Child(output.status.code().unwrap_or(1)));
    }

    Ok(())
}

fn settings(options: &Options, path: &Path) -> Result<toml::Table> {
    let mut arguments = options.rustfmt_arguments();

    arguments.extend([
        "--print-config".into(),
        "current".into(),
        path.as_os_str().into(),
    ]);

    let output = capture(&arguments, None)?;

    if !output.status.success() {
        check_child(&output, None, false)?;
    }

    let contents = String::from_utf8(output.stdout).map_err(|error| at_path(path, error))?;

    contents.parse().map_err(|error| {
        at_path(
            path,
            format!("could not read rustfmt's effective configuration: {error}"),
        )
    })
}

fn edition(settings: &toml::Table) -> Result<Edition> {
    match settings.get("edition").and_then(toml::Value::as_str) {
        Some("2015") => Ok(Edition::Edition2015),
        Some("2018") => Ok(Edition::Edition2018),
        Some("2021") => Ok(Edition::Edition2021),
        Some("2024") => Ok(Edition::Edition2024),

        value => Err(Failure::Wrapper(format!(
            "unsupported rustfmt language edition: {value:?}"
        ))),
    }
}

fn custom(source: &[u8], path: &Path, edition: Edition, options: &Options) -> Result<Vec<u8>> {
    let rules = config::load(
        path.parent().unwrap_or(Path::new(".")),
        options.nitpick_config.as_deref(),
        options.whitespace.as_deref(),
    )?;

    let Some(rules) = rules else {
        return Ok(source.to_vec());
    };

    let source = std::str::from_utf8(source).map_err(|error| at_path(path, error))?;

    whitespace::format(source, edition, rules)
        .map(String::into_bytes)
        .map_err(|error| at_path(path, error))
}

pub fn run(options: &Options) -> Result<i32> {
    // These combinations are rejected by upstream before it reads source.
    if options.check && !options.emit.is_empty() {
        eprintln!("Invalid to use `--emit` and `--check`");

        return Ok(1);
    }

    if options.verbose && options.quiet {
        eprintln!("Can't use both `--verbose` and `--quiet`");

        return Ok(1);
    }

    if options.emit.len() > 1 {
        eprintln!("Option 'emit' given more than once");

        return Ok(1);
    }

    if options.files.iter().any(|path| path == Path::new("-")) {
        if options.files.len() != 1 {
            return Err(Failure::Wrapper(
                "stdin (-) cannot be combined with source file paths".into(),
            ));
        }

        return stdin(options, &read_stdin()?);
    }

    if !options.files.is_empty() {
        return files(options);
    }

    if !io::stdin().is_terminal() {
        let input = read_stdin()?;

        if !input.is_empty() {
            return stdin(options, &input);
        }
    }

    let mut code = 0;

    for group in project::discover(options)? {
        code |= files(&group)?;
    }

    Ok(code)
}

fn read_stdin() -> Result<Vec<u8>> {
    let mut input = Vec::new();

    io::stdin()
        .read_to_end(&mut input)
        .map_err(|error| at_path(Path::new("<stdin>"), error))?;

    Ok(input)
}

fn stdin(options: &Options, original: &[u8]) -> Result<i32> {
    let path = env::current_dir()
        .map_err(|error| at_path(Path::new("."), error))?
        .join("<stdin>");

    let effective = settings(options, &path)?;
    let language = edition(&effective)?;
    let mut arguments = options.rustfmt_arguments();

    arguments.extend([
        "--emit".into(),
        "stdout".into(),
        "--config".into(),
        "emit_mode=Stdout,make_backup=false".into(),
    ]);

    let output = capture(&arguments, Some(original))?;

    check_child(&output, None, true)?;

    let formatted = if effective
        .get("disable_all_formatting")
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        original
    } else {
        &output.stdout
    };

    let final_source = custom(formatted, &path, language, options)?;
    let changed = original != final_source;

    if options.check {
        if changed && options.files_with_diff {
            println!("<stdin>");
        } else if changed {
            print_diff(Path::new("<stdin>"), original, &final_source)?;
        }

        return Ok(i32::from(changed));
    }

    io::stdout()
        .write_all(&final_source)
        .map_err(|error| at_path(&path, error))?;

    Ok(0)
}

fn files(options: &Options) -> Result<i32> {
    let mut stage = Stage::prepare(options)?;
    let arguments = stage.arguments(options)?;
    let output = capture(&arguments, None)?;

    check_child(&output, Some(&stage), false)?;

    let mut results = Vec::new();

    for snapshot in stage
        .snapshots
        .values()
        .filter(|snapshot| snapshot.affected)
    {
        let formatted =
            fs::read(stage.path(&snapshot.path)).map_err(|error| at_path(&snapshot.path, error))?;

        let final_source = custom(&formatted, &snapshot.path, snapshot.edition, options)?;

        results.push((snapshot, final_source));
    }

    // Validate every source before any output or replacement.
    for (snapshot, _final_source) in &results {
        snapshot.verify()?;
    }

    let stdout = options.emit.last().is_some_and(|mode| mode == "stdout");
    let mut changed = false;

    for (snapshot, final_source) in results {
        let differs = snapshot.original != final_source;

        changed |= differs;

        if options.check {
            if differs && options.files_with_diff {
                println!("{}", snapshot.path.display());
            } else if differs {
                print_diff(&snapshot.path, &snapshot.original, &final_source)?;
            }

        } else if stdout {
            let mut output = io::stdout().lock();

            writeln!(output, "{}:\n", snapshot.path.display())
                .map_err(|error| at_path(&snapshot.path, error))?;

            output
                .write_all(&final_source)
                .map_err(|error| at_path(&snapshot.path, error))?;

        } else if differs {
            replace(snapshot, &final_source, options.backup)?;

            if options.files_with_diff {
                println!("{}", snapshot.path.display());
            }
        }
    }

    Ok(i32::from(options.check && changed))
}

fn print_diff(path: &Path, original: &[u8], final_source: &[u8]) -> Result<()> {
    let original = String::from_utf8_lossy(original);
    let final_source = String::from_utf8_lossy(final_source);
    let diff = TextDiff::from_lines(original.as_ref(), final_source.as_ref());
    let label = path.display().to_string();
    let mut output = io::stdout().lock();

    write!(output, "{}", diff.unified_diff().header(&label, &label))
        .map_err(|error| at_path(path, error))?;

    Ok(())
}

fn replace(snapshot: &Snapshot, contents: &[u8], backup: bool) -> Result<()> {
    let path = &snapshot.path;

    if snapshot.permissions.readonly() {
        return Err(at_path(path, "source is read-only"));
    }

    let mut temporary = NamedTempFile::new_in(path.parent().expect("absolute source path"))
        .map_err(|error| at_path(path, error))?;

    temporary
        .write_all(contents)
        .map_err(|error| at_path(path, error))?;

    temporary
        .as_file()
        .set_permissions(snapshot.permissions.clone())
        .map_err(|error| at_path(path, error))?;

    temporary
        .as_file()
        .sync_all()
        .map_err(|error| at_path(path, error))?;

    snapshot.verify()?;

    if backup {
        let backup_path = path.with_extension("bk");

        if backup_path == *path {
            return Err(at_path(path, "backup path would overwrite the source"));
        }

        let mut saved = NamedTempFile::new_in(path.parent().expect("absolute source path"))
            .map_err(|error| at_path(&backup_path, error))?;

        saved
            .write_all(&snapshot.original)
            .map_err(|error| at_path(&backup_path, error))?;

        saved
            .as_file()
            .set_permissions(snapshot.permissions.clone())
            .map_err(|error| at_path(&backup_path, error))?;

        saved
            .persist(&backup_path)
            .map_err(|error| at_path(&backup_path, error))?;
    }

    snapshot.verify()?;

    temporary
        .persist(path)
        .map_err(|error| at_path(path, error))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use tempfile::{TempDir, tempdir};

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    const SOURCE: &str = "trait Store { fn open(&self); fn close(&self); }\nfn main(){let answer=42; if answer>0 {call(answer);} call(answer);}\n";

    fn binary() -> PathBuf {
        if let Some(binary) = option_env!("CARGO_BIN_EXE_nitpick") {
            return binary.into();
        }

        env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("nitpick{}", env::consts::EXE_SUFFIX))
    }

    fn directory() -> TempDir {
        tempdir().unwrap()
    }

    fn invoke(directory: &Path, arguments: &[&str], input: Option<&str>) -> Output {
        let mut child = Command::new(binary())
            .current_dir(directory)
            .args(arguments)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }

        child.wait_with_output().unwrap()
    }

    fn success(output: &Output) {
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn project(directory: &Path, manifest: &str, sources: &[(&str, &str)]) {
        fs::create_dir_all(directory).unwrap();
        fs::write(directory.join("Cargo.toml"), manifest).unwrap();

        for (path, source) in sources {
            let path = directory.join(path);

            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
    }

    #[test]
    fn no_arguments_formats_all_cargo_targets_and_modules() {
        let directory = directory();
        let root = directory.path();

        let sources = [
            (
                "src/lib.rs",
                "mod child;\npub async fn library(){if true{call();}}\n",
            ),
            ("src/child.rs", "fn child(){if true{call();}}\n"),
            ("src/main.rs", SOURCE),
            ("src/bin/extra.rs", SOURCE),
            ("tests/integration.rs", SOURCE),
            ("examples/demo.rs", SOURCE),
            ("benches/speed.rs", SOURCE),
            ("build.rs", SOURCE),
        ];

        project(
            root,
            "[package]\nname = \"project\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            &sources,
        );

        fs::write(
            root.join("unrelated.rs"),
            "invalid Rust that must not be read",
        )
        .unwrap();

        let checked = invoke(root, &["--check", "--files-with-diff"], None);

        assert_eq!(checked.status.code(), Some(1), "{checked:?}");

        let names = String::from_utf8(checked.stdout).unwrap();

        for (path, original) in sources {
            assert!(names.contains(root.join(path).canonicalize().unwrap().to_str().unwrap()));
            assert_eq!(fs::read_to_string(root.join(path)).unwrap(), original);
        }

        let preview = invoke(root, &["--emit", "stdout"], None);

        success(&preview);

        for (path, original) in sources {
            assert_eq!(fs::read_to_string(root.join(path)).unwrap(), original);
        }

        success(&invoke(root, &[], None));

        for (path, original) in sources {
            let formatted = fs::read_to_string(root.join(path)).unwrap();

            assert_ne!(formatted, original, "{path}");
        }

        success(&invoke(root, &["--check"], None));

        assert_eq!(
            fs::read_to_string(root.join("unrelated.rs")).unwrap(),
            "invalid Rust that must not be read"
        );
    }

    #[test]
    fn project_formatting_separates_multiline_fields() {
        let directory = directory();
        let root = directory.path();
        let source = "pub struct Options {\n    /// Override whitespace rules.\n    #[arg(long)]\n    pub whitespace: Option<String>,\n    /// Check formatting without changing files.\n    #[arg(long)]\n    pub check: bool,\n}\n";

        let expected = source.replace(
            "Option<String>,\n    /// Check",
            "Option<String>,\n\n    /// Check",
        );

        project(
            root,
            "[package]\nname = \"fields\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            &[("src/lib.rs", source)],
        );

        let checked = invoke(root, &["--check"], None);

        assert_eq!(checked.status.code(), Some(1), "{checked:?}");
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), source);

        success(&invoke(root, &[], None));

        assert_eq!(
            fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            expected
        );

        success(&invoke(root, &["--check"], None));
    }

    #[test]
    fn project_formatting_separates_multiline_match_arms() {
        let directory = directory();
        let root = directory.path();

        let source = r#"fn main() {
    let configuration = match path {
        Some(path) => {
            let contents = read(path);

            parse(contents)
        }
        None => default(),
    };
}
"#;

        let expected = r#"fn main() {
    let configuration = match path {
        Some(path) => {
            let contents = read(path);

            parse(contents)
        }

        None => default(),
    };
}
"#;

        project(
            root,
            "[package]\nname = \"arms\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            &[("src/main.rs", source)],
        );

        let checked = invoke(root, &["--check"], None);

        assert_eq!(checked.status.code(), Some(1), "{checked:?}");

        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            source
        );

        let piped = invoke(root, &["-"], Some(source));

        success(&piped);

        assert_eq!(String::from_utf8(piped.stdout).unwrap(), expected);

        success(&invoke(root, &[], None));

        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            expected
        );

        success(&invoke(root, &["--check"], None));
    }

    #[test]
    fn workspace_discovery_respects_package_location_editions_and_configs() {
        let directory = directory();
        let root = directory.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"legacy\", \"modern\"]\nresolver = \"3\"\n",
        )
        .unwrap();

        let legacy = root.join("legacy");
        let modern = root.join("modern");
        let legacy_source = "pub fn async(){if true{call();}}\n";
        let modern_source = "pub async fn modern(){if true{call();}}\n";

        project(
            &legacy,
            "[package]\nname = \"legacy\"\nversion = \"0.1.0\"\nedition = \"2015\"\n",
            &[("src/lib.rs", legacy_source)],
        );

        project(
            &modern,
            "[package]\nname = \"modern\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            &[("src/lib.rs", modern_source)],
        );

        fs::write(modern.join("rustfmt.toml"), "hard_tabs = true\n").unwrap();

        fs::write(
            modern.join("nitpick.toml"),
            "[whitespace]\nblank_line_at_end_of_if = false\n",
        )
        .unwrap();

        let checked = invoke(root, &["--check", "--files-with-diff"], None);

        assert_eq!(checked.status.code(), Some(1), "{checked:?}");

        let names = String::from_utf8(checked.stdout).unwrap();

        assert!(names.contains("legacy/src/lib.rs"));
        assert!(names.contains("modern/src/lib.rs"));

        success(&invoke(&modern.join("src"), &[], None));

        assert_eq!(
            fs::read_to_string(legacy.join("src/lib.rs")).unwrap(),
            legacy_source
        );

        let modern_formatted = fs::read_to_string(modern.join("src/lib.rs")).unwrap();

        assert!(modern_formatted.contains("\t\tcall();\n\t}\n"));

        let checked = invoke(root, &["--check", "--files-with-diff"], None);

        assert_eq!(checked.status.code(), Some(1), "{checked:?}");
        assert!(!String::from_utf8_lossy(&checked.stdout).contains("modern/src/lib.rs"));

        success(&invoke(root, &[], None));
        success(&invoke(root, &["--check"], None));
    }

    #[test]
    fn stdin_and_explicit_files_take_precedence_over_project_discovery() {
        let directory = directory();
        let root = directory.path();

        project(
            root,
            "[package]\nname = \"project\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            &[("src/main.rs", SOURCE), ("separate.rs", SOURCE)],
        );

        let piped = invoke(root, &[], Some(SOURCE));
        let explicit = invoke(root, &["-"], Some(SOURCE));
        let empty = invoke(root, &["-"], Some(""));

        success(&piped);
        success(&explicit);
        success(&empty);

        assert_eq!(piped.stdout, explicit.stdout);

        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            SOURCE
        );

        let invalid = invoke(root, &["-", "separate.rs"], Some(SOURCE));

        assert_eq!(invalid.status.code(), Some(2));

        assert_eq!(
            fs::read_to_string(root.join("separate.rs")).unwrap(),
            SOURCE
        );

        success(&invoke(root, &["separate.rs"], None));

        assert_ne!(
            fs::read_to_string(root.join("separate.rs")).unwrap(),
            SOURCE
        );

        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            SOURCE
        );
    }

    #[test]
    fn project_discovery_reports_missing_and_invalid_manifests() {
        let directory = directory();
        let root = directory.path();
        let missing = invoke(root, &[], None);

        assert_eq!(missing.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&missing.stderr).contains("no Cargo.toml found"));

        fs::write(root.join("Cargo.toml"), "[invalid").unwrap();

        let invalid = invoke(root, &[], None);

        assert_eq!(invalid.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("cargo metadata failed"));

        success(&invoke(root, &["-"], Some(SOURCE)));
    }

    #[test]
    fn compact_if_expressions_stay_compact_through_files_and_stdin() {
        let directory = directory();
        let root = directory.path();
        let path = root.join("main.rs");
        let source = "fn main() {\n    let prefix_length = if parsed.command.is_some() {\n        2\n\n    } else {\n        1\n    };\n}\n";

        let expected = "fn main() {\n    let prefix_length = if parsed.command.is_some() { 2 } else { 1 };\n}\n";

        fs::write(&path, source).unwrap();

        let piped = invoke(root, &[], Some(source));

        success(&piped);

        assert_eq!(piped.stdout, expected.as_bytes());

        success(&invoke(root, &["main.rs"], None));

        assert_eq!(fs::read_to_string(&path).unwrap(), expected);

        success(&invoke(root, &["--check", "main.rs"], None));
    }

    #[test]
    fn branch_padding_requires_multiple_body_lines_and_a_following_else() {
        let directory = directory();
        let root = directory.path();
        let path = root.join("main.rs");

        let source = r#"fn main() {
    let expected = if case[0] == "rustfmt" {
        &case[1..]
    } else {
        &case
    };
    if output.status.success() {
        result.map_err(|error| at_path(Path::new("<stdin>"), error))?;
    }
    if ready {
        first();
        second();
    }
    if waiting {
        first();
        second();
    } else {
        other();
    }
}
"#;

        let expected = r#"fn main() {
    let expected = if case[0] == "rustfmt" {
        &case[1..]
    } else {
        &case
    };

    if output.status.success() {
        result.map_err(|error| at_path(Path::new("<stdin>"), error))?;
    }

    if ready {
        first();
        second();
    }

    if waiting {
        first();
        second();

    } else {
        other();
    }
}
"#;

        fs::write(&path, source).unwrap();

        let piped = invoke(root, &[], Some(source));

        success(&piped);

        assert_eq!(piped.stdout, expected.as_bytes());

        success(&invoke(root, &["main.rs"], None));

        assert_eq!(fs::read_to_string(&path).unwrap(), expected);

        success(&invoke(root, &["--check", "main.rs"], None));
    }

    #[test]
    fn combined_check_compares_final_style_without_writing() {
        let directory = directory();
        let path = directory.path().join("main.rs");
        let source = "fn main(){if ready{first();second();}else{third();}}\n";
        let upstream = invoke(directory.path(), &["rustfmt"], Some(source));

        success(&upstream);

        fs::write(&path, &upstream.stdout).unwrap();

        let check = invoke(directory.path(), &["--check", "main.rs"], None);

        assert_eq!(check.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&check.stdout).contains("@@"));
        assert_eq!(fs::read(&path).unwrap(), upstream.stdout);

        success(&invoke(directory.path(), &["main.rs"], None));

        let final_source = fs::read(&path).unwrap();
        let raw_check = invoke(directory.path(), &["rustfmt", "--check", "main.rs"], None);

        assert_eq!(raw_check.status.code(), Some(1));

        success(&invoke(directory.path(), &["--check", "main.rs"], None));
        success(&invoke(directory.path(), &["main.rs"], None));

        assert_eq!(fs::read(&path).unwrap(), final_source);
    }

    #[test]
    fn settings_matrix_is_stable_and_stdin_matches_files() {
        let directory = directory();
        let path = directory.path().join("main.rs");

        let rule_names = [
            "blank_lines_around_multiline",
            "blank_line_at_end_of_if",
            "blank_lines_between_trait_methods",
            "blank_lines_between_node_types",
        ];

        let configurations = [
            "",
            "max_width=40,tab_spaces=2",
            "hard_tabs=true,newline_style=Windows",
            "edition=2024,max_width=60",
        ];

        for rule in rule_names {
            fs::write(
                directory.path().join("nitpick.toml"),
                format!("[whitespace]\n{rule} = false\n"),
            )
            .unwrap();

            for configuration in configurations {
                fs::write(&path, SOURCE).unwrap();

                let flags = if configuration.is_empty() {
                    Vec::new()
                } else {
                    vec!["--config", configuration]
                };

                let stdin = invoke(directory.path(), &flags, Some(SOURCE));

                success(&stdin);

                let arguments = [&flags[..], &["main.rs"]].concat();

                success(&invoke(directory.path(), &arguments, None));

                let first = fs::read(&path).unwrap();

                assert_eq!(first, stdin.stdout, "{rule}, {configuration}");

                success(&invoke(directory.path(), &arguments, None));

                assert_eq!(fs::read(&path).unwrap(), first);

                let check_arguments = [&["--check"][..], &arguments].concat();

                success(&invoke(directory.path(), &check_arguments, None));
            }
        }
    }

    #[test]
    fn stdout_reporting_and_backups_use_final_buffers() {
        let directory = directory();
        let path = directory.path().join("main.rs");

        fs::write(&path, SOURCE).unwrap();

        let stdout = invoke(
            directory.path(),
            &[
                "--emit",
                "stdout",
                "--backup",
                "--files-with-diff",
                "--verbose",
                "main.rs",
            ],
            None,
        );

        success(&stdout);

        let label = format!("{}:\n\n", path.canonicalize().unwrap().display());

        assert!(stdout.stdout.starts_with(label.as_bytes()));
        assert!(!String::from_utf8_lossy(&stdout.stdout).contains("Formatting"));
        assert!(!String::from_utf8_lossy(&stdout.stderr).contains("/tree/"));
        assert_eq!(fs::read_to_string(&path).unwrap(), SOURCE);
        assert!(!path.with_extension("bk").exists());

        let changed = invoke(
            directory.path(),
            &["--check", "--files-with-diff", "main.rs"],
            None,
        );

        assert_eq!(changed.status.code(), Some(1));

        assert_eq!(
            String::from_utf8_lossy(&changed.stdout).trim(),
            path.canonicalize().unwrap().to_str().unwrap()
        );

        assert_eq!(fs::read_to_string(&path).unwrap(), SOURCE);

        let written = invoke(
            directory.path(),
            &[
                "--backup",
                "--files-with-diff",
                "--config",
                "emit_mode=Stdout,make_backup=true",
                "main.rs",
            ],
            None,
        );

        success(&written);

        assert_eq!(written.stdout, changed.stdout);

        assert_eq!(
            fs::read_to_string(path.with_extension("bk")).unwrap(),
            SOURCE
        );

        assert_eq!(fs::read(&path).unwrap(), stdout.stdout[label.len()..]);

        success(&invoke(directory.path(), &["--backup", "main.rs"], None));

        assert_eq!(
            fs::read_to_string(path.with_extension("bk")).unwrap(),
            SOURCE
        );
    }

    #[test]
    fn configs_are_per_file_and_explicit_paths_work_outside_the_source_tree() {
        let directory = directory();
        let root = directory.path().canonicalize().unwrap();

        fs::create_dir(root.join("child")).unwrap();
        fs::create_dir(root.join("outside")).unwrap();

        fs::write(
            root.join("rustfmt.toml"),
            "edition = \"2024\"\nhard_tabs = true\n",
        )
        .unwrap();

        fs::write(root.join("child/rustfmt.toml"), "tab_spaces = 2\n").unwrap();

        fs::write(
            root.join("child/nitpick.toml"),
            "[whitespace]\nblank_line_at_end_of_if = false\n",
        )
        .unwrap();

        fs::write(
            root.join("lib.rs"),
            "mod child;\nfn main(){if true{call(); finish();} else {other();}}\n",
        )
        .unwrap();

        fs::write(
            root.join("child/mod.rs"),
            "fn child(){if true{call(); finish();} else {other();}}\n",
        )
        .unwrap();

        let file = root.join("lib.rs");
        let child = root.join("child/mod.rs");

        let stdout = invoke(
            &root.join("outside"),
            &[
                "--emit",
                "stdout",
                child.to_str().unwrap(),
                file.to_str().unwrap(),
                child.to_str().unwrap(),
            ],
            None,
        );

        success(&stdout);

        let contents = String::from_utf8_lossy(&stdout.stdout);

        assert_eq!(
            contents.matches(&format!("{}:\n", child.display())).count(),
            1
        );

        assert!(contents.contains("\t\tfinish();\n\t} else {"));
        assert!(contents.contains("\t\tfinish();\n\n\t} else {"));

        assert_eq!(
            fs::read_to_string(&child).unwrap(),
            "fn child(){if true{call(); finish();} else {other();}}\n"
        );

        fs::write(
            root.join("selected.toml"),
            "[whitespace]\nblank_line_at_end_of_if = false\n",
        )
        .unwrap();

        let selected = root.join("selected.toml");
        let config_path = root.join("child/rustfmt.toml");

        let flags = [
            "--nitpick-config",
            selected.to_str().unwrap(),
            "--config-path",
            config_path.to_str().unwrap(),
        ];

        let stdin = invoke(
            &root.join("outside"),
            &flags,
            Some("fn main(){if true{call(); finish();} else {other();}}\n"),
        );

        success(&stdin);

        assert!(
            String::from_utf8_lossy(&stdin.stdout)
                .contains("  if true {\n    call();\n    finish();\n  } else {")
        );

        let forced = invoke(
            &root.join("outside"),
            &[&flags[..], &["--whitespace", "on"]].concat(),
            Some("fn main(){if true{call(); finish();} else {other();}}\n"),
        );

        success(&forced);

        assert!(String::from_utf8_lossy(&forced.stdout).contains("    finish();\n\n  } else {"));

        fs::write(root.join("other.rs"), SOURCE).unwrap();
        fs::write(root.join("child/other.rs"), SOURCE).unwrap();

        let distinct = invoke(
            &root,
            &["--emit", "stdout", "other.rs", "child/other.rs"],
            None,
        );

        success(&distinct);

        assert!(String::from_utf8_lossy(&distinct.stdout).contains("  let answer"));
        assert!(String::from_utf8_lossy(&distinct.stdout).contains("\tlet answer"));
    }

    #[test]
    fn explicit_rustfmt_directory_search_matches_the_installed_child() {
        let directory = directory();
        let root = directory.path();

        fs::create_dir_all(root.join("project/source")).unwrap();
        fs::create_dir_all(root.join("settings/nested")).unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        fs::write(root.join("project/rustfmt.toml"), "tab_spaces = 2\n").unwrap();
        fs::write(root.join("settings/.rustfmt.toml"), "hard_tabs = true\n").unwrap();

        let path = root.join("project/source/main.rs");

        for selected in ["settings", "settings/nested", "empty"] {
            fs::write(&path, SOURCE).unwrap();

            let direct = invoke(
                root,
                &[
                    "rustfmt",
                    "--config-path",
                    selected,
                    "project/source/main.rs",
                ],
                None,
            );

            let expected = fs::read(&path).unwrap();

            fs::write(&path, SOURCE).unwrap();

            let arguments = [
                "--whitespace",
                "off",
                "--config-path",
                selected,
                "project/source/main.rs",
            ];

            let staged = invoke(root, &arguments, None);

            assert_eq!(staged.status.code(), direct.status.code(), "{selected}");
            assert_eq!(staged.stderr, direct.stderr, "{selected}");
            assert_eq!(fs::read(&path).unwrap(), expected, "{selected}");
        }
    }

    #[test]
    fn upstream_flag_rejections_and_stdin_reporting_are_explicit() {
        let directory = directory();

        let cases: &[&[&str]] = &[
            &["--check", "--emit", "files"],
            &["--check", "--emit", "stdout"],
            &["--verbose", "--quiet"],
            &["--emit", "files", "--emit", "stdout"],
        ];

        for arguments in cases {
            let wrapped = invoke(directory.path(), arguments, Some(SOURCE));

            let direct = invoke(
                directory.path(),
                &[&["rustfmt"][..], arguments].concat(),
                Some(SOURCE),
            );

            assert_eq!(wrapped.status.code(), direct.status.code());
            assert_eq!(wrapped.stdout, direct.stdout);
            assert_eq!(wrapped.stderr, direct.stderr);
        }

        let changed = invoke(
            directory.path(),
            &["--check", "--files-with-diff"],
            Some(SOURCE),
        );

        assert_eq!(changed.status.code(), Some(1));
        assert_eq!(changed.stdout, b"<stdin>\n");
    }

    #[test]
    fn invalid_config_and_child_failure_leave_all_originals_untouched() {
        let directory = directory();
        let root = directory.path();

        fs::write(root.join("first.rs"), SOURCE).unwrap();
        fs::write(root.join("second.rs"), "fn broken( {").unwrap();

        let invalid = invoke(root, &["--backup", "first.rs", "second.rs"], None);

        assert!(!invalid.status.success());
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("second.rs"));
        assert!(!String::from_utf8_lossy(&invalid.stderr).contains("/tree/"));
        assert_eq!(fs::read_to_string(root.join("first.rs")).unwrap(), SOURCE);
        assert!(!root.join("first.bk").exists());

        fs::write(root.join("second.rs"), SOURCE).unwrap();
        fs::write(root.join("bad.toml"), "[whitespace]\nunknown = true").unwrap();

        let invalid = invoke(
            root,
            &["--nitpick-config", "bad.toml", "first.rs", "second.rs"],
            None,
        );

        assert_eq!(invalid.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("bad.toml"));
        assert_eq!(fs::read_to_string(root.join("first.rs")).unwrap(), SOURCE);
        assert_eq!(fs::read_to_string(root.join("second.rs")).unwrap(), SOURCE);

        let missing = invoke(
            root,
            &["--nitpick-config", "missing.toml", "--whitespace", "off"],
            Some(SOURCE),
        );

        assert_eq!(missing.status.code(), Some(2));

        fs::write(root.join("nitpick.toml"), "malformed = [").unwrap();

        for argument in ["--help", "--version"] {
            success(&invoke(root, &[argument], None));
        }

        let raw = invoke(root, &["rustfmt", "--print-config", "default"], None);

        success(&raw);
    }

    #[test]
    fn protected_regions_survive_the_combined_pass_and_off_matches_rustfmt() {
        let directory = directory();
        let input = "#[rustfmt::skip]\nfn skipped(){if true{call();} let a=1;call(a);}\nfn main(){opaque! { if ready { value } };if ready{call();}}\n";
        let output = invoke(directory.path(), &[], Some(input));

        success(&output);

        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("fn skipped(){if true{call();} let a=1;call(a);}")
        );

        assert!(String::from_utf8_lossy(&output.stdout).contains("opaque! { if ready { value } }"));

        let forced_capture = invoke(
            directory.path(),
            &["--config", "emit_mode=Files"],
            Some(input),
        );

        success(&forced_capture);

        assert_eq!(forced_capture.stdout, output.stdout);

        let direct = invoke(directory.path(), &["rustfmt"], Some(input));
        let off = invoke(directory.path(), &["--whitespace", "off"], Some(input));

        success(&off);

        assert_eq!(off.stdout, direct.stdout);

        let checked = invoke(
            directory.path(),
            &["--check"],
            Some(std::str::from_utf8(&output.stdout).unwrap()),
        );

        success(&checked);

        assert!(checked.stdout.is_empty());
    }

    #[test]
    fn unsupported_module_paths_fail_before_any_source_write() {
        let directory = directory();
        let root = directory.path();

        fs::write(root.join("outside.rs"), SOURCE).unwrap();

        let absolute = format!("#[path = {:?}] mod child;\n", root.join("outside.rs"));

        let cases = [
            absolute.as_str(),
            "#[cfg_attr(feature = \"other\", path = \"outside.rs\")] mod child;\n",
            "cfg_if::cfg_if! { if #[cfg(unix)] { mod child; } }\n",
        ];

        for input in cases {
            fs::write(root.join("lib.rs"), input).unwrap();

            let output = invoke(root, &["lib.rs"], None);

            assert_eq!(output.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&output.stderr).contains("nitpick rustfmt"));
            assert_eq!(fs::read_to_string(root.join("lib.rs")).unwrap(), input);
            assert_eq!(fs::read_to_string(root.join("outside.rs")).unwrap(), SOURCE);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_write_or_backup_failures_report_original_paths() {
        let directory = directory();
        let root = directory.path();
        let path = root.join("main.rs");

        fs::write(&path, SOURCE).unwrap();
        symlink(&path, root.join("link.rs")).unwrap();

        let linked = invoke(root, &["link.rs"], None);

        assert_eq!(linked.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&linked.stderr).contains("symlink"));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();

        let readonly = invoke(root, &["main.rs"], None);

        assert_eq!(readonly.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&readonly.stderr).contains("main.rs"));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        fs::create_dir(root.join("main.bk")).unwrap();

        let backup = invoke(root, &["--backup", "main.rs"], None);

        assert_eq!(backup.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&backup.stderr).contains("main.bk"));
        assert_eq!(fs::read_to_string(&path).unwrap(), SOURCE);

        success(&invoke(root, &["main.rs"], None));

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_source_changes_and_child_partial_output_are_never_published() {
        let directory = directory();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("main.rs");

        fs::write(&path, SOURCE).unwrap();

        let executable = root.join("rustfmt-test");

        let real = Command::new("/bin/sh")
            .args(["-c", "command -v rustfmt"])
            .output()
            .unwrap();

        let real = String::from_utf8(real.stdout).unwrap();
        // All paths travel as arguments, including the source changed by the child.
        let script = "#!/bin/sh\nfor arg do\n if [ \"$arg\" = --print-config ]; then exec \"$TEST_REAL_RUSTFMT\" \"$@\"; fi\ndone\nif [ \"$TEST_CHILD_FAILURE\" = yes ]; then printf 'partial output'; printf 'child failed\\n' >&2; exit 37; fi\nprintf 'changed externally\\n' > \"$TEST_ORIGINAL\"\nexec \"$TEST_REAL_RUSTFMT\" \"$@\"\n";

        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        for failure in [true, false] {
            fs::write(&path, SOURCE).unwrap();

            let output = Command::new(binary())
                .current_dir(&root)
                .arg("main.rs")
                .env("NITPICK_RUSTFMT", &executable)
                .env("TEST_REAL_RUSTFMT", real.trim())
                .env("TEST_ORIGINAL", &path)
                .env("TEST_CHILD_FAILURE", if failure { "yes" } else { "no" })
                .output()
                .unwrap();

            assert_eq!(output.status.code(), Some(if failure { 37 } else { 2 }));
            assert!(output.stdout.is_empty());

            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                if failure {
                    SOURCE
                } else {
                    "changed externally\n"
                }
            );

            if !failure {
                assert!(String::from_utf8_lossy(&output.stderr).contains("source changed"));
            }
        }
    }
}
