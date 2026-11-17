//! Logged-in access to Reddit with the operator's own session cookie.
//!
//! Reddit no longer serves logged-out visitors from many networks. When
//! `REDLIB_REDDIT_COOKIE` holds the `reddit_session` cookie of a Reddit
//! account, Redlib reads Reddit's JSON as that account:
//!
//! - on `www.reddit.com` with the cookie, and
//! - on `oauth.reddit.com` with a token minted for the session.
//!
//! Each host refuses some routes to sessions, so every request tries the
//! better host first and the other one second. Only one session is used.

use crate::client::{reddit_json_value, CLIENT};
use crate::oauth::{session_token, SessionToken};
use hyper::body::Buf;
use log::{info, warn};
use serde_json::Value;
use std::borrow::Cow;
use std::env;
use std::time::{Duration, Instant};
use wreq::{header as wreq_header, Response as WreqResponse, StatusCode};

const COOKIE_ENV: &str = "REDLIB_REDDIT_COOKIE";
const SESSION_COOKIE_NAME: &str = "reddit_session";

const WEB_URL_BASE: &str = "https://www.reddit.com";
const API_URL_BASE: &str = "https://oauth.reddit.com";

// The same opt-in Redlib sends logged out, for quarantined and gated subreddits.
const QUARANTINE_OPTIN_COOKIE: &str = "_options=%7B%22pref_quarantine_optin%22%3A%20true%2C%20%22pref_gated_sr_optin%22%3A%20true%7D";

// A token is renewed this long before it expires.
const TOKEN_RENEW_MARGIN: Duration = Duration::from_secs(60);
// After a failed mint, wait this long before trying again.
const MINT_RETRY_DELAY: Duration = Duration::from_secs(300);
const MAX_REDIRECTS: usize = 3;

const EXPIRED_SESSION_MESSAGE: &str = "Reddit refused the session in REDLIB_REDDIT_COOKIE. Log in to Reddit again and replace the cookie.";

/// Whether a usable session cookie is configured.
pub fn configured() -> bool {
	cookie().is_some()
}

/// The configured cookie, reduced to `reddit_session=<value>`.
fn cookie() -> Option<String> {
	env::var(COOKIE_ENV).ok().and_then(|raw| normalize_cookie(&raw))
}

/// Accepts the bare cookie value, `reddit_session=<value>`, or a whole
/// `Cookie` header copied from the browser, and keeps only the session.
/// Other cookies are dropped so the browser's tracking cookies don't follow
/// the session to every request.
fn normalize_cookie(raw: &str) -> Option<String> {
	let raw = raw.trim();
	let value = if raw.contains('=') {
		raw.split(';').find_map(|pair| pair.trim().strip_prefix("reddit_session="))?
	} else {
		raw
	};
	let value = value.trim().trim_matches('"');
	let valid = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic() && byte != b';' && byte != b',');
	valid.then(|| format!("{SESSION_COOKIE_NAME}={value}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
	Web,
	Api,
}

/// Which host to ask first. Reddit refuses listings and search to sessions on
/// www, and post pages, profiles and `/new` on the API (checked November 2026).
fn hosts(path: &str) -> [Host; 2] {
	let route = path.split('?').next().unwrap_or_default().trim_end_matches(".json");
	let segments: Vec<&str> = route.split('/').filter(|segment| !segment.is_empty()).collect();
	let api_first = match segments.as_slice() {
		["comments", ..] | [_, _, "comments", ..] | ["by_id", ..] | ["duplicates", ..] => false,
		["user" | "u", ..] => false,
		["r", _, "wiki", ..] | ["wiki", ..] => false,
		["r", _, "about", ..] => false,
		["r", _, "api", "widgets"] => true,
		["search", ..] | ["r", _, "search", ..] | ["subreddits" | "users", "search", ..] => true,
		_ => !route.ends_with("/new"),
	};
	if api_first {
		[Host::Api, Host::Web]
	} else {
		[Host::Web, Host::Api]
	}
}

/// Reddit retired the bare r/all route but still serves it as the
/// multireddit `all+`. The `+` is encoded, or the API reads it as a space.
fn transport_path(path: &str) -> Cow<'_, str> {
	match path.strip_prefix("/r/all") {
		Some(rest) if rest.is_empty() || rest.starts_with(['/', '.', '?']) => Cow::Owned(format!("/r/all%2B{rest}")),
		_ => Cow::Borrowed(path),
	}
}

