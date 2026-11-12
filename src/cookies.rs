use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aes::Aes128;
use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
use cbc::Decryptor;
use pbkdf2::pbkdf2_hmac;
use rusqlite::{Connection, OpenFlags};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::error::{ErrorCode, Result, YtrustError};
use crate::util::unix_time_i64;

#[derive(Debug, Clone)]
struct Cookie {
    domain: String,
    path: String,
    name: String,
    value: String,
    expiry: i64,
}

#[derive(Debug, Clone, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    pub fn header_for(&self, host: &str) -> Option<String> {
        let now = unix_time_i64();
        let mut matched: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|cookie| {
                (cookie.expiry == 0 || cookie.expiry >= now) && domain_matches(host, &cookie.domain)
            })
            .collect();

        // Browsers send the most specific path first. Keep the first value for each name.
        matched.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then_with(|| b.expiry.cmp(&a.expiry))
        });

        let mut seen = BTreeSet::new();
        let mut values = Vec::new();
        for cookie in matched {
            if seen.insert(cookie.name.as_str()) {
                values.push(format!("{}={}", cookie.name, cookie.value));
            }
        }

        (!values.is_empty()).then(|| values.join("; "))
    }

    pub fn merge(&mut self, mut other: CookieJar) {
        self.cookies.append(&mut other.cookies);
    }

    fn insert(&mut self, domain: &str, path: &str, name: &str, value: &str, expiry: i64) {
        if name.is_empty() {
            return;
        }
        self.cookies.push(Cookie {
            domain: domain.trim_start_matches('.').to_ascii_lowercase(),
            path: path.to_string(),
            name: name.to_string(),
            value: value.to_string(),
            expiry,
        });
    }

    fn value(&self, name: &str) -> Option<String> {
        let now = unix_time_i64();
        self.cookies
            .iter()
            .filter(|cookie| cookie.name == name && (cookie.expiry == 0 || cookie.expiry >= now))
            .max_by_key(|cookie| (cookie.path.len(), cookie.expiry))
            .map(|cookie| cookie.value.clone())
    }
}

#[derive(Debug, Clone, Default)]
pub struct LoadedCookies {
    pub header: Option<String>,
    pub sapisid: Option<String>,
    pub sapisid1p: Option<String>,
    pub sapisid3p: Option<String>,
}

pub fn load(cookie_file: Option<&Path>, from_browser: Option<&str>) -> Result<LoadedCookies> {
    let mut jar = CookieJar::default();

    if let Some(path) = cookie_file {
        jar.merge(load_netscape(path).map_err(|error| {
            YtrustError::coded(
                ErrorCode::Cookies,
                format!("failed to load cookies file {}: {error}", path.display()),
            )
        })?);
    }
    if let Some(spec) = from_browser {
        jar.merge(load_browser(spec).map_err(|error| {
            YtrustError::coded(
                ErrorCode::Cookies,
                format!("failed to load cookies from {spec}: {error}"),
            )
        })?);
    }

    Ok(LoadedCookies {
        header: jar.header_for("www.youtube.com"),
        sapisid: jar.value("SAPISID"),
        sapisid1p: jar.value("__Secure-1PAPISID"),
        sapisid3p: jar.value("__Secure-3PAPISID"),
    })
}

fn load_netscape(path: &Path) -> Result<CookieJar> {
    let content = std::fs::read_to_string(path)?;
    let mut jar = CookieJar::default();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || (line.starts_with('#') && !line.starts_with("#HttpOnly_")) {
            continue;
        }
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 7 {
            continue;
        }
        let domain = fields[0];
        if !relevant_domain(domain) {
            continue;
        }
        let expiry = fields[4].parse::<i64>().unwrap_or(0);
        jar.insert(domain, fields[2], fields[5], fields[6], expiry);
    }

    Ok(jar)
}

