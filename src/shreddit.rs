// Fallback for post pages when Reddit blocks the `/comments/` JSON API
// (403 "blocked by network security" since 2026-10-20; `client::request`
// currently gets past it by encoding a slash).
//
// The post itself still comes from the JSON API (`/by_id`). Comments are scraped
// from the HTML fragments reddit.com loads them from (`/svc/shreddit/...`) and
// converted into the shape of the `/comments/` JSON response, so post.rs and the
// templates work unchanged.

use crate::client::{json, CLIENT};
use crate::utils::{format_url, param};
use cached::proc_macro::cached;
use log::warn;
use chrono::DateTime;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::{json as json_value, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

const SVC_BASE: &str = "https://www.reddit.com/svc/shreddit";

static COMMENT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("shreddit-comment").unwrap());
static MORE: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"faceplate-partial[src*="/more-comments/"]"#).unwrap());
static CURSOR: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"input[name="cursor"]"#).unwrap());
static MORE_COUNT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("faceplate-number[number]").unwrap());

/// Unix time until which the /svc/shreddit scrape endpoints are assumed rate
/// limited (429). Their budget is small and per-IP (observed: 200 requests per
/// ~10 min, shared with ordinary reddit.com browsing), so after a 429 we stop
/// sending entirely until reddit says the window resets - retrying into the
/// wall just keeps the instance broken and wastes what little budget remains.
static SCRAPE_BACKOFF_UNTIL: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Fail fast while the scrape endpoints are known to be rate limited.
fn check_backoff() -> Result<(), String> {
	let until = SCRAPE_BACKOFF_UNTIL.load(Ordering::Relaxed);
	let now = now_secs();
	if now < until {
		let secs = until - now;
		return Err(format!(
			"Reddit is rate limiting comments (shared web budget exhausted). Try again in about {} minute(s)",
			if secs < 60 { 1 } else { secs / 60 }
		));
	}
	Ok(())
}

/// Record a 429 from the scrape endpoints and build the user-facing error.
fn rate_limited(res: &wreq::Response) -> String {
	let reset = res
		.headers()
		.get("x-ratelimit-reset")
		.and_then(|v| v.to_str().ok())
		.and_then(|s| s.parse::<u64>().ok())
		.or_else(|| res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok()))
		.unwrap_or(300)
		.clamp(30, 900);
	SCRAPE_BACKOFF_UNTIL.store(now_secs() + reset, Ordering::Relaxed);
	warn!("[⏳] Comments scrape endpoint rate limited (429). Backing off for {reset}s.");
	format!("Reddit is rate limiting comments (shared web budget exhausted). Try again in about {} minute(s)", if reset < 60 { 1 } else { reset / 60 })
}

/// Builds a `/comments/` JSON response for `path` (a `/comments/` API path, as
/// built by `post::item`). Supports the `sort` and `cursor` query parameters
/// and single comment threads.
///
/// Cached for a couple of minutes: each call costs scarce scrape-endpoint
/// budget (1-2 requests), and comment threads are re-read far more often than
/// they change (reloads, back-navigation, link previews).
#[cached(size = 256, time = 120, result = true)]
pub async fn thread(path: String, quarantine: bool) -> Result<Value, String> {
	check_backoff()?;
	let path = &path;
	let (id, comment_id) = parse_path(path).ok_or_else(|| format!("Couldn't parse post path | {path}"))?;

	let post = json(format!("/by_id/t3_{id}.json?raw_json=1"), quarantine).await?;
	let sub = post["data"]["children"][0]["data"]["subreddit"]
		.as_str()
		.ok_or_else(|| format!("Post not found | {path}"))?
		.to_string();

	let comments = if let Some(comment_id) = comment_id {
		// The comment itself, then its replies through a cursor naming it as the parent
		let mut html = get(&format!("{SVC_BASE}/comment/t1_{comment_id}?depth=0&subredditName={sub}"), quarantine).await?;
		html += &more(&sub, &id, &cursor_for(&comment_id), quarantine).await?;
		html
	} else if let Some(cursor) = param(path, "cursor") {
		more(&sub, &id, &cursor, quarantine).await?
	} else {
		let sort = param(path, "sort").filter(|s| !s.is_empty()).unwrap_or_else(|| "confidence".to_string());
		get(&format!("{SVC_BASE}/comments/r/{sub}/t3_{id}?sort={}", sort.to_uppercase()), quarantine).await?
	};

	Ok(json_value!([post, listing(parse_comments(&comments, &id))]))
}