/// Fetches `path` as the configured session and parses Reddit's JSON.
pub async fn json(path: &str, quarantine: bool) -> Result<Value, String> {
	let Some(cookie) = cookie() else {
		return Err(format!("{COOKIE_ENV} is not set | {path}"));
	};

	let mut refusals: Vec<String> = Vec::with_capacity(2);
	let mut rate_limited = None;
	for host in hosts(path) {
		let response = match host {
			Host::Web => web_get(path, &cookie, quarantine).await,
			Host::Api => api_get(path, &cookie, quarantine).await,
		};
		let response = match response {
			Ok(response) => response,
			Err(e) => {
				warn!("Session request for {path} on {host:?} failed: {e}");
				refusals.push(format!("{host:?}: {e}"));
				continue;
			}
		};

		let status = response.status();
		if status == StatusCode::TOO_MANY_REQUESTS {
			let reset = header(&response, "x-ratelimit-reset");
			warn!("Reddit rate limited the session on {host:?} for {path}");
			rate_limited = Some(reset);
			continue;
		}
		if !is_answer(&response) {
			warn!("Reddit refused the session on {host:?} for {path}: status {status}");
			refusals.push(format!("{host:?}: {status}"));
			continue;
		}
		return parse(response, path).await;
	}

	if let Some(reset) = rate_limited {
		return Err(match reset {
			Some(reset) => format!("Reddit rate limit exceeded. Try refreshing in a few seconds. Rate limit will reset in: {reset}"),
			None => "Reddit rate limit exceeded. Try refreshing in a few seconds.".to_owned(),
		});
	}
	if refusals
		.iter()
		.all(|refusal| refusal.contains("401") || refusal.contains("login") || refusal.contains("token"))
	{
		return Err(format!("{EXPIRED_SESSION_MESSAGE} | {path}"));
	}
	if is_new_or_rising(path) {
		return Err(format!(
			"Reddit doesn't serve the New and Rising sorts to Redlib sessions. Use Hot or Top instead. | {path}"
		));
	}
	Err(format!("Reddit refused the request ({}) | {path}", refusals.join(", ")))
}

/// Subreddit New and Rising listings, which Reddit refuses to sessions on both
/// hosts (checked November 2026). The front page's `/new` still works.
fn is_new_or_rising(path: &str) -> bool {
	let route = path.split('?').next().unwrap_or_default().trim_end_matches(".json");
	route.starts_with("/r/") && (route.ends_with("/new") || route.ends_with("/rising"))
}

/// An answer is anything Reddit means for the page, including "not found" and
/// "private". A 401, 429, redirect or HTML block page is about the session.
fn is_answer(response: &WreqResponse) -> bool {
	let status = response.status();
	status.is_success() || status == StatusCode::NOT_FOUND || (status == StatusCode::FORBIDDEN && is_json(response))
}

fn is_json(response: &WreqResponse) -> bool {
	response
		.headers()
		.get(wreq_header::CONTENT_TYPE)
		.is_some_and(|value| value.as_bytes().starts_with(b"application/json"))
}

fn header(response: &WreqResponse, name: &str) -> Option<String> {
	response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_owned)
}

async fn parse(response: WreqResponse, path: &str) -> Result<Value, String> {
	let status = response.status();
	let body = response.bytes().await.map_err(|e| format!("Failed receiving body from Reddit: {e} | {path}"))?;
	match serde_json::from_reader::<_, Value>(body.reader()) {
		// Reddit's JSON errors without an `error` field, like {"reason": "WIKI_DISABLED"}.
		Ok(json) if !status.is_success() && json["error"].is_null() => Err(format!(
			"Reddit error {status} {}: {} | {path}",
			json["reason"].as_str().unwrap_or("no reason"),
			json["message"].as_str().unwrap_or_default()
		)),
		Ok(json) => reddit_json_value(json, path),
		Err(_) if status == StatusCode::NOT_FOUND => Err(format!("Not found | {path}")),
		Err(e) => Err(format!("Failed to parse page JSON data: {e} | {path}")),
	}
}

