//! Minimal GitHub REST client for the PR panel. Auth is a bearer token (passed
//! in by the caller — usually obtained via `util::github_auth::github_token`).
//!
//! Only the calls Phase 1 needs: list open PRs, list PR files, fetch raw blob
//! at a sha. Pagination is capped at GitHub's default 100/page; PR lists past
//! that cap are fine to show truncated for v1.

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncReadExt;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, HttpRequestExt, Request};
use serde::Deserialize;

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "Zed-PR-Panel";

#[derive(Clone, Debug)]
pub struct RepoCoords {
    pub owner: String,
    pub repo: String,
}

impl RepoCoords {
    /// Best-effort GitHub remote URL parser. Accepts:
    /// - `https://github.com/owner/repo(.git)`
    /// - `git@github.com:owner/repo(.git)`
    /// - `ssh://git@github.com/owner/repo(.git)`
    pub fn parse_github_url(url: &str) -> Option<Self> {
        let trimmed = url.trim().trim_end_matches('/');
        let after_host = if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("http://github.com/") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
            rest
        } else {
            return None;
        };

        let mut parts = after_host.splitn(2, '/');
        let owner = parts.next()?.to_string();
        let repo_with_ext = parts.next()?;
        let repo = repo_with_ext.trim_end_matches(".git").to_string();
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(Self { owner, repo })
    }
}

/// Filter for PR list state. Maps to GitHub's `state=open|closed|all`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrListState {
    Open,
    Closed,
    All,
}

impl PrListState {
    pub fn as_query_str(self) -> &'static str {
        match self {
            PrListState::Open => "open",
            PrListState::Closed => "closed",
            PrListState::All => "all",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PrListState::Open => "Open",
            PrListState::Closed => "Closed",
            PrListState::All => "All",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: u32,
    pub title: SharedString,
    pub html_url: String,
    pub user_login: SharedString,
    pub user_avatar_url: Option<String>,
    pub head_sha: String,
    pub head_ref: SharedString,
    pub base_ref: SharedString,
    pub body: String,
    pub draft: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
}

/// A top-level conversation comment (the kind that shows in the PR's
/// "Conversation" tab on github.com). Backed by the Issues API.
#[derive(Clone, Debug)]
pub struct IssueComment {
    pub id: u64,
    pub author: SharedString,
    pub avatar_url: Option<String>,
    pub body: String,
    pub html_url: String,
}

/// A review comment anchored to a specific line in the diff.
#[derive(Clone, Debug)]
pub struct ReviewComment {
    pub id: u64,
    pub author: SharedString,
    pub avatar_url: Option<String>,
    pub body: String,
    pub path: SharedString,
    pub line: Option<u32>,
    pub html_url: String,
    pub outdated: bool,
}

#[derive(Clone, Debug)]
pub struct CheckRun {
    pub name: SharedString,
    pub status: SharedString,
    pub conclusion: Option<SharedString>,
    pub html_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PrCommit {
    pub sha: String,
    pub short_sha: SharedString,
    pub message: SharedString,
    pub author: SharedString,
    pub avatar_url: Option<String>,
    pub html_url: String,
}

#[derive(Clone, Debug, Default)]
pub struct PrDetails {
    pub mergeable: Option<bool>,
    pub mergeable_state: Option<String>,
    pub state: SharedString,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum ReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

impl ReviewEvent {
    fn as_api_str(self) -> &'static str {
        match self {
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Comment => "COMMENT",
        }
    }
}

#[derive(Clone)]
pub struct GitHubClient {
    http: Arc<dyn HttpClient>,
    token: Arc<str>,
}

impl GitHubClient {
    pub fn new(http: Arc<dyn HttpClient>, token: String) -> Self {
        Self {
            http,
            token: Arc::from(token),
        }
    }

    pub async fn list_open_prs(&self, repo: &RepoCoords) -> Result<Vec<PullRequest>> {
        self.list_prs(repo, PrListState::Open).await
    }

