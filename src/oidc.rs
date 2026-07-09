// The OpenID Connect endpoints: discovery, JWKS, authorization, token, userinfo,
// introspection, revocation and RP-initiated logout.
//
// URLs are served under Keycloak's layout (/realms/{realm}/protocol/openid-connect/...)
// with short aliases next to them, so both a hardcoded Keycloak client and a plain
// discovery-driven one find their way.

use std::sync::Mutex;

use crate::clients::{Client, Clients};
use crate::http::{read_body, BuiltResponse, Headers, Request};
use crate::json::J;
use crate::jwt;
use crate::rsa::RsaKey;
use crate::store::{AuthCode, RefreshToken, Session, Store};
use crate::url::{form_decode, parse_query, percent_encode, qget};
use crate::users::{User, Users};
use crate::util::{html_escape, now_secs};
use crate::{base64, rand, sha256};

pub const COOKIE: &str = "minicloak_session";
const MAX_BODY: usize = 64 * 1024;
const SUPPORTED_SCOPES: [&str; 5] = ["openid", "profile", "email", "roles", "offline_access"];

pub struct Server {
    pub realm: String,
    pub issuer_override: Option<String>,
    pub key: RsaKey,
    pub kid: String,
    pub users: Users,
    pub clients: Clients,
    pub store: Mutex<Store>,
    pub access_ttl: u64,
    pub refresh_ttl: u64,
    pub code_ttl: u64,
    pub session_ttl: u64,
    pub auto_login: Option<String>,
    pub quick_login: bool,
    pub cors: bool,
}

#[derive(Debug, PartialEq)]
enum Ep {
    Home,
    Discovery,
    Jwks,
    Auth,
    Token,
    UserInfo,
    Introspect,
    Revoke,
    Logout,
}

impl Server {
    /// The issuer this request should see. Defaults to the request's own Host header so
    /// that reaching minicloak via 127.0.0.1, `localhost` or a container name all yield
    /// self-consistent discovery documents and `iss` claims.
    fn issuer(&self, headers: &Headers) -> String {
        if let Some(iss) = &self.issuer_override {
            return iss.clone();
        }
        let host = headers.get("host").unwrap_or("127.0.0.1:9500");
        format!("http://{}/realms/{}", host, self.realm)
    }

    fn oidc_base(&self, headers: &Headers) -> String {
        format!("{}/protocol/openid-connect", self.issuer(headers))
    }

    fn route(&self, path: &str) -> Option<Ep> {
        let realm_prefix = format!("/realms/{}", self.realm);
        let p = path.strip_prefix(realm_prefix.as_str()).unwrap_or(path);
        let p = p.strip_prefix("/protocol/openid-connect").unwrap_or(p);
        Some(match p {
            "" | "/" => Ep::Home,
            "/.well-known/openid-configuration" => Ep::Discovery,
            "/.well-known/jwks.json" | "/jwks.json" | "/certs" => Ep::Jwks,
            "/auth" | "/authorize" => Ep::Auth,
            "/token" => Ep::Token,
            "/userinfo" => Ep::UserInfo,
            "/token/introspect" | "/introspect" => Ep::Introspect,
            "/revoke" => Ep::Revoke,
            "/logout" => Ep::Logout,
            _ => return None,
        })
    }
}

pub fn dispatch<R: std::io::BufRead>(srv: &Server, req: &mut Request<R>) -> BuiltResponse {
    let origin = req.headers.get("origin").map(|s| s.to_string());

    // Browsers preflight the cross-origin token/userinfo calls that SPAs make.
    if req.method == "OPTIONS" {
        return cors(srv, BuiltResponse::new(204), origin.as_deref())
            .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            .header(
                "Access-Control-Allow-Headers",
                "Authorization, Content-Type",
            )
            .header("Access-Control-Max-Age", "600");
    }

    let ep = match srv.route(&req.path) {
        Some(e) => e,
        None => return error_page(404, "Not found", &format!("No endpoint at {}", req.path)),
    };

    let resp = match ep {
        Ep::Home => home(srv, req),
        Ep::Discovery => discovery(srv, req),
        Ep::Jwks => BuiltResponse::new(200).json(jwt::jwks(&srv.key, &srv.kid)),
        Ep::Auth => authorize(srv, req),
        Ep::Token => post_only(req, |r| token(srv, r)),
        Ep::UserInfo => userinfo(srv, req),
        Ep::Introspect => post_only(req, |r| introspect(srv, r)),
        Ep::Revoke => post_only(req, |r| revoke(srv, r)),
        Ep::Logout => logout(srv, req),
    };
    cors(srv, resp, origin.as_deref())
}

fn post_only<R: std::io::BufRead>(
    req: &mut Request<R>,
    f: impl FnOnce(&mut Request<R>) -> BuiltResponse,
) -> BuiltResponse {
    if req.method != "POST" {
        return oauth_error(405, "invalid_request", "POST required");
    }
    f(req)
}

fn cors(srv: &Server, resp: BuiltResponse, origin: Option<&str>) -> BuiltResponse {
    if !srv.cors {
        return resp;
    }
    match origin {
        // Echo the origin (not `*`) so that credentialed fetches are allowed too.
        Some(o) => resp
            .header("Access-Control-Allow-Origin", o)
            .header("Access-Control-Allow-Credentials", "true")
            .header("Vary", "Origin"),
        None => resp.header("Access-Control-Allow-Origin", "*"),
    }
}

// --- discovery ---

