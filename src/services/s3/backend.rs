// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::io::ErrorKind;
use std::io::Result;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use futures::TryStreamExt;
use http::header::HeaderName;
use http::HeaderValue;
use http::StatusCode;
use hyper::Body;
use log::debug;
use log::error;
use log::info;
use log::warn;
use metrics::increment_counter;
use minitrace::trace;
use once_cell::sync::Lazy;
use reqsign::services::aws::loader::CredentialLoadChain;
use reqsign::services::aws::loader::DummyLoader;
use reqsign::services::aws::v4::Signer;

use super::dir_stream::DirStream;
use crate::error::other;
use crate::error::BackendError;
use crate::error::ObjectError;
use crate::io_util::new_http_channel;
use crate::io_util::parse_content_length;
use crate::io_util::parse_error_response;
use crate::io_util::parse_etag;
use crate::io_util::parse_last_modified;
use crate::io_util::HttpBodyWriter;
use crate::io_util::HttpClient;
use crate::ops::BytesRange;
use crate::ops::OpCreate;
use crate::ops::OpDelete;
use crate::ops::OpList;
use crate::ops::OpRead;
use crate::ops::OpStat;
use crate::ops::OpWrite;
use crate::Accessor;
use crate::AccessorMetadata;
use crate::BytesReader;
use crate::BytesWriter;
use crate::DirStreamer;
use crate::ObjectMetadata;
use crate::ObjectMode;
use crate::Scheme;

/// Allow constructing correct region endpoint if user gives a global endpoint.
static ENDPOINT_TEMPLATES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    // AWS S3 Service.
    m.insert(
        "https://s3.amazonaws.com",
        "https://s3.{region}.amazonaws.com",
    );
    m
});

mod constants {
    pub const X_AMZ_SERVER_SIDE_ENCRYPTION: &str = "x-amz-server-side-encryption";
    pub const X_AMZ_SERVER_SIDE_ENCRYPTION_CUSTOMER_ALGORITHM: &str =
        "x-amz-server-side-encryption-customer-algorithm";
    pub const X_AMZ_SERVER_SIDE_ENCRYPTION_CUSTOMER_KEY: &str =
        "x-amz-server-side-encryption-customer-key";
    pub const X_AMZ_SERVER_SIDE_ENCRYPTION_CUSTOMER_KEY_MD5: &str =
        "x-amz-server-side-encryption-customer-key-md5";
    pub const X_AMZ_SERVER_SIDE_ENCRYPTION_AWS_KMS_KEY_ID: &str =
        "x-amz-server-side-encryption-aws-kms-key-id";
}

/// Builder for s3 services
///
/// # Server Side Encryption
///
/// OpenDAL provides full support of S3 Server Side Encryption(SSE) features.
///
/// The easiest way to configure them is to use helper functions like
///
/// - SSE-KMS: `server_side_encryption_with_aws_managed_kms_key`
/// - SSE-KMS: `server_side_encryption_with_customer_managed_kms_key`
/// - SSE-S3: `server_side_encryption_with_s3_key`
/// - SSE-C: `server_side_encryption_with_customer_key`
///
/// If those functions don't fulfill need, low-level options are also provided:
///
/// - Use service managed kms key
///   - `server_side_encryption="aws:kms"`
/// - Use customer provided kms key
///   - `server_side_encryption="aws:kms"`
///   - `server_side_encryption_aws_kms_key_id="your-kms-key"`
/// - Use S3 managed key
///   - `server_side_encryption="AES256"`
/// - Use customer key
///   - `server_side_encryption_customer_algorithm="AES256"`
///   - `server_side_encryption_customer_key="base64-of-your-aes256-key"`
///   - `server_side_encryption_customer_key_md5="base64-of-your-aes256-key-md5"`
///
/// After SSE have been configured, all requests send by this backed will attach those headers.
///
/// Reference: [Protecting data using server-side encryption](https://docs.aws.amazon.com/AmazonS3/latest/userguide/serv-side-encryption.html)
#[derive(Default, Clone)]
pub struct Builder {
    root: Option<String>,

    bucket: String,
    endpoint: Option<String>,
    region: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    server_side_encryption: Option<String>,
    server_side_encryption_aws_kms_key_id: Option<String>,
    server_side_encryption_customer_algorithm: Option<String>,
    server_side_encryption_customer_key: Option<String>,
    server_side_encryption_customer_key_md5: Option<String>,

    disable_credential_loader: bool,
    enable_virtual_host_style: bool,
}