/// GET from www.reddit.com with the session cookie. Same-site redirects are
/// followed; a redirect to the login page means the session is no longer valid.
async fn web_get(path: &str, cookie: &str, quarantine: bool) -> Result<WreqResponse, String> {
	let cookie_header = if quarantine {
		format!("{cookie}; {QUARANTINE_OPTIN_COOKIE}")
	} else {
		cookie.to_owned()
	};

	let mut target = transport_path(path).into_owned();
	for _ in 0..=MAX_REDIRECTS {
		let response = CLIENT
			.get(format!("{WEB_URL_BASE}{target}"))
			.header(wreq_header::COOKIE, &cookie_header)
			.send()
			.await
			.map_err(|e| e.to_string())?;
		if !response.status().is_redirection() {
			return Ok(response);
		}

		let location = header(&response, "location").unwrap_or_default();
		target = match redirect_target(&location) {
			Some(next) => next,
			None if location.contains("/login") => return Err("redirected to login".to_owned()),
			None => return Err(format!("redirected to {location:?}")),
		};
	}
	Err("too many redirects".to_owned())
}

/// The path of a same-site redirect, if it stays on Reddit's JSON routes.
fn redirect_target(location: &str) -> Option<String> {
	let path = location
		.strip_prefix(WEB_URL_BASE)
		.or_else(|| location.strip_prefix("https://reddit.com"))
		.unwrap_or(location);
	let route = path.split('?').next().unwrap_or_default();
	let usable = path.starts_with('/') && !path.starts_with("//") && route.contains(".json") && !route.contains("/login");
	usable.then(|| path.to_owned())
}

/// GET from oauth.reddit.com with a token minted for the session.
async fn api_get(path: &str, cookie: &str, quarantine: bool) -> Result<WreqResponse, String> {
	let token = token(cookie).await?;
	let mut builder = CLIENT
		.get(format!("{API_URL_BASE}{}", transport_path(path)))
		.header(wreq_header::AUTHORIZATION, format!("Bearer {}", token.token));
	for (key, value) in &token.headers {
		builder = builder.header(key, value);
	}
	if quarantine {
		builder = builder.header(wreq_header::COOKIE, QUARANTINE_OPTIN_COOKIE);
	}

	let response = builder.send().await.map_err(|e| e.to_string())?;
	if response.status() == StatusCode::UNAUTHORIZED {
		forget_token().await;
	}
	Ok(response)
}

enum TokenState {
	Empty,
	Minted { token: SessionToken, expires_at: Instant },
	Failed { retry_at: Instant },
}

// One mint at a time: requests that miss the cache together share one token.
static TOKEN: tokio::sync::Mutex<TokenState> = tokio::sync::Mutex::const_new(TokenState::Empty);

async fn token(cookie: &str) -> Result<SessionToken, String> {
	let mut state = TOKEN.lock().await;
	let now = Instant::now();
	match &*state {
		TokenState::Minted { token, expires_at } if *expires_at > now + TOKEN_RENEW_MARGIN => return Ok(token.clone()),
		TokenState::Failed { retry_at } if *retry_at > now => return Err("no token for the session: the last try failed".to_owned()),
		_ => {}
	}

	match session_token(cookie).await {
		Ok(token) => {
			info!("[✅] Minted an API token for the Reddit session, expires in {}s", token.expires_in);
			let expires_at = now + Duration::from_secs(token.expires_in);
			*state = TokenState::Minted { token: token.clone(), expires_at };
			Ok(token)
		}
		Err(e) => {
			warn!("Couldn't mint an API token for the Reddit session: {e}");
			*state = TokenState::Failed { retry_at: now + MINT_RETRY_DELAY };
			Err(format!("no token for the session: {e}"))
		}
	}
}