fn discovery<R: std::io::BufRead>(srv: &Server, req: &Request<R>) -> BuiltResponse {
    let iss = srv.issuer(&req.headers);
    let b = srv.oidc_base(&req.headers);
    let doc = J::obj(vec![
        ("issuer", J::S(iss)),
        ("authorization_endpoint", J::S(format!("{}/auth", b))),
        ("token_endpoint", J::S(format!("{}/token", b))),
        ("userinfo_endpoint", J::S(format!("{}/userinfo", b))),
        ("jwks_uri", J::S(format!("{}/certs", b))),
        ("end_session_endpoint", J::S(format!("{}/logout", b))),
        (
            "introspection_endpoint",
            J::S(format!("{}/token/introspect", b)),
        ),
        ("revocation_endpoint", J::S(format!("{}/revoke", b))),
        ("response_types_supported", J::arr_s(&["code"])),
        ("response_modes_supported", J::arr_s(&["query"])),
        ("subject_types_supported", J::arr_s(&["public"])),
        (
            "id_token_signing_alg_values_supported",
            J::arr_s(&["RS256"]),
        ),
        (
            "grant_types_supported",
            J::arr_s(&[
                "authorization_code",
                "refresh_token",
                "client_credentials",
                "password",
            ]),
        ),
        ("scopes_supported", J::arr_s(&SUPPORTED_SCOPES)),
        (
            "token_endpoint_auth_methods_supported",
            J::arr_s(&["client_secret_basic", "client_secret_post", "none"]),
        ),
        (
            "code_challenge_methods_supported",
            J::arr_s(&["S256", "plain"]),
        ),
        (
            "claims_supported",
            J::arr_s(&[
                "iss",
                "sub",
                "aud",
                "exp",
                "iat",
                "jti",
                "auth_time",
                "nonce",
                "azp",
                "sid",
                "preferred_username",
                "email",
                "email_verified",
                "name",
                "given_name",
                "family_name",
                "roles",
            ]),
        ),
    ]);
    BuiltResponse::new(200).json(doc.to_string())
}

// --- authorization endpoint ---

struct AuthReq {
    client_id: String,
    redirect_uri: String,
    scope: String,
    state: String,
    nonce: String,
    challenge: String,
    challenge_method: String,
}

fn authorize<R: std::io::BufRead>(srv: &Server, req: &mut Request<R>) -> BuiltResponse {
    let params = match req.method.as_str() {
        "GET" => parse_query(&req.query_raw),
        "POST" => match read_body(req, MAX_BODY) {
            Ok(b) => parse_query(&String::from_utf8_lossy(&b)),
            Err(_) => return error_page(400, "Bad request", "Unreadable form body"),
        },
        _ => return error_page(405, "Method not allowed", "Use GET or POST"),
    };
    let get = |k: &str| qget(&params, k).unwrap_or("").to_string();

    // Until client_id and redirect_uri are known-good we must NOT redirect anywhere:
    // that would turn minicloak into an open redirector. Render an error page instead.
    let client = match srv.clients.get(&get("client_id")) {
        Some(c) => c,
        None => return error_page(400, "Unknown client", "No client with that client_id"),
    };
    let redirect_uri = match resolve_redirect(client, &get("redirect_uri")) {
        Some(u) => u,
        None => {
            return error_page(
                400,
                "Invalid redirect_uri",
                "The redirect_uri is not registered for this client",
            )
        }
    };

    let ar = AuthReq {
        client_id: client.id.clone(),
        redirect_uri,
        scope: filter_scope(&get("scope"), true),
        state: get("state"),
        nonce: get("nonce"),
        challenge: get("code_challenge"),
        challenge_method: {
            let m = get("code_challenge_method");
            if m.is_empty() && !get("code_challenge").is_empty() {
                "plain".to_string() // RFC 7636 §4.3 default
            } else {
                m
            }
        },
    };

    // From here on, errors go back to the client as a redirect.
    let response_type = get("response_type");
    if response_type != "code" {
        return redirect_err(&ar, "unsupported_response_type", "only response_type=code");
    }
    if !ar.challenge_method.is_empty()
        && ar.challenge_method != "S256"
        && ar.challenge_method != "plain"
    {
        return redirect_err(&ar, "invalid_request", "unsupported code_challenge_method");
    }
    if client.is_public() && ar.challenge.is_empty() {
        return redirect_err(
            &ar,
            "invalid_request",
            "PKCE is required for public clients",
        );
    }

    let now = now_secs();

    if req.method == "POST" {
        let username = get("username");
        // Quick-login buttons post a username with no password. Dev convenience only.
        let user = if srv.quick_login && get("quick") == "1" {
            srv.users.get(&username)
        } else {
            srv.users.authenticate(&username, &get("password"))
        };
        return match user {
            Some(u) => {
                let (sid, cookie) = new_session(srv, u, now);
                grant_code(srv, &ar, u, &sid, now).header("Set-Cookie", &cookie)
            }
            None => BuiltResponse::new(401).html(login_page(
                srv,
                &req.path,
                &ar,
                Some("Invalid username or password"),
            )),
        };
    }

    // GET: reuse an existing browser session unless the RP demands a fresh login.
    let prompt = get("prompt");
    if let Some(name) = &srv.auto_login {
        if let Some(u) = srv.users.get(name) {
            let (sid, cookie) = new_session(srv, u, now);
            return grant_code(srv, &ar, u, &sid, now).header("Set-Cookie", &cookie);
        }
    }
    if prompt != "login" {
        if let Some((sid, username, auth_time)) = current_session(srv, &req.headers, now) {
            if let Some(u) = srv.users.get(&username) {
                return grant_code_at(srv, &ar, u, &sid, now, auth_time);
            }
        }
    }
    if prompt == "none" {
        return redirect_err(&ar, "login_required", "no active session");
    }
    BuiltResponse::new(200).html(login_page(srv, &req.path, &ar, None))
}

