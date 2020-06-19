use crate::errors::{Error, Result};
use crate::v2::*;
use reqwest::{header::HeaderValue, RequestBuilder, StatusCode, Url};
use std::iter::FromIterator;

/// Represents all supported authentication schemes and is stored by `Client`.
#[derive(Debug, Clone)]
pub enum Auth {
    Bearer(BearerAuth),
    Basic(BasicAuth),
}

impl Auth {
    /// Add authentication headers to a request builder.
    pub(crate) fn add_auth_headers(&self, request_builder: RequestBuilder) -> RequestBuilder {
        match self {
            Auth::Bearer(bearer_auth) => request_builder.bearer_auth(bearer_auth.token.clone()),
            Auth::Basic(basic_auth) => {
                request_builder.basic_auth(basic_auth.user.clone(), basic_auth.password.clone())
            }
        }
    }
}

/// Used for Bearer HTTP Authentication.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BearerAuth {
    token: String,
    expires_in: Option<u32>,
    issued_at: Option<String>,
    refresh_token: Option<String>,
}

/// Used for Basic HTTP Authentication.
#[derive(Debug, Clone)]
pub struct BasicAuth {
    user: String,
    password: Option<String>,
}

/// Structured representation for the content of the authentication response header.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub(crate) enum WwwAuthenticateHeaderContent {
    Bearer(WwwAuthenticateHeaderContentBearer),
    Basic(WwwAuthenticateHeaderContentBasic),
}

