use actix_service::{Service, Transform};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::error::Error;
use actix_web::http::header::{HeaderValue, AUTHORIZATION};
use actix_web::{HttpMessage, HttpRequest, Result};
use futures::future::{ok, Either, FutureResult};
use futures::{Future, IntoFuture, Poll};
use futures3::TryFutureExt;
use jwt::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::fmt::Display;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::db::Db;
use crate::errors::ApiError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimsScope {
    // Permission to list all jobs in the system. Should not be given to untrusted parties.
    Jobs,
    // Permission to create, list, and purge builds, to get a build's jobs, and to commit uploaded files to the build.
    Build,
    // Permission to upload files and refs to builds.
    Upload,
    // Permission to publish builds.
    Publish,
    // Permission to upload deltas for a repo. Should not be given to untrusted parties.
    Generate,
    // Permission to list builds and to download a build repo.
    Download,
    // Permission to republish an app (take it from the repo, re-run the publish hook, and publish it back). Should not
    // be given to untrusted parties.
    Republish,
    // Permission to change the status of any build check (e.g. mark it as successful, failed, etc.) Should only be
    // given to reviewers or passed to the check scripts themselves.
    ReviewCheck,
    // Permission to get usage information for any token and to revoke any token. Should not be given to untrusted
    // parties.
    TokenManagement,

    #[serde(other)]
    Unknown,
}

impl Display for ClaimsScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", format!("{self:?}").to_ascii_lowercase())
    }
}

/* Claims are used in two forms, one for API calls, and one for
 * general repo access, the second one is simpler and just uses scope
 * for the allowed ids, and sub means the user doing the access (which
 * is not verified). */
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub name: Option<String>,
    pub sub: String, // "build", "build/N", user id for repo tokens, or "" for certain management tokens
    pub exp: i64,
    pub jti: Option<String>, // an unique ID for the token, for revocation.

    #[serde(default)]
    pub scope: Vec<ClaimsScope>,
    #[serde(default)]
    pub prefixes: Vec<String>, // [''] => all, ['org.foo'] => org.foo + org.foo.bar (but not org.foobar)
    #[serde(default)]
    pub apps: Vec<String>, // like prefixes, but only exact matches
    #[serde(default)]
    pub repos: Vec<String>, // list of repo names or a '' for match all
    #[serde(default)]
    pub branches: Vec<String>, // list of allowed branches or a '' for match all
    #[serde(default)]
    pub token_type: Option<String>, // "app" to require at least one app ref
}

pub trait ClaimsValidator {
    fn get_claims(&self) -> Option<Claims>;
    fn validate_claims<Func>(&self, func: Func) -> Result<(), ApiError>
    where
        Func: Fn(&Claims) -> Result<(), ApiError>;
    fn has_token_claims(
        &self,
        required_sub: &str,
        required_scope: ClaimsScope,
    ) -> Result<(), ApiError>;
    fn has_token_prefix(&self, id: &str) -> Result<(), ApiError>;
    fn has_token_repo(&self, repo: &str) -> Result<(), ApiError>;
}

pub fn sub_has_prefix(required_sub: &str, claimed_sub: &str) -> bool {
    // Matches using a path-prefix style comparison:
    //  claimed_sub == "build" should match required_sub == "build" or "build/N[/...]"
    //  claimed_sub == "build/N" should only matchs required_sub == "build/N[/...]"
    if let Some(rest) = required_sub.strip_prefix(claimed_sub) {
        if rest.is_empty() || rest.starts_with('/') {
            return true;
        }
    };
    false
}

pub fn id_matches_prefix(id: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if let Some(rest) = id.strip_prefix(prefix) {
        if rest.is_empty() || rest.starts_with('.') {
            return true;
        }
    };
    false
}

pub fn id_matches_one_prefix(id: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| id_matches_prefix(id, prefix))
}

