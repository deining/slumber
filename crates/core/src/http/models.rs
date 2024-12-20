//! HTTP-related data types. The primary term here to know is "exchange". An
//! exchange is a single HTTP request-response pair. It can be in various
//! stages, meaning the request or response may not actually be present, if the
//! exchange is incomplete or failed.

use crate::{
    collection::{Authentication, ProfileId, RecipeBody, RecipeId},
    http::{
        cereal,
        content_type::{ContentType, ResponseContent},
    },
    template::Template,
};
use anyhow::Context;
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use derive_more::{Display, From, FromStr};
use mime::Mime;
use reqwest::{
    header::{self, HeaderMap},
    Body, Client, Method, Request, StatusCode, Url,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::{Debug, Write},
    sync::Arc,
};
use thiserror::Error;
use tracing::error;
use uuid::Uuid;

/// Unique ID for a single request. Can also be used to refer to the
/// corresponding [Exchange] or [ResponseRecord].
#[derive(
    Copy,
    Clone,
    Debug,
    Display,
    Eq,
    FromStr,
    Hash,
    PartialEq,
    Serialize,
    Deserialize,
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

/// The first stage in building a request. This contains the initialization data
/// needed to build a request. This holds owned data because we need to be able
/// to move it between tasks as part of the build process, which requires it
/// to be `'static`.
pub struct RequestSeed {
    /// Unique ID for this request
    pub id: RequestId,
    /// Recipe from which the request should be rendered
    pub recipe_id: RecipeId,
    /// Configuration for the build
    pub options: BuildOptions,
}

impl RequestSeed {
    pub fn new(recipe_id: RecipeId, options: BuildOptions) -> Self {
        Self {
            id: RequestId::new(),
            recipe_id,
            options,
        }
    }
}

/// Options for modifying a recipe during a build, corresponding to changes the
/// user can make in the TUI (as opposed to the collection file). This is
/// helpful for applying temporary modifications made by the user. By providing
/// this in a separate struct, we prevent the need to clone, modify, and pass
/// recipes everywhere. Recipes could be very large so cloning may be expensive,
/// and this options layer makes the available modifications clear and
/// restricted.
///
/// These store *indexes* rather than keys because keys may not be necessarily
/// unique (e.g. in the case of query params). Technically some could use keys
/// and some could use indexes, but I chose consistency.
#[derive(Debug, Default)]
#[cfg_attr(any(test, feature = "test"), derive(PartialEq))]
pub struct BuildOptions {
    /// Authentication can be overridden, but not disabled. For simplicity,
    /// the override is wholesale rather than by field.
    pub authentication: Option<Authentication>,
    pub headers: BuildFieldOverrides,
    pub query_parameters: BuildFieldOverrides,
    pub form_fields: BuildFieldOverrides,
    /// Override body. This should *not* be used for form bodies, since those
    /// can be override on a field-by-field basis.
    pub body: Option<RecipeBody>,
}

/// A collection of modifications made to a particular section of a recipe
/// (query params, headers, etc.). See [BuildFieldOverride]
#[derive(Debug, Default)]
#[cfg_attr(any(test, feature = "test"), derive(PartialEq))]
pub struct BuildFieldOverrides {
    overrides: HashMap<usize, BuildFieldOverride>,
}

impl BuildFieldOverrides {
    /// Get the value to be used for a particular field, keyed by index. Return
    /// `None` if the field should be dropped from the request, and use the
    /// given default if no override is provided.
    pub fn get<'a>(
        &'a self,
        index: usize,
        default: &'a Template,
    ) -> Option<&'a Template> {
        match self.overrides.get(&index) {
            Some(BuildFieldOverride::Omit) => None,
            Some(BuildFieldOverride::Override(template)) => Some(template),
            None => Some(default),
        }
    }
}

impl FromIterator<(usize, BuildFieldOverride)> for BuildFieldOverrides {
    fn from_iter<T: IntoIterator<Item = (usize, BuildFieldOverride)>>(
        iter: T,
    ) -> Self {
        Self {
            overrides: HashMap::from_iter(iter),
        }
    }
}

/// Modifications made to a single field (query param, header, etc.) in a
/// recipe
#[derive(Debug)]
#[cfg_attr(any(test, feature = "test"), derive(PartialEq))]
pub enum BuildFieldOverride {
    /// Do not include this field in the recipe
    Omit,
    /// Replace the value for this field with a different template
    Override(Template),
}

