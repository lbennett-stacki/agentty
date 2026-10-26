use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::ConfigDir;

/// Every account's `skills/` folder, compared skill by skill. agentty only lists and resolves
/// these paths; the `share-skill` agent it can start does any moving and linking.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inventory {
    pub accounts: Vec<ConfigDir>,
    /// Each account's `skills/` once symlinks resolve, `None` when it has none. Two accounts with
    /// the same folder hold the same skills, so linking one links both.
    pub folders: Vec<Option<PathBuf>>,
    /// Sorted by name.
    pub skills: Vec<Skill>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    /// One per account, in `Inventory::accounts` order.
    pub cells: Vec<Cell>,
    /// More than one copy, and their `SKILL.md`s are not byte-identical.
    pub differ: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    Missing,
    /// A symlink whose target is gone.
    Broken,
    /// `real` is the skill folder once symlinks resolve. `linked` means the account reaches it
    /// through a symlink (its own entry, or its whole `skills/` folder).
    Has {
        real: PathBuf,
        linked: bool,
    },
}

/// What a skill needs, ignoring the user's "keep local" mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spread {
    /// Distinct real folders across the accounts that have it.
    pub copies: usize,
    pub missing: Vec<usize>,
    pub broken: Vec<usize>,
}

impl Spread {
    pub fn shared(&self) -> bool {
        self.copies == 1 && self.missing.is_empty() && self.broken.is_empty()
    }
}

/// Claude.ai syncs this folder per org; it is not the user's to share.
const SYNCED: &str = "synced";

pub fn scan(accounts: &[ConfigDir]) -> Inventory {
    let mut by_name: BTreeMap<String, Vec<Cell>> = BTreeMap::new();
    let mut folders = Vec::new();
    for (i, dir) in accounts.iter().enumerate() {
        let root = dir.0.join("skills");
        let folder_linked = root.is_symlink();
        folders.push(fs::canonicalize(&root).ok().filter(|r| r.is_dir()));
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.') || name == SYNCED {
                continue;
            }
            let path = entry.path();
            let cell = match fs::canonicalize(&path) {
                Ok(real) if real.join("SKILL.md").is_file() => Cell::Has {
                    linked: folder_linked || path.is_symlink(),
                    real,
                },
                Ok(_) => continue,
                Err(_) if path.is_symlink() => Cell::Broken,
                Err(_) => continue,
            };
            by_name
                .entry(name)
                .or_insert_with(|| vec![Cell::Missing; accounts.len()])[i] = cell;
        }
    }
    let skills = by_name
        .into_iter()
        .map(|(name, cells)| {
            let differ = differ(&cells);
            Skill {
                name,
                cells,
                differ,
            }
        })
        .collect();
    Inventory {
        accounts: accounts.to_vec(),
        folders,
        skills,
    }
}

fn differ(cells: &[Cell]) -> bool {
    let mut texts: Vec<Option<Vec<u8>>> = reals(cells)
        .iter()
        .map(|r| fs::read(r.join("SKILL.md")).ok())
        .collect();
    texts.dedup();
    texts.len() > 1
}

fn reals(cells: &[Cell]) -> Vec<&Path> {
    let mut out: Vec<&Path> = Vec::new();
    for c in cells {
        if let Cell::Has { real, .. } = c
            && !out.contains(&real.as_path())
        {
            out.push(real);
        }
    }
    out
}

impl Skill {
    pub fn spread(&self) -> Spread {
        let at = |want: fn(&Cell) -> bool| {
            (0..self.cells.len())
                .filter(|&i| want(&self.cells[i]))
                .collect()
        };
        Spread {
            copies: reals(&self.cells).len(),
            missing: at(|c| *c == Cell::Missing),
            broken: at(|c| *c == Cell::Broken),
        }
    }
}

