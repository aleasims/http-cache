#![forbid(unsafe_code, future_incompatible)]
#![deny(
    missing_docs,
    missing_debug_implementations,
    missing_copy_implementations,
    nonstandard_style,
    unused_qualifications,
    unused_import_braces,
    unused_extern_crates,
    trivial_casts,
    trivial_numeric_casts
)]
#![allow(clippy::doc_lazy_continuation)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//! A caching middleware that follows HTTP caching rules, thanks to
//! [`http-cache-semantics`](https://github.com/kornelski/rusty-http-cache-semantics).
//! By default, it uses [`cacache`](https://github.com/zkat/cacache-rs) as the backend cache manager.
//!
//! ## Features
//!
//! The following features are available. By default `manager-cacache` and `cacache-async-std` are enabled.
//!
//! - `manager-cacache` (default): enable [cacache](https://github.com/zkat/cacache-rs),
//! a high-performance disk cache, backend manager.
//! - `cacache-async-std` (default): enable [async-std](https://github.com/async-rs/async-std) runtime support for cacache.
//! - `cacache-tokio` (disabled): enable [tokio](https://github.com/tokio-rs/tokio) runtime support for cacache.
//! - `manager-moka` (disabled): enable [moka](https://github.com/moka-rs/moka),
//! a high-performance in-memory cache, backend manager.
//! - `with-http-types` (disabled): enable [http-types](https://github.com/http-rs/http-types)
//! type conversion support
mod error;
mod managers;

use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt::{self, Debug},
    str::FromStr,
    sync::Arc,
    time::SystemTime,
};

use bytes::{BufMut, Bytes};
use futures::StreamExt;
use http::{header::CACHE_CONTROL, request, response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyDataStream, BodyExt, Full};
use http_cache_semantics::{AfterResponse, BeforeRequest, CachePolicy};
use serde::{Deserialize, Serialize};
use url::Url;

pub use error::{BadHeader, BadVersion, BoxError, Result};

#[cfg(feature = "manager-cacache")]
pub use managers::cacache::CACacheManager;

#[cfg(feature = "manager-moka")]
pub use managers::moka::MokaManager;

// Exposing the moka cache for convenience, renaming to avoid naming conflicts
#[cfg(feature = "manager-moka")]
#[cfg_attr(docsrs, doc(cfg(feature = "manager-moka")))]
pub use moka::future::{Cache as MokaCache, CacheBuilder as MokaCacheBuilder};

// Custom headers used to indicate cache status (hit or miss)
/// `x-cache` header: Value will be HIT if the response was served from cache, MISS if not
pub const XCACHE: &str = "x-cache";
/// `x-cache-lookup` header: Value will be HIT if a response existed in cache, MISS if not
pub const XCACHELOOKUP: &str = "x-cache-lookup";

/// Represents a basic cache status
/// Used in the custom headers `x-cache` and `x-cache-lookup`
#[derive(Debug, Copy, Clone)]
pub enum HitOrMiss {
    /// Yes, there was a hit
    HIT,
    /// No, there was no hit
    MISS,
}

impl fmt::Display for HitOrMiss {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::HIT => write!(f, "HIT"),
            Self::MISS => write!(f, "MISS"),
        }
    }
}

/// Represents an HTTP version
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub enum HttpVersion {
    /// HTTP Version 0.9
    #[serde(rename = "HTTP/0.9")]
    Http09,
    /// HTTP Version 1.0
    #[serde(rename = "HTTP/1.0")]
    Http10,
    /// HTTP Version 1.1
    #[serde(rename = "HTTP/1.1")]
    Http11,
    /// HTTP Version 2.0
    #[serde(rename = "HTTP/2.0")]
    H2,
    /// HTTP Version 3.0
    #[serde(rename = "HTTP/3.0")]
    H3,
}

impl fmt::Display for HttpVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            HttpVersion::Http09 => write!(f, "HTTP/0.9"),
            HttpVersion::Http10 => write!(f, "HTTP/1.0"),
            HttpVersion::Http11 => write!(f, "HTTP/1.1"),
            HttpVersion::H2 => write!(f, "HTTP/2.0"),
            HttpVersion::H3 => write!(f, "HTTP/3.0"),
        }
    }
}