fn load_browser(spec: &str) -> Result<CookieJar> {
    let spec = spec.trim();
    let (browser, profile) = match spec.split_once(':') {
        Some((browser, profile)) if !profile.is_empty() => {
            (browser.to_ascii_lowercase(), Some(profile.to_string()))
        }
        _ => (spec.to_ascii_lowercase(), None),
    };

    let Some(profile) = profile else {
        return match browser.as_str() {
            "firefox" => {
                let path = find_firefox_profile()?;
                load_firefox(&path)
            }
            "chrome" | "chromium" | "brave" | "edge" | "vivaldi" | "opera" => {
                let path = find_chromium_profile(&browser)?;
                load_chromium(&path)
            }
            other => Err(unsupported_browser(other)),
        };
    };

    let path = PathBuf::from(&profile);
    if path.exists()
        || profile.contains('/')
        || profile.contains('\\')
        || Path::new(&profile).is_absolute()
    {
        return load_from_path(&path);
    }

    match browser.as_str() {
        "firefox" => {
            let path = find_firefox_profile_named(&profile)?;
            load_firefox(&path)
        }
        "chrome" | "chromium" | "brave" | "edge" | "vivaldi" | "opera" => {
            let path = find_chromium_profile_named(&browser, &profile)?;
            load_chromium(&path)
        }
        other => Err(unsupported_browser(other)),
    }
}

fn unsupported_browser(browser: &str) -> YtrustError {
    YtrustError::coded(
        ErrorCode::InvalidInput,
        format!(
            "unsupported browser: {browser}. Use firefox, chrome, chromium, brave, edge, vivaldi or opera"
        ),
    )
}

fn load_from_path(path: &Path) -> Result<CookieJar> {
    if path.is_file() {
        let name = path.file_name().unwrap_or_default();
        if name.eq_ignore_ascii_case("cookies.sqlite") {
            return load_firefox_db(path);
        }
        if name.eq_ignore_ascii_case("Cookies") {
            return load_chromium_db(path);
        }
        return load_netscape(path);
    }

    let firefox = path.join("cookies.sqlite");
    if firefox.is_file() {
        return load_firefox(&firefox);
    }

    if let Some(chromium) = chromium_cookie_db(path) {
        return load_chromium_db(&chromium);
    }

    Err(YtrustError::coded(
        ErrorCode::Cookies,
        format!("no supported cookie database found in {}", path.display()),
    ))
}

fn load_firefox(path: &Path) -> Result<CookieJar> {
    let db = if path.is_file() {
        path.to_path_buf()
    } else {
        path.join("cookies.sqlite")
    };
    load_firefox_db(&db)
}

fn load_firefox_db(path: &Path) -> Result<CookieJar> {
    let (connection, _temp_dir) = open_firefox_db(path)?;
    let mut statement =
        connection.prepare("SELECT host, path, name, value, expiry FROM moz_cookies")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;

    let mut jar = CookieJar::default();
    for row in rows {
        let (host, path, name, value, expiry) = row?;
        if !relevant_domain(&host) {
            continue;
        }
        jar.insert(&host, &path, &name, &value, expiry);
    }

    Ok(jar)
}

fn open_firefox_db(path: &Path) -> Result<(Connection, Option<tempfile::TempDir>)> {
    // Firefox usually keeps the live database locked, so always read a snapshot.
    let temp = tempfile::tempdir()?;
    let snapshot = temp.path().join("cookies.sqlite");
    std::fs::copy(path, &snapshot)?;
    for suffix in ["-wal", "-shm"] {
        let source = PathBuf::from(format!("{}{suffix}", path.display()));
        if source.exists() {
            let _ = std::fs::copy(&source, temp.path().join(format!("cookies.sqlite{suffix}")));
        }
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(&snapshot, flags)?;
    Ok((connection, Some(temp)))
}

fn find_firefox_profile() -> Result<PathBuf> {
    let mut candidates = firefox_candidates();
    candidates.sort_by_key(|path| {
        (
            !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("default-release")),
            std::cmp::Reverse(
                std::fs::metadata(path.join("cookies.sqlite"))
                    .and_then(|meta| meta.modified())
                    .ok(),
            ),
        )
    });

    candidates.into_iter().next().ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::Cookies,
            "Firefox profile with cookies.sqlite was not found",
        )
    })
}

fn find_firefox_profile_named(name: &str) -> Result<PathBuf> {
    firefox_candidates()
        .into_iter()
        .find(|path| {
            path.file_name()
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        })
        .ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::Cookies,
                format!("Firefox profile {name:?} was not found"),
            )
        })
}

