use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::model::ConfigDir;

/// `~/.config/agentty/config.toml`, written by the user and only read here. Without it agentty
/// shows every session and has no workspaces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub accounts: Vec<Account>,
    pub workspaces: Vec<Workspace>,
    /// `[skills] shared`: the folder that skills shared across accounts live in.
    pub shared_skills: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub name: String,
    pub dir: ConfigDir,
}

/// Sessions whose cwd is under `root`, and the accounts that `n` may start them on.
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub root: PathBuf,
    pub accounts: Vec<ConfigDir>,
}

// A hand-written file, so a misspelt key is an error rather than a silently ignored setting.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireConfig {
    #[serde(default)]
    accounts: BTreeMap<String, String>,
    #[serde(default)]
    workspaces: BTreeMap<String, WireWorkspace>,
    skills: Option<WireSkills>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSkills {
    shared: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireWorkspace {
    root: String,
    accounts: Vec<String>,
}

impl Config {
    pub fn default_path(home: &Path) -> PathBuf {
        home.join(".config/agentty/config.toml")
    }

    pub fn load(path: &Path, home: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => parse(&text, home).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The workspace whose root holds `cwd`, the deepest one when roots nest.
    pub fn workspace_at(&self, cwd: &Path) -> Option<usize> {
        (0..self.workspaces.len())
            .filter(|&i| cwd.starts_with(&self.workspaces[i].root))
            .max_by_key(|&i| self.workspaces[i].root.components().count())
    }

    pub fn workspace_named(&self, name: &str) -> Option<usize> {
        self.workspaces.iter().position(|w| w.name == name)
    }

    /// The configured name, else the config dir's own suffix (`.claude-wukong` → `wukong`).
    pub fn account_name(&self, dir: &ConfigDir) -> String {
        match self.accounts.iter().find(|a| &a.dir == dir) {
            Some(a) => a.name.clone(),
            None if dir.short().is_empty() => "claude".into(),
            None => dir.short().to_owned(),
        }
    }
}

fn parse(text: &str, home: &Path) -> anyhow::Result<Config> {
    let wire: WireConfig = toml::from_str(text)?;
    let accounts: Vec<Account> = wire
        .accounts
        .into_iter()
        .map(|(name, dir)| Account {
            name,
            dir: ConfigDir(expand(home, &dir)),
        })
        .collect();
    let mut workspaces = Vec::new();
    for (name, w) in wire.workspaces {
        if w.accounts.is_empty() {
            bail!("workspace {name} lists no accounts");
        }
        let dirs = w
            .accounts
            .iter()
            .map(|a| match accounts.iter().find(|acc| &acc.name == a) {
                Some(acc) => Ok(acc.dir.clone()),
                None => {
                    bail!("workspace {name} uses account {a}, which [accounts] does not define")
                }
            })
            .collect::<anyhow::Result<_>>()?;
        workspaces.push(Workspace {
            root: expand(home, &w.root),
            name,
            accounts: dirs,
        });
    }
    Ok(Config {
        accounts,
        workspaces,
        shared_skills: wire.skills.map(|s| expand(home, &s.shared)),
    })
}

fn expand(home: &Path, raw: &str) -> PathBuf {
    match raw.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/u";

    fn config() -> Config {
        parse(
            r#"
            [accounts]
            jnj = "~/.claude"
            personal = "~/.claude-personal"
            wukong = "/Users/u/.claude-wukong"

            [workspaces.blade]
            root = "~/workspace/blade"
            accounts = ["personal", "wukong"]

            [workspaces.jnj]
            root = "~/workspace/jnj"
            accounts = ["jnj"]

            [workspaces.web]
            root = "~/workspace/jnj/web"
            accounts = ["jnj"]

            [skills]
            shared = "~/.claude-personal/skills"
            "#,
            Path::new(HOME),
        )
        .unwrap()
    }

    fn dir(p: &str) -> ConfigDir {
        ConfigDir(PathBuf::from(p))
    }

    #[test]
    fn accounts_and_workspaces_expand_home_and_keep_account_order() {
        let c = config();
        let blade = &c.workspaces[c.workspace_named("blade").unwrap()];
        assert_eq!(blade.root, Path::new("/Users/u/workspace/blade"));
        assert_eq!(
            blade.accounts,
            [
                dir("/Users/u/.claude-personal"),
                dir("/Users/u/.claude-wukong")
            ]
        );
        assert_eq!(c.account_name(&dir("/Users/u/.claude")), "jnj");
        assert_eq!(
            c.shared_skills.as_deref(),
            Some(Path::new("/Users/u/.claude-personal/skills"))
        );
        assert_eq!(c.account_name(&dir("/Users/u/.claude-other")), "other");
        assert_eq!(
            Config::default().account_name(&dir("/Users/u/.claude")),
            "claude"
        );
    }

    #[test]
    fn the_deepest_root_holding_the_cwd_wins() {
        let c = config();
        let at = |p: &str| {
            c.workspace_at(Path::new(p))
                .map(|i| c.workspaces[i].name.as_str())
        };
        assert_eq!(at("/Users/u/workspace/jnj/ai-recruiter"), Some("jnj"));
        assert_eq!(at("/Users/u/workspace/jnj/web/src"), Some("web"));
        assert_eq!(at("/Users/u/workspace/blade"), Some("blade"));
        assert_eq!(at("/Users/u/workspace/bladex"), None);
        assert_eq!(at("/Users/u"), None);
    }

    #[test]
    fn mistakes_in_the_file_are_errors() {
        let home = Path::new(HOME);
        let err = |t: &str| parse(t, home).unwrap_err().to_string();
        assert!(
            err("[workspaces.x]\nroot = \"~/x\"\naccounts = [\"nope\"]")
                .contains("account nope, which [accounts] does not define")
        );
        assert!(err("[workspaces.x]\nroot = \"~/x\"\naccounts = []").contains("lists no accounts"));
        assert!(parse("[workspace.x]\nroot = \"~/x\"", home).is_err());
        assert!(parse("[skills]\nshare = \"~/x\"", home).is_err());
        assert_eq!(parse("", home).unwrap(), Config::default());
    }

    #[test]
    fn a_missing_file_is_an_empty_config() {
        let c = Config::load(
            Path::new("/nonexistent/agentty/config.toml"),
            Path::new(HOME),
        );
        assert_eq!(c.unwrap(), Config::default());
    }
}