impl Debug for Builder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Builder");

        d.field("root", &self.root)
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("disable_credential_loader", &self.disable_credential_loader)
            .field("enable_virtual_host_style", &self.disable_credential_loader);

        if self.access_key_id.is_some() {
            d.field("access_key_id", &"<redacted>");
        }
        if self.secret_access_key.is_some() {
            d.field("secret_access_key", &"<redacted>");
        }
        if self.server_side_encryption.is_some() {
            d.field("server_side_encryption", &"<redacted>");
        }
        if self.server_side_encryption_aws_kms_key_id.is_some() {
            d.field("server_side_encryption_aws_kms_key_id", &"<redacted>");
        }
        if self.server_side_encryption_customer_algorithm.is_some() {
            d.field("server_side_encryption_customer_algorithm", &"<redacted>");
        }
        if self.server_side_encryption_customer_key.is_some() {
            d.field("server_side_encryption_customer_key", &"<redacted>");
        }
        if self.server_side_encryption_customer_key_md5.is_some() {
            d.field("server_side_encryption_customer_key_md5", &"<redacted>");
        }

        d.finish()
    }
}

impl Builder {
    /// Set root of this backend.
    ///
    /// All operations will happen under this root.
    pub fn root(&mut self, root: &str) -> &mut Self {
        self.root = if root.is_empty() {
            None
        } else {
            Some(root.to_string())
        };

        self
    }

    /// Set bucket name of this backend.
    pub fn bucket(&mut self, bucket: &str) -> &mut Self {
        self.bucket = bucket.to_string();

        self
    }

    /// Set endpoint of this backend.
    ///
    /// Endpoint must be full uri, e.g.
    ///
    /// - AWS S3: `https://s3.amazonaws.com` or `https://s3.{region}.amazonaws.com`
    /// - Aliyun OSS: `https://{region}.aliyuncs.com`
    /// - Tencent COS: `https://cos.{region}.myqcloud.com`
    /// - Minio: `http://127.0.0.1:9000`
    ///
    /// If user inputs endpoint without scheme like "s3.amazonaws.com", we
    /// will prepend "https://" before it.
    pub fn endpoint(&mut self, endpoint: &str) -> &mut Self {
        if !endpoint.is_empty() {
            // Trim trailing `/` so that we can accept `http://127.0.0.1:9000/`
            self.endpoint = Some(endpoint.trim_end_matches('/').to_string())
        }

        self
    }

    /// Region represent the signing region of this endpoint.
    ///
    /// - If region is set, we will take user's input first.
    /// - If not, We will try to detect region via [RFC-0057: Auto Region](https://github.com/datafuselabs/opendal/blob/main/docs/rfcs/0057-auto-region.md).
    ///
    /// Most of time, region is not need to be set, especially for AWS S3 and minio.
    pub fn region(&mut self, region: &str) -> &mut Self {
        if !region.is_empty() {
            self.region = Some(region.to_string())
        }

        self
    }

    /// Set access_key_id of this backend.
    ///
    /// - If access_key_id is set, we will take user's input first.
    /// - If not, we will try to load it from environment.
    pub fn access_key_id(&mut self, v: &str) -> &mut Self {
        if !v.is_empty() {
            self.access_key_id = Some(v.to_string())
        }

        self
    }

    /// Set secret_access_key of this backend.
    ///
    /// - If secret_access_key is set, we will take user's input first.
    /// - If not, we will try to load it from environment.
    pub fn secret_access_key(&mut self, v: &str) -> &mut Self {
        if !v.is_empty() {
            self.secret_access_key = Some(v.to_string())
        }

        self
    }

    /// Set server_side_encryption for this backend.
    ///
    /// Available values: `AES256`, `aws:kms`.
    ///
    /// # Note
    ///
    /// This function is the low-level setting for SSE related features.
    ///
    /// SSE related options should be set carefully to make them works.
    /// Please use `server_side_encryption_with_*` helpers if even possible.
    pub fn server_side_encryption(&mut self, v: &str) -> &mut Self {
        if !v.is_empty() {
            self.server_side_encryption = Some(v.to_string())
        }

        self
    }

    /// Set server_side_encryption_aws_kms_key_id for this backend
    ///
    /// - If `server_side_encryption` set to `aws:kms`, and `server_side_encryption_aws_kms_key_id`
    /// is not set, S3 will use aws managed kms key to encrypt data.
    /// - If `server_side_encryption` set to `aws:kms`, and `server_side_encryption_aws_kms_key_id`
    /// is a valid kms key id, S3 will use the provided kms key to encrypt data.
    /// - If the `server_side_encryption_aws_kms_key_id` is invalid or not found, an error will be
    /// returned.
    /// - If `server_side_encryption` is not `aws:kms`, setting `server_side_encryption_aws_kms_key_id`
    /// is a noop.
    ///
    /// # Note
    ///
    /// This function is the low-level setting for SSE related features.
    ///
    /// SSE related options should be set carefully to make them works.
    /// Please use `server_side_encryption_with_*` helpers if even possible.
    pub fn server_side_encryption_aws_kms_key_id(&mut self, v: &str) -> &mut Self {
        if !v.is_empty() {
            self.server_side_encryption_aws_kms_key_id = Some(v.to_string())
        }

        self
    }

