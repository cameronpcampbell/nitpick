//! Mirror only resolved module files and their rustfmt lookup context.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, Metadata, Permissions};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use ra_ap_syntax::ast::{self, HasAttrs, HasName};
use ra_ap_syntax::{AstNode, AstToken, SourceFile, SyntaxKind, SyntaxNode};
use tempfile::{TempDir, tempdir};

use super::{Edition, Options, Result, at_path, edition, settings};

#[derive(Clone)]
pub(super) struct Snapshot {
    pub path: PathBuf,
    pub original: Vec<u8>,
    pub permissions: Permissions,
    pub edition: Edition,
    pub affected: bool,
    directories: BTreeSet<PathBuf>,
    metadata: Metadata,
}

impl Snapshot {
    pub fn verify(&self) -> Result<()> {
        safe_path(&self.path)?;

        let current = fs::read(&self.path).map_err(|error| at_path(&self.path, error))?;

        let metadata = fs::metadata(&self.path).map_err(|error| at_path(&self.path, error))?;

        if current != self.original
            || metadata.permissions() != self.permissions
            || metadata.modified().ok() != self.metadata.modified().ok()
            || !same_file(&metadata, &self.metadata)
        {
            return Err(at_path(&self.path, "source changed during formatting"));
        }

        Ok(())
    }
}

#[cfg(unix)]
fn same_file(current: &Metadata, original: &Metadata) -> bool {
    current.dev() == original.dev() && current.ino() == original.ino()
}

#[cfg(not(unix))]
fn same_file(current: &Metadata, original: &Metadata) -> bool {
    current.created().ok() == original.created().ok()
}

pub(super) struct Stage {
    temporary: TempDir,
    mirror: PathBuf,
    roots: Vec<PathBuf>,
    pub snapshots: BTreeMap<PathBuf, Snapshot>,
}

impl Stage {
    pub fn prepare(options: &Options) -> Result<Self> {
        let temporary = tempdir().map_err(|error| at_path(Path::new("staging"), error))?;
        let mirror = temporary.path().join("tree");

        fs::create_dir(&mirror).map_err(|error| at_path(&mirror, error))?;

        let mirror = fs::canonicalize(&mirror).map_err(|error| at_path(&mirror, error))?;

        // Stop rustfmt from inheriting configs above the isolated tree.
        fs::write(mirror.join("rustfmt.toml"), user_configuration()?)
            .map_err(|error| at_path(&mirror, error))?;

        let mut stage = Self {
            temporary,
            mirror,
            roots: Vec::new(),
            snapshots: BTreeMap::new(),
        };

        let mut graphs = Vec::new();
        let mut listed = BTreeSet::new();

        for path in &options.files {
            let path = safe_path(path)?;

            if !listed.insert(path.clone()) {
                continue;
            }

            let effective = settings(options, &path)?;

            for key in ["ignore", "file_lines"] {
                if effective
                    .get(key)
                    .and_then(toml::Value::as_array)
                    .is_some_and(|values| !values.is_empty())
                {
                    return Err(unsupported(
                        &path,
                        &format!("rustfmt setting `{key}` cannot be mirrored safely"),
                    ));
                }
            }

            let language = edition(&effective)?;

            let recursive = !["skip_children", "disable_all_formatting"]
                .iter()
                .any(|key| effective.get(*key).and_then(toml::Value::as_bool) == Some(true));

            let mut graph = BTreeMap::new();

            discover(
                &path,
                language,
                recursive,
                true,
                None,
                &mut graph,
                &mut BTreeSet::new(),
            )?;

            graphs.push((path, graph));
        }

        for (idx, (root, graph)) in graphs.iter().enumerate() {
            let covered =
                graphs
                    .iter()
                    .enumerate()
                    .any(|(other_idx, (other_root, other_graph))| {
                        other_idx != idx
                            && other_graph.contains_key(root)
                            && (!graph.contains_key(other_root) || other_idx < idx)
                    });

            if covered {
                continue;
            }

            stage.roots.push(root.clone());

            for (path, snapshot) in graph {
                if let Some(previous) = stage.snapshots.get(path) {
                    if previous.edition != snapshot.edition {
                        return Err(unsupported(
                            path,
                            "shared module has conflicting root language editions",
                        ));
                    }

                    continue;
                }

                let snapshot = snapshot.clone();

                stage.copy(path, &snapshot.original)?;

                for directory in &snapshot.directories {
                    fs::create_dir_all(stage.path(directory))
                        .map_err(|error| at_path(directory, error))?;
                }

                stage.copy_context(path.parent().expect("absolute path"))?;

                stage.snapshots.insert(path.clone(), snapshot);
            }
        }

        Ok(stage)
    }

