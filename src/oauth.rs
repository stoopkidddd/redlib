use crate::{
	client::{CLIENT, OAUTH_CLIENT, OAUTH_IS_ROLLING_OVER, OAUTH_RATELIMIT_REMAINING},
	oauth_resources::ANDROID_APP_VERSION_LIST,
};
use base64::{engine::general_purpose, Engine as _};
use log::{error, info, trace, warn};
use serde_json::json;
use std::{collections::HashMap, env, sync::atomic::Ordering, time::Duration};
use tegen::tegen::TextGenerator;
use tokio::time::{error::Elapsed, timeout};

const REDDIT_ANDROID_OAUTH_CLIENT_ID: &str = "ohXpoqrZYub1kg";

const AUTH_ENDPOINT: &str = "https://www.reddit.com";
const REDDIT_API_TOKEN_ENDPOINT: &str = "https://www.reddit.com/api/v1/access_token";

// Credentials for a Reddit app registered at https://www.reddit.com/prefs/apps.
// A "script" app supplies both values; an "installed app" has no secret.
const REDDIT_CLIENT_ID_ENV: &str = "REDLIB_REDDIT_CLIENT_ID";
const REDDIT_CLIENT_SECRET_ENV: &str = "REDLIB_REDDIT_CLIENT_SECRET";
const REDDIT_USER_AGENT_ENV: &str = "REDLIB_REDDIT_USER_AGENT";

const OAUTH_TIMEOUT: Duration = Duration::from_secs(5);

// Response from OAuth backend authentication
#[derive(Debug, Clone)]
pub struct OauthResponse {
	pub token: String,
	pub expires_in: u64,
	pub additional_headers: HashMap<String, String>,
}

// Trait for OAuth backend implementations
trait OauthBackend: Send + Sync {
	fn authenticate(&mut self) -> impl std::future::Future<Output = Result<OauthResponse, AuthError>> + Send;
	fn user_agent(&self) -> &str;
	fn get_headers(&self) -> HashMap<String, String>;
}

// OAuth backend implementations
#[derive(Debug, Clone)]
pub(crate) enum OauthBackendImpl {
	OfficialApi(RedditApiAuth),
	MobileSpoof(MobileSpoofAuth),
	GenericWeb(GenericWebAuth),
}

impl OauthBackend for OauthBackendImpl {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		match self {
			OauthBackendImpl::OfficialApi(backend) => backend.authenticate().await,
			OauthBackendImpl::MobileSpoof(backend) => backend.authenticate().await,
			OauthBackendImpl::GenericWeb(backend) => backend.authenticate().await,
		}
	}

	fn user_agent(&self) -> &str {
		match self {
			OauthBackendImpl::OfficialApi(backend) => backend.user_agent(),
			OauthBackendImpl::MobileSpoof(backend) => backend.user_agent(),
			OauthBackendImpl::GenericWeb(backend) => backend.user_agent(),
		}
	}

	fn get_headers(&self) -> HashMap<String, String> {
		match self {
			OauthBackendImpl::OfficialApi(backend) => backend.get_headers(),
			OauthBackendImpl::MobileSpoof(backend) => backend.get_headers(),
			OauthBackendImpl::GenericWeb(backend) => backend.get_headers(),
		}
	}
}

// Spoofed client for Android devices
#[derive(Debug, Clone)]
pub struct Oauth {
	pub(crate) headers_map: HashMap<String, String>,
	expires_in: u64,
	pub(crate) backend: OauthBackendImpl,
}

impl Oauth {
	/// Create a new OAuth client
	pub(crate) async fn new() -> Self {
		// A registered Reddit app is used whenever its client ID is configured.
		// Otherwise keep the anonymous mobile client for compatibility.
		let mut backend = match RedditApiAuth::from_env() {
			Some(backend) => {
				info!("[🔑] Using the official Reddit API client");
				OauthBackendImpl::OfficialApi(backend)
			}
			None => OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new()),
		};

		let mut failure_count = 0;