/// A request ready to be launched into through the stratosphere. This is
/// basically a two-part ticket: the request is the part we'll hand to the HTTP
/// engine to be launched, and the record is the ticket stub we'll keep for
/// ourselves (to display to the user
#[derive(Debug)]
pub struct RequestTicket {
    /// A record of the request that we can hang onto and persist
    pub(super) record: Arc<RequestRecord>,
    /// reqwest client that should be used to launch the request
    pub(super) client: Client,
    /// Our brave little astronaut, ready to be launched...
    pub(super) request: Request,
}

impl RequestTicket {
    pub fn record(&self) -> &Arc<RequestRecord> {
        &self.record
    }
}

/// A complete request+response pairing. This is generated by
/// [RequestTicket::send] when a response is received successfully for a sent
/// request.
#[derive(Debug)]
#[cfg_attr(any(test, feature = "test"), derive(PartialEq))]
pub struct Exchange {
    /// ID to uniquely refer to this exchange
    pub id: RequestId,
    /// What we said. Use an Arc so the view can hang onto it.
    pub request: Arc<RequestRecord>,
    /// What we heard
    pub response: ResponseRecord,
    /// When was the request sent to the server?
    pub start_time: DateTime<Utc>,
    /// When did we finish receiving the *entire* response?
    pub end_time: DateTime<Utc>,
}

impl Exchange {
    /// Get the elapsed time for this request
    pub fn duration(&self) -> Duration {
        self.end_time - self.start_time
    }
}

/// Metadata about an exchange. Useful in lists where request/response content
/// isn't needed.
#[derive(Copy, Clone, Debug)]
pub struct ExchangeSummary {
    pub id: RequestId,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub status: StatusCode,
}

impl From<&Exchange> for ExchangeSummary {
    fn from(exchange: &Exchange) -> Self {
        Self {
            id: exchange.id,
            start_time: exchange.start_time,
            end_time: exchange.end_time,
            status: exchange.response.status,
        }
    }
}

/// Data for an HTTP request. This is similar to [reqwest::Request], but differs
/// in some key ways:
/// - Each [reqwest::Request] can only exist once (from creation to sending),
///   whereas a record can be hung onto after the launch to keep showing it on
///   screen.
/// - This stores additional Slumber-specific metadata
///
/// This intentionally does *not* implement `Clone`, because request data could
/// potentially be large so we want to be intentional about duplicating it only
/// when necessary.
///
/// Remove serde impls in https://github.com/LucasPickering/slumber/issues/306
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "test"), derive(PartialEq))]
pub struct RequestRecord {
    /// Unique ID for this request
    pub id: RequestId,
    /// The profile used to render this request (for historical context)
    pub profile_id: Option<ProfileId>,
    /// The recipe used to generate this request (for historical context)
    pub recipe_id: RecipeId,

    #[serde(with = "cereal::serde_method")]
    pub method: Method,
    /// URL, including query params/fragment
    pub url: Url,
    #[serde(with = "cereal::serde_header_map")]
    pub headers: HeaderMap,
    /// Body content as bytes. This should be decoded as needed. This will
    /// **not** be populated for bodies that are above the "large" threshold.
    pub body: Option<Bytes>,
}

impl RequestRecord {
    /// Create a new request record from data and metadata. This is the
    /// canonical way to create a record for a new request. This should
    /// *not* be build directly, and instead the data should copy data out of
    /// a [reqwest::Request]. This is to prevent duplicating request
    /// construction logic.
    ///
    /// This will clone all data out of the request. This could potentially be
    /// expensive but we don't have any choice if we want to send it to the
    /// server and show it in the TUI at the same time
    pub(super) fn new(
        seed: RequestSeed,
        profile_id: Option<ProfileId>,
        request: &Request,
        max_body_size: usize,
    ) -> Self {
        Self {
            id: seed.id,
            profile_id,
            recipe_id: seed.recipe_id,

            method: request.method().clone(),
            url: request.url().clone(),
            headers: request.headers().clone(),
            body: request
                .body()
                // Stream bodies and bodies over a certain size threshold are
                // thrown away. Storing request bodies in general doesn't
                // provide a ton of value, so we shouldn't do it at the expense
                // of performance
                .and_then(Body::as_bytes)
                .filter(|body| body.len() <= max_body_size)
                .map(|body| body.to_owned().into()),
        }
    }

