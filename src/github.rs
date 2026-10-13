//! Open pull requests on GitHub: finding the repository and a token, listing
//! open pull requests, and fetching a pull request's diff.
//!
//! Network access only happens through [`Client`], which needs a token.

use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// The remote whose GitHub repository vert lists pull requests for.
pub const ORIGIN: &str = "origin";

/// Environment variables checked for a token, in order.
pub const TOKEN_VARS: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];

/// Media type that makes the pull request endpoint return a unified diff.
const DIFF_MEDIA_TYPE: &str = "application/vnd.github.diff";
const JSON_MEDIA_TYPE: &str = "application/vnd.github+json";

/// Errors talking to GitHub.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GhError {
    /// The token was rejected.
    #[error("GitHub rejected the token; set GH_TOKEN or run `gh auth login`")]
    Unauthorized,
    /// The hourly API quota is used up.
    #[error("GitHub API rate limit reached; try again later")]
    RateLimited,
    /// The repository does not exist or the token cannot see it.
    #[error("GitHub can't find {0} with this token")]
    NotFound(String),
    /// Any other unsuccessful response.
    #[error("GitHub answered {status}: {message}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// GitHub's explanation, if any.
        message: String,
    },
    /// The request did not complete (offline, DNS, TLS, timeout).
    #[error("couldn't reach GitHub: {0}")]
    Network(String),
    /// The response was not what GitHub documents.
    #[error("unexpected response from GitHub: {0}")]
    Decode(String),
}

/// Why pull requests can't be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullsError {
    /// The repository has no GitHub `origin`, or network access is off.
    Unsupported(String),
    /// No token in the environment and `gh` is not logged in.
    NoToken,
    /// GitHub refused or the request failed.
    Failed(String),
}

impl From<GhError> for PullsError {
    fn from(e: GhError) -> Self {
        Self::Failed(e.to_string())
    }
}

/// A GitHub repository, `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slug {
    /// User or organisation.
    pub owner: String,
    /// Repository name.
    pub name: String,
}

impl std::fmt::Display for Slug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// Where the GitHub API lives: `https://api.github.com`, or
/// `https://HOST/api/v3` for GitHub Enterprise Server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    rest: String,
}

impl Endpoint {
    /// An endpoint from the REST API base URL.
    #[must_use]
    pub fn new(rest: &str) -> Self {
        Self {
            rest: rest.trim_end_matches('/').to_owned(),
        }
    }

    /// The host that serves the repositories (what `origin` points at),
    /// without any port.
    ///
    /// ```
    /// use vert::github::Endpoint;
    /// assert_eq!(Endpoint::new("https://api.github.com").web_host(), "github.com");
    /// assert_eq!(Endpoint::new("https://git.corp/api/v3").web_host(), "git.corp");
    /// ```
    #[must_use]
    pub fn web_host(&self) -> String {
        let after_scheme = self
            .rest
            .split_once("://")
            .map_or(self.rest.as_str(), |(_, rest)| rest);
        let (host, path) = after_scheme.split_once('/').unwrap_or((after_scheme, ""));
        let host = host.split(':').next().unwrap_or(host);
        // Enterprise Server serves its API under `/api/` on the web host itself;
        // github.com and GHE.com serve it from an `api.` subdomain.
        if path.starts_with("api/") {
            host.to_owned()
        } else {
            host.strip_prefix("api.").unwrap_or(host).to_owned()
        }
    }

    fn graphql(&self) -> String {
        self.rest.strip_suffix("/v3").map_or_else(
            || format!("{}/graphql", self.rest),
            |api| format!("{api}/graphql"),
        )
    }
}