		loop {
			let attempt = Self::new_with_timeout_with_backend(backend.clone()).await;
			match attempt {
				Ok(Ok(oauth)) => {
					info!("[✅] Successfully created OAuth client");
					return oauth;
				}
				Ok(Err(e)) => {
					let message = match e {
						AuthError::Wreq(error) => error.to_string(),
						AuthError::SerdeDeserialize(error) => error.to_string(),
						AuthError::Field((value, error)) => format!("{error}\n{value}"),
						AuthError::Http { status, body } => format!("Reddit returned HTTP {status}: {body}"),
					};
					error!("[⛔] Failed to create OAuth client: {message}. Retrying in 5 seconds...");
				}
				Err(_) => {
					error!("[⛔] Failed to create OAuth client before timeout. Retrying in 5 seconds...");
				}
			}

			failure_count += 1;

			// Switch to GenericWeb after 5 failures with MobileSpoof. Registered
			// app credentials are never replaced with a spoofed client: that
			// would hide a configuration error and run into Reddit's blocks.
			if matches!(backend, OauthBackendImpl::MobileSpoof(_)) && failure_count >= 5 {
				warn!("[🔄] MobileSpoofAuth failed 5 times. Falling back to GenericWebAuth...");
				backend = OauthBackendImpl::GenericWeb(GenericWebAuth::new());
			}

			// Crash after 10 total failures
			if failure_count >= 10 {
				error!("[⛔] Failed to create OAuth client");
				std::process::exit(1);
			}

			tokio::time::sleep(OAUTH_TIMEOUT).await;
		}
	}

	async fn new_with_timeout_with_backend(mut backend: OauthBackendImpl) -> Result<Result<Self, AuthError>, Elapsed> {
		timeout(OAUTH_TIMEOUT, async move {
			let response = backend.authenticate().await?;

			// Build headers_map from backend headers + Authorization header
			let mut headers_map = backend.get_headers();
			headers_map.insert("Authorization".to_owned(), format!("Bearer {}", response.token));
			headers_map.extend(response.additional_headers);

			Ok(Self {
				headers_map,
				expires_in: response.expires_in,
				backend,
			})
		})
		.await
	}

	pub fn user_agent(&self) -> &str {
		self.backend.user_agent()
	}

	/// Whether requests are authenticated with a registered Reddit app.
	pub fn official_api(&self) -> bool {
		matches!(self.backend, OauthBackendImpl::OfficialApi(_))
	}
}