async fn forget_token() {
	*TOKEN.lock().await = TokenState::Empty;
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_normalize_cookie_accepts_value_pair_and_header() {
		assert_eq!(normalize_cookie("abc.def-ghi").as_deref(), Some("reddit_session=abc.def-ghi"));
		assert_eq!(normalize_cookie(" reddit_session=abc.def \n").as_deref(), Some("reddit_session=abc.def"));
		assert_eq!(
			normalize_cookie("loid=xyz; reddit_session=abc.def; token_v2=t; csv=2").as_deref(),
			Some("reddit_session=abc.def")
		);
		assert_eq!(normalize_cookie("reddit_session=\"abc\"").as_deref(), Some("reddit_session=abc"));
	}

	#[test]
	fn test_normalize_cookie_rejects_missing_or_unsafe_values() {
		assert_eq!(normalize_cookie(""), None);
		assert_eq!(normalize_cookie("   "), None);
		assert_eq!(normalize_cookie("loid=xyz; token_v2=t"), None);
		assert_eq!(normalize_cookie("reddit_session="), None);
		assert_eq!(normalize_cookie("abc\r\nX-Injected: 1"), None);
		assert_eq!(normalize_cookie("abc def"), None);
	}

	#[test]
	fn test_hosts_put_listings_and_search_on_the_api() {
		for path in [
			"/hot.json?raw_json=1",
			"/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL",
			"/r/rust.json?raw_json=1",
			"/r/rust/top.json?t=week",
			"/r/rust+linux/hot.json",
			"/search.json?q=rust",
			"/r/rust/search.json?q=async&restrict_sr=on",
			"/r/rust/api/widgets.json",
		] {
			assert_eq!(hosts(path), [Host::Api, Host::Web], "{path}");
		}
	}

	#[test]
	fn test_hosts_put_posts_profiles_and_new_on_the_web() {
		for path in [
			"/r/rust/comments/abc123/title.json?raw_json=1&sort=confidence",
			"/comments/abc123.json",
			"/by_id/t3_abc123.json",
			"/duplicates/abc123.json",
			"/user/spez/about.json?raw_json=1",
			"/user/spez/overview.json?raw_json=1",
			"/r/rust/about.json?raw_json=1",
			"/r/rust/wiki/index.json",
			"/r/rust/new.json?raw_json=1",
		] {
			assert_eq!(hosts(path), [Host::Web, Host::Api], "{path}");
		}
	}

	#[test]
	fn test_is_new_or_rising_matches_subreddit_sorts_only() {
		assert!(is_new_or_rising("/r/rust/new.json?&raw_json=1"));
		assert!(is_new_or_rising("/r/rust+linux/rising.json"));
		assert!(!is_new_or_rising("/new.json?raw_json=1"));
		assert!(!is_new_or_rising("/r/rust/hot.json"));
		assert!(!is_new_or_rising("/user/spez/submitted.json?sort=new"));
	}

	#[test]
	fn test_transport_path_serves_r_all_as_multireddit() {
		assert_eq!(transport_path("/r/all/hot.json?raw_json=1"), "/r/all%2B/hot.json?raw_json=1");
		assert_eq!(transport_path("/r/all.json"), "/r/all%2B.json");
		assert_eq!(transport_path("/r/allthingsdead/hot.json"), "/r/allthingsdead/hot.json");
		assert_eq!(transport_path("/r/rust/hot.json"), "/r/rust/hot.json");
	}

	#[test]
	fn test_redirect_target_stays_on_json_routes() {
		assert_eq!(
			redirect_target("https://www.reddit.com/r/Rust/hot.json?raw_json=1").as_deref(),
			Some("/r/Rust/hot.json?raw_json=1")
		);
		assert_eq!(redirect_target("/r/Rust/.json").as_deref(), Some("/r/Rust/.json"));
		assert_eq!(redirect_target("https://www.reddit.com/login/?dest=x.json"), None);
		assert_eq!(redirect_target("https://www.reddit.com/r/Rust/"), None);
		assert_eq!(redirect_target("https://evil.example/r/x.json"), None);
		assert_eq!(redirect_target("//evil.example/x.json"), None);
	}
}
