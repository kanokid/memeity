//! Google sign-in (desktop loopback + PKCE) and the few Drive v3 calls memeity needs.
//!
//! The refresh token lives in the OS keychain (Windows Credential Manager,
//! macOS Keychain, Secret Service on Linux), never in a file.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DRIVE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3/files";
/// Only files memeity itself created (or the user opened with it). Non-sensitive, so
/// the shared memeity app can be published without Google's security review. The
/// trade-off: memes must be added through memeity (any device), not Drive's website.
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
/// The Drive folder memeity creates in My Drive (and finds again on other devices).
pub const LIBRARY_FOLDER: &str = "memeity";
const ITEM_FIELDS: &str = "id,name,mimeType,md5Checksum,modifiedTime";
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

const KEYRING_SERVICE: &str = "memeity";
const KEYRING_USER: &str = "google-refresh-token";
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

pub const NOT_SIGNED_IN: &str = "Not signed in to Google.";

#[derive(Clone, Deserialize)]
pub struct ClientCreds {
    pub client_id: String,
    pub client_secret: String,
}

/// memeity's own client, baked in at build time (see build.rs / oauth/HOW-TO.md).
pub fn embedded_client() -> Option<ClientCreds> {
    let client_id = option_env!("MEMEITY_GOOGLE_CLIENT_ID")?;
    let client_secret = option_env!("MEMEITY_GOOGLE_CLIENT_SECRET")?;
    Some(ClientCreds { client_id: client_id.into(), client_secret: client_secret.into() })
}