#[derive(Debug)]
enum AuthError {
	Wreq(wreq::Error),
	SerdeDeserialize(serde_json::Error),
	Field((serde_json::Value, &'static str)),
	Http { status: u16, body: String },
}

impl From<wreq::Error> for AuthError {
	fn from(err: wreq::Error) -> Self {
		AuthError::Wreq(err)
	}
}

impl From<serde_json::Error> for AuthError {
	fn from(err: serde_json::Error) -> Self {
		AuthError::SerdeDeserialize(err)
	}
}

pub async fn token_daemon() {
	// Monitor for refreshing token
	loop {
		// Get expiry time - be sure to not hold the read lock
		let expires_in = { OAUTH_CLIENT.load_full().expires_in };

		// sleep for the expiry time minus 2 minutes
		let duration = Duration::from_secs(expires_in - 120);

		info!("[⏳] Waiting for {duration:?} seconds before refreshing OAuth token...");

		tokio::time::sleep(duration).await;

		info!("[⌛] {duration:?} Elapsed! Refreshing OAuth token...");

		// Refresh token - in its own scope
		{
			force_refresh_token().await;
		}
	}
}

pub async fn force_refresh_token() {
	if OAUTH_IS_ROLLING_OVER.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		trace!("Skipping refresh token roll over, already in progress");
		return;
	}

	trace!("Rolling over refresh token. Current rate limit: {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst));
	let new_client = Oauth::new().await;
	OAUTH_CLIENT.swap(new_client.into());
	OAUTH_RATELIMIT_REMAINING.store(99, Ordering::SeqCst);
	OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
}

#[derive(Debug, Clone, Default)]
struct Device {
	oauth_id: String,
	initial_headers: HashMap<String, String>,
	headers: HashMap<String, String>,
	user_agent: String,
}

// Official API backend - authenticates a Reddit app registered by the operator.
#[derive(Debug, Clone)]
pub struct RedditApiAuth {
	client_id: String,
	client_secret: Option<String>,
	device_id: String,
	user_agent: String,
}

impl RedditApiAuth {
	fn from_env() -> Option<Self> {
		let client_id = env_value(REDDIT_CLIENT_ID_ENV)?;
		let client_secret = env_value(REDDIT_CLIENT_SECRET_ENV);
		let user_agent = env_value(REDDIT_USER_AGENT_ENV).unwrap_or_else(|| default_user_agent(&client_id));

		info!(
			"[🔑] Using registered Reddit app \"{client_id}\" with {} authentication",
			if client_secret.is_some() { "client credentials" } else { "installed app" }
		);

		Some(Self {
			client_id,
			client_secret,
			device_id: random_device_id(),
			user_agent,
		})
	}

	fn token_request_body(&self) -> String {
		match self.client_secret {
			// A "script" app has a secret and uses the application-only flow.
			Some(_) => "grant_type=client_credentials".to_owned(),
			// An "installed app" has no secret and identifies the device instead.
			None => format!("grant_type=https%3A%2F%2Foauth.reddit.com%2Fgrants%2Finstalled_client&device_id={}", self.device_id),
		}
	}
}

impl OauthBackend for RedditApiAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		let credentials = format!("{}:{}", self.client_id, self.client_secret.as_deref().unwrap_or_default());
		let auth = general_purpose::STANDARD.encode(credentials);

		trace!("Requesting a Reddit API token...");
		let response = CLIENT
			.post(REDDIT_API_TOKEN_ENDPOINT)
			.header("User-Agent", &self.user_agent)
			.header("Authorization", format!("Basic {auth}"))
			.header("Content-Type", "application/x-www-form-urlencoded")
			.header("Accept", "application/json")
			.body(self.token_request_body())
			.send()
			.await?;

		let status = response.status();
		let body = response.text().await?;

		if !status.is_success() {
			return Err(AuthError::Http {
				status: status.as_u16(),
				body: truncate_for_log(&body, 300),
			});
		}

		let json: serde_json::Value = serde_json::from_str(&body)?;
		let token = json
			.get("access_token")
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token")))?
			.as_str()
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token: as_str")))?
			.to_owned();
		let expires_in = json
			.get("expires_in")
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in")))?
			.as_u64()
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in: as_u64")))?;

		info!("[✅] Reddit API token acquired, expires in {expires_in} seconds");

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: HashMap::new(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		HashMap::from([("User-Agent".to_owned(), self.user_agent.clone())])
	}
}

fn env_value(key: &str) -> Option<String> {
	env::var(key).ok().map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
}

fn default_user_agent(client_id: &str) -> String {
	format!("web:{client_id}:{} (self-hosted)", env!("CARGO_PKG_VERSION"))
}

fn random_device_id() -> String {
	const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
	(0..30)
		.map(|_| {
			let idx = fastrand::usize(..CHARS.len());
			CHARS[idx] as char
		})
		.collect()
}

fn truncate_for_log(body: &str, max_chars: usize) -> String {
	let mut truncated: String = body.chars().take(max_chars).collect();
	if body.chars().count() > max_chars {
		truncated.push('…');
	}
	truncated
}

// MobileSpoofAuth backend - spoofs an Android mobile device
#[derive(Debug, Clone)]
pub struct MobileSpoofAuth {
	device: Device,
	additional_headers: HashMap<String, String>,
}

impl MobileSpoofAuth {
	fn new() -> Self {
		Self {
			device: Device::new(),
			additional_headers: HashMap::new(),
		}
	}
}

impl OauthBackend for MobileSpoofAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = format!("{AUTH_ENDPOINT}/auth/v2/oauth/access-token/loid");
		let mut builder = CLIENT.post(&url);

		// Add headers from spoofed client
		for (key, value) in &self.device.initial_headers {
			builder = builder.header(key, value);
		}
		// Set up HTTP Basic Auth - basically just the const OAuth ID's with no password,
		// Base64-encoded. https://en.wikipedia.org/wiki/Basic_access_authentication
		// This could be constant, but I don't think it's worth it. OAuth ID's can change
		// over time and we want to be flexible.
		let auth = general_purpose::STANDARD.encode(format!("{}:", self.device.oauth_id));
		builder = builder.header("Authorization", format!("Basic {auth}"));

		// Set JSON body. I couldn't tell you what this means. But that's what the client sends
		let json = json!({
				"scopes": ["*","email", "pii"]
		});

		trace!("Sending token request to {url}...");

		// Send request
		let resp = builder.json(&json).send().await?;