fn resolve_redirect(client: &Client, requested: &str) -> Option<String> {
    if requested.is_empty() {
        // Convenience: a client with exactly one concrete URI may omit it.
        return match client.redirect_uris.as_slice() {
            [only] if !only.ends_with('*') => Some(only.clone()),
            _ => None,
        };
    }
    client
        .allows_redirect(requested)
        .then(|| requested.to_string())
}

fn grant_code(srv: &Server, ar: &AuthReq, user: &User, sid: &str, now: u64) -> BuiltResponse {
    grant_code_at(srv, ar, user, sid, now, now)
}

fn grant_code_at(
    srv: &Server,
    ar: &AuthReq,
    user: &User,
    sid: &str,
    now: u64,
    auth_time: u64,
) -> BuiltResponse {
    let code = rand::token(32);
    srv.store.lock().unwrap().put_code(
        code.clone(),
        AuthCode {
            client_id: ar.client_id.clone(),
            username: user.username.clone(),
            redirect_uri: ar.redirect_uri.clone(),
            scope: ar.scope.clone(),
            nonce: ar.nonce.clone(),
            code_challenge: ar.challenge.clone(),
            code_challenge_method: ar.challenge_method.clone(),
            auth_time,
            sid: sid.to_string(),
            expires_at: now + srv.code_ttl,
        },
    );
    let mut url = format!(
        "{}{}code={}",
        ar.redirect_uri,
        sep(&ar.redirect_uri),
        percent_encode(&code)
    );
    if !ar.state.is_empty() {
        url.push_str(&format!("&state={}", percent_encode(&ar.state)));
    }
    BuiltResponse::redirect(&url)
}

fn redirect_err(ar: &AuthReq, err: &str, desc: &str) -> BuiltResponse {
    let mut url = format!(
        "{}{}error={}&error_description={}",
        ar.redirect_uri,
        sep(&ar.redirect_uri),
        percent_encode(err),
        percent_encode(desc)
    );
    if !ar.state.is_empty() {
        url.push_str(&format!("&state={}", percent_encode(&ar.state)));
    }
    BuiltResponse::redirect(&url)
}

fn sep(uri: &str) -> char {
    if uri.contains('?') {
        '&'
    } else {
        '?'
    }
}

fn new_session(srv: &Server, user: &User, now: u64) -> (String, String) {
    let sid = rand::token(24);
    srv.store.lock().unwrap().put_session(
        sid.clone(),
        Session {
            username: user.username.clone(),
            auth_time: now,
            expires_at: now + srv.session_ttl,
        },
    );
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        COOKIE, sid, srv.session_ttl
    );
    (sid, cookie)
}

fn current_session(srv: &Server, headers: &Headers, now: u64) -> Option<(String, String, u64)> {
    let sid = cookie_value(headers.get("cookie")?, COOKIE)?;
    let store = srv.store.lock().unwrap();
    let s = store.get_session(&sid, now)?;
    Some((sid, s.username.clone(), s.auth_time))
}

fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

// --- token endpoint ---

fn token<R: std::io::BufRead>(srv: &Server, req: &mut Request<R>) -> BuiltResponse {
    let auth_header = req.headers.get("authorization").map(|s| s.to_string());
    let body = match read_body(req, MAX_BODY) {
        Ok(b) => b,
        Err(_) => return oauth_error(400, "invalid_request", "unreadable body"),
    };
    let params = parse_query(&String::from_utf8_lossy(&body));
    let get = |k: &str| qget(&params, k).unwrap_or("").to_string();

    let client = match authenticate_client(srv, auth_header.as_deref(), &params) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let now = now_secs();

    match get("grant_type").as_str() {
        "authorization_code" => {
            let code = get("code");
            let ac = match srv.store.lock().unwrap().take_code(&code, now) {
                Some(c) => c,
                None => {
                    return oauth_error(
                        400,
                        "invalid_grant",
                        "code is invalid, expired or already used",
                    )
                }
            };
            if ac.client_id != client.id {
                return oauth_error(400, "invalid_grant", "code was issued to another client");
            }
            let redirect_uri = get("redirect_uri");
            if !redirect_uri.is_empty() && redirect_uri != ac.redirect_uri {
                return oauth_error(400, "invalid_grant", "redirect_uri mismatch");
            }
            if let Err(e) = verify_pkce(&ac, &get("code_verifier")) {
                return e;
            }
            let user = match srv.users.get(&ac.username) {
                Some(u) => u,
                None => return oauth_error(400, "invalid_grant", "user no longer exists"),
            };
            issue(
                srv,
                req,
                &client,
                Some(user),
                &ac.scope,
                &ac.sid,
                ac.auth_time,
                &ac.nonce,
                now,
            )
        }

        "refresh_token" => {
            let rt = match srv
                .store
                .lock()
                .unwrap()
                .take_refresh(&get("refresh_token"), now)
            {
                Some(r) => r,
                None => {
                    return oauth_error(400, "invalid_grant", "refresh token is invalid or expired")
                }
            };
            if rt.client_id != client.id {
                return oauth_error(
                    400,
                    "invalid_grant",
                    "refresh token was issued to another client",
                );
            }
            // A narrower scope may be requested on refresh, never a wider one.
            let scope = match get("scope").as_str() {
                "" => rt.scope.clone(),
                s => {
                    let narrowed = filter_scope(s, false);
                    if narrowed
                        .split(' ')
                        .any(|x| !x.is_empty() && !rt.scope.split(' ').any(|o| o == x))
                    {
                        return oauth_error(400, "invalid_scope", "cannot widen scope on refresh");
                    }
                    narrowed
                }
            };
            let user = if rt.username.is_empty() {
                None
            } else {
                srv.users.get(&rt.username)
            };
            if user.is_none() && !rt.username.is_empty() {
                return oauth_error(400, "invalid_grant", "user no longer exists");
            }
            issue(
                srv,
                req,
                &client,
                user,
                &scope,
                &rt.sid,
                rt.auth_time,
                "",
                now,
            )
        }

        "password" => {
            let user = match srv.users.authenticate(&get("username"), &get("password")) {
                Some(u) => u,
                None => return oauth_error(401, "invalid_grant", "invalid username or password"),
            };
            let scope = filter_scope(&get("scope"), true);
            let sid = rand::token(24);
            issue(srv, req, &client, Some(user), &scope, &sid, now, "", now)
        }

        "client_credentials" => {
            if client.is_public() {
                return oauth_error(
                    401,
                    "invalid_client",
                    "client_credentials requires a confidential client",
                );
            }
            let scope = filter_scope(&get("scope"), false);
            issue(srv, req, &client, None, &scope, "", now, "", now)
        }

        "" => oauth_error(400, "invalid_request", "grant_type is required"),
        other => oauth_error(
            400,
            "unsupported_grant_type",
            &format!("unsupported grant_type: {}", other),
        ),
    }
}