    /// Lists PRs honoring the requested state filter. Walks `Link` headers
    /// to fetch every page (capped at 500 PRs / 5 pages so we don't pin a
    /// huge buffer for repos with thousands of historical PRs).
    pub async fn list_prs(
        &self,
        repo: &RepoCoords,
        state: PrListState,
    ) -> Result<Vec<PullRequest>> {
        const PAGE_SIZE: u32 = 100;
        const MAX_PAGES: u32 = 5;

        let state_param = state.as_query_str();
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!(
                "{GITHUB_API}/repos/{}/{}/pulls?state={}&per_page={}&sort=updated&direction=desc&page={}",
                repo.owner, repo.repo, state_param, PAGE_SIZE, page
            );
            let body = self
                .send_json(&url, "application/vnd.github+json")
                .await
                .with_context(|| format!("listing pull requests page {page}"))?;
            let batch = decode_pr_list(&body, &url)?;
            let was_full = batch.len() as u32 >= PAGE_SIZE;
            all.extend(batch);
            if !was_full {
                break;
            }
        }
        Ok(all)
    }


    /// Lists check runs (CI workflow steps) attached to a given commit sha.
    /// Used to populate the per-PR CI listing on the overview tab. Status and
    /// conclusion strings come straight from GitHub (`status` ∈ queued /
    /// in_progress / completed; `conclusion` ∈ success / failure / neutral /
    /// cancelled / skipped / timed_out / action_required when status =
    /// completed).
    pub async fn get_check_runs(&self, repo: &RepoCoords, sha: &str) -> Result<Vec<CheckRun>> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/commits/{}/check-runs?per_page=100",
            repo.owner, repo.repo, sha
        );
        let body = self
            .send_json(&url, "application/vnd.github+json")
            .await
            .with_context(|| format!("listing check runs for {sha}"))?;

        #[derive(Deserialize)]
        struct RawRun {
            name: String,
            status: String,
            conclusion: Option<String>,
            html_url: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawList {
            check_runs: Vec<RawRun>,
        }
        let raw: RawList = serde_json::from_slice(&body)
            .with_context(|| format!("decoding check runs from {url}"))?;
        Ok(raw
            .check_runs
            .into_iter()
            .map(|r| CheckRun {
                name: r.name.into(),
                status: r.status.into(),
                conclusion: r.conclusion.map(SharedString::from),
                html_url: r.html_url,
            })
            .collect())
    }