/// A basic generic type that represents an HTTP response
#[derive(Debug)]
pub struct HttpResponse {
    /// HTTP response body
    body: Body,
    /// HTTP response parts
    parts: Parts,
}

/// HTTP response body.
#[derive(Debug)]
pub struct Body {
    inner: BodyInner,
}

#[derive(Debug)]
enum BodyInner {
    Full(Bytes),
    Streaming(BoxBody<Bytes, BoxError>),
}

impl Body {
    /// wrap stream
    pub fn wrap_stream<S>(stream: S) -> Body
    where
        S: futures::stream::TryStream + Send + Sync + 'static,
        S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
        Bytes: From<S::Ok>,
    {
        use futures_util::TryStreamExt;
        use http_body::Frame;
        use http_body_util::StreamBody;

        let body = BoxBody::new(StreamBody::new(
            stream.map_ok(|d| Frame::data(Bytes::from(d))).map_err(Into::into),
        ));
        Body { inner: BodyInner::Streaming(body) }
    }

    /// Get body bytes if body is full.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.inner {
            BodyInner::Full(bytes) => Some(bytes),
            BodyInner::Streaming(_) => None,
        }
    }

    /// Get all bytes of the response, collecting data stream if some.
    pub async fn bytes(self) -> Result<Bytes> {
        Ok(match self.inner {
            BodyInner::Full(bytes) => bytes,
            BodyInner::Streaming(boxed_body) => boxed_body
                .into_data_stream()
                .collect::<Vec<Result<_>>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .fold(bytes::BytesMut::new(), |mut acc, chunk| {
                    acc.put(chunk);
                    acc
                })
                .freeze(),
        })
    }

    /// Into data stream
    pub fn into_data_stream(self) -> BodyDataStream<BoxBody<Bytes, BoxError>> {
        match self.inner {
            BodyInner::Full(data) => {
                Full::new(data).map_err(Into::into).boxed().into_data_stream()
            }
            BodyInner::Streaming(boxed_body) => boxed_body.into_data_stream(),
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(value: Vec<u8>) -> Self {
        Self { inner: BodyInner::Full(value.into()) }
    }
}

impl From<Bytes> for Body {
    fn from(value: Bytes) -> Self {
        Self { inner: BodyInner::Full(value) }
    }
}

/// HTTP response parts consists of status, version, response URL and headers.
///
/// Serializable alternative to [`http::response::Parts`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Parts {
    /// HTTP response headers
    pub headers: HashMap<String, String>,
    /// HTTP response status code
    pub status: u16,
    /// HTTP response url
    pub url: Url,
    /// HTTP response version
    pub version: HttpVersion,
}

impl HttpResponse {
    /// Consumes the response returning the head and body parts.
    pub fn into_parts(self) -> (Parts, Body) {
        (self.parts, self.body)
    }

    /// Creates a new Response with the given head and body.
    pub fn from_parts(parts: Parts, body: Body) -> Self {
        Self { body, parts }
    }

    /// Returns `http::response::Parts`
    pub fn parts(&self) -> Result<response::Parts> {
        let mut converted =
            response::Builder::new().status(self.parts.status).body(())?;
        {
            let headers = converted.headers_mut();
            for header in &self.parts.headers {
                headers.insert(
                    http::header::HeaderName::from_str(header.0.as_str())?,
                    http::HeaderValue::from_str(header.1.as_str())?,
                );
            }
        }
        Ok(converted.into_parts().0)
    }

    /// Returns the status code of the warning header if present
    #[must_use]
    pub fn warning_code(&self) -> Option<usize> {
        self.parts.headers.get("warning").and_then(|hdr| {
            hdr.as_str().chars().take(3).collect::<String>().parse().ok()
        })
    }

