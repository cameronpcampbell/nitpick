//! Discover Cargo targets and their language editions without building the project.
use super::{Failure, Options, Result, at_path};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_root: PathBuf,
    workspace_members: BTreeSet<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    src_path: PathBuf,
    edition: String,
}

pub(super) fn discover(options: &Options) -> Result<Vec<Options>> {
    let directory = env::current_dir()
        .map_err(|error| Failure::Wrapper(format!("could not read working directory: {error}")))?;

    let manifest = directory
        .ancestors()
        .map(|ancestor| ancestor.join("Cargo.toml"))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            Failure::Wrapper(
                "no Cargo.toml found in this directory or its parents. Supply source files or use - for stdin"
                    .into(),
            )
        })?;

    let manifest = fs::canonicalize(&manifest).map_err(|error| at_path(&manifest, error))?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    let output = Command::new(cargo)
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
            "--manifest-path",
        ])
        .arg(&manifest)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Failure::Wrapper(format!("could not run cargo metadata: {error}")))?;

    if !output.status.success() {
        return Err(Failure::Wrapper(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }

    let metadata: Metadata = serde_json::from_slice(&output.stdout)
        .map_err(|error| Failure::Wrapper(format!("could not read cargo metadata: {error}")))?;

    let workspace = fs::canonicalize(&metadata.workspace_root)
        .map_err(|error| at_path(&metadata.workspace_root, error))?;

    let whole_workspace = manifest.parent() == Some(workspace.as_path());
    let mut targets: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();

    for package in metadata.packages {
        if !metadata.workspace_members.contains(&package.id) {
            continue;
        }

        if !whole_workspace && package.manifest_path != manifest {
            continue;
        }

        for target in package.targets {
            let edition = options.edition.last().cloned().unwrap_or(target.edition);

            targets.entry(edition).or_default().insert(target.src_path);
        }
    }

    if targets.is_empty() {
        return Err(at_path(&manifest, "no Cargo targets found to format"));
    }

    Ok(targets
        .into_iter()
        .map(|(edition, files)| {
            let mut group = options.clone();

            group.edition = vec![edition];
            group.files = files.into_iter().collect();

            group
        })
        .collect())
}
