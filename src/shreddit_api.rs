//! Translate Reddit's logged-out web frontend into the JSON shapes Redlib expects.
//!
//! Reddit blocks the anonymous OAuth API for listings, post pages and comments.
//! The same data is still served to logged-out browsers: feed/listing routes
//! under `/svc/shreddit/` return HTML fragments, and a thread page contains its
//! `<shreddit-post>` and comment tree. This module maps the routes Redlib calls
//! onto those pages and rebuilds enough of the old JSON to keep the templates
//! and parsers unchanged.

use crate::client::{shreddit_fragment, shreddit_page};
use crate::shreddit;
use log::warn;
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static POST: LazyLock<Selector> = LazyLock::new(|| Selector::parse("shreddit-post[id]").unwrap());
static POST_FLAIR: LazyLock<Selector> = LazyLock::new(|| Selector::parse("shreddit-post-flair").unwrap());
static PLAYER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("shreddit-player[src]").unwrap());
static GALLERY: LazyLock<Selector> = LazyLock::new(|| Selector::parse("gallery-carousel").unwrap());
static FIGURE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("figure").unwrap());
static IMG: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img").unwrap());
static PRIMARY_IMG: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img[data-post-media-primary]").unwrap());
static NEXT_PARTIAL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"faceplate-partial[slot="load-after"]"#).unwrap());
static CANONICAL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#canonical-url-updater").unwrap());
static SUB_HEADER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("shreddit-subreddit-header").unwrap());
static SUB_ICON: LazyLock<Selector> = LazyLock::new(|| Selector::parse("shreddit-subreddit-header img").unwrap());
static USER_NAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[data-testid="nameplate"]"#).unwrap());
static USER_AVATAR: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"img[alt$="avatar"]"#).unwrap());
static CAKE_DAY: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[data-testid="cake-day"]"#).unwrap());
static KARMA: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[data-testid="karma-number"]"#).unwrap());
static USER_DESCRIPTION: LazyLock<Selector> = LazyLock::new(|| Selector::parse("span.mt-md.whitespace-normal").unwrap());
static SEARCH_TRACKER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("search-telemetry-tracker[data-faceplate-tracking-context]").unwrap());
static NUMBER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("faceplate-number[number]").unwrap());
static TIMEAGO: LazyLock<Selector> = LazyLock::new(|| Selector::parse("faceplate-timeago[ts]").unwrap());

/// Returns `Some` when the path is served from Reddit's web frontend.
pub async fn json(path: String) -> Option<Result<Value, String>> {
	let (route, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
	let route = route.strip_suffix(".json").unwrap_or(route);
	let segments: Vec<&str> = route.split('/').filter(|segment| !segment.is_empty()).collect();
	let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();

	match segments.first().copied()? {
		"r" => route_subreddit(&segments, &params).await,
		"comments" if segments.len() >= 2 => Some(thread_json("", segments[1], &segments[2..], &params).await),
		"user" => route_user(&segments, &params).await,
		"search" => Some(search_json(&params, None).await),
		"subreddits" if segments.get(1) == Some(&"search") => Some(Ok(empty_listing())),
		_ => None,
	}
}

async fn route_subreddit(segments: &[&str], params: &HashMap<String, String>) -> Option<Result<Value, String>> {
	if segments.len() < 3 {
		return None;
	}

	// Redlib encodes the `+` in multi-community paths as `%2B`.
	let sub = percent_encoding::percent_decode_str(segments[1]).decode_utf8_lossy().into_owned();
	let sub = sub.as_str();
	match segments[2] {
		"comments" if segments.len() >= 4 => Some(thread_json(sub, segments[3], &segments[4..], params).await),
		"about" if segments.len() == 3 => Some(subreddit_json(sub).await),
		"search" if segments.len() == 3 => Some(search_json(params, Some(sub)).await),
		// /r/{sub}/wiki/... is not translated yet; let the normal path fail
		// instead of pretending the page does not exist.
		"wiki" => None,
		sort if segments.len() == 3 => {
			let (path_sort, feed_sort) = web_sort(sort)?;
			Some(listing_json(sub, path_sort, feed_sort, params).await)
		}
		_ => None,
	}
}

async fn route_user(segments: &[&str], params: &HashMap<String, String>) -> Option<Result<Value, String>> {
	if segments.len() < 2 {
		return None;
	}

	if segments.get(2) == Some(&"about") {
		return Some(user_json(segments[1]).await);
	}

	if segments.len() != 3 {
		return None;
	}

	let profile = match segments[2] {
		"overview" => "profile_overview-more-posts",
		"submitted" => "profile_posts-more-posts",
		// The comments feed needs one extra request per comment for its body.
		// Returning an empty listing keeps the rest of the profile usable.
		"comments" => return Some(Ok(empty_listing())),
		_ => return None,
	};

	Some(user_listing(segments[1], profile, params).await)
}

fn web_sort(sort: &str) -> Option<(&'static str, &'static str)> {
	match sort {
		"hot" => Some(("hot", "HOT")),
		"new" => Some(("new", "NEW")),
		"top" => Some(("top", "TOP")),
		"rising" => Some(("rising", "RISING")),
		"controversial" => Some(("controversial", "CONTROVERSIAL")),
		"best" => Some(("hot", "BEST")),
		_ => None,
	}
}

fn web_time(time: &str) -> &'static str {
	match time {
		"hour" => "HOUR",
		"day" => "DAY",
		"week" => "WEEK",
		"month" => "MONTH",
		"year" => "YEAR",
		"all" => "ALL",
		_ => "DAY",
	}
}

