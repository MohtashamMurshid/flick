//! Native discovery adapters. Torlink ports are credited in THIRD_PARTY_NOTICES.md.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, time::Duration};

#[derive(Clone, Debug, Serialize)]
pub struct SearchResult {
    pub name: String,
    pub infohash: Option<String>,
    pub magnet: String,
    pub torrent_url: Option<String>,
    pub size_bytes: u64,
    pub seeders: u64,
    pub source: String,
}
impl SearchResult {
    pub fn input(&self) -> crate::torrent::TorrentInput {
        match &self.torrent_url {
            Some(url) => crate::torrent::TorrentInput::Url(url.clone()),
            None => crate::torrent::TorrentInput::Magnet(self.magnet.clone()),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    pub items: Vec<SearchResult>,
    pub unavailable: Vec<String>,
}

pub async fn search(query: &str, mut publish: impl FnMut(SearchResults)) -> Result<SearchResults> {
    let query = query.trim();
    ensure!(
        query.chars().count() >= 3,
        "Type at least 3 characters to search online"
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent(concat!("Flick/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut jobs = tokio::task::JoinSet::new();
    for provider in ["TPB", "Internet Archive", "YTS"] {
        let client = client.clone();
        let query = query.to_owned();
        jobs.spawn(async move {
            let result = match provider {
                "TPB" => bounded(search_general(&client, &query)).await,
                "Internet Archive" => bounded(search_archive(&client, &query)).await,
                _ => bounded(search_movies(&client, &query)).await,
            };
            (provider, result)
        });
    }
    let mut result = SearchResults::default();
    let mut seen = HashSet::new();
    while let Some(job) = jobs.join_next().await {
        let (source, outcome) = job.context("Search source stopped unexpectedly")?;
        match outcome {
            Ok(items) => {
                for item in items {
                    let identity = item.infohash.clone().or_else(|| item.torrent_url.clone());
                    if seen.insert(identity) {
                        result.items.push(item);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(provider=source,%error,"Search provider unavailable");
                result.unavailable.push(source.into());
            }
        }
        result
            .items
            .sort_by_key(|item| std::cmp::Reverse(item.seeders));
        publish(result.clone());
    }
    Ok(result)
}
async fn bounded<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(13), future)
        .await
        .context("Search timed out")?
}
async fn json(client: &reqwest::Client, url: reqwest::Url) -> Result<serde_json::Value> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 4 * 1024 * 1024,
            "Search response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("Invalid search response")
}
async fn search_general(client: &reqwest::Client, query: &str) -> Result<Vec<SearchResult>> {
    let mut url = reqwest::Url::parse("https://apibay.org/q.php")?;
    url.query_pairs_mut()
        .extend_pairs([("q", query), ("cat", "200")]);
    parse_general(json(client, url).await?)
}
#[derive(Deserialize)]
struct GeneralRow {
    id: Option<String>,
    info_hash: Option<String>,
    name: Option<String>,
    size: Option<String>,
    seeders: Option<String>,
    category: Option<String>,
}
fn parse_general(value: serde_json::Value) -> Result<Vec<SearchResult>> {
    let rows: Vec<GeneralRow> = serde_json::from_value(value)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            if row.id.as_deref() == Some("0")
                || !row.category.as_deref().is_some_and(|v| v.starts_with('2'))
            {
                return None;
            }
            item(
                row.info_hash?,
                row.name?,
                row.size.and_then(|s| s.parse().ok()).unwrap_or(0),
                row.seeders.and_then(|s| s.parse().ok()).unwrap_or(0),
                "TPB",
            )
        })
        .collect())
}
async fn search_movies(client: &reqwest::Client, query: &str) -> Result<Vec<SearchResult>> {
    let mut last = None;
    for host in ["yts.mx", "yts.am", "yts.rs"] {
        let mut url = reqwest::Url::parse(&format!("https://{host}/api/v2/list_movies.json"))?;
        url.query_pairs_mut()
            .extend_pairs([("query_term", query), ("limit", "50")]);
        match json(client, url).await.and_then(parse_movies) {
            Ok(items) => return Ok(items),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("Movie search unavailable")))
}
fn parse_movies(value: serde_json::Value) -> Result<Vec<SearchResult>> {
    ensure!(value.get("data").is_some(), "Invalid movie search response");
    let mut items = Vec::new();
    for movie in value["data"]["movies"].as_array().into_iter().flatten() {
        let Some(title) = movie["title_long"]
            .as_str()
            .or_else(|| movie["title"].as_str())
        else {
            continue;
        };
        for torrent in movie["torrents"].as_array().into_iter().flatten() {
            let Some(hash) = torrent["hash"].as_str() else {
                continue;
            };
            let quality = torrent["quality"].as_str().unwrap_or("");
            let name = if quality.is_empty() {
                title.to_owned()
            } else {
                format!("{title} · {quality}")
            };
            if let Some(row) = item(
                hash.into(),
                name,
                torrent["size_bytes"].as_u64().unwrap_or(0),
                torrent["seeds"].as_u64().unwrap_or(0),
                "YTS",
            ) {
                items.push(row);
            }
        }
    }
    Ok(items)
}
async fn search_archive(client: &reqwest::Client, query: &str) -> Result<Vec<SearchResult>> {
    let mut url = reqwest::Url::parse("https://archive.org/advancedsearch.php")?;
    let literal = query.replace('\\', "\\\\").replace('"', "\\\"");
    let q = format!("mediatype:movies AND title:\"{literal}\"");
    url.query_pairs_mut().extend_pairs([
        ("q", q.as_str()),
        ("output", "json"),
        ("fl[]", "identifier"),
        ("fl[]", "title"),
        ("rows", "30"),
    ]);
    parse_archive(json(client, url).await?)
}
fn parse_archive(value: serde_json::Value) -> Result<Vec<SearchResult>> {
    let docs = value["response"]["docs"]
        .as_array()
        .context("Invalid archive search response")?;
    let mut items = Vec::new();
    for doc in docs {
        let (Some(id), Some(name)) = (doc["identifier"].as_str(), doc["title"].as_str()) else {
            continue;
        };
        let mut url = reqwest::Url::parse("https://archive.org/")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid archive address"))?
            .extend(["download", id, &format!("{id}_archive.torrent")]);
        items.push(SearchResult {
            name: name.into(),
            infohash: None,
            magnet: String::new(),
            torrent_url: Some(url.into()),
            size_bytes: 0,
            seeders: 0,
            source: "Internet Archive".into(),
        });
    }
    Ok(items)
}
fn item(
    hash: String,
    name: String,
    size_bytes: u64,
    seeders: u64,
    source: &str,
) -> Option<SearchResult> {
    if hash.len() != 40
        || !hash.bytes().all(|b| b.is_ascii_hexdigit())
        || hash.bytes().all(|b| b == b'0')
        || name.trim().is_empty()
    {
        return None;
    }
    let infohash = hash.to_ascii_lowercase();
    let magnet = librqbit::Magnet::from_id20(infohash.parse().ok()?, vec![], None).to_string();
    Some(SearchResult {
        name,
        infohash: Some(infohash),
        magnet,
        torrent_url: None,
        size_bytes,
        seeders,
        source: source.into(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_mapping_rejects_invalid_hashes_sentinels_and_non_video_categories() {
        let rows=parse_general(serde_json::json!([
            {"id":"1","info_hash":"A".repeat(40),"name":"Video","size":"1234","seeders":"3","category":"201"},
            {"id":"0","info_hash":"0".repeat(40),"name":"No results","category":"200"},
            {"id":"2","info_hash":"B".repeat(40),"name":"Software","category":"300"},
            {"id":"3","info_hash":"bad","name":"Invalid","category":"201"}
        ])).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].size_bytes, 1234);
        assert_eq!(rows[0].infohash, Some("a".repeat(40)));
        assert_eq!(
            librqbit::Magnet::parse(&rows[0].magnet)
                .unwrap()
                .as_id20()
                .unwrap()
                .as_string(),
            "a".repeat(40)
        );
        let movies=parse_movies(serde_json::json!({"data":{"movies":[{"title":"Film","torrents":[{"hash":"b".repeat(40),"quality":"2160p","size_bytes":999}]}]}})).unwrap();
        assert!(movies[0].name.contains("2160p"));
        assert_eq!(movies[0].size_bytes, 999);
        assert!(parse_general(serde_json::json!({"error":"Unavailable"})).is_err());
        assert!(parse_movies(serde_json::json!({"error":"Unavailable"})).is_err());
    }
    #[test]
    fn archive_metadata_urls_keep_identifiers_in_their_path_segment() {
        let results = parse_archive(
            serde_json::json!({"response":{"docs":[{"identifier":"sample?x=1","title":"Sample"}]}}),
        )
        .unwrap();
        let url = reqwest::Url::parse(results[0].torrent_url.as_ref().unwrap()).unwrap();
        assert_eq!(url.host_str(), Some("archive.org"));
        assert!(url.query().is_none());
        assert!(url.path().contains("%3F"));
    }
}