    /// Generate a cURL command equivalent to this request
    ///
    /// This only fails if one of the headers or body is binary and can't be
    /// converted to UTF-8.
    pub fn to_curl(&self) -> anyhow::Result<String> {
        let mut buf = String::new();

        // These writes are all infallible because we're writing to a string,
        // but use ? because it's shorter than unwrap().
        let method = &self.method;
        let url = &self.url;
        write!(&mut buf, "curl -X{method} --url '{url}'")?;

        for (header, value) in &self.headers {
            let value =
                value.to_str().context("Error decoding header value")?;
            write!(&mut buf, " --header '{header}: {value}'")?;
        }

        if let Some(body) = &self.body_str()? {
            write!(&mut buf, " --data '{body}'")?;
        }

        Ok(buf)
    }

    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }

    /// Get the body of the request, decoded as UTF-8. Returns an error if the
    /// body isn't valid UTF-8.
    pub fn body_str(&self) -> anyhow::Result<Option<&str>> {
        if let Some(body) = &self.body {
            Ok(Some(
                std::str::from_utf8(body).context("Error decoding body")?,
            ))
        } else {
            Ok(None)
        }
    }
}

#[cfg(any(test, feature = "test"))]
impl crate::test_util::Factory for RequestRecord {
    fn factory(_: ()) -> Self {
        Self {
            id: RequestId::new(),
            profile_id: None,
            recipe_id: RecipeId::factory(()),
            method: reqwest::Method::GET,
            url: "http://localhost/url".parse().unwrap(),
            headers: HeaderMap::new(),
            body: None,
        }
    }
}

/// Customize profile and recipe ID
#[cfg(any(test, feature = "test"))]
impl crate::test_util::Factory<(Option<ProfileId>, RecipeId)>
    for RequestRecord
{
    fn factory((profile_id, recipe_id): (Option<ProfileId>, RecipeId)) -> Self {
        use crate::test_util::header_map;
        Self {
            id: RequestId::new(),
            profile_id,
            recipe_id,
            method: reqwest::Method::GET,
            url: "http://localhost/url".parse().unwrap(),
            headers: header_map([
                ("Accept", "application/json"),
                ("Content-Type", "application/json"),
                ("User-Agent", "slumber"),
            ]),
            body: None,
        }
    }
}

#[cfg(any(test, feature = "test"))]
impl crate::test_util::Factory for ResponseRecord {
    fn factory(_: ()) -> Self {
        Self {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: ResponseBody::default(),
        }
    }
}

#[cfg(any(test, feature = "test"))]
impl crate::test_util::Factory<StatusCode> for ResponseRecord {
    fn factory(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: ResponseBody::default(),
        }
    }
}

#[cfg(any(test, feature = "test"))]
impl crate::test_util::Factory for Exchange {
    fn factory(_: ()) -> Self {
        Self::factory((None, RecipeId::factory(())))
    }
}

/// Customize recipe ID
#[cfg(any(test, feature = "test"))]
impl crate::test_util::Factory<RecipeId> for Exchange {
    fn factory(params: RecipeId) -> Self {
        Self::factory((None, params))
    }
}

/// Customize profile and recipe ID
#[cfg(any(test, feature = "test"))]
impl crate::test_util::Factory<(Option<ProfileId>, RecipeId)> for Exchange {
    fn factory(params: (Option<ProfileId>, RecipeId)) -> Self {
        Self::factory((
            RequestRecord::factory(params),
            ResponseRecord::factory(()),
        ))
    }
}

/// Customize profile and recipe ID
#[cfg(any(test, feature = "test"))]
impl crate::test_util::Factory<(RequestRecord, ResponseRecord)> for Exchange {
    fn factory((request, response): (RequestRecord, ResponseRecord)) -> Self {
        Self {
            id: request.id,
            request: request.into(),
            response,
            start_time: Utc::now(),
            end_time: Utc::now(),
        }
    }
}