    /// Set server_side_encryption_customer_algorithm for this backend.
    ///
    /// Available values: `AES256`.
    ///
    /// # Note
    ///
    /// This function is the low-level setting for SSE related features.
    ///
    /// SSE related options should be set carefully to make them works.
    /// Please use `server_side_encryption_with_*` helpers if even possible.
    pub fn server_side_encryption_customer_algorithm(&mut self, v: &str) -> &mut Self {
        if !v.is_empty() {
            self.server_side_encryption_customer_algorithm = Some(v.to_string())
        }

        self
    }

    /// Set server_side_encryption_customer_key for this backend.
    ///
    /// # Args
    ///
    /// `v`: base64 encoded key that matches algorithm specified in
    /// `server_side_encryption_customer_algorithm`.
    ///
    /// # Note
    ///
    /// This function is the low-level setting for SSE related features.
    ///
    /// SSE related options should be set carefully to make them works.
    /// Please use `server_side_encryption_with_*` helpers if even possible.
    pub fn server_side_encryption_customer_key(&mut self, v: &str) -> &mut Self {
        if !v.is_empty() {
            self.server_side_encryption_customer_key = Some(v.to_string())
        }

        self
    }

    /// Set server_side_encryption_customer_key_md5 for this backend.
    ///
    /// # Args
    ///
    /// `v`: MD5 digest of key specified in `server_side_encryption_customer_key`.
    ///
    /// # Note
    ///
    /// This function is the low-level setting for SSE related features.
    ///
    /// SSE related options should be set carefully to make them works.
    /// Please use `server_side_encryption_with_*` helpers if even possible.
    pub fn server_side_encryption_customer_key_md5(&mut self, v: &str) -> &mut Self {
        if !v.is_empty() {
            self.server_side_encryption_customer_key_md5 = Some(v.to_string())
        }

        self
    }

    /// Enable server side encryption with aws managed kms key
    ///
    /// As known as: SSE-KMS
    ///
    /// NOTE: This function should not be used along with other `server_side_encryption_with_` functions.
    pub fn server_side_encryption_with_aws_managed_kms_key(&mut self) -> &mut Self {
        self.server_side_encryption = Some("aws:kms".to_string());
        self
    }

    /// Enable server side encryption with customer managed kms key
    ///
    /// As known as: SSE-KMS
    ///
    /// NOTE: This function should not be used along with other `server_side_encryption_with_` functions.
    pub fn server_side_encryption_with_customer_managed_kms_key(
        &mut self,
        aws_kms_key_id: &str,
    ) -> &mut Self {
        self.server_side_encryption = Some("aws:kms".to_string());
        self.server_side_encryption_aws_kms_key_id = Some(aws_kms_key_id.to_string());
        self
    }

    /// Enable server side encryption with s3 managed key
    ///
    /// As known as: SSE-S3
    ///
    /// NOTE: This function should not be used along with other `server_side_encryption_with_` functions.
    pub fn server_side_encryption_with_s3_key(&mut self) -> &mut Self {
        self.server_side_encryption = Some("AES256".to_string());
        self
    }

    /// Enable server side encryption with customer key.
    ///
    /// As known as: SSE-C
    ///
    /// NOTE: This function should not be used along with other `server_side_encryption_with_` functions.
    pub fn server_side_encryption_with_customer_key(
        &mut self,
        algorithm: &str,
        key: &[u8],
    ) -> &mut Self {
        self.server_side_encryption_customer_algorithm = Some(algorithm.to_string());
        self.server_side_encryption_customer_key = Some(base64::encode(key));
        self.server_side_encryption_customer_key_md5 =
            Some(base64::encode(md5::compute(key).as_slice()));
        self
    }

    /// Disable credential loader so that opendal will not load credentials
    /// from environment.
    pub fn disable_credential_loader(&mut self) -> &mut Self {
        self.disable_credential_loader = true;
        self
    }

    /// Enable virtual host style so that opendal will send API requests
    /// in virtual host style instead of path style.
    ///
    /// - By default, opendal will send API to `https://s3.us-east-1.amazonaws.com/bucket_name`
    /// - Enabled, opendal will send API to `https://bucket_name.s3.us-east-1.amazonaws.com`
    pub fn enable_virtual_host_style(&mut self) -> &mut Self {
        self.enable_virtual_host_style = true;
        self
    }

