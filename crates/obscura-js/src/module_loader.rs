use std::cell::RefCell;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use base64::engine::general_purpose::STANDARD_NO_PAD as BASE64_NO_PAD;

use deno_core::error::ModuleLoaderError;
use deno_core::ModuleLoadOptions;
use deno_core::ModuleLoadReferrer;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;

use deno_error::JsErrorBox;

use crate::import_map::ImportMap;
use crate::ops::{ObscuraState, JsNetworkEvent, StoredNetworkResponseBody, record_network_completion};

/// Observable network activity for ES-module graphs.
///
/// deno_core keeps dynamic-import state inside its private module map. The
/// browser lifecycle still needs to distinguish a genuinely idle page from a
/// graph whose fetch future is being advanced in short event-loop slices. A
/// loader-owned counter provides that signal without treating unrelated
/// fetch/XHR analytics as render-blocking work.
#[derive(Debug, Default)]
pub(crate) struct ModuleLoadActivity {
    pending: std::sync::atomic::AtomicUsize,
    last_activity: std::sync::Mutex<Option<std::time::Instant>>,
}

impl ModuleLoadActivity {
    fn begin(self: &Arc<Self>) -> ModuleLoadGuard {
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::time::Instant::now());
        ModuleLoadGuard(self.clone())
    }

    pub(crate) fn is_pending_or_recent(&self, grace: std::time::Duration) -> bool {
        if self.pending.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            return true;
        }
        self.last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|last| last.elapsed() <= grace)
    }
}

struct ModuleLoadGuard(Arc<ModuleLoadActivity>);

impl Drop for ModuleLoadGuard {
    fn drop(&mut self) {
        let previous = self
            .0
            .pending
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        debug_assert!(previous > 0, "module load activity counter underflow");
        *self
            .0
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::time::Instant::now());
    }
}

pub struct ObscuraModuleLoader {
    pub base_url: String,
    /// Proxy URL threaded through to every dynamic ES-module fetch (#139).
    /// `None` keeps the pre-#139 direct-connection behaviour for callers
    /// that haven't been updated.
    pub proxy_url: Option<String>,
    /// The owning page's network context. Production runtimes always install
    /// this so every module in a graph uses the same cookie jar, configured
    /// identity, redirect/security policy, interception, and callbacks as the
    /// entry module. Directly-constructed standalone loaders remain supported.
    page_state: Option<Weak<RefCell<ObscuraState>>>,
    /// Directly-constructed loaders still use Obscura's network policy and
    /// connection pool; they simply have an isolated cookie jar.
    standalone_client: Option<Arc<obscura_net::ObscuraHttpClient>>,
    import_map: Rc<RefCell<ImportMap>>,
    activity: Arc<ModuleLoadActivity>,
    /// Canonical and requested specifiers fetched into deno_core's module map.
    /// The runtime uses a cursor into this append-only list to associate a
    /// prepared root with the dependencies that its successful evaluation also
    /// evaluates.
    loaded_specifiers: Rc<RefCell<Vec<String>>>,
}

impl ObscuraModuleLoader {
    pub fn new(base_url: &str) -> Self {
        Self::with_proxy(base_url, None)
    }

    pub fn with_proxy(base_url: &str, proxy_url: Option<String>) -> Self {
        let import_map = Rc::new(RefCell::new(ImportMap::default()));
        Self::with_proxy_and_import_map(base_url, proxy_url, import_map)
    }

