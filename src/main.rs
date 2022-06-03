use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, HOST, USER_AGENT};
use scraper::{Html, Selector};
use serde::Deserialize;
use time::{OffsetDateTime, UtcOffset};
use tokio::io::{self, AsyncReadExt};
use tokio::{fs, process};

#[tokio::main]
async fn main() {
    let html = parse_input().await;
    let posts = extract_posts(&html);
    let token = snaptik_token().await;

    futures::stream::iter(posts.iter().map(|p| download_video(p, &token)))
        .buffer_unordered(20)
        .collect::<Vec<_>>()
        .await;
}

async fn parse_input() -> Html {
    let mut html = String::new();
    let mut stdin = io::stdin();
    stdin.read_to_string(&mut html).await.unwrap();
    Html::parse_fragment(&html)
}

fn extract_posts(html: &Html) -> Vec<&str> {
    // Find posts list
    let selector = Selector::parse("div[data-e2e=\"user-post-item-list\"]").unwrap();
    let posts_list = html.select(&selector).next().unwrap();

    // Parse posts link
    let selector = Selector::parse("div[data-e2e=\"user-post-item\"] a").unwrap();
    let links = posts_list
        .select(&selector)
        .map(|e| e.value().attr("href").unwrap())
        .collect();

    links
}

#[derive(Debug)]
struct VideoInfo {
    id: String,
    datetime: time::OffsetDateTime,
    user: String,
    download_url: String,
}

async fn video_info(url: &str, snaptik_token: &str) -> Option<VideoInfo> {
    for i in 0..5 {
        // Query TikTok
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("www.tiktok.com"));
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(USER_AGENT, HeaderValue::from_static("HTTPie/2.6.0"));
        let text = reqwest::Client::new()
            .get(url)
            .headers(headers)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let html = Html::parse_fragment(&text);
        let selector =
            Selector::parse("script[type=\"application/json\"][id=\"SIGI_STATE\"]").unwrap();
        let json = html.select(&selector).next().unwrap().inner_html();

        // Get ID
        let id = url.rsplit_once('/').unwrap().1;

        // Parse date and author
        #[derive(Deserialize)]
        struct TTInfo {
            #[serde(rename = "ItemModule")]
            item_module: HashMap<String, TTVideoInfo>,
        }
        #[derive(Deserialize)]
        struct TTVideoInfo {
            #[serde(rename = "createTime")]
            create_time: String,
            author: String,
        }
        let info: TTInfo = serde_json::from_str(&json).unwrap();
        let x = match info.item_module.get(id) {
            Some(x) => x,
            None => {
                let n = 1 << i;
                eprintln!("Missing ID for {}, sleeping {} seconds", &url, n);
                tokio::time::sleep(Duration::from_secs(n)).await;
                continue;
            }
        };
        let datetime = OffsetDateTime::from_unix_timestamp(x.create_time.parse().unwrap())
            .unwrap()
            .to_offset(UtcOffset::from_hms(9, 0, 0).unwrap());

        // Get download url
        let download_url = snaptik_get_video(snaptik_token, url).await;

        return Some(VideoInfo {
            id: id.to_owned(),
            datetime,
            user: x.author.clone(),
            download_url,
        });
    }

    eprintln!("Missing info for {}", url);
    None
}

async fn snaptik_token() -> String {
    let text = reqwest::get("https://snaptik.app/en")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let html = Html::parse_fragment(&text);
    let selector = Selector::parse("input[name=\"token\"]").unwrap();
    let input = html.select(&selector).next().unwrap();
    input.value().attr("value").unwrap().to_owned()
}

async fn snaptik_get_video(token: &str, url: &str) -> String {
    // Query snaptik
    let text = reqwest::Client::new()
        .get("https://snaptik.app/abc.php")
        .query(&[("url", url), ("lang", "en"), ("token", token)])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let html = Html::parse_fragment(&text);
    let selector = Selector::parse("script:not([src])").unwrap();
    let script = html.select(&selector).next().unwrap().inner_html();
    let script = html_escape::decode_html_entities(&script);

    // Deobfuscate
    let decoded = snaptik_decode(&script).await;

    // Extract full hd video
    let re = regex::Regex::new(r"https:[\./\w\?&%=]*?full_hd=1").unwrap();
    let url = re.find(&decoded).unwrap().as_str().to_owned();
    url
}

async fn snaptik_decode(text: &str) -> String {
    let re = regex::Regex::new(r"eval\((?P<func1>function)(?P<func2>.*})\s*\(+(?P<args>.+?)\)+")
        .unwrap();
    let script = re.replace(
        text,
        "${func1} decode_impl${func2}\nfunction decode() { return decode_impl(${args}); }",
    );

    let mut script = js_sandbox::Script::from_string(&script).unwrap();
    script.call("decode", &()).unwrap()
}

async fn download_video(url: &str, snaptik_token: &str) {
    let re = regex::Regex::new(r"@(\w+)").unwrap();
    let dir = re.captures(url).unwrap().get(1).unwrap().as_str();
    let re = regex::Regex::new(r"(\d+)*$").unwrap();
    let id = re.find(url).unwrap().as_str();

    // Check if already downloaded
    if let Some(true) = glob::glob(&format!("{}/[0-9]*_{}_*.mp4", dir, id))
        .ok()
        .map(|g| g.count() > 0)
    {
        return;
    }
    let dir: PathBuf = dir.into();

    // Get video info
    let info = match video_info(url, snaptik_token).await {
        Some(i) => i,
        None => return,
    };

    // Generate output file
    let fmt = time::format_description::parse("[year][month][day]").unwrap();
    let date_str = info.datetime.format(&fmt).unwrap();
    let output_filename = format!("{}_{}_{}.mp4", date_str, &info.id, &info.user);
    let output_filename_temp = format!("{}.temp", output_filename);
    fs::create_dir_all(&dir).await.unwrap();
    let output_filename = dir.join(output_filename);
    let output_filename_temp = dir.join(output_filename_temp);

    // Download video
    let data = reqwest::get(info.download_url)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    fs::write(&output_filename_temp, data).await.unwrap();

    // Convert to mp4
    process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&output_filename_temp)
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&output_filename)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
        .wait()
        .await
        .unwrap();

    // Delete temp file
    fs::remove_file(&output_filename_temp).await.unwrap();

    // Set file metadata time
    let ft = filetime::FileTime::from_unix_time(
        info.datetime.unix_timestamp(),
        info.datetime.nanosecond(),
    );
    filetime::set_file_mtime(&output_filename, ft).unwrap();

    println!("Downloaded {}", output_filename.to_string_lossy());
}
