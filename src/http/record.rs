//! HTTP-related data types

use crate::{
    collection::{ProfileId, RecipeId},
    http::{ContentType, ResponseContent},
    util::ResultExt,
};
use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use derive_more::{Display, From};
use indexmap::IndexMap;
use reqwest::{
    header::{self, HeaderMap, HeaderValue},
    Method, StatusCode,
};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use thiserror::Error;
use uuid::Uuid;

/// An error that can occur while *building* a request
#[derive(Debug, Error)]
#[error("Error building request {id}")]
pub struct RequestBuildError {
    /// ID of the failed request
    pub id: RequestId,
    /// There are a lot of different possible error types, so storing an anyhow
    /// is easiest
    #[source]
    pub error: anyhow::Error,
}

/// An error that can occur during a request. This does *not* including building
/// errors.
#[derive(Debug, Error)]
#[error("Error executing request {}", "request.id")]
pub struct RequestError {
    #[source]
    pub error: reqwest::Error,
    /// The request that caused all this ruckus
    pub request: Request,
    /// When was the request launched?
    pub start_time: DateTime<Utc>,
    /// When did the error occur?
    pub end_time: DateTime<Utc>,
}

/// Unique ID for a single launched request
#[derive(
    Copy, Clone, Debug, Display, Eq, Hash, PartialEq, Serialize, Deserialize,
)]
pub struct RequestId(pub Uuid);

impl RequestId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// A complete request+response pairing. This is generated by [HttpEngine::send]
/// when a response is received successfully for a sent request.
#[derive(Debug)]
pub struct RequestRecord {
    /// ID to uniquely refer to this record. Useful for historical records.
    pub id: RequestId,
    /// What we said
    pub request: Request,
    // What we heard
    pub response: Response,
    /// When was the request sent to the server?
    pub start_time: DateTime<Utc>,
    /// When did we finish receiving the *entire* response?
    pub end_time: DateTime<Utc>,
}

impl RequestRecord {
    /// Get the elapsed time for this request
    pub fn duration(&self) -> Duration {
        self.end_time - self.start_time
    }
}

/// A single instance of an HTTP request. Simpler alternative to
/// [reqwest::Request] that suits our needs better. This intentionally does
/// *not* implement `Clone`, because each request is unique.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// Unique ID for this request. Private to prevent mutation
    pub id: RequestId,
    /// The profile used to render this request (for historical context)
    pub profile_id: Option<ProfileId>,
    /// The recipe used to generate this request (for historical context)
    pub recipe_id: RecipeId,

    #[serde(with = "serde_method")]
    pub method: Method,
    pub url: String,
    #[serde(with = "serde_header_map")]
    pub headers: HeaderMap,
    pub query: IndexMap<String, String>,
    /// Text body content. At some point we'll support other formats (binary,
    /// streaming from file, etc.)
    pub body: Option<String>,
}

/// A resolved HTTP response, with all content loaded and ready to be displayed
/// to the user. A simpler alternative to [reqwest::Response], because there's
/// no way to access all resolved data on that type at once. Resolving the
/// response body requires moving the response.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    #[serde(with = "serde_status_code")]
    pub status: StatusCode,
    #[serde(with = "serde_header_map")]
    pub headers: HeaderMap,
    pub body: Body,
}

impl Response {
    /// Parse the body of this response, based on its `content-type` header
    pub fn parse_body(&self) -> anyhow::Result<Box<dyn ResponseContent>> {
        ContentType::parse_response(self)
            .context("Error parsing response body")
            .traced()
    }

    /// Get the value of the `content-type` header
    pub fn content_type(&self) -> Option<&[u8]> {
        self.headers
            .get(header::CONTENT_TYPE)
            .map(HeaderValue::as_bytes)
    }

    /// Make the response body pretty, if possible. This fails if the response
    /// has an unknown content-type, or if the body doesn't parse according to
    /// the content-type.
    pub fn prettify_body(&self) -> anyhow::Result<String> {
        Ok(self.parse_body()?.prettify())
    }
}

/// HTTP response body. Right now we store as text only, but at some point
/// should add support for binary responses
#[derive(Default, From, Serialize, Deserialize)]
pub struct Body(String);

impl Body {
    pub fn new(text: String) -> Self {
        Self(text)
    }

    pub fn text(&self) -> &str {
        &self.0
    }

    pub fn into_text(self) -> String {
        self.0
    }
}

impl Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't print the actual body because it could be huge
        f.debug_tuple("Body")
            .field(&format!("<{} bytes>", self.0.len()))
            .finish()
    }
}

impl From<&str> for Body {
    fn from(value: &str) -> Self {
        Body::new(value.into())
    }
}

/// Serialization/deserialization for [reqwest::Method]
mod serde_method {
    use super::*;
    use serde::{de, Deserializer, Serializer};

    pub fn serialize<S>(
        method: &Method,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(method.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Method, D::Error>
    where
        D: Deserializer<'de>,
    {
        <&str>::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// Serialization/deserialization for [reqwest::HeaderMap]
mod serde_header_map {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};
    use serde::{de, Deserializer, Serializer};

    pub fn serialize<S>(
        headers: &HeaderMap,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // HeaderValue -> str is fallible, so we'll serialize as bytes instead
        <IndexMap<&str, &[u8]>>::serialize(
            &headers
                .into_iter()
                .map(|(k, v)| (k.as_str(), v.as_bytes()))
                .collect(),
            serializer,
        )
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HeaderMap, D::Error>
    where
        D: Deserializer<'de>,
    {
        <IndexMap<String, Vec<u8>>>::deserialize(deserializer)?
            .into_iter()
            .map::<Result<(HeaderName, HeaderValue), _>, _>(|(k, v)| {
                // Fallibly map each key and value to header types
                Ok((
                    k.try_into().map_err(de::Error::custom)?,
                    v.try_into().map_err(de::Error::custom)?,
                ))
            })
            .collect()
    }
}

/// Serialization/deserialization for [reqwest::StatusCode]
mod serde_status_code {
    use super::*;
    use serde::{de, Deserializer, Serializer};

    pub fn serialize<S>(
        status_code: &StatusCode,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u16(status_code.as_u16())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<StatusCode, D::Error>
    where
        D: Deserializer<'de>,
    {
        StatusCode::from_u16(u16::deserialize(deserializer)?)
            .map_err(de::Error::custom)
    }
}