    /// Read RFC-0057: Auto Region for detailed behavior.
    ///
    /// - If region is already known, the region will be returned directly.
    /// - If region is not known, we will send API to `{endpoint}/{bucket}` to get
    ///   `x-amz-bucket-region`, use `us-east-1` if not found.
    ///
    /// Returning endpoint will trim bucket name:
    ///
    /// - `https://bucket_name.s3.amazonaws.com` => `https://s3.amazonaws.com`
    async fn detect_region(
        &self,
        client: &HttpClient,
        bucket: &str,
        context: &HashMap<String, String>,
    ) -> Result<(String, String)> {
        let mut endpoint = match &self.endpoint {
            Some(endpoint) => {
                if endpoint.starts_with("http") {
                    endpoint.to_string()
                } else {
                    // Prefix https if endpoint doesn't start with scheme.
                    format!("https://{}", endpoint)
                }
            }
            None => "https://s3.amazonaws.com".to_string(),
        };

        // If endpoint contains bucket name, we should trim them.
        endpoint = endpoint.replace(&format!("//{bucket}."), "//");

        if let Some(region) = &self.region {
            let endpoint = if let Some(template) = ENDPOINT_TEMPLATES.get(endpoint.as_str()) {
                template.replace("{region}", region)
            } else {
                endpoint.to_string()
            };

            return Ok((endpoint, region.to_string()));
        }

        let url = format!("{endpoint}/{bucket}");
        debug!("backend detect region with url: {url}");

        let req = hyper::Request::head(&url)
            .body(Body::empty())
            .map_err(|e| {
                error!("backend detect_region {}: {:?}", url, e);
                other(BackendError::new(
                    context.clone(),
                    anyhow!("build request {}: {:?}", url, e),
                ))
            })?;

        let res = client.request(req).await.map_err(|e| {
            error!("backend detect_region: {}: {:?}", url, e);
            other(BackendError::new(
                context.clone(),
                anyhow!("sending request: {}: {:?}", url, e),
            ))
        })?;

        debug!(
            "auto detect region got response: status {:?}, header: {:?}",
            res.status(),
            res.headers()
        );
        match res.status() {
            // The endpoint works, return with not changed endpoint and
            // default region.
            StatusCode::OK | StatusCode::FORBIDDEN => {
                let region = res
                    .headers()
                    .get("x-amz-bucket-region")
                    .unwrap_or(&HeaderValue::from_static("us-east-1"))
                    .to_str()
                    .map_err(|e| other(BackendError::new(context.clone(), e)))?
                    .to_string();
                Ok((endpoint.to_string(), region))
            }
            // The endpoint should move, return with constructed endpoint
            StatusCode::MOVED_PERMANENTLY => {
                let region = res
                    .headers()
                    .get("x-amz-bucket-region")
                    .ok_or_else(|| {
                        other(BackendError::new(
                            context.clone(),
                            anyhow!("can't detect region automatically, region is empty"),
                        ))
                    })?
                    .to_str()
                    .map_err(|e| other(BackendError::new(context.clone(), e)))?
                    .to_string();
                let template = ENDPOINT_TEMPLATES.get(endpoint.as_str()).ok_or_else(|| {
                    other(BackendError::new(
                        context.clone(),
                        anyhow!(
                            "can't detect region automatically, no valid endpoint template for {}",
                            &endpoint
                        ),
                    ))
                })?;

                let endpoint = template.replace("{region}", &region);

                Ok((endpoint, region))
            }
            // Unexpected status code
            code => {
                return Err(other(BackendError::new(
                    context.clone(),
                    anyhow!(
                        "can't detect region automatically, unexpected response: status code {}",
                        code
                    ),
                )))
            }
        }
    }