fn firefox_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for base in [
            home.join(".mozilla/firefox"),
            home.join(".config/mozilla/firefox"),
            home.join("snap/firefox/common/.mozilla/firefox"),
            home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
        ] {
            collect_firefox_profiles(&base, &mut candidates);
        }
    }
    if let Some(config) = dirs::config_dir() {
        collect_firefox_profiles(&config.join("mozilla/firefox"), &mut candidates);
    }
    if let Some(data) = dirs::data_dir() {
        collect_firefox_profiles(&data.join("Mozilla/Firefox"), &mut candidates);
        collect_firefox_profiles(&data.join("Firefox"), &mut candidates);
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn collect_firefox_profiles(base: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join("cookies.sqlite").is_file() {
            out.push(path);
        }
    }
}

fn find_chromium_profile(browser: &str) -> Result<PathBuf> {
    let roots = chromium_roots(browser);
    let mut candidates = Vec::new();

    for root in roots {
        if let Some(db) = chromium_cookie_db(&root) {
            candidates.push(db);
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let profile = entry.path();
            if !profile.is_dir() {
                continue;
            }
            if let Some(db) = chromium_cookie_db(&profile) {
                candidates.push(db);
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates.sort_by_key(|path| {
        (
            !path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "Default"),
            std::cmp::Reverse(
                std::fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .ok(),
            ),
        )
    });

    candidates.into_iter().next().ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::Cookies,
            format!("{browser} profile with cookies was not found"),
        )
    })
}

fn find_chromium_profile_named(browser: &str, name: &str) -> Result<PathBuf> {
    for root in chromium_roots(browser) {
        if root
            .file_name()
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            && let Some(db) = chromium_cookie_db(&root)
        {
            return Ok(db);
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let profile = entry.path();
            if profile.is_dir()
                && profile
                    .file_name()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
                && let Some(db) = chromium_cookie_db(&profile)
            {
                return Ok(db);
            }
        }
    }
    Err(YtrustError::coded(
        ErrorCode::Cookies,
        format!("{browser} profile {name:?} was not found"),
    ))
}