fn encode_query(pairs: &[(&str, &str)]) -> String {
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	for (key, value) in pairs {
		serializer.append_pair(key, value);
	}
	serializer.finish()
}

async fn listing_json(sub: &str, path_sort: &str, feed_sort: &str, params: &HashMap<String, String>) -> Result<Value, String> {
	let time = params.get("t").map(|time| web_time(time));

	// Reddit's community-more-posts partial does not support combined feeds,
	// so fetch each community and interleave the first page of each.
	if sub.contains('+') {
		let mut buckets: Vec<Vec<Value>> = Vec::new();
		for part in sub.split('+').filter(|part| !part.is_empty()).take(8) {
			let mut pairs: Vec<(&str, &str)> = vec![("name", part)];
			if let Some(time) = time {
				pairs.push(("t", time));
			}
			let endpoint = format!("/svc/shreddit/community-more-posts/{path_sort}/?{}", encode_query(&pairs));
			let Ok(html) = shreddit_fragment(endpoint, None).await else {
				continue;
			};
			let Ok((children, _)) = listing_page(&html) else {
				continue;
			};
			buckets.push(children);
		}

		let mut children = Vec::new();
		let mut seen = HashSet::new();
		for index in 0..buckets.iter().map(Vec::len).max().unwrap_or_default() {
			for bucket in &buckets {
				let Some(child) = bucket.get(index) else {
					continue;
				};
				let id = child["data"]["name"].as_str().unwrap_or_default().to_owned();
				if seen.insert(id) {
					children.push(child.clone());
				}
			}
		}

		return Ok(json!({
			"kind": "Listing",
			"data": {
				"children": children,
				"after": "",
				"dist": children.len(),
			}
		}));
	}

	let mut after = params.get("after").cloned().filter(|after| !after.is_empty());
	let mut children: Vec<Value> = Vec::new();
	let mut seen = HashSet::new();
	let mut pages = 0;

	// The popular feed serves only a few posts per partial. Keep pulling
	// continuations until a normal page is full, the feed ends, or the cap is hit.
	loop {
		let mut pairs: Vec<(&str, &str)> = Vec::new();
		if sub == "popular" || sub == "all" {
			pairs.push(("sort", feed_sort));
		} else {
			pairs.push(("name", sub));
		}
		if let Some(time) = time {
			pairs.push(("t", time));
		}
		if let Some(after) = after.as_deref() {
			pairs.push(("after", after));
		}
		let query = encode_query(&pairs);
		let endpoint = if sub == "popular" || sub == "all" {
			format!("/svc/shreddit/feeds/popular-feed?{query}")
		} else {
			format!("/svc/shreddit/community-more-posts/{path_sort}/?{query}")
		};

		let html = match shreddit_fragment(endpoint, None).await {
			Ok(html) => html,
			Err(error) if !children.is_empty() => {
				warn!("Stopping listing pagination after {pages} pages: {error}");
				break;
			}
			Err(error) => return Err(error),
		};

		let (page_children, next_after) = match listing_page(&html) {
			Ok(page) => page,
			Err(error) if !children.is_empty() => {
				warn!("Stopping listing pagination after {pages} pages: {error}");
				break;
			}
			Err(error) => return Err(error),
		};

		for child in page_children {
			let id = child["data"]["name"].as_str().unwrap_or_default().to_owned();
			if seen.insert(id) {
				children.push(child);
			}
		}

		after = next_after;
		pages += 1;
		if children.len() >= 20 || after.is_none() || pages >= 3 {
			break;
		}
	}

	Ok(json!({
		"kind": "Listing",
		"data": {
			"children": children,
			"after": after.unwrap_or_default(),
			"dist": children.len(),
		}
	}))
}