    /// Lists the PR's commits in chronological order. Returned message is
    /// the first line only — full body lives on github.com.
    pub async fn get_pr_commits(
        &self,
        repo: &RepoCoords,
        pr_number: u32,
    ) -> Result<Vec<PrCommit>> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}/commits?per_page=100",
            repo.owner, repo.repo, pr_number
        );
        let body = self
            .send_json(&url, "application/vnd.github+json")
            .await
            .with_context(|| format!("listing commits for PR #{pr_number}"))?;

        #[derive(Deserialize)]
        struct RawAuthor {
            login: Option<String>,
            avatar_url: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawCommitInfo {
            message: String,
            author: Option<RawCommitAuthor>,
        }
        #[derive(Deserialize)]
        struct RawCommitAuthor {
            name: String,
        }
        #[derive(Deserialize)]
        struct RawCommit {
            sha: String,
            commit: RawCommitInfo,
            author: Option<RawAuthor>,
            html_url: String,
        }
        let raw: Vec<RawCommit> = serde_json::from_slice(&body)
            .with_context(|| format!("decoding PR commits from {url}"))?;
        Ok(raw
            .into_iter()
            .map(|c| {
                let short_sha: SharedString = c.sha.chars().take(7).collect::<String>().into();
                let first_line = c
                    .commit
                    .message
                    .lines()
                    .next()
                    .unwrap_or(&c.commit.message)
                    .to_string();
                let (author, avatar_url) = match c.author {
                    Some(a) => (
                        a.login.unwrap_or_else(|| {
                            c.commit
                                .author
                                .as_ref()
                                .map(|a| a.name.clone())
                                .unwrap_or_else(|| "unknown".into())
                        }),
                        a.avatar_url,
                    ),
                    None => (
                        c.commit
                            .author
                            .as_ref()
                            .map(|a| a.name.clone())
                            .unwrap_or_else(|| "unknown".into()),
                        None,
                    ),
                };
                PrCommit {
                    sha: c.sha,
                    short_sha,
                    message: first_line.into(),
                    author: author.into(),
                    avatar_url,
                    html_url: c.html_url,
                }
            })
            .collect())
    }

    /// Returns mergeability + change-stats for a PR. GitHub computes
    /// `mergeable` lazily; the first call after a push may return `None`.
    pub async fn get_pr_details(&self, repo: &RepoCoords, pr_number: u32) -> Result<PrDetails> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}",
            repo.owner, repo.repo, pr_number
        );
        let body = self
            .send_json(&url, "application/vnd.github+json")
            .await
            .with_context(|| format!("fetching PR #{pr_number} details"))?;
        #[derive(Deserialize)]
        struct Raw {
            state: String,
            mergeable: Option<bool>,
            mergeable_state: Option<String>,
            additions: Option<u32>,
            deletions: Option<u32>,
            changed_files: Option<u32>,
        }
        let raw: Raw = serde_json::from_slice(&body)
            .with_context(|| format!("decoding PR details from {url}"))?;
        Ok(PrDetails {
            mergeable: raw.mergeable,
            mergeable_state: raw.mergeable_state,
            state: raw.state.into(),
            additions: raw.additions.unwrap_or(0),
            deletions: raw.deletions.unwrap_or(0),
            changed_files: raw.changed_files.unwrap_or(0),
        })
    }

    /// Returns the unified diff text for the PR (`Accept: application/vnd.github.v3.diff`).
    pub async fn get_pr_diff(&self, repo: &RepoCoords, pr_number: u32) -> Result<String> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}",
            repo.owner, repo.repo, pr_number
        );
        let bytes = self
            .send_json(&url, "application/vnd.github.v3.diff")
            .await
            .with_context(|| format!("fetching PR #{pr_number} unified diff"))?;
        String::from_utf8(bytes).map_err(|err| anyhow!("PR diff is not valid UTF-8: {err}"))
    }

    /// Top-level conversation comments (issue API).
    pub async fn get_issue_comments(
        &self,
        repo: &RepoCoords,
        pr_number: u32,
    ) -> Result<Vec<IssueComment>> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/issues/{}/comments?per_page=100",
            repo.owner, repo.repo, pr_number
        );
        let body = self
            .send_json(&url, "application/vnd.github+json")
            .await
            .context("listing issue comments")?;

        #[derive(Deserialize)]
        struct RawUser {
            login: String,
            avatar_url: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawComment {
            id: u64,
            user: Option<RawUser>,
            body: Option<String>,
            html_url: String,
        }
        let raw: Vec<RawComment> = serde_json::from_slice(&body)
            .with_context(|| format!("decoding issue comments from {url}"))?;
        Ok(raw
            .into_iter()
            .map(|c| {
                let (login, avatar) = match c.user {
                    Some(u) => (u.login, u.avatar_url),
                    None => ("unknown".into(), None),
                };
                IssueComment {
                    id: c.id,
                    author: login.into(),
                    avatar_url: avatar,
                    body: c.body.unwrap_or_default(),
                    html_url: c.html_url,
                }
            })
            .collect())
    }

    /// Line-anchored review comments.
    pub async fn get_review_comments(
        &self,
        repo: &RepoCoords,
        pr_number: u32,
    ) -> Result<Vec<ReviewComment>> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}/comments?per_page=100",
            repo.owner, repo.repo, pr_number
        );
        let body = self
            .send_json(&url, "application/vnd.github+json")
            .await
            .context("listing review comments")?;

        #[derive(Deserialize)]
        struct RawUser {
            login: String,
            avatar_url: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawComment {
            id: u64,
            user: Option<RawUser>,
            body: Option<String>,
            path: String,
            line: Option<u32>,
            original_line: Option<u32>,
            position: Option<u32>,
            html_url: String,
        }
        let raw: Vec<RawComment> = serde_json::from_slice(&body)
            .with_context(|| format!("decoding review comments from {url}"))?;
        Ok(raw
            .into_iter()
            .map(|c| {
                let outdated = c.line.is_none() && c.position.is_none();
                let (login, avatar) = match c.user {
                    Some(u) => (u.login, u.avatar_url),
                    None => ("unknown".into(), None),
                };
                ReviewComment {
                    id: c.id,
                    author: login.into(),
                    avatar_url: avatar,
                    body: c.body.unwrap_or_default(),
                    path: c.path.into(),
                    line: c.line.or(c.original_line),
                    html_url: c.html_url,
                    outdated,
                }
            })
            .collect())
    }

    /// Posts a top-level conversation comment.
    pub async fn post_issue_comment(
        &self,
        repo: &RepoCoords,
        pr_number: u32,
        body: &str,
    ) -> Result<()> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/issues/{}/comments",
            repo.owner, repo.repo, pr_number
        );
        let payload = serde_json::json!({ "body": body });
        self.send_post(&url, &payload).await?;
        Ok(())
    }

    /// Posts a line-anchored review comment as its own one-comment review
    /// (the simplest path on the GitHub API: the dedicated review-comment
    /// endpoint requires a pre-existing pending review, which we don't keep
    /// around between sessions).
    pub async fn post_review_line_comment(
        &self,
        repo: &RepoCoords,
        pr_number: u32,
        commit_sha: &str,
        path: &str,
        line: u32,
        body: &str,
    ) -> Result<()> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}/reviews",
            repo.owner, repo.repo, pr_number
        );
        let payload = serde_json::json!({
            "commit_id": commit_sha,
            "event": "COMMENT",
            "comments": [{
                "path": path,
                "line": line,
                "side": "RIGHT",
                "body": body,
            }],
        });
        self.send_post(&url, &payload).await?;
        Ok(())
    }

    /// Submits a review (approve / request changes / comment).
    pub async fn post_review(
        &self,
        repo: &RepoCoords,
        pr_number: u32,
        event: ReviewEvent,
        body: &str,
    ) -> Result<()> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}/reviews",
            repo.owner, repo.repo, pr_number
        );
        let mut payload = serde_json::json!({ "event": event.as_api_str() });
        if !body.is_empty() {
            payload["body"] = serde_json::Value::String(body.to_string());
        }
        self.send_post(&url, &payload).await?;
        Ok(())
    }

    pub async fn get_pr_files(&self, repo: &RepoCoords, pr_number: u32) -> Result<Vec<ChangedFile>> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}/files?per_page=100",
            repo.owner, repo.repo, pr_number
        );
        let body = self
            .send_json(&url, "application/vnd.github+json")
            .await
            .context("listing PR changed files")?;
        serde_json::from_slice::<Vec<ChangedFile>>(&body)
            .with_context(|| format!("decoding PR files response from {url}"))
    }

    /// Returns raw file content at `sha`. Uses the `application/vnd.github.raw`
    /// accept header so GitHub returns bytes directly instead of a base64 JSON.
    pub async fn get_blob_content(
        &self,
        repo: &RepoCoords,
        sha: &str,
        path: &str,
    ) -> Result<String> {
        let path_encoded = path
            .split('/')
            .map(urlencode_path_segment)
            .collect::<Vec<_>>()
            .join("/");
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/contents/{}?ref={}",
            repo.owner, repo.repo, path_encoded, sha
        );
        let bytes = self
            .send_json(&url, "application/vnd.github.raw")
            .await
            .with_context(|| format!("fetching blob {path} at {sha}"))?;
        String::from_utf8(bytes)
            .map_err(|err| anyhow!("blob is not valid UTF-8: {err}"))
    }

    async fn send_json(&self, url: &str, accept: &str) -> Result<Vec<u8>> {
        let request = Request::get(url)
            .header("Accept", accept)
            .header("User-Agent", USER_AGENT)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Authorization", format!("Bearer {}", self.token))
            .follow_redirects(http_client::RedirectPolicy::FollowAll)
            .body(AsyncBody::default())?;
        let mut response = self.http.send(request).await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            let text = String::from_utf8_lossy(&body);
            bail!("GitHub API {url} returned {}: {text}", status.as_u16());
        }
        Ok(body)
    }

    async fn send_post(&self, url: &str, payload: &serde_json::Value) -> Result<Vec<u8>> {
        let body_bytes = serde_json::to_vec(payload).context("serializing POST payload")?;
        let request = Request::post(url)
            .header("Accept", "application/vnd.github+json")
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Authorization", format!("Bearer {}", self.token))
            .follow_redirects(http_client::RedirectPolicy::FollowAll)
            .body(AsyncBody::from(body_bytes))?;
        let mut response = self.http.send(request).await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            let text = String::from_utf8_lossy(&body);
            bail!("GitHub API POST {url} returned {}: {text}", status.as_u16());
        }
        Ok(body)
    }
}