/// Reads `owner/name` from a remote URL on `host`, in any of git's URL forms.
///
/// ```
/// use vert::github::parse_remote;
/// let slug = parse_remote("git@github.com:adlrocha/vert.git", "github.com").unwrap();
/// assert_eq!(slug.to_string(), "adlrocha/vert");
/// assert!(parse_remote("https://gitlab.com/a/b", "github.com").is_none());
/// ```
#[must_use]
pub fn parse_remote(url: &str, host: &str) -> Option<Slug> {
    let url = url.trim();
    let (authority, path) = if let Some((_, rest)) = url.split_once("://") {
        rest.split_once('/')?
    } else {
        // scp-like syntax: [user@]host:owner/name
        url.split_once(':')?
    };
    let remote_host = authority.rsplit('@').next()?;
    let remote_host = remote_host.split(':').next()?;
    if !remote_host.eq_ignore_ascii_case(host) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let valid = |s: &str| !s.is_empty() && !s.contains('/');
    (valid(owner) && valid(name)).then(|| Slug {
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

/// A GitHub token. Its `Debug` output is redacted so it cannot reach logs.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Wraps a token string.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token itself, for the `Authorization` header.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Finds a token: the first non-empty [`TOKEN_VARS`] entry from `env`, else
/// whatever `gh` returns.
#[must_use]
pub fn resolve_token(
    env: impl Fn(&str) -> Option<String>,
    gh: impl FnOnce() -> Option<String>,
) -> Option<Token> {
    TOKEN_VARS
        .iter()
        .filter_map(|var| env(var))
        .map(|v| v.trim().to_owned())
        .find(|v| !v.is_empty())
        .or_else(gh)
        .map(Token)
}

/// The token from `<program> auth token` (the GitHub CLI), if it is installed
/// and logged in.
#[must_use]
pub fn gh_token(program: &str, host: &str) -> Option<String> {
    let out = Command::new(program)
        .args(["auth", "token", "--hostname", host])
        .output()
        .ok()?;
    let token = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !token.is_empty()).then_some(token)
}

/// The combined state of a pull request's CI checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checks {
    /// Every check passed.
    Passing,
    /// A check failed or errored.
    Failing,
    /// Checks are still running or have not reported yet.
    Pending,
}

impl Checks {
    /// Maps a GitHub `StatusState`, or `None` for one vert does not know.
    #[must_use]
    pub fn from_state(state: &str) -> Option<Self> {
        match state {
            "SUCCESS" => Some(Self::Passing),
            "FAILURE" | "ERROR" => Some(Self::Failing),
            "PENDING" | "EXPECTED" => Some(Self::Pending),
            _ => None,
        }
    }
}

/// An open pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// Pull request number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Author login (empty for deleted accounts).
    pub author: String,
    /// Whether it is a draft.
    pub draft: bool,
    /// Head branch name.
    pub head: String,
    /// Whether the head branch lives in a fork.
    pub fork: bool,
    /// Last update (Unix seconds).
    pub updated: i64,
    /// Lines added.
    pub added: u64,
    /// Lines removed.
    pub removed: u64,
    /// CI checks on the head commit, or `None` when none have run.
    pub checks: Option<Checks>,
}

/// A GitHub API client for one endpoint and token.
#[derive(Debug, Clone)]
pub struct Client {
    agent: ureq::Agent,
    endpoint: Endpoint,
    token: Token,
}

const PULLS_QUERY: &str = "query($owner: String!, $name: String!, $first: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: $first, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        number title isDraft headRefName isCrossRepository
        updatedAt additions deletions
        author { login }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}";

#[derive(Deserialize)]
struct GraphQl {
    data: Option<Data>,
    #[serde(default)]
    errors: Vec<Message>,
}

#[derive(Deserialize)]
struct Data {
    repository: Option<Repo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repo {
    pull_requests: Nodes,
}

#[derive(Deserialize)]
struct Nodes {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    number: u64,
    title: String,
    is_draft: bool,
    head_ref_name: String,
    is_cross_repository: bool,
    updated_at: String,
    additions: u64,
    deletions: u64,
    author: Option<Login>,
    #[serde(default)]
    commits: Commits,
}

/// The head commit, the only one asked for.
#[derive(Deserialize, Default)]
struct Commits {
    nodes: Vec<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Commit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    state: String,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct Message {
    message: String,
}

impl Client {
    /// A client for `endpoint` that authenticates with `token` and gives up
    /// on requests after `timeout`.
    #[must_use]
    pub fn new(endpoint: Endpoint, token: Token, timeout: Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent(concat!("vert/", env!("CARGO_PKG_VERSION")))
            // The OS trust store, like `gh`, so corporate CAs work.
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        Self {
            agent,
            endpoint,
            token,
        }
    }