fn verify_pkce(ac: &AuthCode, verifier: &str) -> Result<(), BuiltResponse> {
    if ac.code_challenge.is_empty() {
        return Ok(());
    }
    if verifier.is_empty() {
        return Err(oauth_error(
            400,
            "invalid_grant",
            "code_verifier is required",
        ));
    }
    let computed = match ac.code_challenge_method.as_str() {
        "S256" => base64::encode_url(&sha256::sha256(verifier.as_bytes())),
        _ => verifier.to_string(),
    };
    if computed != ac.code_challenge {
        return Err(oauth_error(
            400,
            "invalid_grant",
            "PKCE verification failed",
        ));
    }
    Ok(())
}

/// client_secret_basic, client_secret_post, or `none` for a registered public client.
fn authenticate_client(
    srv: &Server,
    auth_header: Option<&str>,
    params: &[(String, String)],
) -> Result<Client, BuiltResponse> {
    let unauthorized = |m: &str| {
        Err(oauth_error(401, "invalid_client", m)
            .header("WWW-Authenticate", "Basic realm=\"minicloak\""))
    };

    let (id, secret) = match auth_header.and_then(parse_basic) {
        Some(pair) => pair,
        None => (
            qget(params, "client_id").unwrap_or("").to_string(),
            qget(params, "client_secret").unwrap_or("").to_string(),
        ),
    };
    if id.is_empty() {
        return unauthorized("client_id is required");
    }
    let client = match srv.clients.get(&id) {
        Some(c) => c.clone(),
        None => return unauthorized("unknown client"),
    };
    if client.is_public() {
        return Ok(client);
    }
    if !client.check_secret(&secret) {
        return unauthorized("invalid client_secret");
    }
    Ok(client)
}

fn parse_basic(header: &str) -> Option<(String, String)> {
    let b64 = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let raw = base64::decode(b64.trim())?;
    let s = String::from_utf8(raw).ok()?;
    let (id, secret) = s.split_once(':')?;
    // RFC 6749 §2.3.1: both halves are form-urlencoded before base64.
    Some((form_decode(id), form_decode(secret)))
}

#[allow(clippy::too_many_arguments)]
fn issue<R: std::io::BufRead>(
    srv: &Server,
    req: &Request<R>,
    client: &Client,
    user: Option<&User>,
    scope: &str,
    sid: &str,
    auth_time: u64,
    nonce: &str,
    now: u64,
) -> BuiltResponse {
    let iss = srv.issuer(&req.headers);
    let exp = now + srv.access_ttl;

    let sub = match user {
        Some(u) => u.sub(),
        None => format!("service-account-{}", client.id),
    };

    let mut access = vec![
        ("iss", J::S(iss.clone())),
        ("sub", J::S(sub.clone())),
        ("aud", J::S(client.id.clone())),
        ("azp", J::S(client.id.clone())),
        ("exp", J::N(exp)),
        ("iat", J::N(now)),
        ("jti", J::S(rand::hex(16))),
        ("typ", J::s("Bearer")),
        ("scope", J::S(scope.to_string())),
    ];
    if !sid.is_empty() {
        access.push(("sid", J::S(sid.to_string())));
    }
    if let Some(u) = user {
        access.push(("preferred_username", J::S(u.username.clone())));
        add_profile_claims(&mut access, u, scope);
    }
    let access_token = jwt::encode(J::O(owned(access)), &srv.key, &srv.kid);

    let wants_openid = has_scope(scope, "openid");
    let id_token = user.filter(|_| wants_openid).map(|u| {
        let mut c = vec![
            ("iss", J::S(iss.clone())),
            ("sub", J::S(sub.clone())),
            ("aud", J::S(client.id.clone())),
            ("azp", J::S(client.id.clone())),
            ("exp", J::N(exp)),
            ("iat", J::N(now)),
            ("auth_time", J::N(auth_time)),
            ("jti", J::S(rand::hex(16))),
            ("typ", J::s("ID")),
            ("preferred_username", J::S(u.username.clone())),
        ];
        if !sid.is_empty() {
            c.push(("sid", J::S(sid.to_string())));
        }
        if !nonce.is_empty() {
            c.push(("nonce", J::S(nonce.to_string())));
        }
        add_profile_claims(&mut c, u, scope);
        jwt::encode(J::O(owned(c)), &srv.key, &srv.kid)
    });

    // client_credentials gets no refresh token: the client can just ask again.
    let refresh_token = user.map(|u| {
        let t = rand::token(32);
        srv.store.lock().unwrap().put_refresh(
            t.clone(),
            RefreshToken {
                client_id: client.id.clone(),
                username: u.username.clone(),
                scope: scope.to_string(),
                sid: sid.to_string(),
                auth_time,
                expires_at: now + srv.refresh_ttl,
            },
        );
        t
    });

    let mut out = vec![
        ("access_token", J::S(access_token)),
        ("token_type", J::s("Bearer")),
        ("expires_in", J::N(srv.access_ttl)),
        ("scope", J::S(scope.to_string())),
        ("not-before-policy", J::N(0)),
    ];
    if let Some(t) = id_token {
        out.push(("id_token", J::S(t)));
    }
    if let Some(t) = refresh_token {
        out.push(("refresh_token", J::S(t)));
        out.push(("refresh_expires_in", J::N(srv.refresh_ttl)));
    }
    if !sid.is_empty() {
        out.push(("session_state", J::S(sid.to_string())));
    }

    BuiltResponse::new(200)
        .header("Cache-Control", "no-store")
        .header("Pragma", "no-cache")
        .json(J::O(owned(out)).to_string())
}