		trace!("Received response with status {} and length {:?}", resp.status(), resp.headers().get("content-length"));
		trace!("OAuth headers: {:#?}", resp.headers());

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers().get("x-reddit-loid") {
			let header_val: &wreq::header::HeaderValue = header;
			self.additional_headers.insert("x-reddit-loid".to_owned(), header_val.to_str().unwrap().to_string());
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers().get("x-reddit-session") {
			let header_val: &wreq::header::HeaderValue = header;
			self.additional_headers.insert("x-reddit-session".to_owned(), header_val.to_str().unwrap().to_string());
		}

		trace!("Serializing response...");

		// Serialize response
		let json: serde_json::Value = resp.json().await?;

		trace!("Accessing relevant fields...");

		// Save token and expiry
		let token = json
			.get("access_token")
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token")))?
			.as_str()
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token: as_str")))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in")))?
			.as_u64()
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in: as_u64")))?;

		info!("[✅] Success - Retrieved token \"{}...\", expires in {}", &token[..32], expires_in);

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.device.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		let mut headers = self.device.headers.clone();
		headers.extend(self.additional_headers.clone());
		headers
	}
}

// GenericWebAuth backend - simple web-based authentication
#[derive(Debug, Clone)]
pub struct GenericWebAuth {
	device_id: String,
	user_agent: String,
	additional_headers: HashMap<String, String>,
}

impl GenericWebAuth {
	fn new() -> Self {
		// Generate random 20-character alphanumeric device_id
		let device_id: String = (0..20)
			.map(|_| {
				let idx = fastrand::usize(..62);
				let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
				chars[idx] as char
			})
			.collect();

		info!("[🔄] Using GenericWebAuth with device_id: \"{device_id}\"");

		Self {
			device_id,
			user_agent: fake_user_agent::get_rua().to_owned(),
			additional_headers: HashMap::new(),
		}
	}
}

impl OauthBackend for GenericWebAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = "https://www.reddit.com/api/v1/access_token";
		let mut builder = CLIENT.post(url);

		// Add minimal headers
		builder = builder.header("Host", "www.reddit.com");
		builder = builder.header("User-Agent", &self.user_agent);
		builder = builder.header("Accept", "*/*");
		builder = builder.header("Accept-Language", "en-US,en;q=0.5");
		// builder = builder.header("Accept-Encoding", "gzip, deflate, br, zstd");
		builder = builder.header("Authorization", "Basic M1hmQkpXbGlIdnFBQ25YcmZJWWxMdzo=");
		builder = builder.header("Content-Type", "application/x-www-form-urlencoded");
		builder = builder.header("Sec-GPC", "1");
		builder = builder.header("Connection", "keep-alive");

		// Set up form body
		let body_str = format!("grant_type=https%3A%2F%2Foauth.reddit.com%2Fgrants%2Finstalled_client&device_id={}", self.device_id);

		trace!("Sending GenericWebAuth token request to {url}...");

		// Send request
		let resp: wreq::Response = builder.body(body_str).send().await?;

		trace!("Received response with status {} and length {:?}", resp.status(), resp.headers().get("content-length"));
		trace!("GenericWebAuth headers: {:#?}", resp.headers());

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers().get("x-reddit-loid") {
			let header_val: &wreq::header::HeaderValue = header;
			self.additional_headers.insert("x-reddit-loid".to_owned(), header_val.to_str().unwrap().to_string());
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers().get("x-reddit-session") {
			let header_val: &wreq::header::HeaderValue = header;
			self.additional_headers.insert("x-reddit-session".to_owned(), header_val.to_str().unwrap().to_string());
		}

		trace!("Serializing GenericWebAuth response...");

		// Serialize response
		let json: serde_json::Value = resp.json().await?;

		trace!("Accessing relevant fields...");

		// Parse response - access_token, token_type, device_id, expires_in, scope
		let token = json
			.get("access_token")
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token")))?
			.as_str()
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token: as_str")))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in")))?
			.as_u64()
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in: as_u64")))?;

		info!(
			"[✅] GenericWebAuth success - Retrieved token \"{}...\", expires in {}",
			&token[..32.min(token.len())],
			expires_in
		);

		// Insert a few necessary headers
		self.additional_headers.insert("Origin".to_owned(), "https://www.reddit.com".to_owned());
		self.additional_headers.insert("User-Agent".to_owned(), self.user_agent.to_owned());

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		self.additional_headers.clone()
	}
}