fn listing_page(html: &str) -> Result<(Vec<Value>, Option<String>), String> {
	let doc = Html::parse_fragment(html);
	let mut children = Vec::new();
	let mut seen = HashSet::new();
	let nsfw_page = html.contains("subreddit-nsfw=\"true\"");

	for element in doc.select(&POST) {
		let id = attribute(element, "id");
		let title = attribute(element, "post-title");
		if !id.starts_with("t3_") || title.is_empty() || !seen.insert(id.to_owned()) {
			continue;
		}
		children.push(json!({
			"kind": "t3",
			"data": post_value(element, nsfw_page),
		}));
	}

	if children.is_empty() && (html.contains("shreddit-forbidden") || html.contains("Page not found")) {
		return Err("Community not found or unavailable".to_owned());
	}

	Ok((children, next_cursor(&doc)))
}

fn next_cursor(doc: &Html) -> Option<String> {
	let src = doc.select(&NEXT_PARTIAL).next()?.value().attr("src")?;
	let query = src.split_once('?')?.1;
	url::form_urlencoded::parse(query.as_bytes()).find_map(|(key, value)| (key == "after").then(|| value.into_owned()))
}

async fn thread_json(sub: &str, post_id: &str, rest: &[&str], params: &HashMap<String, String>) -> Result<Value, String> {
	let slug = rest.first().copied().filter(|slug| !slug.is_empty() && *slug != "_").unwrap_or("");
	let comment_id = rest.get(1).copied().or_else(|| params.get("comment").map(String::as_str)).filter(|id| !id.is_empty());

	let post = post_json(sub, post_id, slug).await?;
	let actual_sub = post["subreddit"].as_str().unwrap_or(sub).to_owned();
	let sort = params.get("sort").cloned().unwrap_or_default();
	let context = params.get("context").cloned();
	let comments = shreddit::comments(actual_sub, post_id.to_owned(), comment_id.map(str::to_owned), sort, context).await?;

	Ok(json!([
		{"kind": "Listing", "data": {"children": [{"kind": "t3", "data": post}]}},
		comments,
	]))
}

async fn post_json(sub: &str, post_id: &str, slug: &str) -> Result<Value, String> {
	let mut path = if sub.is_empty() {
		format!("/comments/{post_id}/")
	} else {
		format!("/r/{sub}/comments/{post_id}/")
	};
	if !slug.is_empty() {
		path.push_str(slug);
		path.push('/');
	}
	path.push_str("?render-mode=partial");

	let mut html = shreddit_page(path).await?;
	for _ in 0..2 {
		if html.contains(&format!("id=\"t3_{post_id}\"")) {
			break;
		}
		let Some(canonical) = canonical_path(&html) else {
			break;
		};
		html = shreddit_page(canonical).await?;
	}

	let doc = Html::parse_document(&html);
	let selector = Selector::parse(&format!("shreddit-post#t3_{post_id}")).unwrap();
	let post = doc.select(&selector).next().ok_or_else(|| "Post not found".to_owned())?;
	Ok(post_value(post, html.contains("subreddit-nsfw=\"true\"")))
}

fn canonical_path(html: &str) -> Option<String> {
	let doc = Html::parse_document(html);
	let value = doc.select(&CANONICAL).next()?.value().attr("value")?;
	let path = value.strip_prefix("https://www.reddit.com").or_else(|| value.strip_prefix("https://reddit.com"))?;
	let separator = if path.contains('?') { '&' } else { '?' };
	Some(format!("{path}{separator}render-mode=partial"))
}

fn attribute<'a>(element: ElementRef<'a>, name: &str) -> &'a str {
	element.value().attr(name).unwrap_or_default()
}

fn number(value: &str) -> i64 {
	value.parse().unwrap_or_default()
}

fn number_attr(element: ElementRef<'_>, name: &str) -> i64 {
	element.value().attr(name).and_then(|value| value.parse().ok()).unwrap_or_default()
}

fn media_id(url: &str) -> String {
	let path = url.split('?').next().unwrap_or(url);
	let name = path.rsplit('/').next().unwrap_or_default();
	name.split('.').next().unwrap_or_default().to_owned()
}

struct MediaFields {
	post_hint: String,
	url: String,
	preview: Value,
	secure_media: Value,
	is_gallery: bool,
	gallery_data: Value,
	media_metadata: Value,
	thumbnail: String,
	thumbnail_width: i64,
	thumbnail_height: i64,
	is_reddit_media_domain: bool,
}

impl Default for MediaFields {
	fn default() -> Self {
		Self {
			post_hint: String::new(),
			url: String::new(),
			preview: json!({}),
			secure_media: Value::Null,
			is_gallery: false,
			gallery_data: Value::Null,
			media_metadata: Value::Null,
			thumbnail: String::new(),
			thumbnail_width: 0,
			thumbnail_height: 0,
			is_reddit_media_domain: false,
		}
	}
}