fn chromium_roots(browser: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.extend(linux_chromium_roots(browser));
    roots.extend(windows_chromium_roots(browser));
    roots.extend(macos_chromium_roots(browser));
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_chromium_roots(browser: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(config) = dirs::config_dir() {
        roots.extend(match browser {
            "chrome" => vec![
                config.join("google-chrome"),
                config.join("google-chrome-beta"),
                config.join("google-chrome-unstable"),
                config.join("google-chrome-for-testing"),
            ],
            "chromium" => vec![config.join("chromium")],
            "brave" => vec![
                config.join("BraveSoftware/Brave-Browser"),
                config.join("BraveSoftware/Brave-Browser-Beta"),
                config.join("BraveSoftware/Brave-Browser-Nightly"),
            ],
            "edge" => vec![config.join("microsoft-edge")],
            "vivaldi" => vec![config.join("vivaldi")],
            "opera" => vec![config.join("opera")],
            _ => Vec::new(),
        });
    }
    if let Some(home) = dirs::home_dir() {
        roots.extend(match browser {
            "chrome" => vec![home.join(".var/app/com.google.Chrome/config/google-chrome")],
            "chromium" => vec![
                home.join("snap/chromium/common/chromium"),
                home.join(".var/app/org.chromium.Chromium/config/chromium"),
            ],
            "brave" => vec![
                home.join("snap/brave/common/.config/BraveSoftware/Brave-Browser"),
                home.join(".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser"),
            ],
            "edge" => vec![home.join(".var/app/com.microsoft.Edge/config/microsoft-edge")],
            "vivaldi" => vec![home.join(".var/app/com.vivaldi.Vivaldi/config/vivaldi")],
            "opera" => vec![home.join(".var/app/com.opera.Opera/config/opera")],
            _ => Vec::new(),
        });
    }
    roots
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn linux_chromium_roots(_browser: &str) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(target_os = "windows")]
fn windows_chromium_roots(browser: &str) -> Vec<PathBuf> {
    let Some(local) = dirs::data_local_dir() else {
        return Vec::new();
    };
    match browser {
        "chrome" => vec![
            local.join("Google/Chrome/User Data"),
            local.join("Google/Chrome Beta/User Data"),
            local.join("Google/Chrome SxS/User Data"),
            local.join("Google/Chrome for Testing/User Data"),
        ],
        "chromium" => vec![local.join("Chromium/User Data")],
        "brave" => vec![
            local.join("BraveSoftware/Brave-Browser/User Data"),
            local.join("BraveSoftware/Brave-Browser-Beta/User Data"),
            local.join("BraveSoftware/Brave-Browser-Nightly/User Data"),
        ],
        "edge" => vec![local.join("Microsoft/Edge/User Data")],
        "vivaldi" => vec![local.join("Vivaldi/User Data")],
        "opera" => vec![
            local.join("Opera Software/Opera Stable"),
            local.join("Opera Software/Opera GX Stable"),
        ],
        _ => Vec::new(),
    }
}

#[cfg(not(target_os = "windows"))]
fn windows_chromium_roots(_browser: &str) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn macos_chromium_roots(browser: &str) -> Vec<PathBuf> {
    let Some(data) = dirs::data_dir() else {
        return Vec::new();
    };
    match browser {
        "chrome" => vec![
            data.join("Google/Chrome"),
            data.join("Google/Chrome Beta"),
            data.join("Google/Chrome Canary"),
        ],
        "chromium" => vec![data.join("Chromium")],
        "brave" => vec![
            data.join("BraveSoftware/Brave-Browser"),
            data.join("BraveSoftware/Brave-Browser-Beta"),
            data.join("BraveSoftware/Brave-Browser-Nightly"),
        ],
        "edge" => vec![data.join("Microsoft Edge")],
        "vivaldi" => vec![data.join("Vivaldi")],
        "opera" => vec![
            data.join("com.operasoftware.Opera"),
            data.join("com.operasoftware.OperaGX"),
        ],
        _ => Vec::new(),
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_chromium_roots(_browser: &str) -> Vec<PathBuf> {
    Vec::new()
}

fn chromium_cookie_db(profile: &Path) -> Option<PathBuf> {
    [
        profile.join("Network").join("Cookies"),
        profile.join("Cookies"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn load_chromium(path: &Path) -> Result<CookieJar> {
    if path.is_file() {
        return load_chromium_db(path);
    }
    let db = chromium_cookie_db(path).ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::Cookies,
            format!("no Chromium Cookies database found in {}", path.display()),
        )
    })?;
    load_chromium_db(&db)
}

fn load_chromium_db(path: &Path) -> Result<CookieJar> {
    let (connection, _temp_dir) = open_chromium_db(path)?;
    let mut statement = connection
        .prepare("SELECT host_key, path, name, value, encrypted_value, expires_utc FROM cookies")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;

    let mut jar = CookieJar::default();
    let mut skipped_encrypted = 0usize;
    for row in rows {
        let (host, path, name, value, encrypted, expires_utc) = row?;
        if !relevant_domain(&host) {
            continue;
        }
        let value = if value.is_empty() {
            match decrypt_chromium_cookie(&encrypted, &host) {
                Some(value) => value,
                None => {
                    skipped_encrypted += 1;
                    continue;
                }
            }
        } else {
            value
        };
        let expiry = if expires_utc > 0 {
            expires_utc / 1_000_000 - 11_644_473_600
        } else {
            0
        };
        jar.insert(&host, &path, &name, &value, expiry);
    }

    if jar.cookies.is_empty() && skipped_encrypted > 0 {
        return Err(YtrustError::coded(
            ErrorCode::Cookies,
            "Chromium cookies are encrypted with an unsupported platform keyring; export cookies.txt or use Firefox",
        ));
    }
    if skipped_encrypted > 0 {
        tracing::warn!(
            "skipped {skipped_encrypted} Chromium cookie(s) encrypted with an unsupported platform keyring"
        );
    }

    Ok(jar)
}

fn open_chromium_db(path: &Path) -> Result<(Connection, Option<tempfile::TempDir>)> {
    let temp = tempfile::tempdir()?;
    let snapshot = temp.path().join("Cookies");
    std::fs::copy(path, &snapshot)?;
    for suffix in ["-journal", "-wal", "-shm"] {
        let source = PathBuf::from(format!("{}{suffix}", path.display()));
        if source.exists() {
            let _ = std::fs::copy(&source, temp.path().join(format!("Cookies{suffix}")));
        }
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(&snapshot, flags)?;
    Ok((connection, Some(temp)))
}

fn decrypt_chromium_cookie(encrypted: &[u8], host: &str) -> Option<String> {
    if encrypted.len() < 3 {
        return None;
    }
    let prefix = &encrypted[..3];
    if prefix != b"v10" && prefix != b"v11" {
        return None;
    }

    let key = chromium_key();
    let iv = [b' '; 16];
    let mut buffer = encrypted[3..].to_vec();
    type Aes128CbcDec = Decryptor<Aes128>;
    let plain = Aes128CbcDec::new(key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(&mut buffer)
        .ok()?;

    let host_hash = Sha256::digest(host.as_bytes());
    let plain = if plain.len() > 32 && plain[..32] == host_hash[..] {
        &plain[32..]
    } else {
        plain
    };

    String::from_utf8(plain.to_vec()).ok()
}

fn chromium_key() -> &'static [u8; 16] {
    static KEY: OnceLock<[u8; 16]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = [0u8; 16];
        pbkdf2_hmac::<Sha1>(b"peanuts", b"saltysalt", 1, &mut key);
        key
    })
}

fn relevant_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.');
    host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host.ends_with(".youtube-nocookie.com")
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host.eq_ignore_ascii_case(domain)
        || host
            .to_ascii_lowercase()
            .ends_with(&format!(".{}", domain.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_domains() {
        assert!(relevant_domain("www.youtube.com"));
        assert!(relevant_domain(".youtube.com"));
        assert!(!relevant_domain("google.com"));
        assert!(!relevant_domain("example.com"));
    }

    fn encrypted_chromium_value(host: &str, value: &str) -> Vec<u8> {
        use aes::cipher::BlockModeEncrypt;
        use cbc::Encryptor;

        let key = *chromium_key();
        let iv = [b' '; 16];
        let mut plain = Sha256::digest(host.as_bytes()).to_vec();
        plain.extend_from_slice(value.as_bytes());

        let msg_len = plain.len();
        let mut buffer = vec![0u8; msg_len + 16];
        buffer[..msg_len].copy_from_slice(&plain);
        let encrypted = Encryptor::<Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded::<Pkcs7>(&mut buffer, msg_len)
            .expect("encrypt")
            .to_vec();
        let mut result = b"v10".to_vec();
        result.extend_from_slice(&encrypted);
        result
    }

    #[test]
    fn decrypts_linux_chromium_cookie() {
        let value = encrypted_chromium_value(".youtube.com", "secret-value");
        assert_eq!(
            decrypt_chromium_cookie(&value, ".youtube.com").as_deref(),
            Some("secret-value")
        );
    }

    #[test]
    fn loads_chromium_profile_database() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("Default");
        std::fs::create_dir_all(profile.join("Network")).unwrap();
        let db = profile.join("Network").join("Cookies");
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cookies (
                    host_key TEXT,
                    path TEXT,
                    name TEXT,
                    value TEXT,
                    encrypted_value BLOB,
                    expires_utc INTEGER
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO cookies VALUES (?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    ".youtube.com",
                    "/",
                    "SID",
                    "",
                    encrypted_chromium_value(".youtube.com", "secret"),
                    0i64
                ],
            )
            .unwrap();
        drop(connection);

        let jar = load_from_path(&profile).unwrap();
        assert_eq!(
            jar.header_for("www.youtube.com"),
            Some("SID=secret".to_string())
        );
    }

    #[test]
    fn builds_youtube_header() {
        let mut jar = CookieJar::default();
        jar.insert(".youtube.com", "/", "SID", "secret", 0);
        jar.insert(".google.com", "/", "SID", "wrong", 0);
        assert_eq!(jar.header_for("www.youtube.com"), Some("SID=secret".into()));
    }
}