impl Device {
	fn android() -> Self {
		// Generate uuid
		let uuid = uuid::Uuid::new_v4().to_string();

		// Generate random user-agent
		let android_app_version = choose(ANDROID_APP_VERSION_LIST).to_string();
		let android_version = fastrand::u8(9..=14);

		let android_user_agent = format!("Reddit/{android_app_version}/Android {android_version}");

		let qos = fastrand::u32(1000..=100_000);
		let qos: f32 = qos as f32 / 1000.0;
		let qos = format!("{qos:.3}");

		let codecs = TextGenerator::new().generate("available-codecs=video/avc, video/hevc{, video/x-vnd.on2.vp9|}");

		// Android device headers
		let headers: HashMap<String, String> = HashMap::from([
			("User-Agent".into(), android_user_agent.clone()),
			("x-reddit-retry".into(), "algo=no-retries".into()),
			("x-reddit-compression".into(), "1".into()),
			("x-reddit-qos".into(), qos),
			("x-reddit-media-codecs".into(), codecs),
			("Content-Type".into(), "application/json; charset=UTF-8".into()),
			("client-vendor-id".into(), uuid.clone()),
			("X-Reddit-Device-Id".into(), uuid.clone()),
		]);

		info!("[🔄] Spoofing Android client with headers: {headers:?}, uuid: \"{uuid}\", and OAuth ID \"{REDDIT_ANDROID_OAUTH_CLIENT_ID}\"");

		Self {
			oauth_id: REDDIT_ANDROID_OAUTH_CLIENT_ID.to_string(),
			headers: headers.clone(),
			initial_headers: headers,
			user_agent: android_user_agent,
		}
	}
	fn new() -> Self {
		// See https://github.com/redlib-org/redlib/issues/8
		Self::android()
	}
}

fn choose<T: Copy>(list: &[T]) -> T {
	*fastrand::choose_multiple(list.iter(), 1)[0]
}

#[cfg(test)]
mod tests {
	use super::*;
	use sealed_test::prelude::*;

	#[test]
	#[sealed_test(env = [("REDLIB_REDDIT_CLIENT_ID", "test-client"), ("REDLIB_REDDIT_CLIENT_SECRET", "test-secret")])]
	fn test_official_auth_script_app() {
		let auth = RedditApiAuth::from_env().expect("client ID should enable the official backend");

		assert_eq!(auth.token_request_body(), "grant_type=client_credentials");
		assert_eq!(auth.user_agent(), default_user_agent("test-client"));
		assert!(!auth.user_agent().contains("Android"));
		assert_eq!(auth.get_headers().get("User-Agent").map(String::as_str), Some(default_user_agent("test-client").as_str()));
	}

	#[test]
	#[sealed_test(env = [("REDLIB_REDDIT_CLIENT_ID", "test-client"), ("REDLIB_REDDIT_CLIENT_SECRET", "")])]
	fn test_official_auth_installed_app() {
		let auth = RedditApiAuth::from_env().expect("client ID should enable the official backend");

		assert!(auth
			.token_request_body()
			.starts_with("grant_type=https%3A%2F%2Foauth.reddit.com%2Fgrants%2Finstalled_client"));
		assert!(auth.token_request_body().contains("&device_id="));
		assert_eq!(auth.device_id.len(), 30);
	}

	#[test]
	#[sealed_test(env = [("REDLIB_REDDIT_CLIENT_ID", ""), ("REDLIB_REDDIT_USER_AGENT", "test-agent/1.0")])]
	fn test_official_auth_requires_client_id() {
		assert!(RedditApiAuth::from_env().is_none());
	}

	#[test]
	#[sealed_test(env = [("REDLIB_REDDIT_CLIENT_ID", "test-client"), ("REDLIB_REDDIT_USER_AGENT", "test-agent/1.0")])]
	fn test_official_auth_user_agent_override() {
		let auth = RedditApiAuth::from_env().unwrap();
		assert_eq!(auth.user_agent(), "test-agent/1.0");
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_mobile_spoof_backend() {
		// Test MobileSpoofAuth backend specifically
		let mut backend = MobileSpoofAuth::new();
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
		assert!(!backend.get_headers().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "todolol"]
	async fn test_generic_web_backend() {
		// Test GenericWebAuth backend specifically
		let mut backend = GenericWebAuth::new();
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_client() {
		// Integration test - tests the overall Oauth client
		assert!(OAUTH_CLIENT.load_full().headers_map.contains_key("Authorization"));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_client_refresh() {
		force_refresh_token().await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_token_exists() {
		let client = OAUTH_CLIENT.load_full();
		let auth_header = client.headers_map.get("Authorization").unwrap();
		assert!(auth_header.starts_with("Bearer "));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_headers_len() {
		assert!(OAUTH_CLIENT.load_full().headers_map.len() >= 3);
	}

	#[test]
	fn test_creating_device() {
		Device::new();
	}

	#[test]
	fn test_creating_backends() {
		// Test that both backends can be created
		MobileSpoofAuth::new();
		GenericWebAuth::new();
	}
}