/// Extracts the post ID and optional comment ID from paths like
/// `/r/{sub}/comments/{id}/{title}/{comment_id}.json?...`.
fn parse_path(path: &str) -> Option<(String, Option<String>)> {
	let path = path.split('?').next()?.trim_end_matches(".json");
	let mut segments = path.split('/').skip_while(|s| *s != "comments").skip(1);
	let id = segments.next().filter(|s| !s.is_empty())?.to_string();
	let comment_id = segments.nth(1).filter(|s| !s.is_empty()).map(str::to_string);
	Some((id, comment_id))
}

/// Cursor for the replies of a comment. Reddit's own cursors carry a hash
/// before the comma, but an empty one is accepted.
fn cursor_for(comment_id: &str) -> String {
	use base64::{engine::general_purpose::STANDARD, Engine as _};
	STANDARD.encode(format!("commenttree:,::1:d1:t1_{comment_id}"))
}

fn cookies(quarantine: bool) -> String {
	let mut cookies = "over18=1".to_string();
	if quarantine {
		cookies += "; _options=%7B%22pref_quarantine_optin%22%3A%20true%2C%20%22pref_gated_sr_optin%22%3A%20true%7D";
	}
	cookies
}

async fn get(url: &str, quarantine: bool) -> Result<String, String> {
	check_backoff()?;
	let res = CLIENT.get(url).headers(scrape_headers(&cookies(quarantine))).send().await.map_err(|e| e.to_string())?;
	if res.status() == 429 {
		return Err(rate_limited(&res));
	}
	if !res.status().is_success() {
		return Err(format!("Reddit returned {} for comments | {url}", res.status()));
	}
	res.text().await.map_err(|e| e.to_string())
}

/// Headers for the shreddit fragment endpoints. The OAuth token is required:
/// reddit.com's edge serves datacenter IPs the "blocked by network security"
/// page for these HTML routes unless the request carries the app-only bearer
/// (the JSON API on oauth.reddit.com is fine without it). The spoofed device
/// headers go along with it, but not the Android User-Agent or Content-Type -
/// the wreq desktop emulation supplies those, and desktop clients get 25
/// comments per request instead of 10.
fn scrape_headers(cookie: &str) -> wreq::header::HeaderMap {
	use std::str::FromStr;
	let mut headers = wreq::header::HeaderMap::new();
	headers.insert(wreq::header::COOKIE, cookie.parse().unwrap());
	let oauth = crate::client::OAUTH_CLIENT.load_full();
	for (key, value) in &oauth.headers_map {
		if key.eq_ignore_ascii_case("user-agent") || key.eq_ignore_ascii_case("content-type") || key.eq_ignore_ascii_case("cookie") {
			continue;
		}
		if let (Ok(name), Ok(value)) = (wreq::header::HeaderName::from_str(key), wreq::header::HeaderValue::from_str(value)) {
			headers.insert(name, value);
		}
	}
	headers
}

/// Loads the comments behind a cursor, like reddit.com's "more replies" buttons.
async fn more(sub: &str, id: &str, cursor: &str, quarantine: bool) -> Result<String, String> {
	check_backoff()?;
	let url = format!("{SVC_BASE}/more-comments/{sub}/t3_{id}");
	// Double-submit CSRF check: any token works as long as cookie and form match
	let csrf = format!("{:032x}", fastrand::u128(..));
	let body = serde_urlencoded::to_string([("cursor", cursor), ("csrf_token", &csrf)]).map_err(|e| e.to_string())?;
	let res = CLIENT
		.post(&url)
		.headers(scrape_headers(&format!("{}; csrf_token={csrf}", cookies(quarantine))))
		.header("Content-Type", "application/x-www-form-urlencoded")
		.body(body)
		.send()
		.await
		.map_err(|e| e.to_string())?;
	if res.status() == 429 {
		return Err(rate_limited(&res));
	}
	if !res.status().is_success() {
		return Err(format!("Reddit returned {} for more comments | {url}", res.status()));
	}
	res.text().await.map_err(|e| e.to_string())
}

