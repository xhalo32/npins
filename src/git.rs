//! Pin a git repository
//!
//! You either a branch or a release can be tracked. Releases are found as git tags
//! that more or less follow [SemVer](https://semver.org).
//!
//! There is special support for repositories that are hosted on GitHub or some GitLab
//! instance. This should be preferred over the generic Git API if possible. See [`Repository`]
//! for more on this.

use crate::*;
use anyhow::{Context, Result};
use lenient_version::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use url::Url;

fn get_github_url() -> String {
    std::env::var("NPINS_GITHUB_HOST").unwrap_or_else(|_| String::from("https://github.com"))
}

fn get_github_api_url() -> String {
    std::env::var("NPINS_GITHUB_API_HOST")
        .unwrap_or_else(|_| String::from("https://api.github.com"))
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
/// A git revision, with an optional timestamp.
///
/// Timestamps are supported for GitHub repositories only.
pub struct GitRevision {
    revision: String,
    timestamp: Option<String>,
}

impl GitRevision {
    pub fn new(revision: String) -> Result<Self> {
        if !revision.chars().all(|c| c.is_digit(16)) || revision.len() != 40 {
            anyhow::bail!("'{revision}' is not a valid git revision (sha1 hash)");
        }
        Ok(Self {
            revision,
            timestamp: None,
        })
    }
}

impl diff::Diff for GitRevision {
    fn properties(&self) -> Vec<(String, String)> {
        vec![
            ("revision".into(), self.revision.clone()),
            (
                "timestamp".into(),
                self.timestamp.clone().unwrap_or_else(|| "N/A".into()),
            ),
        ]
    }
}

/// A hash, but the URL is optional
///
/// If the url is not present, `fetchgit` must be used based on the version information instead.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct OptionalUrlHashes {
    pub url: Option<url::Url>,
    pub hash: String,
}

