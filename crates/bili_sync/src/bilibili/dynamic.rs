use anyhow::{Context, Result, anyhow};
use async_stream::try_stream;
use chrono::{DateTime, Utc};
use futures::Stream;
use reqwest::Method;
use serde_json::Value;

use crate::bilibili::{BiliClient, Credential, ErrorForStatusExt, MIXIN_KEY, Validate, VideoInfo, WbiSign};

pub struct Dynamic<'a> {
    client: &'a BiliClient,
    pub upper_id: String,
    credential: &'a Credential,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DynamicPost {
    pub dynamic_id: String,
    pub author_mid: i64,
    pub author_name: String,
    pub pub_time: DateTime<Utc>,
    pub text: String,
    pub images: Vec<DynamicPostImage>,
    pub raw_json: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicPostImage {
    pub url: String,
    pub width: u64,
    pub height: u64,
}

impl<'a> Dynamic<'a> {
    pub fn new(client: &'a BiliClient, upper_id: String, credential: &'a Credential) -> Self {
        Self {
            client,
            upper_id,
            credential,
        }
    }

    pub async fn get_dynamics(&self, offset: Option<String>) -> Result<Value> {
        self.get_dynamics_by_type(offset, "video").await
    }

    pub async fn get_post_dynamics(&self, offset: Option<String>) -> Result<Value> {
        self.get_dynamics_by_type(offset, "all").await
    }

    async fn get_dynamics_by_type(&self, offset: Option<String>, dynamic_type: &str) -> Result<Value> {
        self.client
            .request(
                Method::GET,
                "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/space",
                self.credential,
            )
            .await
            .query(&[
                ("host_mid", self.upper_id.as_str()),
                ("offset", offset.as_deref().unwrap_or("")),
                ("type", dynamic_type),
            ])
            .wbi_sign(MIXIN_KEY.load().as_deref())?
            .send()
            .await?
            .error_for_status_ext()?
            .json::<serde_json::Value>()
            .await?
            .validate()
    }

    pub fn into_post_stream(self) -> impl Stream<Item = Result<DynamicPost>> + 'a {
        try_stream! {
            let mut offset = None;
            loop {
                let mut res = self
                    .get_post_dynamics(offset.take())
                    .await
                    .with_context(|| "failed to get dynamic posts")?;
                let items = match res["data"]["items"].as_array_mut() {
                    Some(items) if !items.is_empty() => items,
                    _ => {
                        if offset.is_none() {
                            break;
                        }
                        Err(anyhow!("no dynamics found in offset {:?}", offset))?
                    }
                };
                for item in items.iter() {
                    if let Some(post) = DynamicPost::parse(item)? {
                        yield post;
                    }
                }
                if let (Some(has_more), Some(new_offset)) =
                    (res["data"]["has_more"].as_bool(), res["data"]["offset"].as_str())
                {
                    if !has_more {
                        break;
                    }
                    offset = Some(new_offset.to_string());
                } else {
                    Err(anyhow!("no has_more or offset found"))?;
                }
            }
        }
    }

    pub fn into_video_stream(self) -> impl Stream<Item = Result<VideoInfo>> + 'a {
        try_stream! {
            let mut offset = None;
            loop {
                let mut res = self
                    .get_dynamics(offset.take())
                    .await
                    .with_context(|| "failed to get dynamics")?;
                let items = match res["data"]["items"].as_array_mut() {
                    Some(items) if !items.is_empty() => items,
                    _ => {
                        if offset.is_none() {
                            break;
                        }
                        Err(anyhow!("no dynamics found in offset {:?}", offset))?
                    }
                };
                for item in items.iter_mut() {
                    if item["type"].as_str().is_none_or(|t| t != "DYNAMIC_TYPE_AV") {
                        continue;
                    }
                    let pub_ts = item["modules"]["module_author"]["pub_ts"].take();
                    let pub_dt = pub_ts
                        .as_i64()
                        .or_else(|| pub_ts.as_str().and_then(|s| s.parse::<i64>().ok()))
                        .and_then(DateTime::from_timestamp_secs)
                        .with_context(|| format!("invalid pub_ts: {:?}", pub_ts))?;
                    let mut video_info: VideoInfo =
                        serde_json::from_value(item["modules"]["module_dynamic"]["major"]["archive"].take())?;
                    // 这些地方不使用 let else 是因为 try_stream! 宏不支持
                    if let VideoInfo::Dynamic { ref mut pubtime, .. } = video_info {
                        *pubtime = pub_dt;
                        yield video_info;
                    } else {
                        Err(anyhow!("video info is not dynamic"))?;
                    }
                }
                if let (Some(has_more), Some(new_offset)) =
                    (res["data"]["has_more"].as_bool(), res["data"]["offset"].as_str())
                {
                    if !has_more {
                        break;
                    }
                    offset = Some(new_offset.to_string());
                } else {
                    Err(anyhow!("no has_more or offset found"))?;
                }
            }
        }
    }
}

impl DynamicPost {
    fn parse(item: &Value) -> Result<Option<Self>> {
        let Some(dynamic_type) = item["type"].as_str() else {
            return Ok(None);
        };
        if !matches!(dynamic_type, "DYNAMIC_TYPE_DRAW" | "DYNAMIC_TYPE_WORD") {
            return Ok(None);
        }

        let pub_ts = &item["modules"]["module_author"]["pub_ts"];
        let pub_time = value_as_i64(pub_ts)
            .and_then(DateTime::from_timestamp_secs)
            .with_context(|| format!("invalid dynamic post pub_ts: {:?}", pub_ts))?;
        let author_mid = value_as_i64(&item["modules"]["module_author"]["mid"])
            .with_context(|| format!("invalid dynamic post author mid: {}", item))?;
        let author_name = item["modules"]["module_author"]["name"]
            .as_str()
            .with_context(|| format!("invalid dynamic post author name: {}", item))?
            .to_string();
        let dynamic_id = value_as_string(&item["id_str"])
            .or_else(|| value_as_string(&item["id"]))
            .with_context(|| format!("invalid dynamic post id: {}", item))?;

        Ok(Some(Self {
            dynamic_id,
            author_mid,
            author_name,
            pub_time,
            text: extract_text(item),
            images: extract_images(item),
            raw_json: item.clone(),
        }))
    }
}

fn extract_text(item: &Value) -> String {
    [
        "/modules/module_dynamic/desc/text",
        "/modules/module_dynamic/major/opus/summary/text",
    ]
    .into_iter()
    .filter_map(|path| item.pointer(path)?.as_str())
    .find(|text| !text.is_empty())
    .unwrap_or_default()
    .to_string()
}

fn extract_images(item: &Value) -> Vec<DynamicPostImage> {
    [
        "/modules/module_dynamic/major/draw/items",
        "/modules/module_dynamic/major/opus/pics",
    ]
    .into_iter()
    .filter_map(|path| item.pointer(path)?.as_array())
    .flatten()
    .filter_map(parse_image)
    .collect()
}

fn parse_image(image: &Value) -> Option<DynamicPostImage> {
    let url = image["src"]
        .as_str()
        .or_else(|| image["url"].as_str())
        .or_else(|| image["img_src"].as_str())?
        .to_string();
    Some(DynamicPostImage {
        url,
        width: value_as_u64(&image["width"]).unwrap_or_default(),
        height: value_as_u64(&image["height"]).unwrap_or_default(),
    })
}

fn value_as_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}