    pub fn path(&self, original: &Path) -> PathBuf {
        self.mirror.join(
            original
                .strip_prefix(Path::new("/"))
                .expect("absolute path"),
        )
    }

    fn copy(&self, original: &Path, contents: &[u8]) -> Result<()> {
        let target = self.path(original);

        fs::create_dir_all(target.parent().expect("mirrored path"))
            .map_err(|error| at_path(original, error))?;

        fs::write(target, contents).map_err(|error| at_path(original, error))?;

        Ok(())
    }

    fn copy_context(&self, directory: &Path) -> Result<()> {
        for ancestor in directory.ancestors() {
            for name in [".rustfmt.toml", "rustfmt.toml"] {
                let path = ancestor.join(name);

                if path.is_dir() {
                    continue;
                }

                match fs::read(&path) {
                    Ok(contents) => self.copy(&path, &contents)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(at_path(&path, error)),
                }
            }
        }

        Ok(())
    }

    pub fn arguments(&mut self, options: &Options) -> Result<Vec<OsString>> {
        let mut arguments = options.rustfmt_arguments();

        // Config paths retain their original spelling for stdin, but must point
        // into the mirror for file transformations.
        let mut idx = 0;

        while idx + 1 < arguments.len() {
            if arguments[idx] != "--config-path" {
                idx += 1;

                continue;
            }

            let original = safe_path(Path::new(&arguments[idx + 1]))?;
            let metadata = fs::metadata(&original).map_err(|error| at_path(&original, error))?;

            if metadata.is_dir() {
                let found = original.ancestors().any(|ancestor| {
                    [".rustfmt.toml", "rustfmt.toml"]
                        .iter()
                        .any(|name| ancestor.join(name).is_file())
                });

                if !found {
                    // An empty explicit search falls back to each input's context.
                    arguments.drain(idx..idx + 2);

                    continue;
                }

                self.copy_context(&original)?;

                fs::create_dir_all(self.path(&original))
                    .map_err(|error| at_path(&original, error))?;

            } else {
                self.copy(
                    &original,
                    &fs::read(&original).map_err(|error| at_path(&original, error))?,
                )?;
            }

            arguments[idx + 1] = self.path(&original).into_os_string();
            idx += 2;
        }

        arguments.extend([
            "--emit".into(),
            "files".into(),
            "--config".into(),
            "emit_mode=Files,make_backup=false".into(),
        ]);

        if options.verbose {
            arguments.push("--verbose".into());
        }

        if options.quiet {
            arguments.push("--quiet".into());
        }

        arguments.push("--".into());

        arguments.extend(
            self.roots
                .iter()
                .map(|path| self.path(path).into_os_string()),
        );

        Ok(arguments)
    }

    pub fn diagnostics(&self, text: &str) -> String {
        // The mirror contains the original absolute hierarchy, so stripping its
        // prefix also fixes paths to missing modules in child diagnostics.
        text.replace(&self.mirror.to_string_lossy().to_string(), "")
            .replace(
                &self.temporary.path().to_string_lossy().to_string(),
                "<staging>",
            )
    }
}

fn unsupported(path: &Path, reason: &str) -> super::Failure {
    at_path(
        path,
        format!("{reason}. Use `nitpick rustfmt ...` for direct rustfmt use"),
    )
}