/// A resolved HTTP response, with all content loaded and ready to be displayed
/// to the user. A simpler alternative to [reqwest::Response], because there's
/// no way to access all resolved data on that type at once. Resolving the
/// response body requires moving the response.
///
/// This intentionally does not implement Clone, because responses could
/// potentially be very large.
///
/// Remove serde impls in https://github.com/LucasPickering/slumber/issues/306
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "test"), derive(PartialEq))]
pub struct ResponseRecord {
    #[serde(with = "cereal::serde_status_code")]
    pub status: StatusCode,
    #[serde(with = "cereal::serde_header_map")]
    pub headers: HeaderMap,
    pub body: ResponseBody,
}

impl ResponseRecord {
    /// Stored the parsed form of this request's body
    pub fn set_parsed_body(&mut self, body: Box<dyn ResponseContent>) {
        self.body.parsed = Some(body);
    }

    /// Get the content type of the response body, according to the
    /// `Content-Type` header
    pub fn content_type(&self) -> Option<ContentType> {
        // If we've parsed the body, we'll have the content type present. If
        // not, check the header now
        self.body
            .parsed()
            .map(|content| content.content_type())
            .or_else(|| ContentType::from_headers(&self.headers).ok())
    }

    /// Get a suggested file name for the content of this response. First we'll
    /// check the Content-Disposition header. If it's missing or doesn't have a
    /// file name, we'll check the Content-Type to at least guess at an
    /// extension.
    pub fn file_name(&self) -> Option<String> {
        self.headers
            .get(header::CONTENT_DISPOSITION)
            .and_then(|value| {
                // Parse header for the `filename="{}"` parameter
                // https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Disposition
                let value = value.to_str().ok()?;
                value.split(';').find_map(|part| {
                    let (key, value) = part.trim().split_once('=')?;
                    if key == "filename" {
                        Some(value.trim_matches('"').to_owned())
                    } else {
                        None
                    }
                })
            })
            .or_else(|| {
                // Grab the extension from the Content-Type header. Don't use
                // self.conten_type() because we want to accept unknown types.
                let content_type = self.headers.get(header::CONTENT_TYPE)?;
                let mime: Mime = content_type.to_str().ok()?.parse().ok()?;
                Some(format!("data.{}", mime.subtype()))
            })
    }
}

pub enum ParseMode {
    Immediate,
    Background {
        callback: Box<dyn 'static + FnOnce(Box<dyn ResponseContent>) + Send>,
    },
}

/// HTTP response body. Content is stored as bytes because it may not
/// necessarily be valid UTF-8. Converted to text only as needed.
#[derive(Default, Deserialize)]
#[serde(from = "Bytes")] // Can't use into=Bytes because that requires cloning
pub struct ResponseBody {
    /// Raw body
    data: Bytes,
    /// For responses of a known content type, we can parse the body into a
    /// real data structure. This is populated manually; Call
    /// [ResponseRecord::parse_body] to set the parsed body. This uses a lock
    /// so it can be parsed and populated in a background thread.
    #[serde(skip)]
    parsed: Option<Box<dyn ResponseContent>>,
}

impl ResponseBody {
    pub fn new(data: Bytes) -> Self {
        Self {
            data,
            parsed: Default::default(),
        }
    }

    /// Raw content bytes
    pub fn bytes(&self) -> &Bytes {
        &self.data
    }

    /// Owned raw content bytes
    pub fn into_bytes(self) -> Bytes {
        self.data
    }

    /// Get bytes as text, if valid UTF-8
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.data).ok()
    }

    /// Get body size, in bytes
    pub fn size(&self) -> usize {
        self.bytes().len()
    }

    /// Get the parsed version of this body. Must haved call
    /// [ResponseRecord::parse_body] first to actually do the parse. Parsing has
    /// to be done on the parent because we don't have access to the
    /// `Content-Type` header here, which tells us how to parse.
    ///
    /// Return `None` if parsing either hasn't happened yet, or failed.
    pub fn parsed(&self) -> Option<&dyn ResponseContent> {
        self.parsed.as_deref()
    }
}

impl Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't print the actual body because it could be huge
        f.debug_tuple("Body")
            .field(&format!("<{} bytes>", self.data.len()))
            .finish()
    }
}

impl From<Bytes> for ResponseBody {
    fn from(bytes: Bytes) -> Self {
        Self::new(bytes)
    }
}

impl Serialize for ResponseBody {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Serialize just the bytes, everything else is derived
        self.data.serialize(serializer)
    }
}

impl From<Vec<u8>> for ResponseBody {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value.into())
    }
}

#[cfg(test)]
impl From<&str> for ResponseBody {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned().into())
    }
}

