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

/// One installed addon: where it lives, and its manifest.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Addon {
    #[serde(rename = "transportUrl", default)]
    pub transport_url: String,
    pub manifest: Manifest,
}

/// The parts of an addon manifest the bridge acts on. Unknown fields are ignored, so an addon
/// using a newer manifest feature still loads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Manifest {
    /// The addon's own id ("com.stremio.torrentio.addon"), what the bridge's settings name it by.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub catalogs: Vec<Catalog>,
    /// What the addon answers: `catalog`, `meta`, `stream`, `subtitles`, … Each entry is a bare
    /// name or an object narrowing that resource to some types and id prefixes.
    #[serde(default)]
    pub resources: Vec<serde_json::Value>,
    /// The content types the addon serves, when a resource does not narrow them itself.
    #[serde(default)]
    pub types: Vec<String>,
    /// The id prefixes the addon understands (`tt`, `kitsu:`), when a resource does not narrow
    /// them itself. Absent means every id.
    #[serde(rename = "idPrefixes", default)]
    pub id_prefixes: Option<Vec<String>>,
}

/// What an addon is for, as the app tells them apart. One addon can be several: Cinemeta is a
/// catalog and a metadata provider; Torrentio is a stream provider (and lists Real-Debrid
/// downloads as a catalog of its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// Lists of titles to browse.
    Catalog,
    /// A title's details: description, cast, seasons and episodes.
    Meta,
    /// Where a title can be played from.
    Stream,
    /// Subtitle files for a title.
    Subtitles,
}

impl Resource {
    fn wire(self) -> &'static str {
        match self {
            Resource::Catalog => "catalog",
            Resource::Meta => "meta",
            Resource::Stream => "stream",
            Resource::Subtitles => "subtitles",
        }
    }
}

impl Addon {
    /// The addon's base URL: its manifest URL without `/manifest.json`. Every resource lives under
    /// it as `/{resource}/{type}/{id}.json`.
    pub fn base_url(&self) -> &str {
        let url = self.transport_url.as_str();
        url.strip_suffix("/manifest.json").unwrap_or(url).trim_end_matches('/')
    }

    /// Does this addon answer `resource` for an item of `kind` whose id is `id`? The resource's
    /// own `types`/`idPrefixes` win over the manifest's, as Stremio itself reads them.
    pub fn provides(&self, resource: Resource, kind: &str, id: &str) -> bool {
        let m = &self.manifest;
        self.resource_entries(resource).any(|entry| {
            let types: Vec<&str> = entry
                .and_then(|e| e.get("types"))
                .and_then(|t| t.as_array())
                .map(|t| t.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_else(|| m.types.iter().map(String::as_str).collect());
            let prefixes: Option<Vec<&str>> = entry
                .and_then(|e| e.get("idPrefixes"))
                .and_then(|p| p.as_array())
                .map(|p| p.iter().filter_map(|v| v.as_str()).collect())
                .or_else(|| m.id_prefixes.as_ref().map(|p| p.iter().map(String::as_str).collect()));
            // A catalog is named by its own id, never a title id, so prefixes do not apply to it.
            types.contains(&kind)
                && (resource == Resource::Catalog
                    || prefixes.map_or(true, |p| p.is_empty() || p.iter().any(|x| id.starts_with(x))))
        })
    }

    /// Does this addon declare `resource` at all, for any type?
    pub fn has(&self, resource: Resource) -> bool {
        self.resource_entries(resource).next().is_some()
    }

    /// Each manifest entry naming `resource`: `None` for the bare-string form (which inherits the
    /// manifest's types and prefixes), `Some(object)` for the narrowed form.
    fn resource_entries(&self, resource: Resource)
        -> impl Iterator<Item = Option<&serde_json::Value>> + '_ {
        let name = resource.wire();
        self.manifest.resources.iter().filter_map(move |r| match r {
            serde_json::Value::String(s) if s == name => Some(None),
            serde_json::Value::Object(o) if o.get("name").and_then(|n| n.as_str()) == Some(name) => Some(Some(r)),
            _ => None,
        })
    }
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
    #[serde(default)]
    pub name: String,
    #[serde(rename = "isRequired", default)]
    pub is_required: bool,
    /// The values the addon offers for it (a `genre` extra's genres).
    #[serde(default)]
    pub options: Vec<String>,
}

impl Catalog {
    /// A catalog that cannot be listed without a parameter (a search-only catalog, one that needs
    /// a genre chosen first) is not a library: there is nothing to show until someone asks.
    pub fn browsable(&self) -> bool {
        self.extra_required.is_empty() && !self.extra.iter().any(|e| e.is_required)
    }

    /// Does this catalog take a search query (`extra` named `search`)?
    pub fn searchable(&self) -> bool {
        self.extra.iter().any(|e| e.name == "search")
            || self.extra_required.iter().any(|e| e == "search")
    }