fn safe_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path).map_err(|error| at_path(path, error))?;
    let mut resolved = PathBuf::new();

    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                if !resolved.pop() {
                    return Err(unsupported(path, "path escapes the filesystem root"));
                }
            }

            Component::CurDir => {}
            component => resolved.push(component.as_os_str()),
        }

        if fs::symlink_metadata(&resolved).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            // macOS exposes temporary directories through these system aliases.
            if cfg!(target_os = "macos")
                && [Path::new("/var"), Path::new("/tmp"), Path::new("/etc")]
                    .contains(&resolved.as_path())
            {
                resolved = fs::canonicalize(&resolved).map_err(|error| at_path(path, error))?;
            } else {
                return Err(unsupported(
                    path,
                    "symlinked source or module paths are unsupported",
                ));
            }
        }
    }

    Ok(resolved)
}

fn user_configuration() -> Result<Vec<u8>> {
    let home = env::var_os("HOME").map(PathBuf::from);

    let configuration = if cfg!(target_os = "macos") {
        home.as_ref()
            .map(|home| home.join("Library/Application Support"))

    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".config")))
    };

    for directory in [
        home,
        configuration.map(|directory| directory.join("rustfmt")),
    ]
    .into_iter()
    .flatten()
    {
        for name in [".rustfmt.toml", "rustfmt.toml"] {
            let path = directory.join(name);

            if path.is_file() {
                return fs::read(&path).map_err(|error| at_path(&path, error));
            }
        }
    }

    Ok(Vec::new())
}

fn path_text(path: &ast::Path) -> String {
    path.syntax()
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| !matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT))
        .map(|token| token.text().to_owned())
        .collect()
}

fn skipped(node: &SyntaxNode) -> bool {
    node.children().filter_map(ast::Attr::cast).any(|attr| {
        attr.as_simple_path()
            .is_some_and(|path| path_text(&path) == "rustfmt::skip")
    })
}

fn module_path(module: &ast::Module, owner: &Path) -> Result<Option<PathBuf>> {
    let mut value = None;

    for attr in module.attrs() {
        if attr.simple_name().is_some_and(|name| name == "cfg_attr") {
            return Err(unsupported(owner, "cfg_attr on modules is unsupported"));
        }

        if attr.simple_name().is_none_or(|name| name != "path") {
            continue;
        }

        let literal = attr
            .syntax()
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .find_map(ast::String::cast)
            .ok_or_else(|| unsupported(owner, "module path must be a string literal"))?;

        let path = PathBuf::from(
            literal
                .value()
                .map_err(|error| unsupported(owner, &format!("invalid module path: {error:?}")))?
                .as_ref(),
        );

        if path.is_absolute() || value.is_some() {
            return Err(unsupported(
                owner,
                "absolute or repeated module paths are unsupported",
            ));
        }

        value = Some(path);
    }

    Ok(value)
}

fn discover(
    path: &Path,
    language: Edition,
    recursive: bool,
    root: bool,
    relative: Option<PathBuf>,
    graph: &mut BTreeMap<PathBuf, Snapshot>,
    active: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    fs::metadata(path).map_err(|error| at_path(path, error))?;

    let path = safe_path(path)?;

    if active.contains(&path) {
        return Err(unsupported(&path, "cyclic module graph"));
    }

    if graph.contains_key(&path) {
        return Ok(());
    }

    let original = fs::read(&path).map_err(|error| at_path(&path, error))?;

    let metadata = fs::metadata(&path).map_err(|error| at_path(&path, error))?;
    let permissions = metadata.permissions();

    let source = std::str::from_utf8(&original).map_err(|error| at_path(&path, error))?;
    let parsed = SourceFile::parse(source, language);
    let syntax = parsed.tree().syntax().clone();
    let is_skipped = skipped(&syntax);

    graph.insert(
        path.clone(),
        Snapshot {
            path: path.clone(),
            original,
            permissions,
            edition: language,
            affected: root || !is_skipped,
            directories: BTreeSet::new(),
            metadata,
        },
    );

    if !recursive || is_skipped {
        return Ok(());
    }

    active.insert(path.clone());

    scan(
        &syntax,
        &path,
        path.parent().expect("absolute path"),
        relative.as_deref(),
        language,
        graph,
        active,
    )?;

    active.remove(&path);

    Ok(())
}