impl diff::Diff for OptionalUrlHashes {
    fn properties(&self) -> Vec<(String, String)> {
        [
            self.url.as_ref().map(|url| ("url".into(), url.to_string())),
            Some(("hash".into(), self.hash.clone())),
        ]
        .into_iter()
        .flat_map(Option::into_iter)
        .collect()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ReleasePinHashes {
    pub revision: String,
    // This is the URL for the tarball to fetch, if absent use fetchgit instead
    pub url: Option<Url>,
    pub hash: String,
}

impl diff::Diff for ReleasePinHashes {
    fn properties(&self) -> Vec<(String, String)> {
        vec![
            ("revision".into(), self.revision.clone()),
            ("hash".into(), self.hash.clone()),
        ]
    }
}

/// Abstraction over different git repository hosters
///
/// Currently, GitHub and GitLab are supported. Plain git repositories
/// have limited support: they cannot provide tarball urls for downloading
/// versions.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "type")]
pub enum Repository {
    Git {
        /// URL to the Git repository
        url: Url,
    },
    Forgejo {
        server: Url,
        owner: String,
        repo: String,
    },
    GitHub {
        /// "owner/repo"
        owner: String,
        repo: String,
    },
    GitLab {
        /// usually "owner/repo" or "group/owner/repo" (without leading or trailing slashes)
        repo_path: String,
        /// Of the kind <https://gitlab.example.org/>
        ///
        /// It must fit into the schema `<server>/<owner>/<repo>` to get a repository's URL.
        server: Url,
        /// access token for private repositories
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        private_token: Option<String>,
    },
}

impl Repository {
    pub fn git(url: url::Url) -> Self {
        Self::Git { url }
    }

    pub fn github(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Repository::GitHub {
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    pub fn forgejo(server: Url, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Repository::Forgejo {
            server,
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    pub fn gitlab(repo_path: String, server: Option<Url>, private_token: Option<String>) -> Self {
        let server = server.unwrap_or_else(|| "https://gitlab.com/".parse().unwrap());
        Repository::GitLab {
            repo_path,
            server,
            private_token,
        }
    }

    /// Get the URL to the represented Git repository
    fn git_url(&self) -> Result<Url> {
        Ok(match self {
            Repository::Git { url } => url.clone(),
            Repository::GitHub { owner, repo } => {
                format!("{}/{}/{}.git", get_github_url(), owner, repo).parse()?
            },
            Repository::Forgejo {
                server,
                owner,
                repo,
            } => format!("{}/{}/{}.git", server, owner, repo).parse()?,
            Repository::GitLab {
                repo_path,
                server,
                private_token,
            } => {
                let mut server = server.clone();
                if let Some(token) = private_token {
                    server.set_username("oauth2").ok();
                    server.set_password(Some(token)).ok();
                } else if let Ok(token) = std::env::var("GITLAB_TOKEN") {
                    server.set_username("oauth2").ok();
                    server.set_password(Some(&token)).ok();
                }
                server.join(&format!("{}.git", repo_path))?
            },
        })
    }

    /// Get the url to a tarball of the requested revision
    fn url(&self, revision: &str) -> Result<Option<Url>> {
        Ok(match self {
            Repository::Git { .. } => None,
            Repository::GitHub { owner, repo } => Some(
                format!(
                    "{github}/{owner}/{repo}/archive/{revision}.tar.gz",
                    github = get_github_url(),
                    owner = owner,
                    repo = repo,
                    revision = revision,
                )
                .parse()?,
            ),
            Repository::Forgejo {
                server,
                owner,
                repo,
            } => Some(format!("{server}{owner}/{repo}/archive/{revision}.tar.gz",).parse()?),
            Repository::GitLab {
                repo_path,
                server,
                private_token,
            } => {
                let mut url = server.clone();
                url.path_segments_mut()
                    .map_err(|()| anyhow::format_err!("GitLab server URL must be a base"))?
                    .extend(
                        [
                            "api",
                            "v4",
                            "projects",
                            repo_path,
                            "repository",
                            "archive.tar.gz",
                        ]
                        .iter(),
                    );
                url.set_query(Some(&format!("sha={}", revision)));
                if let Some(token) = private_token {
                    url.set_query(Some(&format!("private_token={}", token)));
                }
                Some(url)
            },
        })
    }

    /// Get the url to a tarball of the requested release
    fn release_url(&self, tag: &str) -> Result<Option<Url>> {
        Ok(match self {
            Repository::Git { .. } => None,
            Repository::GitHub { owner, repo } => Some(
                format!(
                    "{github_api}/repos/{owner}/{repo}/tarball/refs/tags/{tag}",
                    github_api = get_github_api_url(),
                    owner = owner,
                    repo = repo,
                    tag = tag,
                )
                .parse()?,
            ),
            Repository::Forgejo {
                server,
                owner,
                repo,
            } => {
                Some(format!("{server}api/v1/repos/{owner}/{repo}/archive/{tag}.tar.gz",).parse()?)
            },
            Repository::GitLab {
                repo_path,
                server,
                private_token,
            } => {
                let mut url = server.clone();
                url.path_segments_mut()
                    .map_err(|()| anyhow::format_err!("GitLab server URL must be a base"))?
                    .extend(
                        [
                            "api",
                            "v4",
                            "projects",
                            repo_path,
                            "repository",
                            "archive.tar.gz",
                        ]
                        .iter(),
                    );
                url.set_query(Some(&format!("sha={}", tag)));
                if let Some(token) = private_token {
                    url.set_query(Some(&format!("private_token={}", token)));
                }
                Some(url)
            },
        })
    }

    async fn get_timestamp(&self, commit: &str) -> Result<Option<String>> {
        Ok(match self {
            Repository::GitHub { owner, repo } => {
                let url: Url = format!(
                    "{github_api}/repos/{owner}/{repo}/commits/{commit}",
                    github_api = get_github_api_url(),
                )
                .parse()?;

                let body: Value = build_client()?
                    .get(url)
                    .send()
                    .await
                    .context("Couldn't fetch timestamp")?
                    .json()
                    .await
                    .context("Couldn't decode response as JSON")?;

                Some(
                    body["commit"]["author"]["date"]
                        .as_str()
                        .context("Expected date in GitHub API response to be a string")?
                        .to_string(),
                )
            },
            _ => None,
        })
    }
}

/// Track a given branch on a repository and always use the latest commit
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct GitPin {
    pub repository: Repository,
    pub branch: String,
    /// Also fetch submodules
    #[serde(default)]
    pub submodules: bool,
}

impl diff::Diff for GitPin {
    fn properties(&self) -> Vec<(String, String)> {
        vec![
            (
                "repository".into(),
                self.repository.git_url().unwrap().to_string(),
            ),
            ("branch".into(), self.branch.clone()),
            ("submodules".into(), self.submodules.to_string()),
        ]
    }
}

impl GitPin {
    pub fn new(repository: Repository, branch: String, submodules: bool) -> Self {
        Self {
            repository,
            branch,
            submodules,
        }
    }
}

#[async_trait::async_trait]
impl Updatable for GitPin {
    type Version = GitRevision;
    type Hashes = OptionalUrlHashes;

    async fn update(&self, _old: Option<&GitRevision>) -> Result<GitRevision> {
        let repo_url = self.repository.git_url()?;
        let latest = fetch_branch_head(&repo_url, &self.branch)
            .await
            .context("Couldn't fetch the latest commit")?
            .revision;

        Ok(GitRevision {
            timestamp: self.repository.get_timestamp(&latest).await?,
            revision: latest,
        })
    }

    async fn fetch(&self, version: &GitRevision) -> Result<OptionalUrlHashes> {
        if self.submodules {
            Ok(OptionalUrlHashes {
                url: None,
                hash: nix::nix_prefetch_git(&self.repository.git_url()?, &version.revision, true)
                    .await?,
            })
        } else {
            // Try to find an URL for fetchtarball first, as it is faster than fetchgit
            let url = self.repository.url(&version.revision)?;
            let hash = match url.as_ref() {
                Some(url) => nix::nix_prefetch_tarball(url).await?,
                None => {
                    nix::nix_prefetch_git(&self.repository.git_url()?, &version.revision, false)
                        .await?
                },
            };

            Ok(OptionalUrlHashes { url, hash })
        }
    }
}

/// Try to follow the latest release of the given project
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct GitReleasePin {
    pub repository: Repository,
    /// Also track pre-releases.
    #[serde(default)]
    pub pre_releases: bool,
    /// Optionally restrict to only pin older releases
    ///
    /// Sometimes, we want to track an older major version separately. For example, set it to
    /// 2.0 to track 1.* releases. (Note that the bound is exclusive. In mathematical terms, it
    /// is the infimum and not a maximum, because the set of compatible releases is not closed.)
    ///
    /// If present, only versions < version_upper_bound will be pinned. This is a restricted
    /// syntax compared to the "version requirement grammar" with multiple different operators
    /// that are common in dependency resolution tools. The reason is, that we aren't interested
    /// in providing lower bounds for releases, so all we need is a "less than" operator.
    ///
    /// Versions will be parsed the in the same rather lenient way as the tags themselves.
    pub version_upper_bound: Option<String>,
    /// Optionally filter the considered release names / tags by a prefix
    ///
    /// Some projects have a more elaborate tag structure that
    /// contains prefixes such as `release/` or `basepoint/` in the
    /// actual tag. With this option set the tags are filtered for all
    /// those tags that contain the specified prefix and have the
    /// prefix stripped before any version comparison happens.
    pub release_prefix: Option<String>,
    /// Also fetch submodules
    #[serde(default)]
    pub submodules: bool,
}

impl diff::Diff for GitReleasePin {
    fn properties(&self) -> Vec<(String, String)> {
        [
            Some((
                "repository".into(),
                self.repository.git_url().unwrap().to_string(),
            )),
            Some(("pre_releases".into(), self.pre_releases.to_string())),
            self.version_upper_bound
                .as_ref()
                .map(|version_upper_bound| {
                    ("version_upper_bound".into(), version_upper_bound.clone())
                }),
            self.release_prefix
                .as_ref()
                .map(|release_prefix| ("release_prefix".into(), release_prefix.clone())),
            Some(("submodules".into(), self.submodules.to_string())),
        ]
        .into_iter()
        .flat_map(Option::into_iter)
        .collect()
    }
}

impl GitReleasePin {
    pub fn new(
        repository: Repository,
        pre_releases: bool,
        version_upper_bound: Option<String>,
        release_prefix: Option<String>,
        submodules: bool,
    ) -> Self {
        Self {
            repository,
            pre_releases,
            version_upper_bound,
            release_prefix,
            submodules,
        }
    }
}

#[async_trait::async_trait]
impl Updatable for GitReleasePin {
    type Version = GenericVersion;
    type Hashes = ReleasePinHashes;

    async fn update(&self, old: Option<&GenericVersion>) -> Result<GenericVersion> {
        let repo_url = self.repository.git_url()?;

        let version_upper_bound: Option<Version<'_>> = self
            .version_upper_bound
            .as_deref()
            .map(lenient_semver_parser::parse::<Version>)
            .transpose()
            .map_err(|err| err.owned())
            .context("Field `version_upper_bound` is invalid")?;

        let latest = latest_release(
            fetch_tags(&repo_url)
                .await
                .context("Couldn't fetch the release tags")?
                .iter()
                /* Strip the common prefix, filter those that don't have it (that should actually never happen) */
                .filter_map(|tag| tag.ref_.strip_prefix("refs/tags/")),
            self.pre_releases,
            version_upper_bound.as_ref(),
            self.release_prefix.as_deref(),
        )
            .ok_or_else(|| anyhow::format_err!("Repository has no matching release tags"))?;

        // If we have a release prefix strip it from the previous version for semver comparison.
        // If the old version didn't have a prefix we keep it as is.
        let old = match (old, &self.release_prefix) {
            (Some(version), None) => Some(version.clone()),
            (Some(old), Some(prefix)) => {
                let version = match old.version.strip_prefix(prefix) {
                    None => old.version.clone(),
                    Some(v) => v.into(),
                };
                Some(GenericVersion { version })
            },
            (None, _) => None,
        };

        if let Some(old) = old {
            let old_version = lenient_semver_parser::parse::<Version>(&old.version);
            let latest = lenient_semver_parser::parse::<Version>(&latest.name)
                /* The first thing we do is filter tags with this exact requirement. */
                .expect("Latest version must parse as SemVer");
            match old_version {
                Ok(old_version) => {
                    anyhow::ensure!(
                       latest >= old_version,
                       "Failed to ensure version monotonicity, latest found version is {} but current is {}",
                       latest,
                       old_version,
                   );
                },
                Err(_) => {
                    log::warn!(
                        "Old version ({}) failed to parse as SemVer, cannot ensure monotonicity",
                        old.version
                    );
                },
            }
        }

        Ok(GenericVersion {
            version: latest.tag,
        })
    }

    async fn fetch(&self, version: &GenericVersion) -> Result<ReleasePinHashes> {
        let repo_url = self.repository.git_url()?;

        let revision = fetch_ref(&repo_url, format!("refs/tags/{}", version.version))
            .await?
            .revision;

        if self.submodules {
            Ok(ReleasePinHashes {
                url: None,
                hash: nix::nix_prefetch_git(&repo_url, &revision, true).await?,
                revision,
            })
        } else {
            // Try to find an URL for fetchtarball first, as it is faster than fetchgit
            let url = self.repository.release_url(&version.version)?;
            let hash = match url.as_ref() {
                Some(url) => nix::nix_prefetch_tarball(url).await?,
                None => nix::nix_prefetch_git(&repo_url, &revision, false).await?,
            };
            Ok(ReleasePinHashes {
                url,
                hash,
                revision,
            })
        }
    }
}

/// Output of `git ls-remote`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteInfo {
    pub revision: String,
    pub ref_: String,
}

impl RemoteInfo {
    pub fn new(revision: impl Into<String>, ref_: impl Into<String>) -> Self {
        Self {
            revision: revision.into(),
            ref_: ref_.into(),
        }
    }
}

/// Convenience wrapper around calling `git ls-remote`
async fn fetch_remote(url: &str, args: &[&str]) -> Result<Vec<RemoteInfo>> {
    let result = async {
        log::debug!("Executing `git ls-remote {}`", args.join(" "));
        let process = Command::new("git")
            // Disable any interactive login attempts, failing gracefully instead
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", "ssh -o StrictHostKeyChecking=yes")
            .arg("ls-remote")
            .args(args)
            .output()
            .await
            .context("Failed waiting for git ls-remote subprocess")?;
        if !process.status.success() {
            anyhow::bail!(
                "git ls-remote failed with exit code {}\n{}",
                process
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "None".into()),
                String::from_utf8_lossy(&process.stderr)
            );
        }
        log::debug!("git ls-remote stdout:");
        String::from_utf8_lossy(&process.stdout)
            .split('\n')
            .for_each(|line| log::debug!("> {}", line));

        String::from_utf8_lossy(&process.stdout)
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (revision, ref_) = line
                    .split_once('\t')
                    .ok_or_else(|| anyhow::format_err!("Output line contains no '\\t'"))?;
                anyhow::ensure!(
                    !ref_.contains('\t'),
                    "Output line contains more than one '\\t'"
                );
                log::debug!("Found remote: {}, {}", revision, ref_);
                Ok(RemoteInfo {
                    revision: revision.into(),
                    ref_: ref_.into(),
                })
            })
            .collect::<Result<Vec<RemoteInfo>>>()
    };
    check_git_url(result.await, url).await
}

/// Get the commit for a ref
pub async fn fetch_ref(repo: &Url, ref_: impl AsRef<str>) -> Result<RemoteInfo> {
    let ref_ = ref_.as_ref();

    let remotes = fetch_remote(repo.as_str(), &["--refs", repo.as_str(), ref_])
        .await
        .with_context(|| format!("Failed to get revision from remote for {} {}", repo, ref_))?;

    anyhow::ensure!(
        !remotes.is_empty(),
        "git ls-remote output is empty. Are you sure '{}' exists? Note: If you want to tag a revision, you need to also specify a branch ('--branch').",
        ref_,
    );

    /* git ls-remote always postfix-matches the ref like a glob, but we want an exact match.
     * See https://github.com/andir/npins/issues/142
     */
    remotes.into_iter().find(|r| r.ref_ == ref_).ok_or_else(
        || anyhow::format_err!("git ls-remote output does not contain the requested remote '{}'. This should not have happened!", ref_)
    )
}

/// Get the revision for a branch
pub async fn fetch_branch_head(repo: &Url, branch: impl AsRef<str>) -> Result<RemoteInfo> {
    fetch_ref(repo, format!("refs/heads/{}", branch.as_ref())).await
}

/// List all tags of a repo
pub async fn fetch_tags(repo: &Url) -> Result<Vec<RemoteInfo>> {
    let remotes = fetch_remote(repo.as_str(), &["--refs", repo.as_str(), "refs/tags/*"])
        .await
        .with_context(|| format!("Failed to list tags for {}", repo))?;

    Ok(remotes)
}

pub async fn fetch_default_branch(repo: &Url) -> Result<String> {
    let remotes = fetch_remote(repo.as_str(), &["--symref", repo.as_str(), "HEAD"])
        .await
        .with_context(|| format!("Failed to resolve default branch for {}", repo))?;

    let info = remotes
        .iter()
        .filter(|info| info.revision.starts_with("ref: refs/heads/") && info.ref_ == "HEAD")
        .next()
        .with_context(|| format!("Failed to resolve HEAD to a ref for {}", repo))?;

    info.revision
        .strip_prefix("ref: refs/heads/")
        .map(|s| s.to_owned())
        .with_context(|| format!("Failed to parse git ls-remote output for {}", repo))
}

#[cfg_attr(test, derive(PartialEq, Debug))]
struct LatestRelease {
    /// The tag as used by git, e.g. release/2.0
    tag: String,

    /// The tag as communicated to the user, e.g. 2.0
    name: String,
}

#[cfg(test)]
impl LatestRelease {
    fn tag(tag: impl Into<String>) -> Self {
        let tag = tag.into();
        Self {
            name: tag.clone(),
            tag,
        }
    }
}

/// Take an iterator of tags and spit out the latest release
fn latest_release<'a>(
    tags: impl Iterator<Item = &'a str>,
    pre_releases: bool,
    version_upper_bound: Option<&Version>,
    prefix: Option<&str>,
) -> Option<LatestRelease> {
    // Optionally filter all tags by a prefix
    let tags: Box<dyn Iterator<Item = &'a str>> = match prefix {
        None => Box::new(tags),
        Some(prefix) => Box::new(tags.filter_map(move |tag| tag.strip_prefix(prefix))),
    };