fn media_fields(element: ElementRef<'_>, post_type: &str, domain: &str, content_href: &str) -> MediaFields {
	let mut fields = MediaFields::default();

	if let Some(player) = element.select(&PLAYER).next() {
		let src = attribute(player, "src");
		if !src.is_empty() {
			let poster = attribute(player, "poster");
			let is_gif = player.value().attr("gif").is_some();
			let video = json!({
				"fallback_url": src,
				"hls_url": src,
				"is_gif": is_gif,
				"width": 0,
				"height": 0,
			});
			fields.secure_media = json!({"reddit_video": video.clone()});
			fields.preview = json!({
				"reddit_video_preview": video,
				"images": [{"source": {"url": poster, "width": 0, "height": 0}}],
				"enabled": true,
			});
			fields.url = if content_href.is_empty() { src.to_owned() } else { content_href.to_owned() };
			fields.thumbnail = poster.to_owned();
			fields.post_hint = "hosted:video".to_owned();
			fields.is_reddit_media_domain = domain == "v.redd.it";
			return fields;
		}
	}

	let gallery = element.select(&GALLERY).next();
	if post_type == "gallery" || gallery.is_some() {
		fields.is_gallery = true;
		fields.url = content_href.to_owned();

		if let Some(gallery) = gallery {
			let mut items = Vec::new();
			let mut metadata = Map::new();

			for (index, figure) in gallery.select(&FIGURE).enumerate() {
				let mut full = String::new();
				let mut preview = String::new();
				let mut width = 0;
				let mut height = 0;

				for image in figure.select(&IMG) {
					let src = attribute(image, "src");
					let lazy = attribute(image, "data-lazy-src");
					let candidate = if src.is_empty() { lazy } else { src };
					if candidate.contains("i.redd.it/") {
						full = candidate.to_owned();
					} else if candidate.contains("preview.redd.it/") && preview.is_empty() {
						preview = candidate.to_owned();
					}
					if let Some(value) = image.value().attr("width").and_then(|value| value.parse().ok()) {
						width = value;
					}
					if let Some(value) = image.value().attr("height").and_then(|value| value.parse().ok()) {
						height = value;
					}
				}

				let url = if full.is_empty() { preview } else { full };
				if url.is_empty() {
					continue;
				}
				let id = media_id(&url);
				if id.is_empty() {
					continue;
				}
				if fields.thumbnail.is_empty() {
					fields.thumbnail = url.clone();
				}
				items.push(json!({"media_id": id, "id": index, "caption": ""}));
				metadata.insert(
					id,
					json!({
						"m": if url.to_lowercase().ends_with(".gif") { "image/gif" } else { "image/jpeg" },
						"s": {"u": url, "x": width, "y": height},
						"status": "valid",
					}),
				);
			}

			fields.gallery_data = json!({"items": items});
			fields.media_metadata = Value::Object(metadata);
		}

		return fields;
	}

	if post_type == "image" || domain == "i.redd.it" {
		let mut image_url = content_href.to_owned();
		if image_url.is_empty() || !image_url.contains("i.redd.it/") {
			for image in element.select(&IMG) {
				let src = attribute(image, "src");
				if src.contains("i.redd.it/") {
					image_url = src.to_owned();
					break;
				}
			}
		}

		let mut preview_element = element.select(&PRIMARY_IMG).next();
		if preview_element.is_none() {
			for image in element.select(&IMG) {
				if attribute(image, "src").contains("preview.redd.it/") {
					preview_element = Some(image);
					break;
				}
			}
		}

		let (preview_url, width, height) = preview_element
			.map(|image| (attribute(image, "src").to_owned(), number_attr(image, "width"), number_attr(image, "height")))
			.unwrap_or_else(|| (image_url.clone(), 0, 0));

		fields.post_hint = "image".to_owned();
		fields.url = if image_url.is_empty() { preview_url.clone() } else { image_url };
		fields.preview = json!({
			"images": [{"source": {"url": preview_url, "width": width, "height": height}}],
			"enabled": true,
		});
		fields.thumbnail = preview_url;
		fields.thumbnail_width = width;
		fields.thumbnail_height = height;
		fields.is_reddit_media_domain = true;
		return fields;
	}

	fields.url = content_href.to_owned();
	fields
}