    /// Up to `limit` open pull requests of `slug`, most recently updated first.
    ///
    /// # Errors
    /// Returns [`GhError`] when the request fails or GitHub refuses it.
    pub fn open_pulls(&self, slug: &Slug, limit: u16) -> Result<Vec<PullRequest>, GhError> {
        let body = serde_json::json!({
            "query": PULLS_QUERY,
            "variables": {"owner": slug.owner, "name": slug.name, "first": limit},
        });
        let text = self.request(
            self.agent.post(self.endpoint.graphql()),
            Some(&body),
            JSON_MEDIA_TYPE,
            slug,
        )?;
        let reply: GraphQl =
            serde_json::from_str(&text).map_err(|e| GhError::Decode(e.to_string()))?;
        let repo = reply.data.and_then(|d| d.repository);
        let Some(repo) = repo else {
            return Err(reply.errors.into_iter().next().map_or_else(
                || GhError::NotFound(slug.to_string()),
                |e| GhError::Decode(e.message),
            ));
        };
        Ok(repo
            .pull_requests
            .nodes
            .into_iter()
            .map(|n| PullRequest {
                checks: n
                    .commits
                    .nodes
                    .last()
                    .and_then(|c| c.commit.status_check_rollup.as_ref())
                    .and_then(|r| Checks::from_state(&r.state)),
                number: n.number,
                title: n.title,
                author: n.author.map(|a| a.login).unwrap_or_default(),
                draft: n.is_draft,
                head: n.head_ref_name,
                fork: n.is_cross_repository,
                updated: parse_time(&n.updated_at).unwrap_or(0),
                added: n.additions,
                removed: n.deletions,
            })
            .collect())
    }

    /// The unified diff of pull request `number`.
    ///
    /// # Errors
    /// Returns [`GhError`] when the request fails or GitHub refuses it (for
    /// example when the diff is too large for the API).
    pub fn pull_diff(&self, slug: &Slug, number: u64) -> Result<String, GhError> {
        let url = format!("{}/repos/{slug}/pulls/{number}", self.endpoint.rest);
        self.request(
            self.agent.get(url).force_send_body(),
            None,
            DIFF_MEDIA_TYPE,
            slug,
        )
    }