fn owned(v: Vec<(&str, J)>) -> Vec<(String, J)> {
    v.into_iter().map(|(k, val)| (k.to_string(), val)).collect()
}

fn add_profile_claims(claims: &mut Vec<(&'static str, J)>, u: &User, scope: &str) {
    if has_scope(scope, "email") && !u.email.is_empty() {
        claims.push(("email", J::S(u.email.clone())));
        claims.push(("email_verified", J::B(true)));
    }
    if has_scope(scope, "profile") && !u.name.is_empty() {
        let (given, family) = u.given_family();
        claims.push(("name", J::S(u.name.clone())));
        claims.push(("given_name", J::S(given)));
        claims.push(("family_name", J::S(family)));
    }
    if !u.roles.is_empty() {
        let roles: Vec<J> = u.roles.iter().map(|r| J::S(r.clone())).collect();
        let realm_roles: Vec<J> = u.roles.iter().map(|r| J::S(r.clone())).collect();
        claims.push(("roles", J::A(roles)));
        // Keycloak clients (and Spring's JwtGrantedAuthoritiesConverter presets) look here.
        claims.push(("realm_access", J::obj(vec![("roles", J::A(realm_roles))])));
    }
}

fn has_scope(scope: &str, want: &str) -> bool {
    scope.split(' ').any(|s| s == want)
}

/// Keep only scopes we understand, in request order, without duplicates.
///
/// `default_openid` applies to the user-facing grants, where an empty scope almost
/// always means "I want a login". A client_credentials grant has no user, so
/// defaulting it to `openid` would claim a scope we cannot honour.
fn filter_scope(requested: &str, default_openid: bool) -> String {
    let mut out: Vec<&str> = Vec::new();
    for s in requested.split([' ', '+']).filter(|s| !s.is_empty()) {
        if SUPPORTED_SCOPES.contains(&s) && !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() && default_openid {
        out.push("openid");
    }
    out.join(" ")
}

// --- userinfo / introspection / revocation ---

fn userinfo<R: std::io::BufRead>(srv: &Server, req: &mut Request<R>) -> BuiltResponse {
    let token = match bearer(req.headers.get("authorization")) {
        Some(t) => t,
        None => {
            return BuiltResponse::new(401)
                .header("WWW-Authenticate", "Bearer realm=\"minicloak\"")
                .json(oauth_error_body("invalid_token", "missing bearer token"))
        }
    };
    let iss = srv.issuer(&req.headers);
    let claims = match jwt::decode(&token, &srv.key, &iss, now_secs(), 10) {
        Ok(c) => c,
        Err(e) => {
            return BuiltResponse::new(401)
                .header(
                    "WWW-Authenticate",
                    &format!(
                        "Bearer error=\"invalid_token\", error_description=\"{}\"",
                        e
                    ),
                )
                .json(oauth_error_body("invalid_token", &e.to_string()))
        }
    };
    if claims.str_field("typ") != Some("Bearer") {
        return BuiltResponse::new(401)
            .json(oauth_error_body("invalid_token", "not an access token"));
    }
    let username = claims.str_field("preferred_username").unwrap_or("");
    let user = match srv.users.get(username) {
        Some(u) => u,
        None => {
            return BuiltResponse::new(401)
                .json(oauth_error_body("invalid_token", "unknown subject"))
        }
    };
    let scope = claims.str_field("scope").unwrap_or("");

    let mut out = vec![
        ("sub", J::S(user.sub())),
        ("preferred_username", J::S(user.username.clone())),
    ];
    add_profile_claims(&mut out, user, scope);
    BuiltResponse::new(200).json(J::O(owned(out)).to_string())
}

fn bearer(header: Option<&str>) -> Option<String> {
    let h = header?;
    let t = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))?;
    Some(t.trim().to_string())
}