fn post_value(element: ElementRef<'_>, nsfw_page: bool) -> Value {
	let id = attribute(element, "id").strip_prefix("t3_").unwrap_or_default().to_owned();
	let post_type = attribute(element, "post-type");
	let domain = attribute(element, "domain");
	let content_href = attribute(element, "content-href");
	let is_self = domain.starts_with("self.") || post_type == "text";
	let media = media_fields(element, post_type, domain, content_href);

	let body_selector = Selector::parse(&format!("#t3_{id}-post-rtjson-content")).unwrap();
	let body = element.select(&body_selector).next();
	let (body_html, body_text) = match body {
		Some(body) => (format!("<div class=\"md\">{}</div>", body.inner_html()), body.text().collect::<String>()),
		None => (String::new(), String::new()),
	};

	let flair = element
		.select(&POST_FLAIR)
		.next()
		.map(|flair| flair.text().collect::<String>().trim().to_owned())
		.unwrap_or_default();
	let created = crate::shreddit::unix_time(attribute(element, "created-timestamp")).unwrap_or_default();
	let nsfw = nsfw_page || attribute(element, "over-18") == "true" || attribute(element, "nsfw") == "true" || attribute(element, "is-nsfw") == "true";
	let spoiler = attribute(element, "spoiler") == "true" || attribute(element, "is-spoiler") == "true";
	let stickied = attribute(element, "stickied") == "true" || attribute(element, "pinned") == "true" || attribute(element, "is-stickied") == "true";
	let permalink = attribute(element, "permalink").to_owned();

	json!({
		"id": id,
		"name": format!("t3_{id}"),
		"title": attribute(element, "post-title"),
		"subreddit": attribute(element, "subreddit-name"),
		"subreddit_id": attribute(element, "subreddit-id"),
		"subreddit_name_prefixed": attribute(element, "subreddit-prefixed-name"),
		"subreddit_type": "public",
		"author": attribute(element, "author"),
		"author_id": attribute(element, "author-id"),
		"author_fullname": attribute(element, "author-id"),
		"permalink": permalink,
		"url": if is_self { permalink.clone() } else { media.url.clone() },
		"url_overridden_by_dest": if is_self { Value::Null } else { json!(content_href) },
		"domain": domain,
		"score": number(attribute(element, "score")),
		"ups": number(attribute(element, "score")),
		"downs": 0,
		"upvote_ratio": attribute(element, "upvote-ratio").parse::<f64>().unwrap_or(1.0),
		"num_comments": number(attribute(element, "comment-count")),
		"created": created,
		"created_utc": created,
		"over_18": nsfw,
		"spoiler": spoiler,
		"stickied": stickied,
		"pinned": stickied,
		"is_self": is_self,
		"is_gallery": media.is_gallery,
		"is_video": media.post_hint == "hosted:video",
		"is_reddit_media_domain": media.is_reddit_media_domain,
		"post_hint": media.post_hint,
		"preview": media.preview,
		"secure_media": media.secure_media,
		"media": media.secure_media,
		"gallery_data": media.gallery_data,
		"media_metadata": media.media_metadata,
		"thumbnail": media.thumbnail,
		"thumbnail_width": media.thumbnail_width,
		"thumbnail_height": media.thumbnail_height,
		"selftext": body_text,
		"selftext_html": body_html,
		"body_html": body_html,
		"link_flair_text": flair,
		"link_flair_type": if flair.is_empty() { "" } else { "text" },
		"link_flair_richtext": [],
		"link_flair_background_color": "",
		"link_flair_text_color": "",
		"author_flair_type": "text",
		"author_flair_richtext": [],
		"author_flair_text": Value::Null,
		"author_flair_background_color": Value::Null,
		"author_flair_text_color": Value::Null,
		"distinguished": "",
		"hide_score": false,
		"num_duplicates": 0,
		"poll_data": Value::Null,
		"all_awardings": [],
		"awarders": [],
		"total_awards_received": 0,
		"gilded": 0,
		"websocket_url": "",
		"removed_by_category": Value::Null,
		"crosspost_parent_list": [],
		"link_title": "",
		"quarantine": false,
		"locked": false,
		"archived": false,
		"edited": false,
		"visited": false,
		"clicked": false,
		"hidden": false,
		"saved": false,
		"view_count": Value::Null,
		"likes": Value::Null,
		"num_reports": Value::Null,
		"report_reasons": Value::Null,
		"mod_reports": [],
		"user_reports": [],
		"treatment_tags": [],
		"subreddit_subscribers": 0,
		"num_crossposts": 0,
		"allow_live_comments": false,
		"is_crosspostable": false,
		"is_created_from_ads_ui": false,
		"is_original_content": false,
		"is_robot_indexable": true,
		"is_meta": false,
		"is_awardable": false,
		"is_embeddable": true,
		"media_only": false,
		"contest_mode": false,
		"can_mod_post": false,
		"send_replies": true,
		"no_follow": false,
		"gildings": {},
		"media_embed": {},
		"secure_media_embed": {},
	})
}