    let tag = tags
        /* Try to parse as version, ignore those that are invalid (not every tag will be a release) */
        .filter_map(|tag| lenient_semver_parser::parse::<Version>(tag)
            .ok()
            .map(|version| (tag, version))
        )
        /* Optionally filter out pre-releases */
        .filter(|(_, version)| pre_releases || !version.is_pre_release())
        /* Filter against our upper bound */
        .filter(|(_, version)| match &version_upper_bound {
            Some(version_upper_bound) => version < version_upper_bound,
            None => true,
        })
        /* Get the latest version */
        .max_by(|(_, version_a), (_, version_b)| version_a.cmp(version_b))
        .map(|(tag, _)| tag.to_owned());

    tag.map(|tag| LatestRelease {
        tag: match prefix {
            Some(p) => format!("{p}{tag}"),
            None => tag.clone(),
        },
        name: tag,
    })
}

/* All repositories used for tests are dead, super dead, or
 * straight up archived. We can safely assume that they will have no
 * activity in the future. This is important because any changes would
 * break our tests. Therefore, we should switch to a different solution
 * (probably by creating our own repos) in the mid to long run.
 */
#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_latest_release() {
        let v2 = lenient_semver_parser::parse::<Version>("2").unwrap();
        assert_eq!(
            latest_release(["foo"].iter().copied(), false, None, None),
            None
        );
        assert_eq!(
            latest_release(["1.0", "foo"].iter().copied(), false, None, None),
            Some(LatestRelease::tag("1.0"))
        );
        assert_eq!(
            latest_release(["1.0", "2.0"].iter().copied(), false, Some(&v2), None),
            Some(LatestRelease::tag("1.0"))
        );
        assert_eq!(
            latest_release(
                ["1.0", "2.0", "2.0-pre"].iter().copied(),
                false,
                Some(&v2),
                None
            ),
            Some(LatestRelease::tag("1.0"))
        );
        assert_eq!(
            latest_release(
                ["1.0", "2.0", "2.0-pre"].iter().copied(),
                true,
                Some(&v2),
                None
            ),
            Some(LatestRelease::tag("2.0-pre"))
        );

        assert_eq!(
            latest_release(
                [
                    "foo/1.0",
                    "bar/2.0",
                    "baz/2.0-pre",
                    "zes/1.0",
                    "zes/2.0",
                    "zes/2.1-b1"
                ]
                .iter()
                .copied(),
                false,
                None,
                Some("zes/")
            ),
            Some(LatestRelease {
                tag: "zes/2.0".into(),
                name: "2.0".into()
            })
        );
    }

    #[tokio::test]
    async fn test_fetch_branch() -> Result<()> {
        let branch = fetch_branch_head(
            &"https://github.com/oliverwatkins/swing_library.git"
                .parse()
                .unwrap(),
            "master",
        )
        .await?;
        assert_eq!(&branch.revision, "1edb0a9cebe046cc915a218c57dbf7f40739aeee");
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_tags() -> Result<()> {
        let tags = fetch_tags(
            &"https://gitlab.com/maxigaz/gitlab-dark.git"
                .parse()
                .unwrap(),
        )
        .await?;
        #[rustfmt::skip]
        assert_eq!(
            &tags,
            &[
                RemoteInfo::new("f8fcf75f8273e4b4fdd4b3583cc75de5713a5c9e", "refs/tags/v0.1"),
                RemoteInfo::new("ad7a0efac0457fa72f85457b19e2b64617e4196b", "refs/tags/v0.10"),
                RemoteInfo::new("0deda883cc9120f1380286898f37263fc3d85029", "refs/tags/v0.2"),
                RemoteInfo::new("e6c43f0bdc4ee1e040a9c615b44d2d386c90873f", "refs/tags/v0.3.0"),
                RemoteInfo::new("1ea14b7256830b1c201d763d6465b27875f18b54", "refs/tags/v0.3.1"),
                RemoteInfo::new("225c35a1958fcb448d0dec08038cfb107aef9f37", "refs/tags/v0.3.2"),
                RemoteInfo::new("c5eb79300534103403e4d31042c49b03ca64d5a3", "refs/tags/v0.4"),
                RemoteInfo::new("22639951411450125d1bf4a6e67bbc0e9a599fbb", "refs/tags/v0.5"),
                RemoteInfo::new("a3580b27a611ba4e8ca5dfb18938230e0190f8fa", "refs/tags/v0.5.1"),
                RemoteInfo::new("4bf0fcc55e7dd09b5998233b945eb994588a4fc2", "refs/tags/v0.6"),
                RemoteInfo::new("612a368e93d89f145b94a7f21b17a144948f9a3f", "refs/tags/v0.7"),
                RemoteInfo::new("282e922f60f338be9ee4a87f8466ff1e264ea1c9", "refs/tags/v0.8"),
                RemoteInfo::new("89aa73c9741b7c433d0a19ed406027918894fb24", "refs/tags/v0.9"),
                RemoteInfo::new("ff98a5a914fda20fe93a70ddb35846c5d55153c1", "refs/tags/v0.9.1"),
                RemoteInfo::new("ff4d31039579620c9d7777e13562244487d9133a", "refs/tags/v1.0"),
                RemoteInfo::new("57792f92b8702e89e421cdd7167af0f67ed70d3a", "refs/tags/v1.1"),
                RemoteInfo::new("8e8408c7f7b16b56e3f9a8ae8b528c2bb2027a1d", "refs/tags/v1.10.0"),
                RemoteInfo::new("e30f2856b9a9e4dfa6923ac55a6c4f2a57926847", "refs/tags/v1.11.0"),
                RemoteInfo::new("2a9ebf92ce3fcafea5f5ee99946511146cd5ab89", "refs/tags/v1.11.1"),
                RemoteInfo::new("a37dffbb2682047a1cd0d309d037a68680cb2b1d", "refs/tags/v1.11.2"),
                RemoteInfo::new("27de4ac103eb79874fb06335b753ca4e69ebae75", "refs/tags/v1.12.0"),
                RemoteInfo::new("c060769747bf05fefa341ccec521844d648f7e78", "refs/tags/v1.13.0"),
                RemoteInfo::new("9a52eada4ecfc4964004685dfa49c20e7eeafddf", "refs/tags/v1.14.0"),
                RemoteInfo::new("82939f17b5b40bf690c205c42f7f52a6d753b5b0", "refs/tags/v1.14.1"),
                RemoteInfo::new("245ad342c66941fd94639bd05bc62940fbc92789", "refs/tags/v1.15.0"),
                RemoteInfo::new("cb30bd2aca6dca7fc7d3007360ad326d0149e6b8", "refs/tags/v1.15.1"),
                RemoteInfo::new("d42ec2b04df9da97e465883fcd1f9a5d6e794027", "refs/tags/v1.16.0"),
                RemoteInfo::new("798f09bfdbc55b5752546d35da77d607c78b603b", "refs/tags/v1.2"),
                RemoteInfo::new("9d16676e290e26dd606a6f4e2686bd1a7152a11d", "refs/tags/v1.3"),
                RemoteInfo::new("c37acdbd015f0c8d6cfe0793caa515fa255e6a9d", "refs/tags/v1.4.0"),
                RemoteInfo::new("00edce1d0d87e75b85bc85bba000dcead3932dde", "refs/tags/v1.4.1"),
                RemoteInfo::new("1b57bd7903bac0784d39ff20c22001dabf928ba7", "refs/tags/v1.5.0"),
                RemoteInfo::new("80b8f6c571396e1ee76b214c515d62ee226bfc45", "refs/tags/v1.6.0"),
                RemoteInfo::new("8a593dd10c6b291726a3b41b50afc1828185bfba", "refs/tags/v1.6.1"),
                RemoteInfo::new("573f94897158de2d79e0b7f5301ee3c2e665920e", "refs/tags/v1.7.0"),
                RemoteInfo::new("f015530bcc4a22b7245c9b2e4699885962cd7d8e", "refs/tags/v1.8.0"),
                RemoteInfo::new("87c5cc3362c9565b5ed2d90984b589ee6ecc5a3b", "refs/tags/v1.9.0"),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_ref() {
        /* Regression test for https://github.com/andir/npins/issues/142 */
        assert_eq!(
            fetch_ref(
                &"https://seed.radicle.garden/z3gqcJUoA1n9HaHKufZs5FCSGazv5.git"
                    .parse()
                    .unwrap(),
                "refs/heads/master"
            )
            .await
            .unwrap()
            .ref_,
            "refs/heads/master".to_string()
        );
    }

    #[tokio::test]
    async fn test_git_update() -> Result<()> {
        let pin = GitPin {
            repository: Repository::Git {
                url: "https://github.com/oliverwatkins/swing_library.git"
                    .parse()
                    .unwrap(),
            },
            branch: "master".into(),
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            GitRevision {
                revision: "1edb0a9cebe046cc915a218c57dbf7f40739aeee".into(),
                timestamp: None,
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            OptionalUrlHashes {
                url: None,
                hash: "sha256-zUM/evAqAwwjGXg67IVzqZvvwp2NjFG1HAUSdLv98Z0=".into(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_git_release_update() -> Result<()> {
        let pin = GitReleasePin {
            repository: Repository::Git {
                url: "https://github.com/jstutters/MidiOSC.git".parse().unwrap(),
            },
            pre_releases: false,
            version_upper_bound: None,
            release_prefix: None,
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            GenericVersion {
                version: "v1.1".into(),
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            ReleasePinHashes {
                url: None,
                hash: "sha256-BjxJ5aG8NyfDLcBNZrDVV2CAK4tdHNCBdiuJYKB8BmA=".into(),
                revision: "35be5b2b2c3431de1100996487d53134f658b866".into(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_github_update() -> Result<()> {
        let pin = GitPin {
            repository: Repository::GitHub {
                owner: "oliverwatkins".into(),
                repo: "swing_library".into(),
            },
            branch: "master".into(),
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            GitRevision {
                revision: "1edb0a9cebe046cc915a218c57dbf7f40739aeee".into(),
                timestamp: Some("2018-12-17T09:26:57Z".into()),
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            OptionalUrlHashes {
                url: Some("https://github.com/oliverwatkins/swing_library/archive/1edb0a9cebe046cc915a218c57dbf7f40739aeee.tar.gz".parse().unwrap()),
                hash: "sha256-zUM/evAqAwwjGXg67IVzqZvvwp2NjFG1HAUSdLv98Z0=".into(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_github_release_update() -> Result<()> {
        let pin = GitReleasePin {
            repository: Repository::GitHub {
                owner: "jstutters".into(),
                repo: "MidiOSC".into(),
            },
            pre_releases: false,
            version_upper_bound: None,
            release_prefix: None,
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            GenericVersion {
                version: "v1.1".into(),
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            ReleasePinHashes {
                revision: "35be5b2b2c3431de1100996487d53134f658b866".into(),
                url: Some(
                    "https://api.github.com/repos/jstutters/MidiOSC/tarball/refs/tags/v1.1"
                        .parse()
                        .unwrap()
                ),
                hash: "sha256-BjxJ5aG8NyfDLcBNZrDVV2CAK4tdHNCBdiuJYKB8BmA=".into(),
            }
        );
        Ok(())
    }

    // That repo has a tag and a branch with the same name, and the naive endpoint for
    // GitHub which usually works then returns
    // {
    //   "message": "'0.2.1' has multiple possibilities: https://github.com/alexfedosov/AFHorizontalDayPicker/tarball/refs/heads/0.2.1, https://github.com/alexfedosov/AFHorizontalDayPicker/tarball/refs/tags/0.2.1",
    //   "documentation_url": "https://docs.github.com/rest/repos/contents#download-a-repository-archive-tar",
    //   "status": "300"
    // }
    #[tokio::test]
    async fn test_github_release_ambiguous() -> Result<()> {
        let pin = GitReleasePin {
            repository: Repository::github("alexfedosov", "AFHorizontalDayPicker"),
            pre_releases: false,
            version_upper_bound: None,
            release_prefix: None,
            submodules: false,
        };
        let version = GenericVersion {
            version: "0.2.1".into(),
        };
        assert_eq!(
            pin.fetch(&version).await?,
            ReleasePinHashes {
                revision: "ca59ad1dc1b55108f1d17f20bdf443aad3e2f0f5".into(),
                url: Some(
                    "https://api.github.com/repos/alexfedosov/AFHorizontalDayPicker/tarball/refs/tags/0.2.1"
                        .parse()
                        .unwrap()
                ),
                hash: "sha256-++ywpuReqTb6tn8DddmLpOK4yOOTgX7M8X5YkJS8OCs=".into(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_forgejo_update() -> Result<()> {
        let pin = GitPin {
            repository: Repository::Forgejo {
                server: "https://git.lix.systems".parse().unwrap(),
                owner: "lix-project".into(),
                repo: "lix".into(),
            },
            branch: "release-2.90".into(),
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            GitRevision {
                revision: "4bbdb2f5564b9b42bcaf0e1eec28325300f31c72".into(),
                timestamp: None,
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            OptionalUrlHashes {
                url: Some("https://git.lix.systems/lix-project/lix/archive/4bbdb2f5564b9b42bcaf0e1eec28325300f31c72.tar.gz".parse().unwrap()),
                hash: "sha256-w8JAk9Z3Fmkyway0VCjy/PtoBC6bGQVhNfTzFA98Pg8=".into(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_forgejo_release_update() -> Result<()> {
        let pin = GitReleasePin {
            repository: Repository::Forgejo {
                server: "https://git.lix.systems".parse().unwrap(),
                owner: "lix-project".into(),
                repo: "lix".into(),
            },
            pre_releases: false,
            version_upper_bound: Some("2.90.1".to_string()),
            release_prefix: None,
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            GenericVersion {
                version: "2.90.0".into(),
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            ReleasePinHashes {
                revision: "2a4376be20d70feaa2b0e640c5041fb66ddc67ed".into(),
                url: Some(
                    "https://git.lix.systems/api/v1/repos/lix-project/lix/archive/2.90.0.tar.gz"
                        .parse()
                        .unwrap()
                ),
                hash: "sha256-f8k+BezKdJfmE+k7zgBJiohtS3VkkriycdXYsKOm3sc=".into(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_gitlab_update() -> Result<()> {
        let pin = GitPin {
            repository: Repository::GitLab {
                repo_path: "maxigaz/gitlab-dark".into(),
                server: "https://gitlab.com/".parse().unwrap(),
                private_token: None,
            },
            branch: "master".into(),
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            git::GitRevision {
                revision: "e7145078163692697b843915a665d4f41139a65c".into(),
                timestamp: None,
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            OptionalUrlHashes {
                url: Some("https://gitlab.com/api/v4/projects/maxigaz%2Fgitlab-dark/repository/archive.tar.gz?sha=e7145078163692697b843915a665d4f41139a65c".parse().unwrap()),
                hash: "sha256-WzPqIwEe6HzISyeg1XBSHNO2fd9+Pc1T90RXBh7IrFo=".into(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_gitlab_release_update() -> Result<()> {
        let pin = GitReleasePin {
            repository: Repository::GitLab {
                repo_path: "maxigaz/gitlab-dark".into(),
                server: "https://gitlab.com/".parse().unwrap(),
                private_token: None,
            },
            pre_releases: false,
            version_upper_bound: None,
            release_prefix: None,
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            GenericVersion {
                version: "v1.16.0".into(),
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            ReleasePinHashes {
                revision: "d42ec2b04df9da97e465883fcd1f9a5d6e794027".into(),
                url: Some("https://gitlab.com/api/v4/projects/maxigaz%2Fgitlab-dark/repository/archive.tar.gz?sha=v1.16.0"
                    .parse()
                    .unwrap()),
                hash: "sha256-jcOkr5tJdEw1RL3jB8ItE8PLOVNzQtOyzDv8x/ySiiA=".into(),
            }
        );
        Ok(())
    }

    // Test that once a pin is pinned, it will stay pinned as is
    // Regression test for https://github.com/andir/npins/issues/146
    // We pin some old GNOME version, to make sure the pin won't fail
    #[tokio::test]
    async fn test_gitlab_release_noupdate() -> Result<()> {
        let pin = GitReleasePin {
            repository: Repository::GitLab {
                repo_path: "GNOME/gnome-shell".into(),
                server: "https://gitlab.gnome.org/".parse().unwrap(),
                private_token: None,
            },
            pre_releases: false,
            version_upper_bound: None,
            release_prefix: None,
            submodules: false,
        };
        let version = GenericVersion {
            version: "40.0".into(),
        };

        assert_eq!(
            pin.fetch(&version).await?,
            ReleasePinHashes {
                revision: "435d48ad7eaf9d91cc6719fda852cd9fd54afa2e".into(),
                url: Some("https://gitlab.gnome.org/api/v4/projects/GNOME%2Fgnome-shell/repository/archive.tar.gz?sha=40.0"
                    .parse()
                    .unwrap()),
                hash: "sha256-dxgbhEQt9FIjsleC6ob6FJv5XdxmKMb+NWbxEtAJYtA=".into(),
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_gitlab_selfhosted_update() -> Result<()> {
        let pin = GitPin {
            repository: Repository::GitLab {
                repo_path: "Archive/gnome-games".into(),
                server: "https://gitlab.gnome.org/".parse().unwrap(),
                private_token: None,
            },
            branch: "master".into(),
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            git::GitRevision {
                revision: "bca2071b6923d45d9aabac27b3ea1e40f5fa3006".into(),
                timestamp: None,
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            OptionalUrlHashes {
                url: Some("https://gitlab.gnome.org/api/v4/projects/Archive%2Fgnome-games/repository/archive.tar.gz?sha=bca2071b6923d45d9aabac27b3ea1e40f5fa3006".parse().unwrap()),
                hash: "sha256-r84Y5/hI0rM/UWK569+nWo+BHuovmlQh3Zs6U2Srx14=".into(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_gitlab_selfhosted_release_update() -> Result<()> {
        let pin = GitReleasePin {
            repository: Repository::GitLab {
                repo_path: "Archive/gnome-games".into(),
                server: "https://gitlab.gnome.org/".parse().unwrap(),
                private_token: None,
            },
            pre_releases: false,
            version_upper_bound: None,
            release_prefix: None,
            submodules: false,
        };
        let version = pin.update(None).await?;
        assert_eq!(
            version,
            GenericVersion {
                version: "40.0".into(),
            }
        );
        assert_eq!(
            pin.fetch(&version).await?,
            ReleasePinHashes {
                revision: "2c89145d52d072a4ca5da900c2676d890bfab1ff".into(),
                url: Some("https://gitlab.gnome.org/api/v4/projects/Archive%2Fgnome-games/repository/archive.tar.gz?sha=40.0".parse().unwrap()),
                hash: "sha256-6+XMyOJOm2DTqnr4iCFupjW+Z7td4J+GJwSv1Am/5e8=".into(),
            }
        );
        Ok(())
    }
}