    /// Adds a warning header to a response
    pub fn add_warning(&mut self, url: &Url, code: usize, message: &str) {
        // warning    = "warning" ":" 1#warning-value
        // warning-value = warn-code SP warn-agent SP warn-text [SP warn-date]
        // warn-code  = 3DIGIT
        // warn-agent = ( host [ ":" port ] ) | pseudonym
        //                 ; the name or pseudonym of the server adding
        //                 ; the warning header, for use in debugging
        // warn-text  = quoted-string
        // warn-date  = <"> HTTP-date <">
        // (https://tools.ietf.org/html/rfc2616#section-14.46)
        self.parts.headers.insert(
            "warning".to_string(),
            format!(
                "{} {} {:?} \"{}\"",
                code,
                url.host().expect("Invalid URL"),
                message,
                httpdate::fmt_http_date(SystemTime::now())
            ),
        );
    }

    /// Removes a warning header from a response
    pub fn remove_warning(&mut self) {
        self.parts.headers.remove("warning");
    }

    /// Update the headers from `http::response::Parts`
    pub fn update_headers(&mut self, parts: &response::Parts) -> Result<()> {
        for header in parts.headers.iter() {
            self.parts.headers.insert(
                header.0.as_str().to_string(),
                header.1.to_str()?.to_string(),
            );
        }
        Ok(())
    }

    /// Checks if the Cache-Control header contains the must-revalidate directive
    #[must_use]
    pub fn must_revalidate(&self) -> bool {
        self.parts.headers.get(CACHE_CONTROL.as_str()).is_some_and(|val| {
            val.as_str().to_lowercase().contains("must-revalidate")
        })
    }

    /// Adds the custom `x-cache` header to the response
    pub fn cache_status(&mut self, hit_or_miss: HitOrMiss) {
        self.parts.headers.insert(XCACHE.to_string(), hit_or_miss.to_string());
    }

    /// Adds the custom `x-cache-lookup` header to the response
    pub fn cache_lookup_status(&mut self, hit_or_miss: HitOrMiss) {
        self.parts
            .headers
            .insert(XCACHELOOKUP.to_string(), hit_or_miss.to_string());
    }
}

/// A trait providing methods for storing, reading, and removing cache records.
///
/// Generic argument `R` defines the type of HTTP response body which may be put into cache.
#[async_trait::async_trait]
pub trait CacheManager: Send + Sync + 'static {
    /// Attempts to pull a cached response and related policy from cache.
    async fn get(
        &self,
        cache_key: &str,
    ) -> Result<Option<(HttpResponse, CachePolicy)>>;
    /// Attempts to cache a response and related policy.
    async fn put(
        &self,
        cache_key: String,
        res: HttpResponse,
        policy: CachePolicy,
    ) -> Result<HttpResponse>;
    /// Attempts to remove a record from cache.
    async fn delete(&self, cache_key: &str) -> Result<()>;
}

/// Describes the functionality required for interfacing with HTTP client middleware
#[async_trait::async_trait]
pub trait Middleware: Send {
    /// Allows the cache mode to be overridden.
    ///
    /// This overrides any cache mode set in the configuration, including cache_mode_fn.
    fn overridden_cache_mode(&self) -> Option<CacheMode> {
        None
    }
    /// Determines if the request method is either GET or HEAD
    fn is_method_get_head(&self) -> bool;
    /// Returns a new cache policy with default options
    fn policy(&self, response: &HttpResponse) -> Result<CachePolicy>;
    /// Returns a new cache policy with custom options
    fn policy_with_options(
        &self,
        response: &HttpResponse,
        options: CacheOptions,
    ) -> Result<CachePolicy>;
    /// Attempts to update the request headers with the passed `http::request::Parts`
    fn update_headers(&mut self, parts: &request::Parts) -> Result<()>;
    /// Attempts to force the "no-cache" directive on the request
    fn force_no_cache(&mut self) -> Result<()>;
    /// Attempts to construct `http::request::Parts` from the request
    fn parts(&self) -> Result<request::Parts>;
    /// Attempts to determine the requested url
    fn url(&self) -> Result<Url>;
    /// Attempts to determine the request method
    fn method(&self) -> Result<String>;
    /// Attempts to fetch an upstream resource and return an [`HttpResponse`]
    async fn remote_fetch(&mut self) -> Result<HttpResponse>;
}