    fn with_proxy_and_import_map(
        base_url: &str,
        proxy_url: Option<String>,
        import_map: Rc<RefCell<ImportMap>>,
    ) -> Self {
        let standalone_client = Arc::new(obscura_net::ObscuraHttpClient::with_options(
            Arc::new(obscura_net::CookieJar::new()),
            proxy_url.as_deref(),
        ));
        ObscuraModuleLoader {
            base_url: base_url.to_string(),
            proxy_url,
            page_state: None,
            standalone_client: Some(standalone_client),
            import_map,
            activity: Arc::new(ModuleLoadActivity::default()),
            loaded_specifiers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(crate) fn with_page_state(
        base_url: &str,
        proxy_url: Option<String>,
        page_state: &Rc<RefCell<ObscuraState>>,
        import_map: Rc<RefCell<ImportMap>>,
    ) -> Self {
        ObscuraModuleLoader {
            base_url: base_url.to_string(),
            proxy_url,
            page_state: Some(Rc::downgrade(page_state)),
            standalone_client: None,
            import_map,
            activity: Arc::new(ModuleLoadActivity::default()),
            loaded_specifiers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(crate) fn activity(&self) -> Arc<ModuleLoadActivity> {
        self.activity.clone()
    }

    pub(crate) fn loaded_specifiers(&self) -> Rc<RefCell<Vec<String>>> {
        self.loaded_specifiers.clone()
    }
}

fn io_err(msg: String) -> ModuleLoaderError {
    JsErrorBox::from_err(std::io::Error::new(std::io::ErrorKind::Other, msg))
}

/// Decodes a `data:` URL's own payload as module source text.
///
/// A data URL is fetchable per the URL and HTML specs, and a module graph can
/// name one directly (`import x from "data:text/javascript,..."`) or through
/// `<script type="module" src="data:...">`. There is no transport behind it:
/// the payload *is* the resource. The fragment is not part of that resource,
/// `;base64` selects base64 decoding (padding optional, ASCII whitespace
/// ignored) and anything else is percent-decoded. The declared media type is
/// otherwise ignored, since a module is JavaScript text either way.
fn decode_data_module_url(url: &str) -> Result<String, ModuleLoaderError> {
    let payload = url
        .strip_prefix("data:")
        .ok_or_else(|| io_err(format!("Not a data URL: {}", url)))?;
    let (meta, data) = payload
        .split_once(',')
        .ok_or_else(|| io_err(format!("Invalid data URL, no comma: {}", url)))?;
    let data = data.split('#').next().unwrap_or(data);
    let is_base64 = meta
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"));
    if !is_base64 {
        return percent_decode_data_payload(data);
    }
    let compact: String = data
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = BASE64
        .decode(&compact)
        .or_else(|_| BASE64_NO_PAD.decode(&compact))
        .map_err(|e| io_err(format!("Invalid base64 in data URL: {}", e)))?;
    String::from_utf8(bytes)
        .map_err(|e| io_err(format!("Data URL payload is not UTF-8: {}", e)))
}

/// Percent-decodes a non-base64 data URL payload. `+` is literal here: form
/// encoding's space substitution does not apply to data URLs.
fn percent_decode_data_payload(data: &str) -> Result<String, ModuleLoaderError> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = data.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex),
                bytes.get(index + 2).copied().and_then(hex),
            ) else {
                return Err(io_err(format!(
                    "Invalid percent escape in data URL payload at byte {}",
                    index
                )));
            };
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|e| io_err(format!("Data URL payload is not UTF-8: {}", e)))
}

