use crate::model::SafeText;

#[derive(Clone, Debug, PartialEq)]
pub struct PrRef {
    pub repo: Option<SafeText>,
    pub number: u32,
}

impl PrRef {
    pub fn bare(number: u32) -> PrRef {
        PrRef { repo: None, number }
    }

    /// `<scheme>://<host>/<owner>/<repo>/pull/<n>`, optionally followed by `/…`, `#…` or `?…`.
    /// Any other URL with `/pull/<n>` keeps the number without a repo.
    pub fn from_url(url: &str) -> Option<PrRef> {
        let (head, tail) = url.rsplit_once("/pull/")?;
        let digits = tail.split(['/', '#', '?']).next()?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let number: u32 = digits.parse().ok().filter(|&n| n > 0)?;
        let path: Vec<&str> = head.split_once("://")?.1.split('/').collect();
        let repo = match path[..] {
            [_, owner, repo] if !owner.is_empty() && !repo.is_empty() => {
                Some(SafeText::redact(&format!("{owner}/{repo}")))
            }
            _ => None,
        };
        Some(PrRef { repo, number })
    }

    /// The repo name without its owner.
    pub fn repo_name(&self) -> Option<&str> {
        let r = self.repo.as_ref()?.as_str();
        Some(r.rsplit_once('/').map_or(r, |(_, name)| name))
    }

    fn same(&self, other: &PrRef) -> bool {
        self.number == other.number
            && match (&self.repo, &other.repo) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pr {
    pub r: PrRef,
    /// The session linked or opened it itself, as opposed to only mentioning it.
    pub own: bool,
}

/// Pull requests in order of first appearance, one entry per PR.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prs(Vec<Pr>);

impl Prs {
    pub fn add(&mut self, r: PrRef, own: bool) {
        match self.0.iter_mut().find(|p| p.r.same(&r)) {
            Some(p) => {
                if p.r.repo.is_none() {
                    p.r.repo = r.repo;
                }
                p.own |= own;
            }
            None => self.0.push(Pr { r, own }),
        }
    }

    pub fn extend(&mut self, other: &Prs) {
        for p in &other.0 {
            self.add(p.r.clone(), p.own);
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Pr> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
impl Prs {
    pub fn tuples(&self) -> Vec<(Option<&str>, u32, bool)> {
        self.0
            .iter()
            .map(|p| (p.r.repo.as_ref().map(SafeText::as_str), p.r.number, p.own))
            .collect()
    }
}

/// PR references written in prose: links, `owner/repo#N`, and bare `#N`.
pub fn mentions(text: &str) -> Vec<PrRef> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let rest = &text[i..];
        if rest.starts_with("https://") || rest.starts_with("http://") {
            let end = rest
                .find(|c: char| c.is_whitespace() || "<>()[]{}\"'`".contains(c))
                .unwrap_or(rest.len());
            let url = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
            out.extend(PrRef::from_url(url));
            i += end;
            continue;
        }
        if b[i] == b'#'
            && let Some((number, len)) = number_at(&text[i + 1..])
        {
            let before = &text[..i];
            if let Some(repo) = shorthand_repo(before) {
                out.push(PrRef {
                    repo: Some(SafeText::redact(repo)),
                    number,
                });
            } else if before
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || "_&/#".contains(c)))
            {
                out.push(PrRef::bare(number));
            }
            i += 1 + len;
            continue;
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    out
}

/// 1 to 7 digits without a leading zero, not running into a word.
fn number_at(s: &str) -> Option<(u32, usize)> {
    let len = s.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=7).contains(&len) || s.starts_with('0') {
        return None;
    }
    if s[len..]
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    Some((s[..len].parse().ok()?, len))
}

/// The `owner/repo` that `before` ends with, when it starts a word.
fn shorthand_repo(before: &str) -> Option<&str> {
    let repo_chars = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    let owner_chars = |c: char| c.is_ascii_alphanumeric() || c == '-';
    let repo_start = before.trim_end_matches(repo_chars).len();
    let owner_end = repo_start.checked_sub(1)?;
    if repo_start == before.len() || before.as_bytes()[owner_end] != b'/' {
        return None;
    }
    let owner_start = before[..owner_end].trim_end_matches(owner_chars).len();
    if owner_start == owner_end {
        return None;
    }
    let lead = before[..owner_start].chars().next_back();
    if lead.is_some_and(|c| c.is_alphanumeric() || "_&/#.:".contains(c)) {
        return None;
    }
    Some(&before[owner_start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner_repo: &str, number: u32) -> PrRef {
        PrRef {
            repo: Some(SafeText::redact(owner_repo)),
            number,
        }
    }

    fn numbers(text: &str) -> Vec<u32> {
        mentions(text).iter().map(|r| r.number).collect()
    }

    #[test]
    fn bare_numbers_need_a_word_boundary_on_both_sides() {
        for (text, want) in [
            ("PR #2558.", vec![2558]),
            ("(#12)", vec![12]),
            ("#1306 merged", vec![1306]),
            ("see #7, #8 and #9", vec![7, 8, 9]),
            ("#1234567", vec![1234567]),
            ("é#5", vec![]),
            ("foo#12", vec![]),
            ("&#123;", vec![]),
            ("#0", vec![]),
            ("#012", vec![]),
            ("#12abc", vec![]),
            ("#12_x", vec![]),
            ("##12", vec![]),
            ("page#3", vec![]),
            ("a/#4", vec![]),
            ("#12345678", vec![]),
            ("# 12", vec![]),
        ] {
            assert_eq!(numbers(text), want, "{text:?}");
        }
    }

    #[test]
    fn links_and_shorthand_carry_the_repo() {
        assert_eq!(
            mentions("opened https://github.com/acme/web/pull/4812, and acme/api#77 too"),
            [repo("acme/web", 4812), repo("acme/api", 77)]
        );
        assert_eq!(
            mentions("<https://github.com/acme/web.git-x/pull/9/files?w=1>"),
            [repo("acme/web.git-x", 9)]
        );
        assert_eq!(mentions("x/acme/web#3 nope"), []);
    }

    #[test]
    fn a_fragment_inside_a_link_is_not_another_ref() {
        assert_eq!(
            mentions("https://github.com/acme/web/pull/12#issuecomment-3 then #4"),
            [repo("acme/web", 12), PrRef::bare(4)]
        );
        assert_eq!(mentions("https://example.com/docs#3 and /pull/5"), []);
    }

    #[test]
    fn from_url_keeps_the_number_when_the_repo_is_unclear() {
        assert_eq!(
            PrRef::from_url("https://github.com/acme/web/pull/12#discussion_r1"),
            Some(repo("acme/web", 12))
        );
        assert_eq!(
            PrRef::from_url("https://ghe.example/o/r/x/pull/12"),
            Some(PrRef::bare(12))
        );
        assert_eq!(
            PrRef::from_url("https://github.com/acme/web/pull/abc"),
            None
        );
        assert_eq!(
            PrRef::from_url("https://github.com/acme/web/issues/12"),
            None
        );
    }

    #[test]
    fn add_merges_by_number_unless_repos_differ() {
        let mut prs = Prs::default();
        prs.add(PrRef::bare(12), false);
        prs.add(repo("acme/web", 12), true);
        prs.add(PrRef::bare(12), false);
        prs.add(repo("acme/api", 12), false);
        prs.add(repo("acme/api", 13), false);
        prs.add(PrRef::bare(13), true);
        assert_eq!(
            prs.tuples(),
            [
                (Some("acme/web"), 12, true),
                (Some("acme/api"), 12, false),
                (Some("acme/api"), 13, true),
            ]
        );
    }
}