fn record_directory(
    owner: &Path,
    directory: &Path,
    graph: &mut BTreeMap<PathBuf, Snapshot>,
) -> Result<()> {
    for ancestor in directory.ancestors() {
        let normalized = safe_path(ancestor)?;

        if ancestor.is_dir() {
            graph
                .get_mut(owner)
                .expect("owner snapshot")
                .directories
                .insert(normalized);
        }
    }

    Ok(())
}

fn scan(
    node: &SyntaxNode,
    owner: &Path,
    directory: &Path,
    relative: Option<&Path>,
    language: Edition,
    graph: &mut BTreeMap<PathBuf, Snapshot>,
    active: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    for child in node.children() {
        if skipped(&child) {
            continue;
        }

        if let Some(call) = ast::MacroCall::cast(child.clone()) {
            let name = call.path().map(|path| path_text(&path)).unwrap_or_default();

            if ["cfg_if", "cfg_select"].contains(&name.rsplit("::").next().unwrap_or_default()) {
                return Err(unsupported(
                    owner,
                    "modules inside cfg_if! or cfg_select! are unsupported",
                ));
            }

            continue;
        }

        if matches!(
            child.kind(),
            SyntaxKind::TOKEN_TREE
                | SyntaxKind::MACRO_RULES
                | SyntaxKind::MACRO_DEF
                | SyntaxKind::ATTR
        ) {
            continue;
        }

        let Some(module) = ast::Module::cast(child.clone()) else {
            // Rustfmt resolves file modules at item-list boundaries only.
            continue;
        };

        let name = module
            .name()
            .ok_or_else(|| unsupported(owner, "module name is missing"))?
            .text()
            .to_string();

        let name = name.strip_prefix("r#").unwrap_or(&name);
        let explicit = module_path(&module, owner)?;

        if let Some(items) = module.item_list() {
            let directory = explicit.map_or_else(
                || directory.join(relative.unwrap_or(Path::new(""))).join(name),
                |path| directory.join(path),
            );

            record_directory(owner, &directory, graph)?;

            let directory = safe_path(&directory)?;

            scan(
                items.syntax(),
                owner,
                &directory,
                None,
                language,
                graph,
                active,
            )?;

            continue;
        }

        let (path, relative) = if let Some(explicit) = explicit {
            (directory.join(explicit), None)
        } else {
            let base = directory.join(relative.unwrap_or(Path::new("")));
            let direct = base.join(format!("{name}.rs"));
            let nested = base.join(name).join("mod.rs");

            if direct.exists() && nested.exists() {
                return Err(unsupported(
                    owner,
                    &format!(
                        "ambiguous module files {} and {}",
                        direct.display(),
                        nested.display()
                    ),
                ));
            }

            if nested.exists() {
                (nested, None)
            } else {
                (direct, Some(PathBuf::from(name)))
            }
        };

        record_directory(owner, path.parent().expect("module parent"), graph)?;
        discover(&path, language, true, false, relative, graph, active)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use nitpick::cli::{self, Invocation};

    use super::super::{capture, check_child};

    fn options(arguments: &[OsString]) -> Box<Options> {
        let invocation = cli::invocation(
            [OsString::from("nitpick")]
                .into_iter()
                .chain(arguments.iter().cloned()),
        )
        .unwrap();

        let Invocation::Format(options) = invocation else {
            panic!("combined invocation");
        };

        options
    }

    #[test]
    fn prototype_matches_rustfmt_module_set_and_root_configuration() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();

        fs::create_dir_all(root.join("nested/inline")).unwrap();
        fs::create_dir_all(root.join("alternate")).unwrap();
        fs::create_dir_all(root.join("flat/inside")).unwrap();

        fs::write(
            root.join("rustfmt.toml"),
            "edition = \"2021\"\nhard_tabs = true\n",
        )
        .unwrap();

        fs::write(
            root.join("nested/rustfmt.toml"),
            "edition = \"2015\"\nhard_tabs = false\n",
        )
        .unwrap();

        let sources = [
            (
                "lib.rs",
                "mod nested;\nmod flat;\n#[path = \"alternate/leaf.rs\"] mod renamed;\n",
            ),
            (
                "nested/mod.rs",
                "pub async fn ready(){if true{call();}}\nmod inline { mod child; }\n",
            ),
            ("nested/inline/child.rs", "fn child(){if true{call();}}\n"),
            (
                "alternate/leaf.rs",
                "mod sibling; fn leaf(){if true{call();}}\n",
            ),
            ("alternate/sibling.rs", "fn sibling(){if true{call();}}\n"),
            (
                "flat.rs",
                "mod inside { mod next; #[path = \"../../neighbor.rs\"] mod shared; }\n#[path = \"neighbor.rs\"] mod neighbor;\n",
            ),
            ("flat/inside/next.rs", "fn next(){if true{call();}}\n"),
            ("neighbor.rs", "fn neighbor(){if true{call();}}\n"),
            ("unrelated.rs", "fn unrelated( {"),
        ];

        for (name, contents) in sources {
            fs::write(root.join(name), contents).unwrap();
        }

        let options = options(&[root.join("lib.rs").into_os_string()]);
        let mut stage = Stage::prepare(&options).unwrap();

        assert_eq!(stage.snapshots.len(), 8);

        assert!(
            stage
                .snapshots
                .values()
                .all(|snapshot| snapshot.edition == Edition::Edition2021)
        );

        assert!(!stage.path(&root.join("unrelated.rs")).exists());

        let arguments = stage.arguments(&options).unwrap();
        let staged = capture(&arguments, None).unwrap();

        check_child(&staged, Some(&stage), false).unwrap();

        let direct = capture(&[root.join("lib.rs").into_os_string()], None).unwrap();

        check_child(&direct, None, false).unwrap();

        for snapshot in stage.snapshots.values() {
            assert_eq!(
                fs::read(stage.path(&snapshot.path)).unwrap(),
                fs::read(&snapshot.path).unwrap(),
                "{}",
                snapshot.path.display()
            );
        }
    }

    #[test]
    fn unchanged_bytes_in_a_replaced_source_still_count_as_a_concurrent_change() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("source.rs");

        fs::write(&path, "fn main() {}\n").unwrap();

        let options = options(&[path.clone().into_os_string()]);
        let stage = Stage::prepare(&options).unwrap();
        let snapshot = stage.snapshots.values().next().unwrap();
        let replacement = temporary.path().join("replacement.rs");

        fs::write(&replacement, &snapshot.original).unwrap();
        fs::rename(replacement, &path).unwrap();

        assert!(
            snapshot
                .verify()
                .unwrap_err()
                .to_string()
                .contains("source changed")
        );
    }

    #[test]
    fn language_edition_comes_from_child_settings_not_style_or_package() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("source.rs");

        fs::write(&path, "fn main() {}\n").unwrap();

        let cases: &[(&[&str], Edition)] = &[
            (&[], Edition::Edition2015),
            (&["--style-edition", "2024"], Edition::Edition2015),
            (&["--edition", "2024"], Edition::Edition2024),
            (
                &["--edition", "2024", "--config", "edition=2018"],
                Edition::Edition2018,
            ),
        ];

        for (flags, expected) in cases {
            let arguments: Vec<_> = flags
                .iter()
                .map(OsString::from)
                .chain([path.clone().into_os_string()])
                .collect();

            let options = options(&arguments);
            let effective = settings(&options, &path).unwrap();

            assert_eq!(edition(&effective).unwrap(), *expected);
        }
    }
}