    fn request(
        &self,
        req: ureq::RequestBuilder<ureq::typestate::WithBody>,
        body: Option<&serde_json::Value>,
        accept: &str,
        slug: &Slug,
    ) -> Result<String, GhError> {
        let req = req
            .header("Authorization", &format!("Bearer {}", self.token.expose()))
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        let sent = match body {
            Some(json) => req
                .header("Content-Type", "application/json")
                .send(json.to_string()),
            None => req.send_empty(),
        };
        let mut resp = sent.map_err(|e| GhError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let exhausted = resp
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v.as_bytes() == b"0");
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| GhError::Network(e.to_string()))?;
        match status {
            200..=299 => Ok(text),
            401 => Err(GhError::Unauthorized),
            403 | 429 if exhausted || status == 429 => Err(GhError::RateLimited),
            404 => Err(GhError::NotFound(slug.to_string())),
            _ => Err(GhError::Status {
                status,
                message: serde_json::from_str::<Message>(&text)
                    .map_or_else(|_| text.trim().to_owned(), |m| m.message),
            }),
        }
    }
}

/// Parses GitHub's `2024-05-06T07:08:09Z` timestamps into Unix seconds.
///
/// ```
/// assert_eq!(vert::github::parse_time("1970-01-02T00:00:01Z"), Some(86_401));
/// assert_eq!(vert::github::parse_time("yesterday"), None);
/// ```
#[must_use]
pub fn parse_time(stamp: &str) -> Option<i64> {
    let (date, clock) = stamp.strip_suffix('Z')?.split_once('T')?;
    let fields = |text: &str, sep: char| -> Option<[i64; 3]> {
        let mut it = text.splitn(3, sep).map(str::parse::<i64>);
        Some([it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?])
    };
    let [year, month, day] = fields(date, '-')?;
    let [hour, minute, second] = fields(clock, ':')?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
pub(crate) mod testserver {
    //! A one-connection-per-response HTTP server for exercising [`super::Client`].
    use std::fmt::Write as _;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    /// A canned response: status, extra headers, body.
    pub type Reply = (u16, Vec<(&'static str, &'static str)>, String);

    /// Serves `replies` in order; returns the base URL and the raw requests.
    pub fn serve(replies: Vec<Reply>) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for (status, headers, body) in replies {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    request.push_str(&line);
                }
                let mut payload = vec![0; length];
                let _ = reader.read_exact(&mut payload);
                request.push_str(&String::from_utf8_lossy(&payload));
                let _ = tx.send(request);
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    let _ = write!(out, "{k}: {v}\r\n");
                }
                out.push_str("\r\n");
                out.push_str(&body);
                let mut stream = reader.into_inner();
                let _ = stream.write_all(out.as_bytes());
            }
        });
        (url, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::testserver::serve;
    use super::*;

    fn slug() -> Slug {
        Slug {
            owner: "o".to_owned(),
            name: "r".to_owned(),
        }
    }

    fn client(url: &str) -> Client {
        Client::new(
            Endpoint::new(url),
            Token::new("t0k"),
            Duration::from_secs(5),
        )
    }

    const PULLS_JSON: &str = r#"{"data":{"repository":{"pullRequests":{"nodes":[
        {"number":7,"title":"Fix auth","isDraft":true,"headRefName":"agent/auth",
         "isCrossRepository":false,"updatedAt":"2026-10-13T10:00:00Z",
         "additions":12,"deletions":3,"author":{"login":"claude"},
         "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}},
        {"number":5,"title":"Ghost","isDraft":false,"headRefName":"x",
         "isCrossRepository":true,"updatedAt":"bad","additions":0,"deletions":0,
         "author":null,"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}]}}}}"#;

    #[test]
    fn parses_remote_urls() {
        let ok = [
            "git@github.com:o/r.git",
            "git@github.com:o/r",
            "ssh://git@github.com/o/r.git",
            "ssh://git@github.com:22/o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/",
            "https://user:secret@GitHub.com/o/r",
            "git://github.com/o/r.git",
        ];
        for url in ok {
            assert_eq!(parse_remote(url, "github.com"), Some(slug()), "{url}");
        }
        let bad = [
            "https://gitlab.com/o/r",
            "https://github.com/o",
            "https://github.com/o/r/extra",
            "/local/path",
            "github.com",
            "git@github.com:/r",
        ];
        for url in bad {
            assert_eq!(parse_remote(url, "github.com"), None, "{url}");
        }
        assert_eq!(slug().to_string(), "o/r");
    }

    #[test]
    fn endpoints() {
        let gh = Endpoint::new("https://api.github.com/");
        assert_eq!(gh.web_host(), "github.com");
        assert_eq!(gh.graphql(), "https://api.github.com/graphql");
        let ghe = Endpoint::new("https://git.corp/api/v3");
        assert_eq!(ghe.web_host(), "git.corp");
        assert_eq!(ghe.graphql(), "https://git.corp/api/graphql");
        assert_eq!(
            Endpoint::new("http://127.0.0.1:8080").web_host(),
            "127.0.0.1"
        );
        assert_eq!(Endpoint::new("localhost:1").web_host(), "localhost");
        assert_eq!(
            Endpoint::new("https://api.corp.example/api/v3").web_host(),
            "api.corp.example",
            "Enterprise Server keeps a host that starts with api."
        );
        assert_eq!(
            Endpoint::new("https://api.acme.ghe.com").web_host(),
            "acme.ghe.com",
            "GHE.com serves its API from an api. subdomain"
        );
    }

    #[test]
    fn token_resolution_order() {
        let env = |pairs: &'static [(&str, &str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let gh = || Some("from-gh".to_owned());
        let both = env(&[("GH_TOKEN", " a "), ("GITHUB_TOKEN", "b")]);
        assert_eq!(
            resolve_token(both, gh).as_ref().map(Token::expose),
            Some("a")
        );
        let blank = env(&[("GH_TOKEN", "  "), ("GITHUB_TOKEN", "b")]);
        assert_eq!(
            resolve_token(blank, gh).as_ref().map(Token::expose),
            Some("b")
        );
        assert_eq!(
            resolve_token(env(&[]), gh).as_ref().map(Token::expose),
            Some("from-gh")
        );
        assert_eq!(resolve_token(env(&[]), || None), None);
        let client = Client::new(
            Endpoint::new("https://api.github.com"),
            Token::new("s3cret"),
            Duration::from_secs(1),
        );
        assert!(!format!("{client:?}").contains("s3cret"), "token redacted");
    }

    #[cfg(unix)]
    #[test]
    fn gh_cli_fallback() {
        // `echo` stands in for a logged-in `gh`, `false` for a logged-out one.
        assert_eq!(
            gh_token("echo", "github.com").as_deref(),
            Some("auth token --hostname github.com")
        );
        assert_eq!(gh_token("false", "github.com"), None);
        assert_eq!(gh_token("/definitely/not/gh", "github.com"), None);
    }

    #[test]
    fn check_states() {
        let cases = [
            ("SUCCESS", Some(Checks::Passing)),
            ("FAILURE", Some(Checks::Failing)),
            ("ERROR", Some(Checks::Failing)),
            ("PENDING", Some(Checks::Pending)),
            ("EXPECTED", Some(Checks::Pending)),
            ("SOMETHING_NEW", None),
        ];
        for (state, want) in cases {
            assert_eq!(Checks::from_state(state), want, "{state}");
        }
    }

    #[test]
    fn lists_open_pulls() {
        let (url, requests) = serve(vec![(200, vec![], PULLS_JSON.to_owned())]);
        let pulls = client(&url).open_pulls(&slug(), 20).expect("pulls");
        assert_eq!(pulls.len(), 2);
        let first = &pulls[0];
        assert_eq!(first.number, 7);
        assert_eq!(first.author, "claude");
        assert!(first.draft && !first.fork);
        assert_eq!((first.added, first.removed), (12, 3));
        assert_eq!(
            first.updated,
            parse_time("2026-10-13T10:00:00Z").expect("t")
        );
        assert_eq!(first.head, "agent/auth");
        assert_eq!(pulls[1].author, "");
        assert!(pulls[1].fork);
        assert_eq!(pulls[1].updated, 0);
        assert_eq!(first.checks, Some(Checks::Failing));
        assert_eq!(pulls[1].checks, None, "no checks ran");

        let req = requests.recv().expect("request");
        assert!(req.starts_with("POST /graphql"), "{req}");
        assert!(
            req.contains("authorization: Bearer t0k") || req.contains("Authorization: Bearer t0k")
        );
        assert!(req.contains(r#""first":20"#), "{req}");
    }

    #[test]
    fn graphql_errors_and_missing_repos() {
        let (url, _r) = serve(vec![
            (
                200,
                vec![],
                r#"{"data":{"repository":null},"errors":[{"message":"Could not resolve"}]}"#
                    .to_owned(),
            ),
            (200, vec![], r#"{"data":{"repository":null}}"#.to_owned()),
            (200, vec![], "not json".to_owned()),
        ]);
        let c = client(&url);
        assert_eq!(
            c.open_pulls(&slug(), 5),
            Err(GhError::Decode("Could not resolve".to_owned()))
        );
        assert_eq!(
            c.open_pulls(&slug(), 5),
            Err(GhError::NotFound("o/r".to_owned()))
        );
        assert!(matches!(c.open_pulls(&slug(), 5), Err(GhError::Decode(_))));
    }

    #[test]
    fn fetches_diffs_and_maps_statuses() {
        let (url, requests) = serve(vec![
            (200, vec![], "diff --git a/x b/x\n".to_owned()),
            (401, vec![], String::new()),
            (403, vec![("x-ratelimit-remaining", "0")], String::new()),
            (429, vec![], String::new()),
            (404, vec![], String::new()),
            (406, vec![], r#"{"message":"diff too large"}"#.to_owned()),
            (500, vec![], "oops\n".to_owned()),
            (
                403,
                vec![("x-ratelimit-remaining", "10")],
                r#"{"message":"SSO"}"#.to_owned(),
            ),
        ]);
        let c = client(&url);
        assert_eq!(
            c.pull_diff(&slug(), 7).as_deref(),
            Ok("diff --git a/x b/x\n")
        );
        let req = requests.recv().expect("request");
        assert!(req.starts_with("GET /repos/o/r/pulls/7"), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains("accept: application/vnd.github.diff")
        );
        assert_eq!(c.pull_diff(&slug(), 7), Err(GhError::Unauthorized));
        assert_eq!(c.pull_diff(&slug(), 7), Err(GhError::RateLimited));
        assert_eq!(c.pull_diff(&slug(), 7), Err(GhError::RateLimited));
        assert_eq!(
            c.pull_diff(&slug(), 7),
            Err(GhError::NotFound("o/r".to_owned()))
        );
        let too_large = c.pull_diff(&slug(), 7).expect_err("406");
        assert_eq!(too_large.to_string(), "GitHub answered 406: diff too large");
        assert_eq!(
            c.pull_diff(&slug(), 7),
            Err(GhError::Status {
                status: 500,
                message: "oops".to_owned()
            })
        );
        assert!(
            c.pull_diff(&slug(), 7)
                .expect_err("sso")
                .to_string()
                .contains("SSO")
        );
    }

    #[test]
    fn network_failures() {
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            format!("http://{}", l.local_addr().expect("addr"))
        };
        let err = client(&dead)
            .pull_diff(&slug(), 1)
            .expect_err("closed port");
        assert!(matches!(err, GhError::Network(_)), "{err:?}");
        assert!(err.to_string().starts_with("couldn't reach GitHub"));
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_time("2026-10-13T10:00:00Z"), Some(1_791_885_600));
        assert_eq!(parse_time("1969-12-31T23:59:59Z"), Some(-1));
        for bad in [
            "2026-13-01T00:00:00Z",
            "2026-10-13T10:00:00",
            "2026-10-13",
            "x-y-zTa:b:cZ",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }
}