impl Inventory {
    /// The prompt for a `share-skill` agent: where each copy is, which folders lack it, and where
    /// shared skills live. `name_of` gives each account's configured name.
    pub fn share_prompt(
        &self,
        skill: &Skill,
        shared: &Path,
        name_of: impl Fn(&ConfigDir) -> String,
    ) -> String {
        let spread = skill.spread();
        let mut out = format!(
            "/share-skill {}\n\nShared skills folder: {}\n",
            skill.name,
            shared.display()
        );
        out.push_str("Copies:\n");
        for real in reals(&skill.cells) {
            let holders: Vec<String> = (0..self.accounts.len())
                .filter(|&i| matches!(&skill.cells[i], Cell::Has { real: r, .. } if r == real))
                .map(|i| name_of(&self.accounts[i]))
                .collect();
            out.push_str(&format!(
                "- {} (used by {})\n",
                real.display(),
                holders.join(", ")
            ));
        }
        if skill.differ {
            out.push_str(
                "The copies' SKILL.md files differ: reconcile them into one before linking.\n",
            );
        }
        // Accounts that share one `skills/` folder need one link, so group them by folder.
        let mut lacking: Vec<(String, Vec<String>, bool)> = Vec::new();
        for &i in spread.missing.iter().chain(&spread.broken) {
            let folder = self.folders[i]
                .clone()
                .unwrap_or_else(|| self.accounts[i].0.join("skills"))
                .display()
                .to_string();
            let name = name_of(&self.accounts[i]);
            let broken = skill.cells[i] == Cell::Broken;
            match lacking.iter_mut().find(|(f, ..)| *f == folder) {
                Some((_, names, b)) => {
                    names.push(name);
                    *b |= broken;
                }
                None => lacking.push((folder, vec![name], broken)),
            }
        }
        if !lacking.is_empty() {
            out.push_str("Link it into:\n");
            for (folder, names, broken) in lacking {
                let note = if broken {
                    "; its entry there is a broken symlink"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "- {folder} (account {}){note}\n",
                    names.join(", ")
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::tests::Scratch;
    use std::os::unix::fs::symlink;

    fn skill(s: &Scratch, rel: &str, text: &str) {
        s.write(&format!("{rel}/SKILL.md"), text.as_bytes());
    }

    fn link(s: &Scratch, target: &str, at: &str) {
        let at = s.path().join(at);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        symlink(s.path().join(target), at).unwrap();
    }

    fn dirs(s: &Scratch, names: &[&str]) -> Vec<ConfigDir> {
        names.iter().map(|n| ConfigDir(s.path().join(n))).collect()
    }

    impl Inventory {
        fn skill(&self, name: &str) -> Option<&Skill> {
            self.skills.iter().find(|s| s.name == name)
        }
    }

    fn real(s: &Scratch, rel: &str) -> PathBuf {
        fs::canonicalize(s.path().join(rel)).unwrap()
    }

    #[test]
    fn scan_resolves_links_and_skips_synced_hidden_and_non_skills() {
        let s = Scratch::new("skills-scan");
        skill(&s, ".claude/skills/verify", "jnj only");
        skill(&s, ".claude-personal/skills/jot", "jot");
        link(&s, ".claude-personal/skills/jot", ".claude/skills/jot");
        link(&s, ".claude-personal/skills", ".claude-wukong/skills");
        link(&s, ".claude/skills/gone", ".claude/skills/dead");
        skill(&s, ".claude/skills/synced/org/docs", "org");
        skill(&s, ".claude/skills/.hidden", "x");
        s.write(".claude/skills/splice-pr-workspace/grade.py", b"");
        let inv = scan(&dirs(
            &s,
            &[".claude", ".claude-personal", ".claude-wukong"],
        ));

        let names: Vec<&str> = inv.skills.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["dead", "jot", "verify"]);
        assert_eq!(inv.folders[1], inv.folders[2]);

        let jot = inv.skill("jot").unwrap();
        let shared = real(&s, ".claude-personal/skills/jot");
        let has = |linked| Cell::Has {
            real: shared.clone(),
            linked,
        };
        assert_eq!(jot.cells, [has(true), has(false), has(true)]);
        assert!(jot.spread().shared());

        let verify = inv.skill("verify").unwrap().spread();
        assert_eq!((verify.copies, verify.missing), (1, vec![1, 2]));
        assert_eq!(inv.skill("dead").unwrap().spread().broken, [0]);
    }

    #[test]
    fn copies_count_once_per_real_folder_and_differ_compares_skill_md() {
        let s = Scratch::new("skills-copies");
        skill(&s, "a/skills/x", "same");
        skill(&s, "b/skills/x", "same");
        skill(&s, "a/skills/y", "one");
        skill(&s, "b/skills/y", "two");
        let inv = scan(&dirs(&s, &["a", "b", "c"]));
        let x = inv.skill("x").unwrap();
        assert_eq!(x.spread().copies, 2);
        assert!(!x.differ);
        assert!(inv.skill("y").unwrap().differ);
        assert_eq!(inv.folders[2], None);
    }

    #[test]
    fn share_prompt_names_each_copy_and_each_lacking_folder_once() {
        let s = Scratch::new("skills-prompt");
        skill(&s, ".claude/skills/standup", "standup");
        s.mkdir(".claude-personal/skills");
        link(&s, ".claude-personal/skills", ".claude-wukong/skills");
        let inv = scan(&dirs(
            &s,
            &[".claude", ".claude-personal", ".claude-wukong"],
        ));
        let names = |d: &ConfigDir| match d.short() {
            "" => "jnj".to_owned(),
            short => short.to_owned(),
        };
        let shared = s.path().join(".claude-personal/skills");
        let prompt = inv.share_prompt(inv.skill("standup").unwrap(), &shared, names);
        let personal = real(&s, ".claude-personal/skills");
        assert_eq!(
            prompt,
            format!(
                "/share-skill standup\n\nShared skills folder: {}\nCopies:\n- {} (used by jnj)\nLink it into:\n- {} (account personal, wukong)\n",
                shared.display(),
                real(&s, ".claude/skills/standup").display(),
                personal.display(),
            )
        );
    }
}