fn introspect<R: std::io::BufRead>(srv: &Server, req: &mut Request<R>) -> BuiltResponse {
    let auth_header = req.headers.get("authorization").map(|s| s.to_string());
    let body = match read_body(req, MAX_BODY) {
        Ok(b) => b,
        Err(_) => return oauth_error(400, "invalid_request", "unreadable body"),
    };
    let params = parse_query(&String::from_utf8_lossy(&body));
    if let Err(r) = authenticate_client(srv, auth_header.as_deref(), &params) {
        return r;
    }
    let token = qget(&params, "token").unwrap_or("");
    let now = now_secs();
    let inactive =
        || BuiltResponse::new(200).json(J::obj(vec![("active", J::B(false))]).to_string());

    // A JWT access/ID token?
    if let Ok(c) = jwt::decode(token, &srv.key, &srv.issuer(&req.headers), now, 10) {
        let mut out = vec![("active", J::B(true))];
        for k in [
            "iss",
            "sub",
            "aud",
            "azp",
            "typ",
            "scope",
            "sid",
            "preferred_username",
            "jti",
        ] {
            if let Some(v) = c.str_field(k) {
                out.push((k, J::S(v.to_string())));
            }
        }
        for k in ["exp", "iat", "auth_time"] {
            if let Some(v) = c.u64_field(k) {
                out.push((k, J::N(v)));
            }
        }
        out.push(("token_type", J::s("Bearer")));
        out.push((
            "client_id",
            J::S(c.str_field("azp").unwrap_or("").to_string()),
        ));
        return BuiltResponse::new(200).json(J::O(owned(out)).to_string());
    }

    // Otherwise: an opaque refresh token.
    let store = srv.store.lock().unwrap();
    match store.refresh.get(token) {
        Some(rt) if now < rt.expires_at => {
            let out = vec![
                ("active", J::B(true)),
                ("typ", J::s("Refresh")),
                ("client_id", J::S(rt.client_id.clone())),
                ("username", J::S(rt.username.clone())),
                ("scope", J::S(rt.scope.clone())),
                ("exp", J::N(rt.expires_at)),
            ];
            BuiltResponse::new(200).json(J::O(owned(out)).to_string())
        }
        _ => inactive(),
    }
}

fn revoke<R: std::io::BufRead>(srv: &Server, req: &mut Request<R>) -> BuiltResponse {
    let auth_header = req.headers.get("authorization").map(|s| s.to_string());
    let body = match read_body(req, MAX_BODY) {
        Ok(b) => b,
        Err(_) => return oauth_error(400, "invalid_request", "unreadable body"),
    };
    let params = parse_query(&String::from_utf8_lossy(&body));
    if let Err(r) = authenticate_client(srv, auth_header.as_deref(), &params) {
        return r;
    }
    // RFC 7009: revoking an unknown or already-revoked token is still a success.
    srv.store
        .lock()
        .unwrap()
        .revoke_refresh(qget(&params, "token").unwrap_or(""));
    BuiltResponse::new(200).json("{}".to_string())
}

// --- logout ---

fn logout<R: std::io::BufRead>(srv: &Server, req: &mut Request<R>) -> BuiltResponse {
    let params = match req.method.as_str() {
        "POST" => match read_body(req, MAX_BODY) {
            Ok(b) => parse_query(&String::from_utf8_lossy(&b)),
            Err(_) => return error_page(400, "Bad request", "Unreadable form body"),
        },
        _ => parse_query(&req.query_raw),
    };

    if let Some((sid, _, _)) = current_session(srv, &req.headers, now_secs()) {
        let mut store = srv.store.lock().unwrap();
        store.drop_session(&sid);
        store.revoke_session_refresh(&sid);
    }
    let clear = format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0", COOKIE);

    let target = qget(&params, "post_logout_redirect_uri").unwrap_or("");
    if !target.is_empty() {
        // Only redirect somewhere the requesting client has registered.
        let allowed = match qget(&params, "client_id").and_then(|id| srv.clients.get(id)) {
            Some(c) => c.allows_redirect(target),
            None => srv.clients.list().iter().any(|c| c.allows_redirect(target)),
        };
        if !allowed {
            return error_page(
                400,
                "Invalid redirect",
                "post_logout_redirect_uri is not registered",
            );
        }
        let mut url = target.to_string();
        if let Some(state) = qget(&params, "state") {
            url.push_str(&format!("{}state={}", sep(&url), percent_encode(state)));
        }
        return BuiltResponse::redirect(&url).header("Set-Cookie", &clear);
    }
    BuiltResponse::new(200)
        .header("Set-Cookie", &clear)
        .html(page("Signed out", "<p>You are signed out.</p>"))
}

// --- HTML ---

const STYLE: &str = "body{font:15px/1.5 system-ui,sans-serif;background:#f6f7f9;color:#222;\
display:flex;min-height:100vh;margin:0;align-items:center;justify-content:center}\
.card{background:#fff;padding:2rem;border-radius:10px;box-shadow:0 1px 4px #0002;width:22rem;max-width:90vw}\
h1{font-size:1.25rem;margin:0 0 1rem}label{display:block;margin:.75rem 0 .25rem;font-size:.85rem;color:#555}\
input[type=text],input[type=password]{width:100%;padding:.5rem;border:1px solid #ccd;border-radius:5px;\
box-sizing:border-box;font-size:1rem}\
button{margin-top:1.25rem;width:100%;padding:.6rem;border:0;border-radius:5px;background:#2f6feb;color:#fff;\
font-size:1rem;cursor:pointer}button:hover{background:#2557b8}\
.err{background:#fdeaea;color:#a11;padding:.5rem .75rem;border-radius:5px;font-size:.875rem}\
.hint{margin-top:1.5rem;border-top:1px solid #eee;padding-top:1rem;font-size:.8rem;color:#666}\
.hint form{display:inline}.hint button{width:auto;margin:.2rem .2rem 0 0;padding:.25rem .6rem;\
background:#eef1f6;color:#334;font-size:.8rem}.hint button:hover{background:#dde3ec}\
code{background:#f0f2f5;padding:.1rem .3rem;border-radius:3px}a{color:#2f6feb}";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title>\
<style>{s}</style></head><body><div class=\"card\"><h1>{t}</h1>{b}</div></body></html>",
        t = html_escape(title),
        s = STYLE,
        b = body
    )
}