#[cfg(test)]
impl From<&[u8]> for ResponseBody {
    fn from(value: &[u8]) -> Self {
        Self::new(value.to_owned().into())
    }
}

#[cfg(test)]
impl From<serde_json::Value> for ResponseBody {
    fn from(value: serde_json::Value) -> Self {
        Self::new(value.to_string().into())
    }
}

#[cfg(any(test, feature = "test"))]
impl PartialEq for ResponseBody {
    fn eq(&self, other: &Self) -> bool {
        // Ignore derived data
        self.data == other.data
    }
}

/// An error that can occur while *building* a request
#[derive(Debug, Error)]
#[error("Error building request {id}")]
pub struct RequestBuildError {
    /// There are multiple possible error types and anyhow's Error makes
    /// display easier
    #[source]
    pub error: anyhow::Error,

    /// ID of the profile being rendered under
    pub profile_id: Option<ProfileId>,
    /// ID of the recipe being rendered
    pub recipe_id: RecipeId,
    /// ID of the failed request
    pub id: RequestId,
    /// When did the build start?
    pub start_time: DateTime<Utc>,
    /// When did the build end, i.e. when did the error occur?
    pub end_time: DateTime<Utc>,
}

#[cfg(any(test, feature = "test"))]
impl PartialEq for RequestBuildError {
    fn eq(&self, other: &Self) -> bool {
        self.profile_id == other.profile_id
            && self.recipe_id == other.recipe_id
            && self.id == other.id
            && self.start_time == other.start_time
            && self.end_time == other.end_time
            && self.error.to_string() == other.error.to_string()
    }
}

/// An error that can occur during a request. This does *not* including building
/// errors.
#[derive(Debug, Error)]
#[error(
    "Error executing request for `{}` (request `{}`)",
    .request.recipe_id,
    .request.id,
)]
pub struct RequestError {
    /// Underlying error. This will always be a `reqwest::Error`, but wrapping
    /// it in anyhow makes it easier to render
    #[source]
    pub error: anyhow::Error,

    /// The request that caused all this ruckus
    pub request: Arc<RequestRecord>,
    /// When was the request launched?
    pub start_time: DateTime<Utc>,
    /// When did the error occur?
    pub end_time: DateTime<Utc>,
}

#[cfg(any(test, feature = "test"))]
impl PartialEq for RequestError {
    fn eq(&self, other: &Self) -> bool {
        self.error.to_string() == other.error.to_string()
            && self.request == other.request
            && self.start_time == other.start_time
            && self.end_time == other.end_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{header_map, Factory};
    use indexmap::indexmap;
    use rstest::rstest;
    use serde_json::json;

    #[rstest]
    #[case::content_disposition(
        ResponseRecord {
            headers: header_map(indexmap! {
                "content-disposition" => "form-data;name=\"field\"; filename=\"fish.png\"",
                "content-type" => "image/png",
            }),
            ..ResponseRecord::factory(())
        },
        Some("fish.png")
    )]
    #[case::content_type_known(
        ResponseRecord {
            headers: header_map(indexmap! {
                "content-disposition" => "form-data",
                "content-type" => "application/json",
            }),
            ..ResponseRecord::factory(())
        },
        Some("data.json")
    )]
    #[case::content_type_unknown(
        ResponseRecord {
            headers: header_map(indexmap! {
                "content-disposition" => "form-data",
                "content-type" => "image/jpeg",
            }),
            ..ResponseRecord::factory(())
        },
        Some("data.jpeg")
    )]
    #[case::none(ResponseRecord::factory(()), None)]
    fn test_file_name(
        #[case] response: ResponseRecord,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(response.file_name().as_deref(), expected);
    }

    #[test]
    fn test_to_curl() {
        let headers = indexmap! {
            "accept" => "application/json",
            "content-type" => "application/json",
        };
        let body = json!({"data": "value"});
        let request = RequestRecord {
            method: Method::DELETE,
            headers: header_map(headers),
            body: Some(serde_json::to_vec(&body).unwrap().into()),
            ..RequestRecord::factory(())
        };

        assert_eq!(
            request.to_curl().unwrap(),
            "curl -XDELETE --url 'http://localhost/url' \
            --header 'accept: application/json' \
            --header 'content-type: application/json' \
            --data '{\"data\":\"value\"}'"
        );
    }
}