/// Parses the JSON that Google Cloud Console downloads for a "Desktop app" OAuth client.
pub fn parse_client_json(bytes: &[u8]) -> Result<ClientCreds, String> {
    #[derive(Deserialize)]
    struct ClientFile {
        installed: Option<ClientCreds>,
    }
    let file: ClientFile =
        serde_json::from_slice(bytes).map_err(|e| format!("That isn't a Google OAuth client file: {e}"))?;
    file.installed.ok_or_else(|| {
        "That client isn't a \"Desktop app\" client. Create one of type Desktop app and download its JSON.".into()
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub mime_type: String,
    pub md5_checksum: Option<String>,
    pub modified_time: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

pub struct Google {
    http: reqwest::Client,
    creds: Mutex<Option<ClientCreds>>,
    refresh_token: Mutex<Option<String>>,
    access: tokio::sync::Mutex<Option<(String, Instant)>>,
}

fn keyring_entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
}

fn random_string(len: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

fn net(e: reqwest::Error) -> String {
    format!("Couldn't reach Google: {e}")
}

/// Turns a non-2xx response into a readable error, preferring Google's own message.
async fn check(resp: reqwest::Response) -> Result<reqwest::Response, String> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/error_description"))
                .and_then(|m| m.as_str().map(str::to_owned))
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    Err(format!("Google Drive error ({status}): {message}"))
}

impl Google {
    pub fn new(creds: Option<ClientCreds>) -> Self {
        let refresh = keyring_entry().ok().and_then(|e| e.get_password().ok());
        Google {
            http: reqwest::Client::new(),
            creds: Mutex::new(creds),
            refresh_token: Mutex::new(refresh),
            access: tokio::sync::Mutex::new(None),
        }
    }

    pub fn has_creds(&self) -> bool {
        self.creds.lock().unwrap().is_some()
    }

    pub fn set_creds(&self, creds: ClientCreds) {
        *self.creds.lock().unwrap() = Some(creds);
    }

    pub fn signed_in(&self) -> bool {
        self.refresh_token.lock().unwrap().is_some()
    }

    pub fn sign_out(&self) {
        *self.refresh_token.lock().unwrap() = None;
        if let Ok(mut access) = self.access.try_lock() {
            *access = None;
        }
        if let Ok(entry) = keyring_entry() {
            let _ = entry.delete_credential();
        }
    }

    fn creds(&self) -> Result<ClientCreds, String> {
        self.creds.lock().unwrap().clone().ok_or_else(|| "No Google OAuth client loaded yet.".into())
    }

    /// Runs the browser sign-in. `open_url` should open the URL in the user's browser.
    pub async fn sign_in(&self, open_url: impl FnOnce(&str) + Send) -> Result<(), String> {
        let creds = self.creds()?;
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let redirect = format!("http://127.0.0.1:{port}");

        let verifier = random_string(64);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_string(24);
        let url = reqwest::Url::parse_with_params(
            AUTH_URL,
            &[
                ("client_id", creds.client_id.as_str()),
                ("redirect_uri", &redirect),
                ("response_type", "code"),
                ("scope", SCOPE),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("state", &state),
                ("access_type", "offline"),
                ("prompt", "consent"),
            ],
        )
        .expect("static auth URL is valid");
        open_url(url.as_str());

        let code = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_for_code(&listener, &state))
            .await
            .map_err(|_| "Timed out waiting for Google sign-in.".to_string())??;

        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("code", code.as_str()),
                ("client_id", &creds.client_id),
                ("client_secret", &creds.client_secret),
                ("redirect_uri", &redirect),
                ("grant_type", "authorization_code"),
                ("code_verifier", &verifier),
            ])
            .send()
            .await
            .map_err(net)?;
        let token: TokenResponse = check(resp).await?.json().await.map_err(net)?;
        let refresh = token
            .refresh_token
            .ok_or("Google didn't return a refresh token. Remove memeity's access at myaccount.google.com/permissions and try again.")?;

        keyring_entry()
            .and_then(|e| e.set_password(&refresh))
            .map_err(|e| format!("Couldn't save sign-in to the system keychain: {e}"))?;
        *self.refresh_token.lock().unwrap() = Some(refresh);
        *self.access.lock().await = Some((token.access_token, expiry(token.expires_in)));
        Ok(())
    }

    /// A valid access token, refreshing it if needed.
    async fn token(&self) -> Result<String, String> {
        let mut access = self.access.lock().await;
        if let Some((token, expires)) = access.as_ref()
            && Instant::now() < *expires {
                return Ok(token.clone());
            }
        let creds = self.creds()?;
        let refresh = self.refresh_token.lock().unwrap().clone().ok_or(NOT_SIGNED_IN)?;
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", creds.client_id.as_str()),
                ("client_secret", &creds.client_secret),
                ("refresh_token", &refresh),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(net)?;
        if resp.status() == reqwest::StatusCode::BAD_REQUEST || resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // invalid_grant: revoked, or expired (test-mode apps expire tokens after 7 days).
            drop(access);
            self.sign_out();
            return Err("Your Google sign-in expired. Sign in again.".into());
        }
        let token: TokenResponse = check(resp).await?.json().await.map_err(net)?;
        *access = Some((token.access_token.clone(), expiry(token.expires_in)));
        Ok(token.access_token)
    }

    // ---------- Drive ----------

    pub async fn account_email(&self) -> Result<String, String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct About {
            user: User,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct User {
            email_address: String,
        }
        let resp = self
            .http
            .get(format!("{DRIVE}/about"))
            .query(&[("fields", "user(emailAddress)")])
            .bearer_auth(self.token().await?)
            .send()
            .await
            .map_err(net)?;
        let about: About = check(resp).await?.json().await.map_err(net)?;
        Ok(about.user.email_address)
    }

    async fn list(&self, q: &str, order_by: Option<&str>) -> Result<Vec<DriveItem>, String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            #[serde(default)]
            files: Vec<DriveItem>,
            next_page_token: Option<String>,
        }
        let fields = format!("nextPageToken,files({ITEM_FIELDS})");
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut req = self
                .http
                .get(format!("{DRIVE}/files"))
                .bearer_auth(self.token().await?)
                .query(&[("q", q), ("fields", &fields), ("pageSize", "1000"), ("spaces", "drive")]);
            if let Some(order) = order_by {
                req = req.query(&[("orderBy", order)]);
            }
            if let Some(token) = &page_token {
                req = req.query(&[("pageToken", token.as_str())]);
            }
            let page: Page = check(req.send().await.map_err(net)?).await?.json().await.map_err(net)?;
            out.extend(page.files);
            match page.next_page_token {
                Some(t) => page_token = Some(t),
                None => return Ok(out),
            }
        }
    }

    /// The "memeity" folder at the top of My Drive: the oldest one memeity can see (one
    /// made earlier on any device), or a new one. With the drive.file scope, folders the
    /// user made by hand are invisible to us, so this only ever finds memeity's own.
    pub async fn find_or_create_library(&self) -> Result<DriveItem, String> {
        let q = format!(
            "name = '{LIBRARY_FOLDER}' and mimeType = '{FOLDER_MIME}' and 'root' in parents and trashed = false"
        );
        if let Some(existing) = self.list(&q, Some("createdTime")).await?.into_iter().next() {
            return Ok(existing);
        }
        let id = self.create_folder("root", LIBRARY_FOLDER).await?;
        Ok(DriveItem {
            id,
            name: LIBRARY_FOLDER.into(),
            mime_type: FOLDER_MIME.into(),
            md5_checksum: None,
            modified_time: None,
        })
    }

    /// Everything directly inside `parent` that isn't trashed.
    pub async fn list_children(&self, parent: &str) -> Result<Vec<DriveItem>, String> {
        self.list(&format!("'{parent}' in parents and trashed=false"), None).await
    }

    pub async fn download(&self, id: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .http
            .get(format!("{DRIVE}/files/{id}"))
            .query(&[("alt", "media")])
            .bearer_auth(self.token().await?)
            .send()
            .await
            .map_err(net)?;
        Ok(check(resp).await?.bytes().await.map_err(net)?.to_vec())
    }

    pub async fn upload(&self, parent: &str, name: &str, mime: &str, bytes: Vec<u8>) -> Result<DriveItem, String> {
        let boundary = format!("memeity-{}", random_string(16));
        let meta = serde_json::json!({ "name": name, "parents": [parent] }).to_string();
        let mut body = Vec::with_capacity(bytes.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n\
                 --{boundary}\r\nContent-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let resp = self
            .http
            .post(UPLOAD)
            .query(&[("uploadType", "multipart"), ("fields", ITEM_FIELDS)])
            .bearer_auth(self.token().await?)
            .header(reqwest::header::CONTENT_TYPE, format!("multipart/related; boundary={boundary}"))
            .body(body)
            .send()
            .await
            .map_err(net)?;
        check(resp).await?.json().await.map_err(net)
    }

    pub async fn create_folder(&self, parent: &str, name: &str) -> Result<String, String> {
        let resp = self
            .http
            .post(format!("{DRIVE}/files"))
            .query(&[("fields", "id")])
            .bearer_auth(self.token().await?)
            .json(&serde_json::json!({ "name": name, "mimeType": FOLDER_MIME, "parents": [parent] }))
            .send()
            .await
            .map_err(net)?;
        #[derive(Deserialize)]
        struct Created {
            id: String,
        }
        let created: Created = check(resp).await?.json().await.map_err(net)?;
        Ok(created.id)
    }

    /// Renames a file, optionally moving it: `reparent` = (new parent, old parent).
    pub async fn rename(&self, id: &str, new_name: &str, reparent: Option<(&str, &str)>) -> Result<DriveItem, String> {
        let mut req = self
            .http
            .patch(format!("{DRIVE}/files/{id}"))
            .query(&[("fields", ITEM_FIELDS)])
            .bearer_auth(self.token().await?)
            .json(&serde_json::json!({ "name": new_name }));
        if let Some((add, remove)) = reparent {
            req = req.query(&[("addParents", add), ("removeParents", remove)]);
        }
        check(req.send().await.map_err(net)?).await?.json().await.map_err(net)
    }

    /// A bookmark for "now" in this account's Drive change feed.
    pub async fn start_page_token(&self) -> Result<String, String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Token {
            start_page_token: String,
        }
        let resp = self
            .http
            .get(format!("{DRIVE}/changes/startPageToken"))
            .bearer_auth(self.token().await?)
            .send()
            .await
            .map_err(net)?;
        let token: Token = check(resp).await?.json().await.map_err(net)?;
        Ok(token.start_page_token)
    }

    /// Has anything memeity can see changed since `token`? Returns `None` if so (the
    /// caller then does a full sync and takes a fresh bookmark), or `Some(new_token)`
    /// when nothing changed. One tiny request in the common case.
    pub async fn changes_since(&self, token: &str) -> Result<Option<String>, String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            #[serde(default)]
            changes: Vec<serde_json::Value>,
            next_page_token: Option<String>,
            new_start_page_token: Option<String>,
        }
        let mut token = token.to_string();
        loop {
            let resp = self
                .http
                .get(format!("{DRIVE}/changes"))
                .query(&[
                    ("pageToken", token.as_str()),
                    ("pageSize", "100"),
                    ("spaces", "drive"),
                    ("fields", "nextPageToken,newStartPageToken,changes(fileId)"),
                ])
                .bearer_auth(self.token().await?)
                .send()
                .await
                .map_err(net)?;
            let page: Page = check(resp).await?.json().await.map_err(net)?;
            if !page.changes.is_empty() {
                return Ok(None);
            }
            match (page.next_page_token, page.new_start_page_token) {
                (Some(next), _) => token = next,
                (None, Some(fresh)) => return Ok(Some(fresh)),
                (None, None) => return Ok(None), // unexpected: play safe, do a full sync
            }
        }
    }

    /// Replaces an existing file's contents (same id, name and folder).
    pub async fn update_content(&self, id: &str, mime: &str, bytes: Vec<u8>) -> Result<DriveItem, String> {
        let resp = self
            .http
            .patch(format!("{UPLOAD}/{id}"))
            .query(&[("uploadType", "media"), ("fields", ITEM_FIELDS)])
            .bearer_auth(self.token().await?)
            .header(reqwest::header::CONTENT_TYPE, mime)
            .body(bytes)
            .send()
            .await
            .map_err(net)?;
        check(resp).await?.json().await.map_err(net)
    }

    /// Takes a file back out of Drive's trash (for Undo).
    pub async fn untrash(&self, id: &str) -> Result<(), String> {
        let resp = self
            .http
            .patch(format!("{DRIVE}/files/{id}"))
            .bearer_auth(self.token().await?)
            .json(&serde_json::json!({ "trashed": false }))
            .send()
            .await
            .map_err(net)?;
        check(resp).await.map(|_| ())
    }

    /// Moves a file to Drive's trash (recoverable for 30 days), not a permanent delete.
    pub async fn trash(&self, id: &str) -> Result<(), String> {
        let resp = self
            .http
            .patch(format!("{DRIVE}/files/{id}"))
            .bearer_auth(self.token().await?)
            .json(&serde_json::json!({ "trashed": true }))
            .send()
            .await
            .map_err(net)?;
        check(resp).await.map(|_| ())
    }
}