    /// Finish the build process and create a new accessor.
    pub async fn finish(&mut self) -> Result<Arc<dyn Accessor>> {
        info!("backend build started: {:?}", &self);

        let root = match &self.root {
            // Use "/" as root if user not specified.
            None => "/".to_string(),
            Some(v) => {
                let mut v = v
                    .split('/')
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<&str>>()
                    .join("/");
                if !v.starts_with('/') {
                    v.insert(0, '/');
                }
                if !v.ends_with('/') {
                    v.push('/')
                }
                v
            }
        };
        info!("backend use root {}", &root);

        // Handle endpoint, region and bucket name.
        let bucket = match self.bucket.is_empty() {
            false => Ok(&self.bucket),
            true => Err(other(BackendError::new(
                HashMap::from([("bucket".to_string(), "".to_string())]),
                anyhow!("bucket is empty"),
            ))),
        }?;
        debug!("backend use bucket {}", &bucket);

        // Setup error context so that we don't need to construct many times.
        let mut context: HashMap<String, String> =
            HashMap::from([("bucket".to_string(), bucket.to_string())]);

        let server_side_encryption = match &self.server_side_encryption {
            None => None,
            Some(v) => Some(v.parse().map_err(|e| {
                other(BackendError::new(
                    context.clone(),
                    anyhow!("server_side_encryption value {} invalid: {}", v, e),
                ))
            })?),
        };

        let server_side_encryption_aws_kms_key_id =
            match &self.server_side_encryption_aws_kms_key_id {
                None => None,
                Some(v) => Some(v.parse().map_err(|e| {
                    other(BackendError::new(
                        context.clone(),
                        anyhow!(
                            "server_side_encryption_aws_kms_key_id value {} invalid: {}",
                            v,
                            e
                        ),
                    ))
                })?),
            };

        let server_side_encryption_customer_algorithm =
            match &self.server_side_encryption_customer_algorithm {
                None => None,
                Some(v) => Some(v.parse().map_err(|e| {
                    other(BackendError::new(
                        context.clone(),
                        anyhow!(
                            "server_side_encryption_customer_algorithm value {} invalid: {}",
                            v,
                            e
                        ),
                    ))
                })?),
            };

        let server_side_encryption_customer_key = match &self.server_side_encryption_customer_key {
            None => None,
            Some(v) => Some(v.parse().map_err(|e| {
                other(BackendError::new(
                    context.clone(),
                    anyhow!(
                        "server_side_encryption_customer_key value {} invalid: {}",
                        v,
                        e
                    ),
                ))
            })?),
        };
        let server_side_encryption_customer_key_md5 =
            match &self.server_side_encryption_customer_key_md5 {
                None => None,
                Some(v) => Some(v.parse().map_err(|e| {
                    other(BackendError::new(
                        context.clone(),
                        anyhow!(
                            "server_side_encryption_customer_key_md5 value {} invalid: {}",
                            v,
                            e
                        ),
                    ))
                })?),
            };

        let client = HttpClient::new();

        let (mut endpoint, region) = self.detect_region(&client, bucket, &context).await?;
        // Construct endpoint which contains bucket name.
        if self.enable_virtual_host_style {
            endpoint = endpoint.replace("//", &format!("//{bucket}."))
        } else {
            endpoint.push_str(&format!("/{bucket}"))
        }
        context.insert("endpoint".to_string(), endpoint.clone());
        context.insert("region".to_string(), region.clone());
        debug!("backend use endpoint: {}, region: {}", &endpoint, &region);

        let mut signer_builder = Signer::builder();
        signer_builder.service("s3");
        signer_builder.region(&region);
        signer_builder.allow_anonymous();
        if self.disable_credential_loader {
            signer_builder.credential_loader({
                let mut chain = CredentialLoadChain::default();
                chain.push(DummyLoader {});

                chain
            });
        }

        if let (Some(ak), Some(sk)) = (&self.access_key_id, &self.secret_access_key) {
            signer_builder.access_key(ak);
            signer_builder.secret_key(sk);
        }

        let signer = signer_builder
            .build()
            .map_err(|e| other(BackendError::new(context, e)))?;

        info!("backend build finished: {:?}", &self);
        Ok(Arc::new(Backend {
            root,
            endpoint,
            signer: Arc::new(signer),
            bucket: self.bucket.clone(),
            client,

            server_side_encryption,
            server_side_encryption_aws_kms_key_id,
            server_side_encryption_customer_algorithm,
            server_side_encryption_customer_key,
            server_side_encryption_customer_key_md5,
        }))
    }
}

/// Backend for s3 services.
#[derive(Debug, Clone)]
pub struct Backend {
    bucket: String,
    endpoint: String,
    signer: Arc<Signer>,
    client: HttpClient,
    // root will be "/" or "/abc/"
    root: String,

    server_side_encryption: Option<HeaderValue>,
    server_side_encryption_aws_kms_key_id: Option<HeaderValue>,
    server_side_encryption_customer_algorithm: Option<HeaderValue>,
    server_side_encryption_customer_key: Option<HeaderValue>,
    server_side_encryption_customer_key_md5: Option<HeaderValue>,
}

impl Backend {
    /// Create a new builder for s3.
    pub fn build() -> Builder {
        Builder::default()
    }

    /// get_abs_path will return the absolute path of the given path in the s3 format.
    ///
    /// Read [RFC-112](https://github.com/datafuselabs/opendal/pull/112) for more details.
    pub(crate) fn get_abs_path(&self, path: &str) -> String {
        if path == "/" {
            return self.root.trim_start_matches('/').to_string();
        }
        // root must be normalized like `/abc/`
        format!("{}{}", self.root, path)
            .trim_start_matches('/')
            .to_string()
    }

    /// get_rel_path will return the relative path of the given path in the s3 format.
    pub(crate) fn get_rel_path(&self, path: &str) -> String {
        let path = format!("/{}", path);

        match path.strip_prefix(&self.root) {
            Some(v) => v.to_string(),
            None => unreachable!(
                "invalid path {} that not start with backend root {}",
                &path, &self.root
            ),
        }
    }