fn listing(children: Vec<Value>) -> Value {
	json_value!({ "kind": "Listing", "data": { "children": children } })
}

/// Converts `<shreddit-comment>` elements and "more replies" partials into the
/// comment tree of the JSON API.
fn parse_comments(html: &str, post_id: &str) -> Vec<Value> {
	let doc = Html::parse_document(html);
	let post_fullname = format!("t3_{post_id}");

	// (parent fullname, item) in document order
	let mut items: Vec<(String, Value)> = Vec::new();

	for el in doc.select(&COMMENT) {
		let attr = |name: &str| el.value().attr(name).unwrap_or_default();
		let fullname = attr("thingid");
		let Some(id) = fullname.strip_prefix("t1_") else { continue };
		let parent = el.value().attr("parentid").unwrap_or(&post_fullname).to_string();

		let body = if el.value().attr("is-comment-deleted").is_some() {
			"<p>[deleted]</p>".to_string()
		} else {
			// Select the body by ID so a reply's body is never picked up for a comment without one
			Selector::parse(&format!(r#"[id="{fullname}-comment-rtjson-content"]"#))
				.ok()
				.and_then(|s| el.select(&s).next())
				.map(clean_html)
				.unwrap_or_default()
		};

		let created = DateTime::parse_from_str(attr("created"), "%Y-%m-%dT%H:%M:%S%.f%z").map_or(0, |t| t.timestamp());

		items.push((
			parent.clone(),
			json_value!({
				"kind": "t1",
				"data": {
					"id": id,
					"name": fullname,
					"author": attr("author"),
					"body_html": format!("<div class=\"md\">{body}</div>"),
					"created_utc": created,
					"score": attr("score").parse::<i64>().unwrap_or_default(),
					"parent_id": parent,
					"replies": "",
				}
			}),
		));
	}

	for el in doc.select(&MORE) {
		let src = el.value().attr("src").unwrap_or_default();
		let Some(cursor) = el.select(&CURSOR).next().and_then(|i| i.value().attr("value")) else {
			continue;
		};

		if src.contains("top-level=1") {
			// More top-level comments: rendered as a link to the next page of comments
			let count = param(src, "comments-remaining").and_then(|c| c.parse::<i64>().ok()).unwrap_or_default();
			items.push((
				post_fullname.clone(),
				json_value!({
					"kind": "more_comments",
					"data": { "id": utf8_percent_encode(cursor, NON_ALPHANUMERIC).to_string(), "count": count, "parent_id": post_fullname }
				}),
			));
			continue;
		}

		// More replies: the cursor ends with the parent comment, e.g. "commenttree:abc,::1:d1:t1_xyz"
		use base64::{engine::general_purpose::STANDARD, Engine as _};
		let Some(parent) = STANDARD
			.decode(cursor)
			.ok()
			.and_then(|c| String::from_utf8(c).ok())
			.and_then(|c| c.rsplit(':').next().filter(|p| p.starts_with("t1_")).map(str::to_string))
		else {
			continue;
		};
		let count = el
			.select(&MORE_COUNT)
			.next()
			.and_then(|n| n.value().attr("number"))
			.and_then(|n| n.parse::<i64>().ok())
			.unwrap_or_default();
		items.push((parent.clone(), json_value!({ "kind": "more", "data": { "id": "", "count": count, "parent_id": parent } })));
	}

	// Build the tree. Items whose parent isn't on the page are top-level.
	let names: std::collections::HashSet<String> = items.iter().filter_map(|(_, v)| v["data"]["name"].as_str().map(str::to_string)).collect();
	let mut children: HashMap<String, Vec<Value>> = HashMap::new();
	let mut top = Vec::new();
	for (parent, item) in items {
		if names.contains(&parent) {
			children.entry(parent).or_default().push(item);
		} else {
			top.push(item);
		}
	}
	top.into_iter().map(|item| attach_replies(item, &mut children)).collect()
}

fn attach_replies(mut item: Value, children: &mut HashMap<String, Vec<Value>>) -> Value {
	let name = item["data"]["name"].as_str().unwrap_or_default().to_string();
	if let Some(replies) = children.remove(&name) {
		let replies = replies.into_iter().map(|r| attach_replies(r, children)).collect();
		item["data"]["replies"] = listing(replies);
	}
	item
}

const ALLOWED_TAGS: &[&str] = &[
	"p",
	"br",
	"hr",
	"a",
	"em",
	"strong",
	"b",
	"i",
	"del",
	"s",
	"sup",
	"sub",
	"code",
	"pre",
	"blockquote",
	"ul",
	"ol",
	"li",
	"h1",
	"h2",
	"h3",
	"h4",
	"h5",
	"h6",
	"table",
	"thead",
	"tbody",
	"tr",
	"th",
	"td",
	"span",
	"img",
	"figure",
];
const VOID_TAGS: &[&str] = &["br", "hr", "img"];

/// Rebuilds comment body HTML with only markdown tags and the attributes
/// links, images and tables need, dropping Reddit's wrappers and styling.
/// Media URLs are rewritten to Redlib's proxy here, as rewrite_urls only
/// handles Reddit media in the link format of the JSON API.
fn clean_html(el: ElementRef) -> String {
	let mut out = String::new();
	for child in el.children() {
		match child.value() {
			Node::Text(text) => out += &htmlescape::encode_minimal(text),
			Node::Element(e) => {
				let tag = e.name();
				if matches!(tag, "script" | "style" | "svg") {
					continue;
				}
				let inner = ElementRef::wrap(child).map(clean_html).unwrap_or_default();
				match tag {
					"shreddit-spoiler" => out += &format!("<span class=\"md-spoiler-text\">{inner}</span>"),
					// GIFs and videos in comments
					"shreddit-player" => {
						let src = url_attr(e.attr("src"));
						if !src.is_empty() {
							let play = if e.attr("gif").is_some() { "autoplay loop muted playsinline" } else { "controls" };
							out += &format!("<video src=\"{src}\" {play}></video>");
						}
					}
					_ if !ALLOWED_TAGS.contains(&tag) => out += &inner,
					_ => {
						out += &format!("<{tag}");
						for name in ["href", "src", "alt", "title", "align"] {
							let Some(value) = e.attr(name) else { continue };
							let value = if matches!(name, "href" | "src") {
								url_attr(Some(value))
							} else {
								htmlescape::encode_minimal(value)
							};
							if !value.is_empty() {
								out += &format!(" {name}=\"{value}\"");
							}
						}
						if tag == "img" {
							out += " loading=\"lazy\"";
						}
						out += ">";
						if !VOID_TAGS.contains(&tag) {
							out += &format!("{inner}</{tag}>");
						}
					}
				}
			}
			_ => {}
		}
	}
	out
}

/// Escaped URL attribute value, proxied through Redlib for Reddit media. Empty for `javascript:` URLs.
fn url_attr(value: Option<&str>) -> String {
	let value = value.unwrap_or_default().trim();
	if value.to_lowercase().starts_with("javascript:") {
		return String::new();
	}
	htmlescape::encode_minimal(&format_url(value))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_parse_path() {
		assert_eq!(parse_path("/r/linux/comments/1xaxcyl/some_title/.json?&raw_json=1"), Some(("1xaxcyl".into(), None)));
		assert_eq!(parse_path("/r/linux/comments/1xaxcyl.json?sort=new&raw_json=1"), Some(("1xaxcyl".into(), None)));
		assert_eq!(
			parse_path("/r/linux/comments/1xaxcyl/some_title/19re6tdr7osth.json?&raw_json=1"),
			Some(("1xaxcyl".into(), Some("19re6tdr7osth".into())))
		);
		assert_eq!(parse_path("/comments/1xaxcyl/comments/abc.json?"), Some(("1xaxcyl".into(), Some("abc".into()))));
		assert_eq!(parse_path("/user/spez/comments/1xaxcyl.json?"), Some(("1xaxcyl".into(), None)));
		assert_eq!(parse_path("/r/linux/hot.json"), None);
	}

	#[test]
	fn test_parse_comments() {
		let html = r#"
<shreddit-comment thingId="t1_a" author="alice" created="2026-10-20T17:00:53.706000+0000" score="18" depth="0">
  <div id="t1_a-comment-rtjson-content" slot="comment"><div class="x" dir="auto"><p rpl class="y">Hi <a class="z" href="https://www.reddit.com/r/rust" target="_blank">r/rust</a> &amp; <script>x</script></p></div></div>
  <div id="comment-children">
    <shreddit-comment thingId="t1_b" author="bob" created="2026-10-20T17:01:00.000000+0000" score="-2" depth="1" parentId="t1_a">
      <div id="t1_b-comment-rtjson-content" slot="comment"><p>reply</p></div>
    </shreddit-comment>
    <faceplate-partial src="/svc/shreddit/more-comments/linux/t3_p?startingDepth=1" method="post" slot="children">
      <input type="hidden" name="cursor" value="Y29tbWVudHRyZWU6NGFmMDViYzcsOjoxOmQxOnQxX2E="><faceplate-number number="7"></faceplate-number> more replies
    </faceplate-partial>
  </div>
</shreddit-comment>
<shreddit-comment thingId="t1_c" author="[deleted]" created="" score="1" depth="0" is-comment-deleted>
  <div id="comment-children"><shreddit-comment thingId="t1_d" author="dan" created="" score="3" depth="1" parentId="t1_c"><div id="t1_d-comment-rtjson-content" slot="comment"><p>orphan body</p></div></shreddit-comment></div>
</shreddit-comment>
<shreddit-comment thingId="t1_e" author="eve" created="" score="1" depth="0">
  <div id="t1_e-comment-rtjson-content" slot="comment"><p><shreddit-spoiler>secret</shreddit-spoiler></p><figure class="rte-media"><a href="https://preview.redd.it/x-v0-abc.jpeg?width=400&amp;s=1" class="w-fit"><img src="https://preview.redd.it/x-v0-abc.jpeg?width=400&amp;s=1" alt="Comment Image" srcset="a 1x" style="x"></a></figure><figure class="rte-media"><shreddit-player src="https://external-preview.redd.it/g.gif?width=356&amp;format=mp4&amp;s=2" gif autoplay><source src="https://external-preview.redd.it/g.gif"></shreddit-player></figure></div>
</shreddit-comment>
<faceplate-partial src="/svc/shreddit/more-comments/linux/t3_p?top-level=1&amp;comments-remaining=90&amp;commentPositionOffset=10" method="post"><input type="hidden" name="cursor" value="ab+/="></faceplate-partial>
"#;
		let comments = parse_comments(html, "p");
		assert_eq!(comments.len(), 4);

		let a = &comments[0]["data"];
		assert_eq!(a["author"], "alice");
		assert_eq!(a["parent_id"], "t3_p");
		assert_eq!(a["score"], 18);
		assert_eq!(a["created_utc"], 1792515653);
		assert_eq!(a["body_html"], r#"<div class="md"><p>Hi <a href="/r/rust">r/rust</a> &amp; </p></div>"#);

		let replies = a["replies"]["data"]["children"].as_array().unwrap();
		assert_eq!(replies.len(), 2);
		assert_eq!(replies[0]["data"]["author"], "bob");
		assert_eq!(replies[0]["data"]["score"], -2);
		assert_eq!(replies[1]["kind"], "more");
		assert_eq!(replies[1]["data"]["parent_id"], "t1_a");
		assert_eq!(replies[1]["data"]["count"], 7);

		// A deleted comment must not show its reply's body
		let c = &comments[1]["data"];
		assert_eq!(c["body_html"], r#"<div class="md"><p>[deleted]</p></div>"#);
		assert_eq!(c["replies"]["data"]["children"][0]["data"]["author"], "dan");

		assert_eq!(
			comments[2]["data"]["body_html"],
			concat!(
				r#"<div class="md"><p><span class="md-spoiler-text">secret</span></p>"#,
				r#"<figure><a href="/preview/pre/x-v0-abc.jpeg?width=400&amp;s=1"><img src="/preview/pre/x-v0-abc.jpeg?width=400&amp;s=1" alt="Comment Image" loading="lazy"></a></figure>"#,
				r#"<figure><video src="/preview/external-pre/g.gif?width=356&amp;format=mp4&amp;s=2" autoplay loop muted playsinline></video></figure></div>"#
			)
		);

		assert_eq!(comments[3]["kind"], "more_comments");
		assert_eq!(comments[3]["data"]["count"], 90);
		assert_eq!(comments[3]["data"]["id"], "ab%2B%2F%3D");
	}
}