async fn user_listing(user: &str, profile: &str, params: &HashMap<String, String>) -> Result<Value, String> {
	let mut pairs: Vec<(&str, &str)> = vec![("name", user)];
	if let Some(sort) = params.get("sort") {
		pairs.push(("sort", sort));
	}
	if let Some(time) = params.get("t") {
		pairs.push(("t", web_time(time)));
	}
	if let Some(after) = params.get("after") {
		if !after.is_empty() {
			pairs.push(("after", after));
		}
	}
	let endpoint = format!("/svc/shreddit/profiles/{profile}/new/?{}", encode_query(&pairs));
	let html = shreddit_fragment(endpoint, None).await?;
	let (children, after) = listing_page(&html)?;
	Ok(json!({
		"kind": "Listing",
		"data": {
			"children": children,
			"after": after.unwrap_or_default(),
			"dist": children.len(),
		}
	}))
}

async fn subreddit_json(sub: &str) -> Result<Value, String> {
	let html = shreddit_fragment(
		format!(
			"/svc/shreddit/feeds/subreddit-right-rail?name={}",
			url::form_urlencoded::byte_serialize(sub.as_bytes()).collect::<String>()
		),
		None,
	)
	.await?;

	// Keep scraper values out of the async state so the future stays `Send`.
	let (name, title, description, weekly_active, mut icon, nsfw) = {
		let doc = Html::parse_fragment(&html);
		let header = doc.select(&SUB_HEADER).next().ok_or_else(|| "Community not found".to_owned())?;
		let name = attribute(header, "name").to_owned();
		let title = attribute(header, "display-name").to_owned();
		let description = attribute(header, "description").to_owned();
		let weekly_active = number(attribute(header, "weekly-active-users"));
		let icon = header.select(&SUB_ICON).next().map(|image| attribute(image, "src").to_owned()).unwrap_or_default();
		(name, title, description, weekly_active, icon, html.contains("subreddit-nsfw=\"true\""))
	};

	if icon.is_empty() {
		if let Ok(card) = shreddit_fragment(format!("/svc/shreddit/community-hover-card/{name}"), None).await {
			let card = Html::parse_fragment(&card);
			if let Some(image) = card.select(&IMG).next() {
				icon = attribute(image, "src").to_owned();
			}
		}
	}

	Ok(json!({
		"kind": "t5",
		"data": {
			"display_name": if name.is_empty() { sub.to_owned() } else { name },
			"title": title,
			"public_description": description.clone(),
			"description_html": format!("<div class=\"md\"><p>{}</p></div>", htmlescape::encode_minimal(&description)),
			"subscribers": 0,
			"accounts_active": weekly_active,
			"community_icon": icon.clone(),
			"icon_img": icon,
			"wiki_enabled": true,
			"over18": nsfw,
		}
	}))
}

async fn user_json(name: &str) -> Result<Value, String> {
	let html = shreddit_fragment(
		format!(
			"/svc/shreddit/user-hover-card/{}",
			url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>()
		),
		None,
	)
	.await?;
	let doc = Html::parse_fragment(&html);

	let display_name = doc
		.select(&USER_NAME)
		.next()
		.map(|element| element.text().collect::<String>().trim().to_owned())
		.filter(|name| !name.is_empty())
		.unwrap_or_else(|| name.to_owned());
	let icon = doc.select(&USER_AVATAR).next().map(|image| attribute(image, "src").to_owned()).unwrap_or_default();
	let description = doc
		.select(&USER_DESCRIPTION)
		.next()
		.map(|element| element.text().collect::<String>().trim().to_owned())
		.unwrap_or_default();
	let created = doc
		.select(&CAKE_DAY)
		.next()
		.and_then(|element| element.value().attr("datetime"))
		.and_then(unix_datetime)
		.unwrap_or_default();
	let karma: i64 = doc
		.select(&KARMA)
		.filter_map(|element| element.text().collect::<String>().replace(',', "").parse::<i64>().ok())
		.sum();

	Ok(json!({
		"kind": "t2",
		"data": {
			"name": display_name,
			"title": "",
			"icon_img": icon,
			"total_karma": karma,
			"created": created,
			"banner_img": "",
			"public_description": description,
			"subreddit": {"over_18": false},
		}
	}))
}

fn unix_datetime(value: &str) -> Option<f64> {
	chrono::DateTime::parse_from_rfc3339(value).ok().map(|date| date.timestamp() as f64)
}