    /// # Note
    ///
    /// header like X_AMZ_SERVER_SIDE_ENCRYPTION doesn't need to set while
    //  get or stat.
    pub(crate) fn insert_sse_headers(
        &self,
        mut req: http::request::Builder,
        is_write: bool,
    ) -> http::request::Builder {
        if is_write {
            if let Some(v) = &self.server_side_encryption {
                let mut v = v.clone();
                v.set_sensitive(true);

                req = req.header(
                    HeaderName::from_static(constants::X_AMZ_SERVER_SIDE_ENCRYPTION),
                    v,
                )
            }
            if let Some(v) = &self.server_side_encryption_aws_kms_key_id {
                let mut v = v.clone();
                v.set_sensitive(true);

                req = req.header(
                    HeaderName::from_static(constants::X_AMZ_SERVER_SIDE_ENCRYPTION_AWS_KMS_KEY_ID),
                    v,
                )
            }
        }

        if let Some(v) = &self.server_side_encryption_customer_algorithm {
            let mut v = v.clone();
            v.set_sensitive(true);

            req = req.header(
                HeaderName::from_static(constants::X_AMZ_SERVER_SIDE_ENCRYPTION_CUSTOMER_ALGORITHM),
                v,
            )
        }
        if let Some(v) = &self.server_side_encryption_customer_key {
            let mut v = v.clone();
            v.set_sensitive(true);

            req = req.header(
                HeaderName::from_static(constants::X_AMZ_SERVER_SIDE_ENCRYPTION_CUSTOMER_KEY),
                v,
            )
        }
        if let Some(v) = &self.server_side_encryption_customer_key_md5 {
            let mut v = v.clone();
            v.set_sensitive(true);

            req = req.header(
                HeaderName::from_static(constants::X_AMZ_SERVER_SIDE_ENCRYPTION_CUSTOMER_KEY_MD5),
                v,
            )
        }

        req
    }
}

#[async_trait]
impl Accessor for Backend {
    fn metadata(&self) -> AccessorMetadata {
        let mut am = AccessorMetadata::default();
        am.set_scheme(Scheme::S3)
            .set_root(&self.root)
            .set_name(&self.bucket);

        am
    }

    #[trace("create")]
    async fn create(&self, args: &OpCreate) -> Result<()> {
        increment_counter!("opendal_s3_create_requests");
        let p = self.get_abs_path(args.path());

        let req = self.put_object(&p, 0, Body::empty()).await?;
        let resp = self.client.request(req).await.map_err(|e| {
            error!("object {} put_object: {:?}", args.path(), e);
            other(ObjectError::new("read", args.path(), e))
        })?;

        match resp.status() {
            StatusCode::CREATED | StatusCode::OK => {
                debug!("object {} create finished", args.path());
                Ok(())
            }
            _ => {
                let err = parse_error_response("create", args.path(), parse_error_kind, resp).await;
                warn!("object {} create: {:?}", args.path(), err);
                Err(err)
            }
        }
    }

