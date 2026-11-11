//! Nitpick configuration discovery and invocation overrides.
use crate::whitespace::RuleOptions;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Configuration {
    whitespace: RuleOptions,
}

pub fn discover(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .map(|ancestor| ancestor.join("nitpick.toml"))
        .find(|path| path.exists())
}

pub fn load(
    directory: &Path,
    selected: Option<&Path>,
    override_value: Option<&str>,
) -> Result<Option<RuleOptions>, String> {
    let path = selected
        .map(Path::to_path_buf)
        .or_else(|| discover(directory));

    let configuration = match path {
        Some(path) => {
            let contents = fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;

            toml::from_str::<Configuration>(&contents)
                .map_err(|error| format!("{}: {error}", path.display()))?
        }

        None => Configuration::default(),
    };

    match override_value {
        Some("off") => Ok(None),
        Some("on") => Ok(Some(RuleOptions::default())),
        _ => Ok(Some(configuration.whitespace)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn nearest_configuration_replaces_ancestors_and_cli_overrides_it() {
        let directory = tempdir().unwrap();
        let child = directory.path().join("child");

        fs::create_dir(&child).unwrap();

        let parent = directory.path().join("nitpick.toml");

        fs::write(
            &parent,
            "[whitespace]\nblank_lines_between_node_types = false\n",
        )
        .unwrap();

        fs::write(
            child.join("nitpick.toml"),
            "[whitespace]\nblank_line_at_end_of_if = false\n",
        )
        .unwrap();

        let options = load(&child, None, None).unwrap().unwrap();

        assert!(options.blank_lines_between_node_types);
        assert!(!options.blank_line_at_end_of_if);

        assert!(
            !load(&child, Some(&parent), None)
                .unwrap()
                .unwrap()
                .blank_lines_between_node_types
        );

        assert_eq!(
            load(&child, None, Some("on")).unwrap(),
            Some(RuleOptions::default())
        );

        assert_eq!(load(&child, None, Some("off")).unwrap(), None);
    }

    #[test]
    fn invalid_configuration_is_an_error_even_with_an_override() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nitpick.toml");

        for contents in [
            "[",
            "unknown = true",
            "[whitespace]\nunknown = true",
            "[whitespace]\nblank_line_at_end_of_if = 1",
        ] {
            fs::write(&path, contents).unwrap();

            for override_value in [None, Some("on"), Some("off")] {
                assert!(
                    load(directory.path(), None, override_value)
                        .unwrap_err()
                        .contains("nitpick.toml")
                );
            }
        }

        assert!(
            load(
                directory.path(),
                Some(&directory.path().join("missing")),
                None
            )
            .is_err()
        );
    }
}