fn decode_pr_list(body: &[u8], url: &str) -> Result<Vec<PullRequest>> {
    #[derive(Deserialize)]
    struct RawUser {
        login: String,
        avatar_url: Option<String>,
    }
    #[derive(Deserialize)]
    struct RawRef {
        sha: String,
        #[serde(rename = "ref")]
        ref_name: String,
    }
    #[derive(Deserialize)]
    struct RawPr {
        number: u32,
        title: String,
        html_url: String,
        body: Option<String>,
        user: Option<RawUser>,
        head: RawRef,
        base: RawRef,
        draft: Option<bool>,
    }

    let raw: Vec<RawPr> = serde_json::from_slice(body)
        .with_context(|| format!("decoding PR list response from {url}"))?;
    Ok(raw
        .into_iter()
        .map(|r| {
            let (login, avatar) = match r.user {
                Some(u) => (u.login, u.avatar_url),
                None => ("unknown".into(), None),
            };
            PullRequest {
                number: r.number,
                title: r.title.into(),
                html_url: r.html_url,
                user_login: login.into(),
                user_avatar_url: avatar,
                head_sha: r.head.sha,
                head_ref: r.head.ref_name.into(),
                base_ref: r.base.ref_name.into(),
                body: r.body.unwrap_or_default(),
                draft: r.draft.unwrap_or(false),
            }
        })
        .collect())
}