    #[trace("read")]
    async fn read(&self, args: &OpRead) -> Result<BytesReader> {
        increment_counter!("opendal_s3_read_requests");

        let p = self.get_abs_path(args.path());
        debug!(
            "object {} read start: offset {:?}, size {:?}",
            &p,
            args.offset(),
            args.size()
        );

        let resp = self
            .get_object(&p, args.offset(), args.size())
            .await
            .map_err(|e| {
                error!("object {} get_object: {:?}", p, e);
                e
            })?;

        match resp.status() {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                debug!(
                    "object {} reader created: offset {:?}, size {:?}",
                    &p,
                    args.offset(),
                    args.size()
                );

                Ok(Box::new(
                    resp.into_body()
                        .into_stream()
                        .map_err(move |e| other(ObjectError::new("read", &p, e)))
                        .into_async_read(),
                ))
            }
            _ => {
                let err = parse_error_response("read", args.path(), parse_error_kind, resp).await;
                warn!("object {} read: {:?}", args.path(), err);
                Err(err)
            }
        }
    }

    #[trace("write")]
    async fn write(&self, args: &OpWrite) -> Result<BytesWriter> {
        let p = self.get_abs_path(args.path());
        debug!("object {} write start: size {}", &p, args.size());

        let (tx, body) = new_http_channel();

        let req = self.put_object(&p, args.size(), body).await?;

        let bs = HttpBodyWriter::new(
            args,
            tx,
            self.client.request(req),
            HashSet::from([StatusCode::CREATED, StatusCode::OK]),
            parse_error_kind,
        );

        Ok(Box::new(bs))
    }

    #[trace("stat")]
    async fn stat(&self, args: &OpStat) -> Result<ObjectMetadata> {
        increment_counter!("opendal_s3_stat_requests");

        let p = self.get_abs_path(args.path());
        debug!("object {} stat start", &p);

        // Stat root always returns a DIR.
        if self.get_rel_path(&p).is_empty() {
            let mut m = ObjectMetadata::default();
            m.set_mode(ObjectMode::DIR);

            debug!("backed root object stat finished");
            return Ok(m);
        }

        let resp = self.head_object(&p).await?;

        match resp.status() {
            StatusCode::OK => {
                let mut m = ObjectMetadata::default();

                if let Some(v) = parse_content_length(resp.headers())
                    .map_err(|e| other(ObjectError::new("stat", &p, e)))?
                {
                    m.set_content_length(v);
                }

                if let Some(v) = parse_etag(resp.headers())
                    .map_err(|e| other(ObjectError::new("stat", &p, e)))?
                {
                    m.set_etag(v);
                    m.set_content_md5(v.trim_matches('"'));
                }

                if let Some(v) = parse_last_modified(resp.headers())
                    .map_err(|e| other(ObjectError::new("stat", &p, e)))?
                {
                    m.set_last_modified(v);
                }

                if p.ends_with('/') {
                    m.set_mode(ObjectMode::DIR);
                } else {
                    m.set_mode(ObjectMode::FILE);
                };

                debug!("object {} stat finished: {:?}", &p, m);
                Ok(m)
            }
            StatusCode::NOT_FOUND if p.ends_with('/') => {
                let mut m = ObjectMetadata::default();
                m.set_mode(ObjectMode::DIR);

                debug!("object {} stat finished", &p);
                Ok(m)
            }
            _ => {
                let err = parse_error_response("stat", args.path(), parse_error_kind, resp).await;
                warn!("object {} stat: {:?}", args.path(), err);
                Err(err)
            }
        }
    }

    #[trace("delete")]
    async fn delete(&self, args: &OpDelete) -> Result<()> {
        increment_counter!("opendal_s3_delete_requests");

        let p = self.get_abs_path(args.path());
        debug!("object {} delete start", &p);

        let resp = self.delete_object(&p).await?;

        match resp.status() {
            StatusCode::NO_CONTENT => {
                debug!("object {} delete finished", &p);
                Ok(())
            }
            _ => {
                let err = parse_error_response("delete", args.path(), parse_error_kind, resp).await;
                warn!("object {} delete: {:?}", args.path(), err);
                Err(err)
            }
        }
    }

    #[trace("list")]
    async fn list(&self, args: &OpList) -> Result<DirStreamer> {
        increment_counter!("opendal_s3_list_requests");

        let mut path = self.get_abs_path(args.path());
        // Make sure list path is endswith '/'
        if !path.ends_with('/') && !path.is_empty() {
            path.push('/')
        }
        debug!("object {} list start", &path);

        Ok(Box::new(DirStream::new(Arc::new(self.clone()), &path)))
    }
}

impl Backend {
    #[trace("get_object")]
    pub(crate) async fn get_object(
        &self,
        path: &str,
        offset: Option<u64>,
        size: Option<u64>,
    ) -> Result<hyper::Response<hyper::Body>> {
        let url = format!("{}/{}", self.endpoint, path);

        let mut req = hyper::Request::get(&url);

        if offset.is_some() || size.is_some() {
            req = req.header(
                http::header::RANGE,
                BytesRange::new(offset, size).to_string(),
            );
        }

        // Set SSE headers.
        req = self.insert_sse_headers(req, false);

        let mut req = req.body(hyper::Body::empty()).map_err(|e| {
            error!("object {path} get_object: {url} {e:?}");
            other(ObjectError::new(
                "read",
                path,
                anyhow!("build request {url}: {e:?}"),
            ))
        })?;

        self.signer.sign(&mut req).map_err(|e| {
            error!("object {path} get_object: {url} {e:?}");
            other(ObjectError::new(
                "read",
                path,
                anyhow!("sign request: {url}: {e:?}"),
            ))
        })?;

        self.client.request(req).await.map_err(|e| {
            error!("object {path} get_object: {url} {e:?}");
            other(ObjectError::new(
                "read",
                path,
                anyhow!("send request: {url}: {e:?}"),
            ))
        })
    }

    #[trace("put_object")]
    pub(crate) async fn put_object(
        &self,
        path: &str,
        size: u64,
        body: hyper::Body,
    ) -> Result<hyper::Request<hyper::Body>> {
        let url = format!("{}/{}", self.endpoint, path);

        let mut req = hyper::Request::put(&url);

        // Set content length.
        req = req.header(http::header::CONTENT_LENGTH, size.to_string());

        // Set SSE headers.
        req = self.insert_sse_headers(req, true);

        // Set body
        let mut req = req.body(body).map_err(|e| {
            error!("object {path} put_object: {url} {e:?}");
            other(ObjectError::new(
                "write",
                path,
                anyhow!("build request {url}: {e:?}"),
            ))
        })?;

        self.signer.sign(&mut req).map_err(|e| {
            error!("object {path} put_object: {url} {e:?}");
            other(ObjectError::new(
                "write",
                path,
                anyhow!("sign request: {url}: {e:?}"),
            ))
        })?;

        Ok(req)
    }