impl ModuleLoader for ObscuraModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: deno_core::ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        // deno_core represents the root passed to load_side_es_module with a
        // synthetic "." referrer. A browser resolves <script type=module src>
        // as a resource URL before it starts a graph; the document import map
        // must not remap that root URL.
        if referrer == "." {
            return deno_core::resolve_import(specifier, &self.base_url)
                .map_err(|error| io_err(error.to_string()));
        }

        let base = if referrer.is_empty()
            || referrer.starts_with('<')
            || referrer == "about:blank"
        {
            &self.base_url
        } else {
            referrer
        };

        let base = ModuleSpecifier::parse(base)
            .map_err(|e| io_err(format!("Invalid module referrer {}: {}", base, e)))?;
        self.import_map
            .try_borrow_mut()
            .map_err(|_| io_err("Import map is already borrowed".to_string()))?
            .resolve(specifier, &base)
            .map_err(io_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let url = module_specifier.to_string();
        // Module-graph CORS and same-origin credentials are relative to the
        // owning document, not to the importing module. The importer remains
        // the HTTP referrer for a dependency; keeping these URLs distinct
        // prevents a cross-origin parent module from gaining CDN cookies when
        // it imports a sibling on that CDN.
        let document_url = ModuleSpecifier::parse(&self.base_url)
            .unwrap_or_else(|_| module_specifier.clone());
        let referrer = maybe_referrer
            .map(|referrer| referrer.specifier.clone())
            .unwrap_or_else(|| document_url.clone());
        // Capture the loader's proxy here so the async closure below owns a
        // plain Option<String> rather than borrowing &self across an `await`.
        let proxy_url = self.proxy_url.clone();
        let activity = self.activity.clone();
        let loaded_specifiers = self.loaded_specifiers.clone();
        loaded_specifiers.borrow_mut().push(url.clone());
        // A data URL carries its module source itself, so it never reaches the
        // network: sending it to the HTTP client fails with "Failed to fetch
        // module data:...", which silently drops the module and everything
        // importing it. Decode the payload here instead. Nothing is recorded
        // as a network request, and none of the transport wiring below (proxy,
        // stealth identity, cookie jar, activity counter) applies.
        if module_specifier.scheme() == "data" {
            return ModuleLoadResponse::Sync(decode_data_module_url(&url).map(|code| {
                ModuleSource::new_with_redirect(
                    deno_core::ModuleType::JavaScript,
                    ModuleSourceCode::String(code.into()),
                    module_specifier,
                    module_specifier,
                    None,
                )
            }));
        }
        // Register before returning the future. The lifecycle can inspect the
        // runtime between deno_core accepting the load and first polling it.
        // Keeping the guard inside the future makes cancellation/navigation
        // decrement the count through Drop as well as success and failure.
        let is_dyn_import = options.is_dynamic_import;
        let activity_guard = is_dyn_import.then(|| activity.begin());
        let page_state = self.page_state.clone();
        let page_network = match self.page_state.as_ref() {
            Some(weak) => (|| {
                let state = weak
                    .upgrade()
                    .ok_or_else(|| "Module loader page state was dropped".to_string())?;
                let state = state
                    .try_borrow()
                    .map_err(|_| "Module loader page state is already borrowed".to_string())?;
                let client = state
                    .http_client
                    .clone()
                    .ok_or_else(|| "No http_client wired to module loader".to_string())?;
                // Fork: in stealth mode an ES module must be fetched over the
                // same transport as the document. Upstream sends it through the
                // plain reqwest client, so a `type="module"` script arrives with
                // a different TLS fingerprint and none of the browser identity
                // headers, while the HTML that referenced it came over wreq.
                // That cross-transport mismatch is trivially detectable.
                #[cfg(feature = "stealth")]
                let stealth = state.stealth_client.clone();
                #[cfg(not(feature = "stealth"))]
                let stealth: Option<std::sync::Arc<()>> = None;
                Ok((client, stealth, state.callbacks.clone()))
            })(),
            None => self
                .standalone_client
                .clone()
                .map(|client| {
                    #[cfg(feature = "stealth")]
                    let stealth = None;
                    #[cfg(not(feature = "stealth"))]
                    let stealth: Option<std::sync::Arc<()>> = None;
                    (client, stealth, None)
                })
                .ok_or_else(|| "No network context wired to module loader".to_string()),
        };

        ModuleLoadResponse::Async(Pin::from(Box::new(async move {
            // deno_core propagates `is_dynamic_import` to every dependency edge in
            // the recursive graph, so this excludes parser-discovered/static
            // graphs without losing descendant fetches of a lazy graph.
            let _activity_guard = activity_guard;
            tracing::debug!(
                "Loading ES module: {} (proxy: {})",
                url,
                proxy_url.as_deref().unwrap_or("direct")
            );

            match page_network {
                Ok((client, stealth, callbacks)) => {
                    let requested = ModuleSpecifier::parse(&url)
                        .map_err(|e| io_err(format!("Invalid module URL {}: {}", url, e)))?;
                    let request =
                        obscura_net::ResourceRequest::module_script(&document_url, &referrer);
                    #[cfg(feature = "stealth")]
                    let resp = match stealth {
                        Some(stealth) => stealth
                            .fetch_resource_with_callbacks(
                                &requested,
                                request,
                                callbacks.as_deref(),
                            )
                            .await,
                        None => {
                            client
                                .fetch_resource_with_callbacks(
                                    &requested,
                                    request,
                                    callbacks.as_deref(),
                                )
                                .await
                        }
                    }
                    .map_err(|e| io_err(format!("Failed to fetch module {}: {}", url, e)))?;
                    #[cfg(not(feature = "stealth"))]
                    let resp = {
                        let _ = &stealth;
                        client
                            .fetch_resource_with_callbacks(
                                &requested,
                                request,
                                callbacks.as_deref(),
                            )
                            .await
                            .map_err(|e| io_err(format!("Failed to fetch module {}: {}", url, e)))?
                    };
                    // A module response belongs to the owning page regardless
                    // of whether graph preparation or evaluation later fails.
                    // Status 0 is the transport's blocked-tracker sentinel.
                    if resp.status != 0 {
                        if let Some(state) = page_state.as_ref().and_then(Weak::upgrade) {
                            let mut state = state.try_borrow_mut().map_err(|_| {
                                io_err("Module loader page state is already borrowed".to_string())
                            })?;
                            static NEXT_MODULE_REQUEST_ID: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(1);
                            let request_id = format!("module-{}", NEXT_MODULE_REQUEST_ID.fetch_add(
                                1, std::sync::atomic::Ordering::Relaxed,
                            ));
                            record_network_completion(&mut state, JsNetworkEvent {
                                request_id, intercepted: false,
                                url: resp.url.to_string(), method: "GET".to_string(),
                                resource_type: "Script".to_string(), status: resp.status,
                                response_headers: resp.headers.clone(), body_size: resp.body.len(),
                                timestamp: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default().as_secs_f64(),
                                error_text: None,
                            }, || match std::str::from_utf8(&resp.body) {
                                Ok(body) => StoredNetworkResponseBody {
                                    body: body.to_string(), base64_encoded: false,
                                },
                                Err(_) => StoredNetworkResponseBody {
                                    body: BASE64.encode(&resp.body), base64_encoded: true,
                                },
                            });
                        }
                    }
                    if !(200..=299).contains(&resp.status) {
                        return Err(io_err(format!(
                            "Module {} returned HTTP {}",
                            url, resp.status
                        )));
                    }
                    let found = ModuleSpecifier::parse(resp.url.as_str()).map_err(|e| {
                        io_err(format!("Invalid final module URL {}: {}", resp.url, e))
                    })?;
                    if found.as_str() != requested.as_str() {
                        loaded_specifiers
                            .borrow_mut()
                            .push(found.to_string());
                    }
                    let code = obscura_net::decode_non_html(&resp.body, resp.content_type());
                    Ok(ModuleSource::new_with_redirect(
                        deno_core::ModuleType::JavaScript,
                        ModuleSourceCode::String(code.into()),
                        &requested,
                        &found,
                        None,
                    ))
                }
                Err(error) => Err(io_err(error)),
            }
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::decode_data_module_url;

    #[test]
    fn data_module_payload_is_the_source() {
        assert_eq!(
            decode_data_module_url("data:text/javascript,export default 42").unwrap(),
            "export default 42"
        );
    }

    #[test]
    fn data_module_percent_decodes() {
        assert_eq!(
            decode_data_module_url("data:text/javascript,window.__a%20%3D%201").unwrap(),
            "window.__a = 1"
        );
    }

    #[test]
    fn data_module_plus_stays_literal() {
        assert_eq!(
            decode_data_module_url("data:text/javascript,let%20a%3D1+2").unwrap(),
            "let a=1+2"
        );
    }

    #[test]
    fn data_module_fragment_is_not_part_of_the_payload() {
        assert_eq!(
            decode_data_module_url("data:text/javascript,let%20a%3D1#fragment").unwrap(),
            "let a=1"
        );
    }

    #[test]
    fn data_module_base64_with_padding() {
        assert_eq!(
            decode_data_module_url("data:text/javascript;base64,ZXhwb3J0IGRlZmF1bHQgNw==").unwrap(),
            "export default 7"
        );
    }

    #[test]
    fn data_module_base64_padding_optional_and_whitespace_ignored() {
        assert_eq!(
            decode_data_module_url(
                "data:text/javascript;base64,ZXhwb3J0IGRlZmF1bHQgNw"
            )
            .unwrap(),
            "export default 7"
        );
        assert_eq!(
            decode_data_module_url(
                "data:text/javascript;base64;charset=utf-8,ZXhwb3J0IGRl\nZmF1bHQgNw"
            )
            .unwrap(),
            "export default 7"
        );
    }

    #[test]
    fn data_module_rejects_malformed_urls() {
        for url in [
            "data:text/javascript;base64,not base64!",
            "data:text/javascript,truncated%2",
            "data:text/javascript",
            "https://example.com/mod.js",
        ] {
            assert!(decode_data_module_url(url).is_err(), "should reject {url}");
        }
    }
}