fn urlencode_path_segment(segment: &str) -> String {
    // Conservative percent-encoding for path components: encode anything outside
    // unreserved set (A-Z a-z 0-9 - _ . ~).
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        let b = *byte;
        let is_unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_remote() {
        let r = RepoCoords::parse_github_url("https://github.com/zed-industries/zed.git").unwrap();
        assert_eq!(r.owner, "zed-industries");
        assert_eq!(r.repo, "zed");
    }

    #[test]
    fn parse_ssh_remote() {
        let r = RepoCoords::parse_github_url("git@github.com:zed-industries/zed.git").unwrap();
        assert_eq!(r.owner, "zed-industries");
        assert_eq!(r.repo, "zed");
    }

    #[test]
    fn parse_no_dotgit() {
        let r = RepoCoords::parse_github_url("https://github.com/foo/bar").unwrap();
        assert_eq!(r.owner, "foo");
        assert_eq!(r.repo, "bar");
    }

    #[test]
    fn parse_non_github() {
        assert!(RepoCoords::parse_github_url("https://gitlab.com/foo/bar").is_none());
    }

    #[test]
    fn url_encode_segment() {
        assert_eq!(urlencode_path_segment("foo bar.rs"), "foo%20bar.rs");
        assert_eq!(urlencode_path_segment("a-b_c.rs"), "a-b_c.rs");
    }
}