/// Similar to [make-fetch-happen cache options](https://github.com/npm/make-fetch-happen#--optscache).
/// Passed in when the [`HttpCache`] struct is being built.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Will inspect the HTTP cache on the way to the network.
    /// If there is a fresh response it will be used.
    /// If there is a stale response a conditional request will be created,
    /// and a normal request otherwise.
    /// It then updates the HTTP cache with the response.
    /// If the revalidation request fails (for example, on a 500 or if you're offline),
    /// the stale response will be returned.
    #[default]
    Default,
    /// Behaves as if there is no HTTP cache at all.
    NoStore,
    /// Behaves as if there is no HTTP cache on the way to the network.
    /// Ergo, it creates a normal request and updates the HTTP cache with the response.
    Reload,
    /// Creates a conditional request if there is a response in the HTTP cache
    /// and a normal request otherwise. It then updates the HTTP cache with the response.
    NoCache,
    /// Uses any response in the HTTP cache matching the request,
    /// not paying attention to staleness. If there was no response,
    /// it creates a normal request and updates the HTTP cache with the response.
    ForceCache,
    /// Uses any response in the HTTP cache matching the request,
    /// not paying attention to staleness. If there was no response,
    /// it returns a network error.
    OnlyIfCached,
    /// Overrides the check that determines if a response can be cached to always return true on 200.
    /// Uses any response in the HTTP cache matching the request,
    /// not paying attention to staleness. If there was no response,
    /// it creates a normal request and updates the HTTP cache with the response.
    IgnoreRules,
}

impl TryFrom<http::Version> for HttpVersion {
    type Error = BoxError;

    fn try_from(value: http::Version) -> Result<Self> {
        Ok(match value {
            http::Version::HTTP_09 => Self::Http09,
            http::Version::HTTP_10 => Self::Http10,
            http::Version::HTTP_11 => Self::Http11,
            http::Version::HTTP_2 => Self::H2,
            http::Version::HTTP_3 => Self::H3,
            _ => return Err(Box::new(BadVersion)),
        })
    }
}

impl From<HttpVersion> for http::Version {
    fn from(value: HttpVersion) -> Self {
        match value {
            HttpVersion::Http09 => Self::HTTP_09,
            HttpVersion::Http10 => Self::HTTP_10,
            HttpVersion::Http11 => Self::HTTP_11,
            HttpVersion::H2 => Self::HTTP_2,
            HttpVersion::H3 => Self::HTTP_3,
        }
    }
}

#[cfg(feature = "http-types")]
impl TryFrom<http_types::Version> for HttpVersion {
    type Error = BoxError;

    fn try_from(value: http_types::Version) -> Result<Self> {
        Ok(match value {
            http_types::Version::Http0_9 => Self::Http09,
            http_types::Version::Http1_0 => Self::Http10,
            http_types::Version::Http1_1 => Self::Http11,
            http_types::Version::Http2_0 => Self::H2,
            http_types::Version::Http3_0 => Self::H3,
            _ => return Err(Box::new(BadVersion)),
        })
    }
}

#[cfg(feature = "http-types")]
impl From<HttpVersion> for http_types::Version {
    fn from(value: HttpVersion) -> Self {
        match value {
            HttpVersion::Http09 => Self::Http0_9,
            HttpVersion::Http10 => Self::Http1_0,
            HttpVersion::Http11 => Self::Http1_1,
            HttpVersion::H2 => Self::Http2_0,
            HttpVersion::H3 => Self::Http3_0,
        }
    }
}

/// Options struct provided by
/// [`http-cache-semantics`](https://github.com/kornelski/rusty-http-cache-semantics).
pub use http_cache_semantics::CacheOptions;

/// A closure that takes [`http::request::Parts`] and returns a [`String`].
/// By default, the cache key is a combination of the request method and uri with a colon in between.
pub type CacheKey = Arc<dyn Fn(&request::Parts) -> String + Send + Sync>;