pub fn repo_matches_claimed(repo: &str, claimed_repo: &str) -> bool {
    if claimed_repo.is_empty() {
        return true;
    }
    repo == claimed_repo
}

pub fn repo_matches_one_claimed(repo: &str, claimed_repos: &[String]) -> bool {
    claimed_repos
        .iter()
        .any(|claimed_repo| repo_matches_claimed(repo, claimed_repo))
}

impl ClaimsValidator for HttpRequest {
    fn get_claims(&self) -> Option<Claims> {
        self.extensions().get::<Claims>().cloned()
    }

    fn validate_claims<Func>(&self, func: Func) -> Result<(), ApiError>
    where
        Func: Fn(&Claims) -> Result<(), ApiError>,
    {
        if let Some(claims) = self.extensions().get::<Claims>() {
            func(claims)
        } else {
            Err(ApiError::NotEnoughPermissions(
                "No token specified".to_string(),
            ))
        }
    }

    fn has_token_claims(
        &self,
        required_sub: &str,
        required_scope: ClaimsScope,
    ) -> Result<(), ApiError> {
        self.validate_claims(|claims| {
            // Matches using a path-prefix style comparison:
            //  claim.sub == "build" should match required_sub == "build" or "build/N[/...]"
            //  claim.sub == "build/N" should only matchs required_sub == "build/N[/...]"
            if !sub_has_prefix(required_sub, &claims.sub) {
                return Err(ApiError::NotEnoughPermissions(format!(
                    "Not matching sub '{required_sub}' in token"
                )));
            }
            if !claims.scope.contains(&required_scope) {
                return Err(ApiError::NotEnoughPermissions(format!(
                    "Not matching scope '{required_scope}' in token"
                )));
            }
            Ok(())
        })
    }

    /* A token prefix is something like org.my.App, and should allow
     * you to create refs like org.my.App, org.my.App.Debug, and
     * org.my.App.Some.Long.Thing. However, it should not allow
     * org.my.AppSuffix. Also checks the "apps" field for exact matches
     * only.
     */
    fn has_token_prefix(&self, id: &str) -> Result<(), ApiError> {
        self.validate_claims(|claims| {
            if claims.prefixes.is_empty() {
                return Ok(());
            }
            if !id_matches_one_prefix(id, &claims.prefixes)
                && !claims.apps.contains(&id.to_string())
            {
                return Err(ApiError::NotEnoughPermissions(format!(
                    "Id {id} not matching prefix in token"
                )));
            }
            Ok(())
        })
    }

    fn has_token_repo(&self, repo: &str) -> Result<(), ApiError> {
        self.validate_claims(|claims| {
            if !repo_matches_one_claimed(repo, &claims.repos) {
                return Err(ApiError::NotEnoughPermissions(
                    "Not matching repo in token".to_string(),
                ));
            }
            Ok(())
        })
    }
}

pub struct Inner {
    db: Db,
    prefix: Option<String>,
    secret: Vec<u8>,
    optional: bool,
}

fn parse_authorization(prefix: Option<String>, header: &HeaderValue) -> Result<String, ApiError> {
    // "Bearer *" length
    if header.len() < 8 {
        return Err(ApiError::InvalidToken(
            "Header length too short".to_string(),
        ));
    }

    let mut parts = header
        .to_str()
        .map_err(|_| ApiError::InvalidToken("Cannot convert header to string".to_string()))?
        .splitn(2, ' ');
    match parts.next() {
        Some("Bearer") => (),
        _ => {
            return Err(ApiError::InvalidToken(
                "Token scheme is not Bearer".to_string(),
            ))
        }
    }

    let mut token = parts
        .next()
        .ok_or_else(|| ApiError::InvalidToken("No token value in header".to_string()))?;

    if let Some(prefix) = prefix {
        token = token.strip_prefix(&prefix).unwrap_or(token);
    }

    Ok(token.to_string())
}