    #[trace("head_object")]
    pub(crate) async fn head_object(&self, path: &str) -> Result<hyper::Response<hyper::Body>> {
        let url = format!("{}/{}", self.endpoint, path);

        let mut req = hyper::Request::head(&url);

        // Set SSE headers.
        req = self.insert_sse_headers(req, false);

        let mut req = req.body(hyper::Body::empty()).map_err(|e| {
            error!("object {path} head_object: {url} {e:?}");
            other(ObjectError::new(
                "stat",
                path,
                anyhow!("build request {url}: {e:?}"),
            ))
        })?;

        self.signer.sign(&mut req).map_err(|e| {
            error!("object {path} head_object: {url} {e:?}");
            other(ObjectError::new(
                "stat",
                path,
                anyhow!("sign request: {url}: {e:?}"),
            ))
        })?;

        self.client.request(req).await.map_err(|e| {
            error!("object {path} head_object: {url} {e:?}");
            other(ObjectError::new(
                "stat",
                path,
                anyhow!("send request {url}: {e:?}"),
            ))
        })
    }

    #[trace("delete_object")]
    pub(crate) async fn delete_object(&self, path: &str) -> Result<hyper::Response<hyper::Body>> {
        let url = format!("{}/{}", self.endpoint, path);

        let mut req = hyper::Request::delete(&url)
            .body(hyper::Body::empty())
            .map_err(|e| {
                error!("object {path} delete_object: {url} {e:?}");
                other(ObjectError::new(
                    "delete",
                    path,
                    anyhow!("build request {url}: {e:?}"),
                ))
            })?;

        self.signer.sign(&mut req).map_err(|e| {
            error!("object {path} delete_object: {url} {e:?}");
            other(ObjectError::new(
                "delete",
                path,
                anyhow!("sign request {url}: {e:?}"),
            ))
        })?;

        self.client.request(req).await.map_err(|e| {
            error!("object {path} delete_object: {url} {e:?}");
            other(ObjectError::new(
                "delete",
                path,
                anyhow!("send request {url}: {e:?}"),
            ))
        })
    }

    #[trace("list_objects")]
    pub(crate) async fn list_objects(
        &self,
        path: &str,
        continuation_token: &str,
    ) -> Result<hyper::Response<hyper::Body>> {
        let mut url = format!("{}?list-type=2&delimiter=/&prefix={}", self.endpoint, path);
        if !continuation_token.is_empty() {
            url.push_str(&format!("&continuation-token={continuation_token}"))
        }

        let mut req = hyper::Request::get(&url)
            .body(hyper::Body::empty())
            .map_err(|e| {
                error!("object {path} list_objects: {url} {e:?}");
                other(ObjectError::new(
                    "list",
                    path,
                    anyhow!("build request {url}: {e:?}"),
                ))
            })?;

        self.signer.sign(&mut req).map_err(|e| {
            error!("object {path} list_objects: {url} {e:?}");
            other(ObjectError::new(
                "list",
                path,
                anyhow!("sign request {url}: {e:?}"),
            ))
        })?;

        self.client.request(req).await.map_err(|e| {
            error!("object {path} list_object: {url} {e:?}");
            other(ObjectError::new(
                "list",
                path,
                anyhow!("send request {url}: {e:?}"),
            ))
        })
    }
}

pub fn parse_error_kind(code: StatusCode) -> ErrorKind {
    match code {
        StatusCode::NOT_FOUND => ErrorKind::NotFound,
        StatusCode::FORBIDDEN => ErrorKind::PermissionDenied,
        StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => ErrorKind::Interrupted,
        _ => ErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use itertools::iproduct;

    use super::*;

    #[tokio::test]
    async fn test_detect_region() {
        let client = HttpClient::new();

        let endpoint_cases = vec![
            Some("s3.amazonaws.com"),
            Some("https://s3.amazonaws.com"),
            Some("https://s3.us-east-2.amazonaws.com"),
            None,
        ];

        let region_cases = vec![Some("us-east-2"), None];

        for (endpoint, region) in iproduct!(endpoint_cases, region_cases) {
            let mut b = Builder::default();
            if let Some(endpoint) = endpoint {
                b.endpoint(endpoint);
            }
            if let Some(region) = region {
                b.region(region);
            }

            let (endpoint, region) = b
                .detect_region(&client, "test", &HashMap::new())
                .await
                .expect("detect region must success");
            assert_eq!(endpoint, "https://s3.us-east-2.amazonaws.com");
            assert_eq!(region, "us-east-2");
        }
    }
}