async fn search_json(params: &HashMap<String, String>, sub: Option<&str>) -> Result<Value, String> {
	if params.get("type").map(String::as_str) == Some("sr_user") {
		return Ok(empty_listing());
	}

	let query = params.get("q").map(String::as_str).unwrap_or_default();
	if query.is_empty() {
		return Ok(empty_listing());
	}

	let mut pairs: Vec<(&str, &str)> = vec![("q", query), ("type", "posts")];
	if let Some(sort) = params.get("sort") {
		pairs.push(("sort", sort));
	}
	if let Some(time) = params.get("t") {
		pairs.push(("t", web_time(time)));
	}
	let query = encode_query(&pairs);
	let endpoint = match sub {
		Some(sub) => format!("/svc/shreddit/r/{sub}/search/?{query}"),
		None => format!("/svc/shreddit/search/?{query}"),
	};

	let html = shreddit_fragment(endpoint, None).await?;
	let doc = Html::parse_fragment(&html);
	let mut children = Vec::new();
	let mut seen = HashSet::new();

	for tracker in doc.select(&SEARCH_TRACKER) {
		let Some(raw) = tracker.value().attr("data-faceplate-tracking-context") else {
			continue;
		};
		let Ok(context) = serde_json::from_str::<Value>(raw) else {
			continue;
		};
		let id = context["post"]["id"].as_str().unwrap_or_default();
		let title = context["post"]["title"].as_str().unwrap_or_default();
		if !id.starts_with("t3_") || title.is_empty() || !seen.insert(id.to_owned()) {
			continue;
		}

		let subreddit = context["subreddit"]["name"].as_str().unwrap_or_default();
		let author = context["profile"]["name"].as_str().unwrap_or_default();
		let (score, comments, created) = search_result_details(tracker).unwrap_or_default();
		let post_id = id.strip_prefix("t3_").unwrap_or(id);

		children.push(json!({
			"kind": "t3",
			"data": {
				"id": post_id,
				"name": id,
				"title": title,
				"subreddit": subreddit,
				"author": author,
				"permalink": format!("/r/{subreddit}/comments/{post_id}/"),
				"url": format!("/r/{subreddit}/comments/{post_id}/"),
				"domain": "",
				"score": score,
				"ups": score,
				"num_comments": comments,
				"created": created,
				"created_utc": created,
				"over_18": context["post"]["nsfw"].as_bool().unwrap_or(false),
				"spoiler": context["post"]["spoiler"].as_bool().unwrap_or(false),
				"is_self": true,
				"post_hint": "",
				"selftext": "",
				"selftext_html": "",
				"body_html": "",
				"thumbnail": "",
				"thumbnail_width": 0,
				"thumbnail_height": 0,
				"link_flair_text": "",
				"link_flair_type": "",
				"link_flair_richtext": [],
				"author_flair_type": "text",
				"author_flair_richtext": [],
				"author_flair_text": Value::Null,
				"distinguished": "",
				"hide_score": false,
				"num_duplicates": 0,
				"poll_data": Value::Null,
				"all_awardings": [],
				"websocket_url": "",
				"removed_by_category": Value::Null,
				"crosspost_parent_list": [],
				"link_title": "",
				"quarantine": false,
				"locked": false,
				"archived": false,
				"edited": false,
				"visited": false,
				"preview": {},
				"secure_media": Value::Null,
				"media": Value::Null,
				"gallery_data": Value::Null,
				"media_metadata": Value::Null,
			}
		}));
	}

	Ok(json!({
		"kind": "Listing",
		"data": {
			"children": children,
			"after": next_cursor(&doc).unwrap_or_default(),
			"dist": children.len(),
		}
	}))
}

fn search_result_details(tracker: ElementRef<'_>) -> Option<(i64, i64, f64)> {
	let mut current = Some(tracker);
	while let Some(element) = current {
		if matches!(element.value().attr("data-testid"), Some("search-post-unit") | Some("search-post-with-content-preview")) {
			let numbers: Vec<i64> = element
				.select(&NUMBER)
				.filter_map(|number| number.value().attr("number").and_then(|value| value.parse().ok()))
				.collect();
			let created = element
				.select(&TIMEAGO)
				.next()
				.and_then(|time| time.value().attr("ts"))
				.and_then(crate::shreddit::unix_time)
				.unwrap_or_default();
			return Some((numbers.first().copied().unwrap_or_default(), numbers.get(1).copied().unwrap_or_default(), created));
		}
		current = element.parent().and_then(ElementRef::wrap);
	}
	None
}