impl WwwAuthenticateHeaderContent {
    /// Create a `WwwAuthenticateHeaderContent` by parsing a `HeaderValue` instance.
    pub(crate) fn from_www_authentication_header(header_value: HeaderValue) -> Result<Self> {
        let header = String::from_utf8(header_value.as_bytes().to_vec())?;

        // This regex will result in multiple captures which will contain one key-value pair each.
        // The first capture will be the only one with the "method" group set.
        let re = regex::Regex::new(
            r#"(?x)\s*
            ((?P<method>[A-Z][a-z]+)\s*)?
            (
                \s*
                    (?P<key>[a-z]+)
                \s*
                    =
                \s*
                    "(?P<value>[^"]+)"
                \s*
            )
        "#,
        )?;
        let captures = re.captures_iter(&header).collect::<Vec<_>>();

        let method = captures
            .get(0)
            .ok_or_else(|| {
                Error::from(format!("regex '{}' didn't match '{}'", re.as_str(), header))
            })?
            .name("method")
            .ok_or_else(|| Error::from(format!("method not found in {}", header)))?
            .as_str();

        let serialized_content = format!(
            r#"{{ "{}": {{ {} }} }}"#,
            method,
            captures
                .iter()
                .filter_map(|capture| {
                    match (
                        capture.name("key").map(|n| n.as_str().to_string()),
                        capture.name("value").map(|n| n.as_str().to_string()),
                    ) {
                        (Some(key), Some(value)) => Some(format!(r#""{}": "{}""#, key, value)),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
        );

        // Deserialize the content
        let mut unsupported_keys = std::collections::HashSet::new();
        let content: WwwAuthenticateHeaderContent = serde_ignored::deserialize(
            &mut serde_json::Deserializer::from_str(&serialized_content),
            |path| {
                unsupported_keys.insert(path.to_string());
            },
        )?;

        if !unsupported_keys.is_empty() {
            warn!("unsupported keys remaining {:#?}", unsupported_keys);
        }

        Ok(content)
    }
}

/// Structured content for the Bearer authentication response header.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct WwwAuthenticateHeaderContentBearer {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

impl WwwAuthenticateHeaderContentBearer {
    fn auth_ep(&self, scopes: &[&str]) -> String {
        let service = self
            .service
            .as_ref()
            .map(|sv| format!("?service={}", sv))
            .unwrap_or_default();

        let scope = scopes.iter().enumerate().fold(
            if scopes.is_empty() {
                ""
            } else if service.is_empty() {
                "?"
            } else {
                "&"
            }
            .to_string(),
            |acc, (i, &s)| {
                let separator = if i > 1 { "&" } else { "" };
                acc + separator + "scope=" + s
            },
        );

        format!("{}{}{}", self.realm, service, scope)
    }
}

/// Structured content for the Basic authentication response header.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct WwwAuthenticateHeaderContentBasic {
    realm: String,
}

impl Client {
    /// Make a request and return the response's www authentication header.
    async fn get_www_authentication_header(&self) -> Result<HeaderValue> {
        let url = {
            let ep = format!("{}/v2/", self.base_url.clone(),);
            match reqwest::Url::parse(&ep) {
                Ok(url) => url,
                Err(e) => {
                    bail!("failed to parse url from string '{}': {}", ep, e);
                }
            }
        };

        let r = self
            .build_reqwest(Method::GET, url.clone())
            .send()
            .map_err(|e| Error::from(format!("{}", e)))
            .await?;

        trace!("GET '{}' status: {:?}", r.url(), r.status());
        r.headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .ok_or_else(|| {
                Error::from(format!(
                    "missing {:?} header",
                    reqwest::header::WWW_AUTHENTICATE
                ))
            })
            .map(ToOwned::to_owned)
    }

    /// Perform registry authentication and return the authenticated client.
    ///
    /// If Bearer authentication is used the returned client will be authorized for the requested scopes.
    pub async fn authenticate(mut self, scopes: &[&str]) -> Result<Self> {
        let credentials = if let Some(credentials) = self.credentials.clone() {
            credentials
        } else {
            bail!("cannot authenticate without credentials");
        };

        self.auth = None;

        let authentication_header = self.get_www_authentication_header().await?;
        match WwwAuthenticateHeaderContent::from_www_authentication_header(authentication_header)? {
            WwwAuthenticateHeaderContent::Basic(_) => {
                self.auth = Some(Auth::Basic(BasicAuth {
                    user: credentials.0,
                    password: Some(credentials.1),
                }));
            }
            WwwAuthenticateHeaderContent::Bearer(bearer_header_content) => {
                let auth_ep = bearer_header_content.auth_ep(scopes);
                trace!("authenticate: token endpoint: {}", auth_ep);

                let url = reqwest::Url::parse(&auth_ep).map_err(|e| {
                    Error::from(format!(
                        "failed to parse url from string '{}': {}",
                        auth_ep, e
                    ))
                })?;

                let auth_req = match self.credentials.clone() {
                    None => bail!("cannot authenticate without credentials"),

                    Some(credentials) => Client {
                        auth: Some(Auth::Basic(BasicAuth {
                            user: credentials.0,
                            password: Some(credentials.1),
                        })),
                        ..self.clone()
                    }
                    .build_reqwest(Method::GET, url),
                };

                let r = auth_req.send().await?;
                let status = r.status();
                trace!("authenticate: got status {}", status);
                match status {
                    StatusCode::OK => {}
                    _ => return Err(format!("authenticate: wrong HTTP status '{}'", status).into()),
                }

                let bearer_auth = r.json::<BearerAuth>().await?;

                match bearer_auth.token.as_str() {
                    "unauthenticated" => bail!("token is unauthenticated"),
                    "" => bail!("received an empty token"),
                    _ => {}
                };

                // mask the token before logging it
                let chars_count = bearer_auth.token.chars().count();
                let mask_start = std::cmp::min(1, chars_count - 1);
                let mask_end = std::cmp::max(chars_count - 1, 1);
                let mut masked_token = bearer_auth.token.clone();
                masked_token
                    .replace_range(mask_start..mask_end, &"*".repeat(mask_end - mask_start));

                trace!("authenticate: got token: {:?}", masked_token);

                self.auth = Some(Auth::Bearer(bearer_auth));
            }
        };

        if !self.is_auth().await? {
            self.auth = None;
            bail!("login failed")
        }

        trace!("authenticate: login succeeded");

        Ok(self)
    }

    /// Check whether the client can successfully make requests to the registry.
    ///
    /// This could be due to granted anonymous access or valid credentials.
    pub async fn is_auth(&self) -> Result<bool> {
        let url = {
            let ep = format!("{}/v2/", self.base_url.clone(),);
            match Url::parse(&ep) {
                Ok(url) => url,
                Err(e) => {
                    return Err(Error::from(format!(
                        "failed to parse url from string '{}': {}",
                        ep, e
                    )));
                }
            }
        };

        let req = self.build_reqwest(Method::GET, url.clone());

        trace!("Sending request to '{}'", url);
        let resp = req.send().await?;
        trace!("GET '{:?}'", resp);

        let status = resp.status();
        match status {
            reqwest::StatusCode::OK => Ok(true),
            reqwest::StatusCode::UNAUTHORIZED => Ok(false),
            _ => Err(format!("is_auth: wrong HTTP status '{}'", status).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_realm_parses_correctly() -> Result<()> {
        let realm = "https://sat-r220-02.lab.eng.rdu2.redhat.com/v2/token";
        let service = "sat-r220-02.lab.eng.rdu2.redhat.com";
        let scope = "repository:registry:pull,push";

        let header_value = HeaderValue::from_str(&format!(
            r#"Bearer realm="{}",service="{}",scope="{}""#,
            realm, service, scope
        ))?;

        let content = WwwAuthenticateHeaderContent::from_www_authentication_header(header_value)?;

        assert_eq!(
            WwwAuthenticateHeaderContent::Bearer(WwwAuthenticateHeaderContentBearer {
                realm: realm.to_string(),
                service: Some(service.to_string()),
                scope: Some(scope.to_string()),
            }),
            content
        );

        Ok(())
    }

    // Testing for this situation to work:
    // [TRACE dkregistry::v2::auth] Sending request to 'https://localhost:5000/v2/'
    // [TRACE dkregistry::v2::auth] GET 'Response { url: "https://localhost:5000/v2/", status: 401, headers: {"content-type": "application/json; charset=utf-8", "docker-distribution-api-version": "registry/2.0", "www-authenticate": "Basic realm=\"Registry\"", "x-content-type-options": "nosniff", "date": "Thu, 18 Jun 2020 09:04:24 GMT", "content-length": "87"} }'
    // [TRACE dkregistry::v2::auth] GET 'https://localhost:5000/v2/' status: 401
    // [TRACE dkregistry::v2::auth] Token provider: Registry
    // [TRACE dkregistry::v2::auth] login: token endpoint: Registry&scope=repository:cincinnati-ci/ocp-release-dev:pull
    // [ERROR graph_builder::graph] failed to fetch all release metadata
    // [ERROR graph_builder::graph] failed to parse url from string 'Registry&scope=repository:cincinnati-ci/ocp-release-dev:pull': relative URL without a base
    #[test]
    fn basic_realm_parses_correctly() -> Result<()> {
        let realm = "Registry realm";

        let header_value = HeaderValue::from_str(&format!(r#"Basic realm="{}""#, realm))?;

        let content = WwwAuthenticateHeaderContent::from_www_authentication_header(header_value)?;

        assert_eq!(
            WwwAuthenticateHeaderContent::Basic(WwwAuthenticateHeaderContentBasic {
                realm: realm.to_string(),
            }),
            content
        );

        Ok(())
    }
}