    /// The name to show for this catalog: its own, else its id.
    pub fn title(&self) -> &str {
        self.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&self.id)
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
    /// The account's library items (`libraryItem` collection) as Stremio stores them: what is
    /// saved, and how far each was watched.
    fn library(&self, auth_key: &str) -> Result<Vec<serde_json::Value>, ApiError>;
    /// Writes changed library items back (watch progress, watched state).
    fn library_put(&self, auth_key: &str, items: &[serde_json::Value]) -> Result<(), ApiError>;
    /// A public resource: an addon's catalog, meta, stream or subtitles JSON, or an image.
    fn fetch(&self, url: &str) -> Result<Vec<u8>, ApiError>;
    /// An image: [`fetch`](Stremio::fetch), asking for the formats the app decodes (JPEG, PNG)
    /// rather than JSON — an image host that negotiates on `Accept` answered the JSON ask with
    /// something that was not a picture, and the card stayed blank.
    fn fetch_image(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        self.fetch(url)
    }
    /// `len` bytes of the resource at `url` starting at `from` (an HTTP range request, following
    /// redirects): the first bytes of a video file, to read its tracks.
    fn fetch_range(&self, url: &str, from: u64, len: u64) -> Result<Vec<u8>, ApiError>;
}

/// The network implementation.
pub struct Live;

impl Live {
    fn get(url: &str) -> Result<Vec<u8>, ApiError> {
        Self::get_as(url, "application/json")
    }

    fn get_as(url: &str, accept: &str) -> Result<Vec<u8>, ApiError> {
        let headers = [format!("Accept: {accept}")];
        // Addons and image hosts sit behind redirects often enough (a moved host, a CDN) that a
        // public read follows them; nothing here carries a credential.
        let resp = plx_net::net::request(url, &headers, "GET", None, plx_net::net::API, true,
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

    fn library(&self, auth_key: &str) -> Result<Vec<serde_json::Value>, ApiError> {
        let body = Self::post("datastoreGet", &serde_json::json!({
            "authKey": auth_key, "collection": "libraryItem", "ids": [], "all": true}))?;
        result_of::<Vec<serde_json::Value>>(&body)?.ok_or(ApiError::Unauthorized)
    }

    fn library_put(&self, auth_key: &str, items: &[serde_json::Value]) -> Result<(), ApiError> {
        let body = Self::post("datastorePut", &serde_json::json!({
            "authKey": auth_key, "collection": "libraryItem", "changes": items}))?;
        result_of::<serde_json::Value>(&body)?.map(|_| ()).ok_or(ApiError::Unauthorized)
    }

    fn fetch(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        Self::get(url)
    }

    fn fetch_image(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        Self::get_as(url, "image/jpeg,image/png;q=0.9,*/*;q=0.1")
    }

    fn fetch_range(&self, url: &str, from: u64, len: u64) -> Result<Vec<u8>, ApiError> {
        // The player's own media transport first: it follows a debrid service's redirect chain
        // hop by hop, waits out a slow first byte (an addon resolving the torrent on the request)
        // and reads a server that ignores the Range as a plain stream it simply stops reading —
        // the three ways the control-plane request below failed on real Comet links, which then
        // played with tracks guessed from the release name.
        if plx_media::curlio::available() {
            if let Some(bytes) = media_read(url, from, len) {
                return Ok(bytes);
            }
        }
        let headers = [format!("Range: bytes={from}-{}", from + len.max(1) - 1)];
        // A server that ignores the range answers the whole file; the cap turns that into a
        // failure rather than a download.
        let cap = usize::try_from(len).unwrap_or(usize::MAX).saturating_add(64 * 1024);
        let resp = plx_net::net::request(url, &headers, "GET", None, plx_net::net::API, true,
            Some(cap), None).ok_or(ApiError::Unavailable)?;
        if !resp.ok() {
            return Err(ApiError::Unavailable);
        }
        Ok(resp.body)
    }
}

/// Up to `len` bytes of `url` from byte `from`, through `plx_media::curlio` on a private transfer
/// (its abort handle is its own, so the player's teardown never reaches it and it never takes the
/// player's slot). `None` when nothing could be read in [`MEDIA_READ_BUDGET`].
fn media_read(url: &str, from: u64, len: u64) -> Option<Vec<u8>> {
    let len = usize::try_from(len).ok()?;
    let (mut src, _abort) = plx_media::curlio::CurlSource::open_private(url, i64::try_from(from).ok()?).ok()?;
    let deadline = std::time::Instant::now() + MEDIA_READ_BUDGET;
    let mut out = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        let n = src.read_until(&mut out[got..], Some(deadline), &mut plx_base::checkpoint::NoCheckpoint);
        if n <= 0 {
            break;
        }
        got += n as usize;
    }
    (got > 0).then(|| {
        out.truncate(got);
        out
    })
}

/// How long one track read may take, first byte included: a debrid addon may add the torrent to
/// the account on the request before it redirects. A cached link answers in a few seconds; one
/// that has not after this is passed over for the next (`bridge::play`), which is already being
/// read by then.
pub(super) const MEDIA_READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(12);
