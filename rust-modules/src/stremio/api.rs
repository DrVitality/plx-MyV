//! The Stremio services the bridge reads: the account API (`api.strem.io`) and the device-link
//! service (`link.stremio.com`) that turns a code shown on the television into an account key.
//!
//! The wire shapes are Stremio's own, as its core library defines them
//! (`stremio-core`, `src/types/api/request.rs` and `response.rs`): every account call is a
//! `POST /api/<method>` whose JSON body carries a `type` tag naming the method, and every answer
//! is an envelope holding either `result` or `error`. The link service is two `GET`s under
//! `/api/v2`: `create` mints a code and the URL a phone opens to approve it; `read` answers with
//! the account's `authKey` once someone has.
//!
//! [`Stremio`] is the seam the router is written against, so its rules are host-tested with a
//! scripted account ([`super::tests`]) and never touch the network. [`Live`] is the one
//! implementation that does, through `plx_net::net` (libcurl, the app's only HTTPS stack).
//!
//! The auth key is a credential. Nothing here logs it, a URL that carries it, or a response body.

use serde::Deserialize;

const ACCOUNT_API: &str = "https://api.strem.io/api";
const LINK_API: &str = "https://link.stremio.com/api/v2";
/// An addon collection embeds every installed addon's whole manifest, so it is the largest body
/// the bridge reads; a few hundred kilobytes is typical. The cap only stops a runaway answer.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Why a call produced nothing. The router turns these into the statuses the app already knows
/// how to read: a refused key is a `401` (sign in again), anything else a `502` (try again).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiError {
    /// The service answered and refused the key (an `error` envelope where a `result` belongs).
    Unauthorized,
    /// No answer, a non-2xx status, or a body that is not the documented shape.
    Unavailable,
}

/// A freshly minted device-link code: what to show, and the address a phone opens to approve it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LinkCode {
    pub code: String,
    pub link: String,
}

/// The signed-in account, as much of it as the app is shown.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(default)]
    pub email: String,
}

/// One installed addon. Only its manifest is read so far: the catalogs it declares are what the
/// libraries are made of. (Its `transportUrl`, where its catalogs and streams are fetched from,
/// joins this struct with the first route that fetches one.)
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Addon {
    pub manifest: Manifest,
}

/// The parts of an addon manifest the bridge acts on. Unknown fields are ignored, so an addon
/// using a newer manifest feature still loads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub catalogs: Vec<Catalog>,
}

/// One catalog an addon declares: a browsable list of one content type.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Catalog {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    /// The full form of the extra parameters this catalog takes.
    #[serde(default)]
    pub extra: Vec<ExtraProp>,
    /// The legacy short form: the names of the extra parameters this catalog requires.
    #[serde(rename = "extraRequired", default)]
    pub extra_required: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ExtraProp {
    #[serde(rename = "isRequired", default)]
    pub is_required: bool,
}

impl Catalog {
    /// A catalog that cannot be listed without a parameter (a search-only catalog, one that needs
    /// a genre chosen first) is not a library: there is nothing to show until someone asks.
    pub fn browsable(&self) -> bool {
        self.extra_required.is_empty() && !self.extra.iter().any(|e| e.is_required)
    }
}

/// Stremio's answer envelope: `{"result": …}` or `{"error": …}`. Only the result is read; an
/// error is told apart by its absence.
#[derive(Deserialize)]
struct Envelope<T> {
    result: Option<T>,
}

#[derive(Deserialize)]
struct LinkAuthKey {
    #[serde(rename = "authKey")]
    auth_key: String,
}

#[derive(Deserialize)]
struct AddonCollection {
    #[serde(default)]
    addons: Vec<Addon>,
}

/// What the bridge needs from Stremio. Implemented by [`Live`] and by the tests' scripted account.
pub trait Stremio: Send + Sync {
    /// Mints a new device-link code.
    fn create_link(&self) -> Result<LinkCode, ApiError>;
    /// The account key once `code` has been approved on a phone; `Ok(None)` while it has not.
    fn read_link(&self, code: &str) -> Result<Option<String>, ApiError>;
    /// The account behind `auth_key`.
    fn user(&self, auth_key: &str) -> Result<User, ApiError>;
    /// The addons installed on the account behind `auth_key`, in the account's own order.
    fn addons(&self, auth_key: &str) -> Result<Vec<Addon>, ApiError>;
}

/// The network implementation.
pub struct Live;

impl Live {
    fn get(url: &str) -> Result<Vec<u8>, ApiError> {
        let headers = ["Accept: application/json".to_string()];
        let resp = plx_net::net::request(url, &headers, "GET", None, plx_net::net::API, false,
            Some(MAX_BODY), None).ok_or(ApiError::Unavailable)?;
        if !resp.ok() {
            return Err(ApiError::Unavailable);
        }
        Ok(resp.body)
    }

    fn post(method: &str, body: &serde_json::Value) -> Result<Vec<u8>, ApiError> {
        let url = format!("{ACCOUNT_API}/{method}");
        let headers = [
            "Accept: application/json".to_string(),
            "Content-Type: application/json".to_string(),
        ];
        let body = serde_json::to_vec(body).map_err(|_| ApiError::Unavailable)?;
        let resp = plx_net::net::request(&url, &headers, "POST", Some(&body), plx_net::net::API,
            false, Some(MAX_BODY), None).ok_or(ApiError::Unavailable)?;
        if !resp.ok() {
            return Err(ApiError::Unavailable);
        }
        Ok(resp.body)
    }
}

/// Reads an envelope. A body that does not parse is `Unavailable`; a parsed envelope with no
/// `result` is the service refusing, which for a keyed call means the key.
fn result_of<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<Option<T>, ApiError> {
    let envelope: Envelope<T> = serde_json::from_slice(body).map_err(|_| ApiError::Unavailable)?;
    Ok(envelope.result)
}

impl Stremio for Live {
    fn create_link(&self) -> Result<LinkCode, ApiError> {
        let body = Self::get(&format!("{LINK_API}/create?type=Create"))?;
        result_of::<LinkCode>(&body)?.ok_or(ApiError::Unavailable)
    }

    fn read_link(&self, code: &str) -> Result<Option<String>, ApiError> {
        let code: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let body = Self::get(&format!("{LINK_API}/read?type=Read&code={code}"))?;
        // Until the code is approved the service answers with an error envelope: pending, not
        // a failure.
        Ok(result_of::<LinkAuthKey>(&body).ok().flatten().map(|r| r.auth_key))
    }

    fn user(&self, auth_key: &str) -> Result<User, ApiError> {
        let body = Self::post("getUser", &serde_json::json!({"type": "GetUser", "authKey": auth_key}))?;
        result_of::<User>(&body)?.ok_or(ApiError::Unauthorized)
    }

    fn addons(&self, auth_key: &str) -> Result<Vec<Addon>, ApiError> {
        let body = Self::post("addonCollectionGet",
            &serde_json::json!({"type": "AddonCollectionGet", "authKey": auth_key, "update": true}))?;
        Ok(result_of::<AddonCollection>(&body)?.ok_or(ApiError::Unauthorized)?.addons)
    }
}