/// A closure that takes [`http::request::Parts`] and returns a [`CacheMode`]
pub type CacheModeFn = Arc<dyn Fn(&request::Parts) -> CacheMode + Send + Sync>;

/// A closure that takes [`http::request::Parts`], [`Option<CacheKey>`], the default cache key ([`&str``]) and returns [`Vec<String>`] of keys to bust the cache for.
/// An empty vector means that no cache busting will be performed.
pub type CacheBust = Arc<
    dyn Fn(&request::Parts, &Option<CacheKey>, &str) -> Vec<String>
        + Send
        + Sync,
>;

/// Can be used to override the default [`CacheOptions`] and cache key.
/// The cache key is a closure that takes [`http::request::Parts`] and returns a [`String`].
#[derive(Clone)]
pub struct HttpCacheOptions {
    /// Override the default cache options.
    pub cache_options: Option<CacheOptions>,
    /// Override the default cache key generator.
    pub cache_key: Option<CacheKey>,
    /// Override the default cache mode.
    pub cache_mode_fn: Option<CacheModeFn>,
    /// Bust the caches of the returned keys.
    pub cache_bust: Option<CacheBust>,
    /// Determines if the cache status headers should be added to the response.
    pub cache_status_headers: bool,
}

impl Default for HttpCacheOptions {
    fn default() -> Self {
        Self {
            cache_options: None,
            cache_key: None,
            cache_mode_fn: None,
            cache_bust: None,
            cache_status_headers: true,
        }
    }
}

impl Debug for HttpCacheOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpCacheOptions")
            .field("cache_options", &self.cache_options)
            .field("cache_key", &"Fn(&request::Parts) -> String")
            .field("cache_mode_fn", &"Fn(&request::Parts) -> CacheMode")
            .field("cache_bust", &"Fn(&request::Parts) -> Vec<String>")
            .field("cache_status_headers", &self.cache_status_headers)
            .finish()
    }
}

impl HttpCacheOptions {
    fn create_cache_key(
        &self,
        parts: &request::Parts,
        override_method: Option<&str>,
    ) -> String {
        if let Some(cache_key) = &self.cache_key {
            cache_key(parts)
        } else {
            format!(
                "{}:{}",
                override_method.unwrap_or_else(|| parts.method.as_str()),
                parts.uri
            )
        }
    }
}

/// Caches requests according to http spec.
#[derive(Debug, Clone)]
pub struct HttpCache<T: CacheManager> {
    /// Determines the manager behavior.
    pub mode: CacheMode,
    /// Manager instance that implements the [`CacheManager`] trait.
    /// By default, a manager implementation with [`cacache`](https://github.com/zkat/cacache-rs)
    /// as the backend has been provided, see [`CACacheManager`].
    pub manager: T,
    /// Override the default cache options.
    pub options: HttpCacheOptions,
}

#[allow(dead_code)]
impl<T: CacheManager> HttpCache<T> {
    /// Determines if the request should be cached
    pub fn can_cache_request(
        &self,
        middleware: &impl Middleware,
    ) -> Result<bool> {
        let mode = self.cache_mode(middleware)?;

        Ok(mode == CacheMode::IgnoreRules
            || middleware.is_method_get_head() && mode != CacheMode::NoStore)
    }

    /// Runs the actions to preform when the client middleware is running without the cache
    pub async fn run_no_cache(
        &self,
        middleware: &mut impl Middleware,
    ) -> Result<()> {
        self.manager
            .delete(
                &self
                    .options
                    .create_cache_key(&middleware.parts()?, Some("GET")),
            )
            .await
            .ok();

        let cache_key =
            self.options.create_cache_key(&middleware.parts()?, None);

        if let Some(cache_bust) = &self.options.cache_bust {
            for key_to_cache_bust in cache_bust(
                &middleware.parts()?,
                &self.options.cache_key,
                &cache_key,
            ) {
                self.manager.delete(&key_to_cache_bust).await?;
            }
        }

        Ok(())
    }