fn error_page(status: u16, title: &str, msg: &str) -> BuiltResponse {
    BuiltResponse::new(status).html(page(title, &format!("<p>{}</p>", html_escape(msg))))
}

fn hidden(name: &str, value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    format!(
        "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
        name,
        html_escape(value)
    )
}

fn login_page(srv: &Server, action: &str, ar: &AuthReq, error: Option<&str>) -> String {
    // Every parameter of the original GET must survive the POST round trip.
    let carried: String = [
        hidden("response_type", "code"),
        hidden("client_id", &ar.client_id),
        hidden("redirect_uri", &ar.redirect_uri),
        hidden("scope", &ar.scope),
        hidden("state", &ar.state),
        hidden("nonce", &ar.nonce),
        hidden("code_challenge", &ar.challenge),
        hidden("code_challenge_method", &ar.challenge_method),
    ]
    .concat();

    let err = error
        .map(|e| format!("<p class=\"err\">{}</p>", html_escape(e)))
        .unwrap_or_default();

    // Dev affordance: one-click login as any configured user, no password typing.
    let hints = if srv.quick_login && !srv.users.is_empty() {
        let buttons: String = srv
            .users
            .list()
            .iter()
            .map(|u| {
                format!(
                    "<form method=\"post\" action=\"{a}\">{c}{q}<button name=\"username\" value=\"{n}\">{n}</button></form>",
                    a = html_escape(action),
                    c = carried,
                    q = hidden("quick", "1"),
                    n = html_escape(&u.username),
                )
            })
            .collect();
        format!(
            "<div class=\"hint\">Sign in as (dev shortcut, no password):<br>{}</div>",
            buttons
        )
    } else {
        String::new()
    };

    let body = format!(
        "{err}<form method=\"post\" action=\"{action}\">{carried}\
<label for=\"u\">Username</label><input id=\"u\" type=\"text\" name=\"username\" autofocus autocomplete=\"username\">\
<label for=\"p\">Password</label><input id=\"p\" type=\"password\" name=\"password\" autocomplete=\"current-password\">\
<button type=\"submit\">Sign in</button></form>{hints}",
        err = err,
        action = html_escape(action),
        carried = carried,
        hints = hints,
    );
    page(&format!("Sign in to {}", srv.realm), &body)
}

fn home<R: std::io::BufRead>(srv: &Server, req: &Request<R>) -> BuiltResponse {
    let iss = srv.issuer(&req.headers);
    let users: String = srv
        .users
        .list()
        .iter()
        .map(|u| format!("<li><code>{}</code></li>", html_escape(&u.username)))
        .collect();
    let clients: String = srv
        .clients
        .list()
        .iter()
        .map(|c| {
            format!(
                "<li><code>{}</code> — {}</li>",
                html_escape(&c.id),
                if c.is_public() {
                    "public"
                } else {
                    "confidential"
                }
            )
        })
        .collect();
    let body = format!(
        "<p>A tiny OIDC provider for development.</p>\
<p>Discovery: <a href=\"{i}/.well-known/openid-configuration\">{i}/.well-known/openid-configuration</a></p>\
<p><strong>Users</strong></p><ul>{u}</ul><p><strong>Clients</strong></p><ul>{c}</ul>",
        i = html_escape(&iss),
        u = users,
        c = clients
    );
    BuiltResponse::new(200).html(page("minicloak", &body))
}

// --- errors ---

fn oauth_error_body(err: &str, desc: &str) -> String {
    J::obj(vec![
        ("error", J::s(err)),
        ("error_description", J::s(desc)),
    ])
    .to_string()
}