fn expiry(expires_in: u64) -> Instant {
    Instant::now() + Duration::from_secs(expires_in.saturating_sub(60))
}

/// The page the browser lands on after Google sign-in.
///
/// On success it tries to close its own tab. Browsers only allow that for tabs a script
/// opened, and this one was opened by the OS and went through Google's pages, so most
/// browsers ignore it. Then it simply shows a tidy confirmation; memeity has already
/// come back to the front by then.
fn result_page(ok: bool) -> String {
    use base64::engine::general_purpose::STANDARD;
    let logo = STANDARD.encode(include_bytes!("../icons/128x128.png"));
    let (title, detail, close) = if ok {
        ("You're signed in", "You can close this tab and head back to memeity.", "<script>window.close()</script>")
    } else {
        ("Sign-in didn't finish", "Head back to memeity and try again.", "")
    };
    format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>memeity</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
  :root {{ color-scheme: light dark; --bg: #fff; --fg: #09090b; --muted: #71717a; }}
  @media (prefers-color-scheme: dark) {{ :root {{ --bg: #09090b; --fg: #fafafa; --muted: #a1a1aa; }} }}
  body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--fg);
         font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; text-align: center; }}
  img {{ width: 64px; height: 64px; margin-bottom: 16px; }}
  h1 {{ margin: 0 0 6px; font-size: 20px; font-weight: 600; letter-spacing: -0.02em; }}
  p {{ margin: 0; color: var(--muted); }}
</style>
<main><img src="data:image/png;base64,{logo}" alt=""><h1>{title}</h1><p>{detail}</p></main>
{close}</html>"#
    )
}

/// Waits for Google to redirect the browser to our loopback server, and returns the auth code.
async fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String, String> {
    loop {
        let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 16 * 1024];
        let mut n = 0;
        while n < buf.len() {
            let read = sock.read(&mut buf[n..]).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            n += read;
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let params: HashMap<String, String> = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default();

        if !params.contains_key("code") && !params.contains_key("error") {
            // e.g. /favicon.ico
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            continue;
        }

        let result = if let Some(err) = params.get("error") {
            Err(format!("Google sign-in was cancelled ({err})."))
        } else if params.get("state").map(String::as_str) != Some(expected_state) {
            Err("Sign-in response didn't match. Try again.".to_string())
        } else {
            Ok(params["code"].clone())
        };
        let page = result_page(result.is_ok());
        let _ = sock
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                )
                .as_bytes(),
            )
            .await;
        return result;
    }
}

#[cfg(test)]
mod tests {
    use super::result_page;

    #[test]
    fn result_pages() {
        let ok = result_page(true);
        assert!(ok.contains("You're signed in") && ok.contains("window.close()"));
        assert!(ok.contains("data:image/png;base64,iVBOR"), "logo embedded as a valid PNG");
        let fail = result_page(false);
        assert!(fail.contains("didn't finish") && !fail.contains("window.close()"), "failure page stays open");
    }
}