fn validate_claims(secret: Vec<u8>, token: String) -> Result<Claims, ApiError> {
    let mut validation = Validation::default();

    validation.validate_exp = false;

    let token_data = match decode::<Claims>(
        &token,
        &DecodingKey::from_secret(secret.as_ref()),
        &validation,
    ) {
        Ok(c) => c,
        Err(_err) => return Err(ApiError::InvalidToken("Invalid token claims".to_string())),
    };

    let claims = token_data.claims;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    if claims.exp < now {
        return Err(ApiError::InvalidToken("Token is expired".to_string()));
    }

    Ok(claims)
}

pub struct TokenParser(Rc<Inner>);

impl TokenParser {
    pub fn new(db: Db, config: &Config, secret: &[u8]) -> TokenParser {
        TokenParser(Rc::new(Inner {
            db,
            prefix: config.token_prefix.clone(),
            secret: secret.to_vec(),
            optional: false,
        }))
    }
    pub fn optional(db: Db, config: &Config, secret: &[u8]) -> TokenParser {
        TokenParser(Rc::new(Inner {
            db,
            prefix: config.token_prefix.clone(),
            secret: secret.to_vec(),
            optional: true,
        }))
    }
}

impl<S: 'static, B> Transform<S> for TokenParser
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = TokenParserMiddleware<S>;
    type Future = FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(TokenParserMiddleware {
            service: Rc::new(RefCell::new(service)),
            inner: self.0.clone(),
        })
    }
}

/// TokenParser middleware
pub struct TokenParserMiddleware<S> {
    service: Rc<RefCell<S>>,
    inner: Rc<Inner>,
}

fn get_token(
    optional: bool,
    prefix: Option<String>,
    req: &ServiceRequest,
) -> Result<Option<String>, ApiError> {
    let header = match req.headers().get(AUTHORIZATION) {
        Some(h) => h,
        None => {
            if optional {
                return Ok(None);
            }
            return Err(ApiError::InvalidToken(
                "No Authorization header".to_string(),
            ));
        }
    };
    let token = parse_authorization(prefix, header)?;
    Ok(Some(token))
}

async fn check_token_async(db: Db, secret: Vec<u8>, token: String) -> Result<Claims, ApiError> {
    let claims = validate_claims(secret, token)?;

    /* If the token has an ID, make sure it has not been revoked. */
    if let Some(jti) = &claims.jti {
        if let Err(e) = db.check_token(jti.clone(), claims.exp).await {
            log::warn!("Attempt to use a revoked token: '{jti}'");
            return Err(e);
        }
    }

    Ok(claims)
}

fn check_token(
    db: Db,
    secret: Vec<u8>,
    token: String,
) -> impl futures::Future<Item = Claims, Error = ApiError> {
    Box::pin(check_token_async(db, secret, token)).compat()
}

impl<S, B> Service for TokenParserMiddleware<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    #[allow(clippy::type_complexity)]
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.borrow_mut().poll_ready()
    }

    fn call(&mut self, req: ServiceRequest) -> Self::Future {
        let srv = self.service.clone();
        let secret = self.inner.secret.clone();
        let prefix = self.inner.prefix.clone();
        let db = self.inner.db.clone();

        let token = get_token(self.inner.optional, prefix, &req)
            .into_future()
            .and_then(|token| token.map(|t| check_token(db, secret, t)));

        let fut = token.then(move |maybe_claims| {
            let maybe_claims = match maybe_claims {
                Err(e) => return Either::B(ok(req.error_response(e))),
                Ok(c) => c,
            };

            let c = maybe_claims.clone();

            if let Some(claims) = maybe_claims {
                req.extensions_mut().insert(claims);
            }

            Either::A(Box::new(srv.borrow_mut().call(req).and_then(move |resp| {
                if resp.status() == 401 || resp.status() == 403 {
                    if let Some(ref claims) = c {
                        log::info!("Presented claims: {:?}", claims);
                    }
                }
                Ok(resp)
            })))
        });

        Box::new(fut)
    }
}