    /// Attempts to run the passed middleware along with the cache
    pub async fn run(
        &self,
        mut middleware: impl Middleware,
    ) -> Result<HttpResponse> {
        let is_cacheable = self.can_cache_request(&middleware)?;
        if !is_cacheable {
            return self.remote_fetch(&mut middleware).await;
        }

        let cache_key =
            self.options.create_cache_key(&middleware.parts()?, None);

        if let Some(cache_bust) = &self.options.cache_bust {
            for key_to_cache_bust in cache_bust(
                &middleware.parts()?,
                &self.options.cache_key,
                &cache_key,
            ) {
                self.manager.delete(&key_to_cache_bust).await?;
            }
        }

        if let Some(store) = self.manager.get(&cache_key).await? {
            let (mut res, policy) = store;
            if self.options.cache_status_headers {
                res.cache_lookup_status(HitOrMiss::HIT);
            }
            if let Some(warning_code) = res.warning_code() {
                // https://tools.ietf.org/html/rfc7234#section-4.3.4
                //
                // If a stored response is selected for update, the cache MUST:
                //
                // * delete any warning header fields in the stored response with
                //   warn-code 1xx (see Section 5.5);
                //
                // * retain any warning header fields in the stored response with
                //   warn-code 2xx;
                //
                if (100..200).contains(&warning_code) {
                    res.remove_warning();
                }
            }

            match self.cache_mode(&middleware)? {
                CacheMode::Default => {
                    self.conditional_fetch(middleware, res, policy).await
                }
                CacheMode::NoCache => {
                    middleware.force_no_cache()?;
                    let mut res = self.remote_fetch(&mut middleware).await?;
                    if self.options.cache_status_headers {
                        res.cache_lookup_status(HitOrMiss::HIT);
                    }
                    Ok(res)
                }
                CacheMode::ForceCache
                | CacheMode::OnlyIfCached
                | CacheMode::IgnoreRules => {
                    //   112 Disconnected operation
                    // SHOULD be included if the cache is intentionally disconnected from
                    // the rest of the network for a period of time.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    res.add_warning(
                        &res.parts.url.clone(),
                        112,
                        "Disconnected operation",
                    );
                    if self.options.cache_status_headers {
                        res.cache_status(HitOrMiss::HIT);
                    }
                    Ok(res)
                }
                _ => self.remote_fetch(&mut middleware).await,
            }
        } else {
            match self.cache_mode(&middleware)? {
                CacheMode::OnlyIfCached => {
                    // ENOTCACHED
                    let mut res = HttpResponse {
                        body: b"GatewayTimeout".to_vec().into(),
                        parts: Parts {
                            headers: HashMap::default(),
                            status: 504,
                            url: middleware.url()?,
                            version: HttpVersion::Http11,
                        },
                    };
                    if self.options.cache_status_headers {
                        res.cache_status(HitOrMiss::MISS);
                        res.cache_lookup_status(HitOrMiss::MISS);
                    }
                    Ok(res)
                }
                _ => self.remote_fetch(&mut middleware).await,
            }
        }
    }

    fn cache_mode(&self, middleware: &impl Middleware) -> Result<CacheMode> {
        Ok(if let Some(mode) = middleware.overridden_cache_mode() {
            mode
        } else if let Some(cache_mode_fn) = &self.options.cache_mode_fn {
            cache_mode_fn(&middleware.parts()?)
        } else {
            self.mode
        })
    }

    async fn remote_fetch(
        &self,
        middleware: &mut impl Middleware,
    ) -> Result<HttpResponse> {
        let mut res = middleware.remote_fetch().await?;
        if self.options.cache_status_headers {
            res.cache_status(HitOrMiss::MISS);
            res.cache_lookup_status(HitOrMiss::MISS);
        }
        let policy = match self.options.cache_options {
            Some(options) => middleware.policy_with_options(&res, options)?,
            None => middleware.policy(&res)?,
        };
        let is_get_head = middleware.is_method_get_head();
        let mode = self.cache_mode(middleware)?;
        let mut is_cacheable = is_get_head
            && mode != CacheMode::NoStore
            && res.parts.status == 200
            && policy.is_storable();
        if mode == CacheMode::IgnoreRules && res.parts.status == 200 {
            is_cacheable = true;
        }
        if is_cacheable {
            Ok(self
                .manager
                .put(
                    self.options.create_cache_key(&middleware.parts()?, None),
                    res,
                    policy,
                )
                .await?)
        } else if !is_get_head {
            self.manager
                .delete(
                    &self
                        .options
                        .create_cache_key(&middleware.parts()?, Some("GET")),
                )
                .await
                .ok();
            Ok(res)
        } else {
            Ok(res)
        }
    }

    async fn conditional_fetch(
        &self,
        mut middleware: impl Middleware,
        mut cached_res: HttpResponse,
        mut policy: CachePolicy,
    ) -> Result<HttpResponse> {
        let before_req =
            policy.before_request(&middleware.parts()?, SystemTime::now());
        match before_req {
            BeforeRequest::Fresh(parts) => {
                cached_res.update_headers(&parts)?;
                if self.options.cache_status_headers {
                    cached_res.cache_status(HitOrMiss::HIT);
                    cached_res.cache_lookup_status(HitOrMiss::HIT);
                }
                return Ok(cached_res);
            }
            BeforeRequest::Stale { request: parts, matches } => {
                if matches {
                    middleware.update_headers(&parts)?;
                }
            }
        }
        let req_url = middleware.url()?;
        match middleware.remote_fetch().await {
            Ok(mut cond_res) => {
                let status = StatusCode::from_u16(cond_res.parts.status)?;
                if status.is_server_error() && cached_res.must_revalidate() {
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    cached_res.add_warning(
                        &req_url,
                        111,
                        "Revalidation failed",
                    );
                    if self.options.cache_status_headers {
                        cached_res.cache_status(HitOrMiss::HIT);
                    }
                    Ok(cached_res)
                } else if cond_res.parts.status == 304 {
                    let after_res = policy.after_response(
                        &middleware.parts()?,
                        &cond_res.parts()?,
                        SystemTime::now(),
                    );
                    match after_res {
                        AfterResponse::Modified(new_policy, parts)
                        | AfterResponse::NotModified(new_policy, parts) => {
                            policy = new_policy;
                            cached_res.update_headers(&parts)?;
                        }
                    }
                    if self.options.cache_status_headers {
                        cached_res.cache_status(HitOrMiss::HIT);
                        cached_res.cache_lookup_status(HitOrMiss::HIT);
                    }
                    let res = self
                        .manager
                        .put(
                            self.options
                                .create_cache_key(&middleware.parts()?, None),
                            cached_res,
                            policy,
                        )
                        .await?;
                    Ok(res)
                } else if cond_res.parts.status == 200 {
                    let policy = match self.options.cache_options {
                        Some(options) => middleware
                            .policy_with_options(&cond_res, options)?,
                        None => middleware.policy(&cond_res)?,
                    };
                    if self.options.cache_status_headers {
                        cond_res.cache_status(HitOrMiss::MISS);
                        cond_res.cache_lookup_status(HitOrMiss::HIT);
                    }
                    let res = self
                        .manager
                        .put(
                            self.options
                                .create_cache_key(&middleware.parts()?, None),
                            cond_res,
                            policy,
                        )
                        .await?;
                    Ok(res)
                } else {
                    if self.options.cache_status_headers {
                        cached_res.cache_status(HitOrMiss::HIT);
                    }
                    Ok(cached_res)
                }
            }
            Err(e) => {
                if cached_res.must_revalidate() {
                    Err(e)
                } else {
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    cached_res.add_warning(
                        &req_url,
                        111,
                        "Revalidation failed",
                    );
                    if self.options.cache_status_headers {
                        cached_res.cache_status(HitOrMiss::HIT);
                    }
                    Ok(cached_res)
                }
            }
        }
    }
}

#[cfg(test)]
mod test;