fn oauth_error(status: u16, err: &str, desc: &str) -> BuiltResponse {
    BuiltResponse::new(status)
        .header("Cache-Control", "no-store")
        .json(oauth_error_body(err, desc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::Client;
    use crate::users::User;

    fn server() -> Server {
        let mut users = Users::new();
        users.add(User {
            username: "alice".into(),
            password: "pw".into(),
            email: "alice@example.com".into(),
            name: "Alice Admin".into(),
            roles: vec!["admin".into()],
        });
        let mut clients = Clients::new();
        clients.add(Client {
            id: "myapp".into(),
            secret: Some("s3cret".into()),
            redirect_uris: vec!["http://localhost:3000/cb".into()],
        });
        clients.add(Client {
            id: "spa".into(),
            secret: None,
            redirect_uris: vec!["http://localhost:5173/*".into()],
        });
        let key = RsaKey::generate(512);
        let kid = jwt::kid(&key);
        Server {
            realm: "dev".into(),
            issuer_override: Some("http://t/realms/dev".into()),
            key,
            kid,
            users,
            clients,
            store: Mutex::new(Store::new()),
            access_ttl: 300,
            refresh_ttl: 1800,
            code_ttl: 60,
            session_ttl: 3600,
            auto_login: None,
            quick_login: false,
            cors: true,
        }
    }

    #[test]
    fn routing_accepts_keycloak_paths_and_aliases() {
        let s = server();
        assert_eq!(
            s.route("/realms/dev/protocol/openid-connect/auth"),
            Some(Ep::Auth)
        );
        assert_eq!(s.route("/authorize"), Some(Ep::Auth));
        assert_eq!(
            s.route("/realms/dev/protocol/openid-connect/token"),
            Some(Ep::Token)
        );
        assert_eq!(s.route("/token"), Some(Ep::Token));
        assert_eq!(
            s.route("/realms/dev/protocol/openid-connect/certs"),
            Some(Ep::Jwks)
        );
        assert_eq!(s.route("/jwks.json"), Some(Ep::Jwks));
        assert_eq!(
            s.route("/realms/dev/protocol/openid-connect/token/introspect"),
            Some(Ep::Introspect)
        );
        assert_eq!(
            s.route("/realms/dev/.well-known/openid-configuration"),
            Some(Ep::Discovery)
        );
        assert_eq!(
            s.route("/.well-known/openid-configuration"),
            Some(Ep::Discovery)
        );
        assert_eq!(s.route("/realms/dev"), Some(Ep::Home));
        assert_eq!(s.route("/realms/other/protocol/openid-connect/auth"), None);
        assert_eq!(s.route("/nope"), None);
    }

    #[test]
    fn scope_filter_drops_unknown_and_dedupes() {
        assert_eq!(
            filter_scope("openid profile email", true),
            "openid profile email"
        );
        assert_eq!(filter_scope("openid bogus openid", true), "openid");
        assert_eq!(filter_scope("profile", true), "profile");
        // Empty only defaults to openid for the grants that have a user behind them.
        assert_eq!(filter_scope("", true), "openid");
        assert_eq!(filter_scope("", false), "");
        assert_eq!(filter_scope("bogus", false), "");
    }

    #[test]
    fn redirect_resolution_honours_wildcards_and_omission() {
        let exact = Client {
            id: "a".into(),
            secret: None,
            redirect_uris: vec!["http://localhost:3000/cb".into()],
        };
        assert_eq!(
            resolve_redirect(&exact, ""),
            Some("http://localhost:3000/cb".into())
        );
        assert_eq!(resolve_redirect(&exact, "http://evil/cb"), None);

        let wild = Client {
            id: "b".into(),
            secret: None,
            redirect_uris: vec!["http://localhost:5173/*".into()],
        };
        assert_eq!(
            resolve_redirect(&wild, "http://localhost:5173/deep/link"),
            Some("http://localhost:5173/deep/link".into())
        );
        // A wildcard client must still name its redirect_uri explicitly.
        assert_eq!(resolve_redirect(&wild, ""), None);
    }

    #[test]
    fn pkce_s256_and_plain() {
        let mut ac = AuthCode {
            client_id: "spa".into(),
            username: "alice".into(),
            redirect_uri: "http://localhost:5173/cb".into(),
            scope: "openid".into(),
            nonce: String::new(),
            code_challenge: base64::encode_url(&sha256::sha256(b"verifier-123")),
            code_challenge_method: "S256".into(),
            auth_time: 0,
            sid: "s".into(),
            expires_at: 0,
        };
        assert!(verify_pkce(&ac, "verifier-123").is_ok());
        assert!(verify_pkce(&ac, "wrong").is_err());
        assert!(verify_pkce(&ac, "").is_err());

        ac.code_challenge_method = "plain".into();
        ac.code_challenge = "verifier-123".into();
        assert!(verify_pkce(&ac, "verifier-123").is_ok());
        assert!(verify_pkce(&ac, "nope").is_err());

        // No challenge recorded: nothing to verify.
        ac.code_challenge = String::new();
        assert!(verify_pkce(&ac, "").is_ok());
    }

    #[test]
    fn basic_auth_parsing() {
        let h = format!("Basic {}", base64::encode(b"myapp:s3cret"));
        assert_eq!(parse_basic(&h), Some(("myapp".into(), "s3cret".into())));
        // Percent-encoded halves, per RFC 6749.
        let h2 = format!("Basic {}", base64::encode(b"my%20app:p%3Aw"));
        assert_eq!(parse_basic(&h2), Some(("my app".into(), "p:w".into())));
        assert_eq!(parse_basic("Bearer x"), None);
        assert_eq!(parse_basic("Basic !!!"), None);
    }

    #[test]
    fn cookie_extraction() {
        assert_eq!(
            cookie_value("foo=1; minicloak_session=abc; bar=2", COOKIE),
            Some("abc".into())
        );
        assert_eq!(
            cookie_value("minicloak_session=xyz", COOKIE),
            Some("xyz".into())
        );
        assert_eq!(cookie_value("other=1", COOKIE), None);
    }

    #[test]
    fn client_auth_rejects_bad_secret_and_allows_public() {
        let s = server();
        let post = |id: &str, secret: &str| {
            vec![
                ("client_id".to_string(), id.to_string()),
                ("client_secret".to_string(), secret.to_string()),
            ]
        };
        assert!(authenticate_client(&s, None, &post("myapp", "s3cret")).is_ok());
        assert!(authenticate_client(&s, None, &post("myapp", "wrong")).is_err());
        assert!(authenticate_client(&s, None, &post("ghost", "x")).is_err());
        // Public client authenticates with no secret at all.
        assert!(authenticate_client(&s, None, &post("spa", "")).is_ok());

        let basic = format!("Basic {}", base64::encode(b"myapp:s3cret"));
        assert!(authenticate_client(&s, Some(&basic), &[]).is_ok());
    }

    #[test]
    fn query_separator_respects_existing_query() {
        assert_eq!(sep("http://a/cb"), '?');
        assert_eq!(sep("http://a/cb?x=1"), '&');
    }

    #[test]
    fn profile_claims_follow_scope() {
        let u = User {
            username: "alice".into(),
            password: "pw".into(),
            email: "a@x.de".into(),
            name: "Alice Admin".into(),
            roles: vec!["admin".into()],
        };
        let keys = |scope: &str| {
            let mut c = Vec::new();
            add_profile_claims(&mut c, &u, scope);
            c.into_iter().map(|(k, _)| k).collect::<Vec<_>>()
        };
        assert!(keys("openid").contains(&"roles"));
        assert!(!keys("openid").contains(&"email"));
        assert!(keys("openid email").contains(&"email"));
        assert!(keys("openid profile").contains(&"given_name"));
        assert!(!keys("openid email").contains(&"name"));
    }
}