fn empty_listing() -> Value {
	json!({"kind": "Listing", "data": {"children": [], "after": "", "dist": 0}})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_sort_and_time_mapping() {
		assert_eq!(web_sort("hot"), Some(("hot", "HOT")));
		assert_eq!(web_sort("top"), Some(("top", "TOP")));
		assert_eq!(web_sort("best"), Some(("hot", "BEST")));
		assert_eq!(web_sort("nope"), None);
		assert_eq!(web_time("hour"), "HOUR");
		assert_eq!(web_time("week"), "WEEK");
		assert_eq!(web_time("nope"), "DAY");
	}

	#[test]
	fn test_listing_parses_posts_and_cursor() {
		let html = r#"
			<shreddit-post id="t3_a" post-title="Text post" post-type="text" domain="self.test" score="12" comment-count="3" created-timestamp="2026-10-29T00:00:00.000000+0000">
				<div id="t3_a-post-rtjson-content" class="md"><p>Hello</p></div>
			</shreddit-post>
			<shreddit-post id="t3_b" post-title="Image post" post-type="image" domain="i.redd.it" content-href="https://i.redd.it/b.jpeg">
				<img data-post-media-primary src="https://preview.redd.it/b.jpeg" width="640" height="480">
			</shreddit-post>
			<faceplate-partial slot="load-after" src="/svc/shreddit/community-more-posts/hot/?name=test&amp;after=dDNfYQ%3D%3D"></faceplate-partial>
		"#;

		let (children, after) = listing_page(html).unwrap();
		assert_eq!(children.len(), 2);
		assert_eq!(children[0]["data"]["id"], "a");
		assert_eq!(children[0]["data"]["score"].as_i64(), Some(12));
		assert_eq!(children[0]["data"]["num_comments"].as_i64(), Some(3));
		assert!(children[0]["data"]["is_self"].as_bool().unwrap());
		assert_eq!(children[0]["data"]["selftext_html"], "<div class=\"md\"><p>Hello</p></div>");
		assert_eq!(children[1]["data"]["post_hint"], "image");
		assert_eq!(children[1]["data"]["url"], "https://i.redd.it/b.jpeg");
		assert_eq!(children[1]["data"]["thumbnail_width"], 640);
		assert_eq!(after.as_deref(), Some("dDNfYQ=="));
	}

	#[test]
	fn test_listing_skips_empty_and_duplicate_posts() {
		let html = r#"
			<shreddit-post id="t3_a" post-title="One"></shreddit-post>
			<shreddit-post-overflow-menu post-id="t3_a"></shreddit-post-overflow-menu>
			<shreddit-post id="t3_a" post-title="Duplicate"></shreddit-post>
			<shreddit-post id="t3_b" post-title=""></shreddit-post>
		"#;
		let (children, after) = listing_page(html).unwrap();
		assert_eq!(children.len(), 1);
		assert_eq!(children[0]["data"]["title"], "One");
		assert!(after.is_none());
	}

	#[test]
	fn test_gallery_post_fields() {
		let html = r#"
			<shreddit-post id="t3_g" post-title="Gallery" post-type="gallery" domain="reddit.com" content-href="https://www.reddit.com/gallery/g">
				<gallery-carousel>
					<figure><img src="https://preview.redd.it/g-v0-a.jpeg" width="640" height="480"><img src="https://i.redd.it/a.jpeg"></figure>
					<figure><img data-lazy-src="https://preview.redd.it/g-v0-b.jpeg" width="800" height="600"><img src="https://i.redd.it/b.jpeg"></figure>
				</gallery-carousel>
			</shreddit-post>
		"#;
		let doc = Html::parse_fragment(html);
		let post = doc.select(&POST).next().unwrap();
		let value = post_value(post, false);
		assert!(value["is_gallery"].as_bool().unwrap());
		assert_eq!(value["gallery_data"]["items"].as_array().unwrap().len(), 2);
		assert_eq!(value["gallery_data"]["items"][0]["media_id"], "a");
		assert_eq!(value["media_metadata"]["a"]["s"]["u"], "https://i.redd.it/a.jpeg");
		assert_eq!(value["media_metadata"]["b"]["s"]["x"], 800);
	}

	#[test]
	fn test_video_post_fields() {
		let html = r#"
			<shreddit-post id="t3_v" post-title="Video" post-type="video" domain="v.redd.it" content-href="https://v.redd.it/v">
				<shreddit-player src="https://v.redd.it/v/HLSPlaylist.m3u8" poster="https://preview.redd.it/v-poster.jpeg"></shreddit-player>
			</shreddit-post>
		"#;
		let doc = Html::parse_fragment(html);
		let post = doc.select(&POST).next().unwrap();
		let value = post_value(post, false);
		assert_eq!(value["post_hint"], "hosted:video");
		assert!(value["is_video"].as_bool().unwrap());
		assert_eq!(value["secure_media"]["reddit_video"]["hls_url"], "https://v.redd.it/v/HLSPlaylist.m3u8");
		assert_eq!(value["thumbnail"], "https://preview.redd.it/v-poster.jpeg");
	}

	#[test]
	fn test_canonical_path() {
		let html = r#"
			<div id="canonical-url-updater" value="https://www.reddit.com/r/rust/comments/abc/slug/"></div>
		"#;
		assert_eq!(canonical_path(html).as_deref(), Some("/r/rust/comments/abc/slug/?render-mode=partial"));
	}

	#[tokio::test]
	async fn test_unsupported_routes_are_not_translated() {
		assert!(json("/by_id/t3_abc.json".to_owned()).await.is_none());
		assert!(json("/r/rust/wiki/index.json".to_owned()).await.is_none());
	}
}
