use anyhow::{anyhow, Context, Result};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac_array;
use sha2::Sha512;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

const PAGE_SZ: usize = 4096;
const KEY_SZ: usize = 32;
const SALT_SZ: usize = 16;
const MAX_REGION_SIZE: usize = 500 * 1024 * 1024;
const KEY_CACHE_FILE: &str = ".mimicwx-keycache.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbRole {
    Contact,
    Session,
    Message,
    Other,
}

#[derive(Clone)]
pub struct DbFingerprint {
    rel_path: String,
    abs_path: PathBuf,
    role: DbRole,
    salt: [u8; SALT_SZ],
    page1: [u8; PAGE_SZ],
}

impl DbFingerprint {
    pub fn rel_path(&self) -> &str {
        &self.rel_path
    }

    fn page1(&self) -> &[u8; PAGE_SZ] {
        &self.page1
    }

    pub fn salt(&self) -> &[u8; SALT_SZ] {
        &self.salt
    }
}

impl std::fmt::Debug for DbFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbFingerprint")
            .field("rel_path", &self.rel_path)
            .field("abs_path", &self.abs_path)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct DbCatalog {
    db_dir: PathBuf,
    entries: Vec<DbFingerprint>,
}

impl DbCatalog {
    pub fn discover(db_dir: PathBuf) -> Result<Self> {
        let mut entries = Vec::new();
        collect_db_entries(&db_dir, &db_dir, &mut entries)?;
        anyhow::ensure!(
            !entries.is_empty(),
            "数据库目录中未找到可扫描的 .db 文件: {}",
            db_dir.display()
        );
        entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        Ok(Self { db_dir, entries })
    }

    pub fn db_dir(&self) -> &Path {
        &self.db_dir
    }

    pub fn entries(&self) -> &[DbFingerprint] {
        &self.entries
    }

    pub fn entry(&self, rel_path: &str) -> Option<&DbFingerprint> {
        self.entries.iter().find(|entry| entry.rel_path == rel_path)
    }

    pub fn message_paths(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|entry| entry.role == DbRole::Message)
            .map(|entry| entry.rel_path.as_str())
    }

    pub fn required_paths(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.role,
                    DbRole::Contact | DbRole::Session | DbRole::Message
                )
            })
            .map(|entry| entry.rel_path.as_str())
            .collect()
    }
}

#[derive(Clone)]
pub struct VerifiedKey {
    rel_path: String,
    enc_key: [u8; KEY_SZ],
    pid: i32,
    addr: usize,
}

impl VerifiedKey {
    pub fn enc_key(&self) -> [u8; KEY_SZ] {
        self.enc_key
    }
}

impl std::fmt::Debug for VerifiedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedKey")
            .field("rel_path", &self.rel_path)
            .field("pid", &self.pid)
            .field("addr", &format_args!("0x{:x}", self.addr))
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct KeyRegistry {
    keys: HashMap<String, VerifiedKey>,
}

impl KeyRegistry {
    fn new(keys: HashMap<String, VerifiedKey>) -> Self {
        Self { keys }
    }

    pub fn count(&self) -> usize {
        self.keys.len()
    }

    pub fn contains(&self, rel_path: &str) -> bool {
        self.keys.contains_key(rel_path)
    }

    pub fn enc_key_for(&self, rel_path: &str) -> Result<[u8; KEY_SZ]> {
        self.keys
            .get(rel_path)
            .map(VerifiedKey::enc_key)
            .ok_or_else(|| anyhow!("未找到数据库密钥: {rel_path}"))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct KeyCacheFile {
    version: u32,
    entries: Vec<KeyCacheEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct KeyCacheEntry {
    rel_path: String,
    enc_key_hex: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub process_count: usize,
    pub hex_patterns: usize,
    pub resolved_keys: usize,
    pub db_files: usize,
}

#[derive(Debug, Clone)]
pub struct ResolvedCatalog {
    pub catalog: Arc<DbCatalog>,
    pub registry: Arc<KeyRegistry>,
    pub summary: ScanSummary,
}

pub fn resolve_catalog(db_dir: PathBuf) -> Result<ResolvedCatalog> {
    let catalog = Arc::new(DbCatalog::discover(db_dir)?);
    let db_count = catalog.entries().len();
    info!(
        "🗂️ 数据库目录已发现: {} ({} 个 DB)",
        catalog.db_dir().display(),
        db_count
    );

    if let Some(registry) = load_cached_registry(&catalog)? {
        info!("🔐 已复用缓存数据库密钥: {} 个", registry.count());
        return Ok(ResolvedCatalog {
            catalog,
            registry: Arc::new(registry),
            summary: ScanSummary {
                resolved_keys: db_count,
                db_files: db_count,
                ..ScanSummary::default()
            },
        });
    }

    let mut resolver = MemoryKeyResolver::new(Arc::clone(&catalog));
    let summary = resolver.scan()?;

    let registry = Arc::new(KeyRegistry::new(resolver.into_registry()));
    let required: Vec<&str> = catalog.required_paths();
    let missing_required: Vec<&str> = required
        .into_iter()
        .filter(|rel_path| !registry.contains(rel_path))
        .collect();
    anyhow::ensure!(
        missing_required.is_empty(),
        "关键数据库密钥未解析完成: {}",
        missing_required.join(", ")
    );

    if registry.count() < catalog.entries().len() {
        let unresolved: Vec<&str> = catalog
            .entries()
            .iter()
            .filter(|entry| !registry.contains(entry.rel_path()))
            .map(DbFingerprint::rel_path)
            .collect();
        warn!(
            "⚠️ 仍有 {} 个非关键数据库未解析: {}",
            unresolved.len(),
            unresolved.join(", ")
        );
    }

    if let Err(err) = persist_registry_cache(&catalog, registry.as_ref()) {
        warn!("⚠️ 写入密钥缓存失败: {}", err);
    }

    Ok(ResolvedCatalog {
        catalog,
        registry: Arc::clone(&registry),
        summary: ScanSummary {
            resolved_keys: registry.count(),
            db_files: db_count,
            ..summary
        },
    })
}

struct MemoryKeyResolver {
    catalog: Arc<DbCatalog>,
    salt_to_indices: HashMap<[u8; SALT_SZ], Vec<usize>>,
    resolved: HashMap<usize, VerifiedKey>,
    unique_keys: HashMap<[u8; KEY_SZ], (i32, usize)>,
    hex_patterns: usize,
}

impl MemoryKeyResolver {
    fn new(catalog: Arc<DbCatalog>) -> Self {
        let mut salt_to_indices = HashMap::new();
        for (idx, entry) in catalog.entries().iter().enumerate() {
            salt_to_indices
                .entry(*entry.salt())
                .or_insert_with(Vec::new)
                .push(idx);
        }
        Self {
            catalog,
            salt_to_indices,
            resolved: HashMap::new(),
            unique_keys: HashMap::new(),
            hex_patterns: 0,
        }
    }

    fn into_registry(self) -> HashMap<String, VerifiedKey> {
        self.resolved
            .into_values()
            .map(|resolved| (resolved.rel_path.clone(), resolved))
            .collect()
    }

    fn scan(&mut self) -> Result<ScanSummary> {
        let processes = find_wechat_processes()?;
        let process_count = processes.len();

        for (pid, rss_kb) in &processes {
            let regions = match get_readable_regions(*pid) {
                Ok(value) => value,
                Err(err) => {
                    warn!("⚠️ 读取进程映射失败, 跳过 PID {}: {}", pid, err);
                    continue;
                }
            };
            let total_mb = regions.iter().map(|(_, size)| *size as u64).sum::<u64>() / 1024 / 1024;
            debug!(
                "扫描微信进程 PID={} (rss={}MB, 可读区域={} 个, {}MB)",
                pid,
                *rss_kb / 1024,
                regions.len(),
                total_mb
            );

            let mut mem = match File::open(format!("/proc/{pid}/mem")) {
                Ok(value) => value,
                Err(err) => {
                    warn!("⚠️ 打开进程内存失败, 跳过 PID {}: {}", pid, err);
                    continue;
                }
            };
            for (base, size) in regions {
                let mut buf = vec![0u8; size];
                if mem.seek(SeekFrom::Start(base as u64)).is_err() {
                    continue;
                }
                let read = match mem.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                buf.truncate(read);
                self.scan_region(*pid, base, &buf);
            }
        }

        self.cross_verify_pending();

        Ok(ScanSummary {
            process_count,
            hex_patterns: self.hex_patterns,
            resolved_keys: self.resolved.len(),
            db_files: self.catalog.entries().len(),
        })
    }

    fn unresolved_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.catalog
            .entries()
            .iter()
            .enumerate()
            .filter(|(idx, _)| !self.resolved.contains_key(idx))
            .map(|(idx, _)| idx)
    }

    fn scan_region(&mut self, pid: i32, base_addr: usize, data: &[u8]) {
        let mut idx = 0usize;
        while idx + 3 < data.len() {
            if data[idx] != b'x' || data[idx + 1] != b'\'' {
                idx += 1;
                continue;
            }
            let candidate_addr = base_addr + idx;
            let start = idx + 2;
            let mut end = start;
            while end < data.len() && data[end].is_ascii_hexdigit() {
                end += 1;
            }
            if end >= data.len() || data[end] != b'\'' {
                idx += 1;
                continue;
            }
            let hex = &data[start..end];
            let len = hex.len();
            idx = end + 1;
            if len < 64 || len > 192 || len % 2 != 0 {
                continue;
            }
            self.hex_patterns += 1;
            self.try_candidate(pid, candidate_addr, hex);
        }
    }

    fn try_candidate(&mut self, pid: i32, addr: usize, hex: &[u8]) {
        let len = hex.len();
        if len == 64 {
            if let Some(key_bytes) = decode_key(hex) {
                self.try_key_against_unresolved(pid, addr, key_bytes);
            }
            return;
        }

        let Some(key_bytes) = decode_key(&hex[..64]) else {
            return;
        };
        let Some(salt) = decode_salt(&hex[len - 32..]) else {
            return;
        };

        self.unique_keys.entry(key_bytes).or_insert((pid, addr));

        if let Some(indices) = self.salt_to_indices.get(&salt).cloned() {
            for idx in indices {
                if self.resolved.contains_key(&idx) {
                    continue;
                }
                let entry = &self.catalog.entries()[idx];
                if verify_enc_key(&key_bytes, entry.page1()) {
                    self.record_resolution(idx, pid, addr, key_bytes);
                }
            }
        }
    }

    fn try_key_against_unresolved(&mut self, pid: i32, addr: usize, key_bytes: [u8; KEY_SZ]) {
        self.unique_keys.entry(key_bytes).or_insert((pid, addr));
        let indices: Vec<usize> = self.unresolved_indices().collect();
        for idx in indices {
            let entry = &self.catalog.entries()[idx];
            if verify_enc_key(&key_bytes, entry.page1()) {
                self.record_resolution(idx, pid, addr, key_bytes);
            }
        }
    }

    fn record_resolution(&mut self, idx: usize, pid: i32, addr: usize, key_bytes: [u8; KEY_SZ]) {
        let entry = &self.catalog.entries()[idx];
        self.resolved.insert(
            idx,
            VerifiedKey {
                rel_path: entry.rel_path().to_string(),
                enc_key: key_bytes,
                pid,
                addr,
            },
        );
    }

    fn cross_verify_pending(&mut self) {
        if self.unique_keys.is_empty() {
            return;
        }
        let known_keys: Vec<([u8; KEY_SZ], (i32, usize))> = self
            .unique_keys
            .iter()
            .map(|(key, meta)| (*key, *meta))
            .collect();
        let indices: Vec<usize> = self.unresolved_indices().collect();
        for idx in indices {
            let entry = &self.catalog.entries()[idx];
            for (key_bytes, (pid, addr)) in &known_keys {
                if verify_enc_key(key_bytes, entry.page1()) {
                    self.record_resolution(idx, *pid, *addr, *key_bytes);
                    break;
                }
            }
        }
    }
}

fn collect_db_entries(root: &Path, current: &Path, entries: &mut Vec<DbFingerprint>) -> Result<()> {
    for entry in std::fs::read_dir(current)
        .with_context(|| format!("遍历数据库目录失败: {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_db_entries(root, &path, entries)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !name.ends_with(".db") || name.ends_with("-wal") || name.ends_with("-shm") {
            continue;
        }
        if entry.metadata()?.len() < PAGE_SZ as u64 {
            continue;
        }

        let mut file =
            File::open(&path).with_context(|| format!("读取数据库首页失败: {}", path.display()))?;
        let mut page1 = [0u8; PAGE_SZ];
        file.read_exact(&mut page1)
            .with_context(|| format!("读取数据库 page1 失败: {}", path.display()))?;
        let mut salt = [0u8; SALT_SZ];
        salt.copy_from_slice(&page1[..SALT_SZ]);

        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        entries.push(DbFingerprint {
            role: classify_role(&rel_path),
            rel_path,
            abs_path: path,
            salt,
            page1,
        });
    }
    Ok(())
}

fn cache_path(catalog: &DbCatalog) -> PathBuf {
    catalog.db_dir().join(KEY_CACHE_FILE)
}

fn load_cached_registry(catalog: &DbCatalog) -> Result<Option<KeyRegistry>> {
    let path = cache_path(catalog);
    if !path.exists() {
        return Ok(None);
    }

    let data =
        std::fs::read(&path).with_context(|| format!("读取密钥缓存失败: {}", path.display()))?;
    let cache: KeyCacheFile = serde_json::from_slice(&data)
        .with_context(|| format!("解析密钥缓存失败: {}", path.display()))?;
    if cache.version != 1 {
        warn!("⚠️ 密钥缓存版本不兼容, 将重建: {}", path.display());
        return Ok(None);
    }

    let mut keys = HashMap::new();
    for entry in cache.entries {
        let Some(db) = catalog.entry(&entry.rel_path) else {
            continue;
        };
        let Some(enc_key) = decode_fixed_hex::<KEY_SZ>(&entry.enc_key_hex) else {
            warn!("⚠️ 密钥缓存格式损坏, 将重建: {}", entry.rel_path);
            return Ok(None);
        };
        if !verify_enc_key(&enc_key, db.page1()) {
            info!("🔄 密钥缓存失效, 将重新扫描: {}", entry.rel_path);
            return Ok(None);
        }
        keys.insert(
            entry.rel_path.clone(),
            VerifiedKey {
                rel_path: entry.rel_path,
                enc_key,
                pid: 0,
                addr: 0,
            },
        );
    }

    let registry = KeyRegistry::new(keys);
    let missing_required: Vec<&str> = catalog
        .required_paths()
        .into_iter()
        .filter(|rel_path| !registry.contains(rel_path))
        .collect();
    if !missing_required.is_empty() {
        info!(
            "🔄 密钥缓存不完整, 将重新扫描: {}",
            missing_required.join(", ")
        );
        return Ok(None);
    }

    Ok(Some(registry))
}

fn persist_registry_cache(catalog: &DbCatalog, registry: &KeyRegistry) -> Result<()> {
    let path = cache_path(catalog);
    let mut entries = Vec::new();
    for db in catalog.entries() {
        let Some(enc_key) = registry.keys.get(db.rel_path()).map(VerifiedKey::enc_key) else {
            continue;
        };
        entries.push(KeyCacheEntry {
            rel_path: db.rel_path().to_string(),
            enc_key_hex: hex_encode(&enc_key),
        });
    }

    let payload = serde_json::to_vec_pretty(&KeyCacheFile {
        version: 1,
        entries,
    })
    .context("序列化密钥缓存失败")?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, payload)
        .with_context(|| format!("写入密钥缓存临时文件失败: {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("替换密钥缓存失败: {}", path.display()))?;
    Ok(())
}

fn classify_role(rel_path: &str) -> DbRole {
    match rel_path {
        "contact/contact.db" => DbRole::Contact,
        "session/session.db" => DbRole::Session,
        _ if rel_path
            .strip_prefix("message/")
            .is_some_and(is_message_db_name) =>
        {
            DbRole::Message
        }
        _ => DbRole::Other,
    }
}

fn is_message_db_name(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("message_") {
        if let Some(num_part) = rest.strip_suffix(".db") {
            return !num_part.is_empty() && num_part.chars().all(|ch| ch.is_ascii_digit());
        }
    }
    false
}

fn find_wechat_processes() -> Result<Vec<(i32, u64)>> {
    let mut processes = Vec::new();
    for entry in std::fs::read_dir("/proc").context("读取 /proc 失败")? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };
        if let Some(rss_kb) = is_wechat_process(pid)
            .then(|| read_rss_kb(pid).ok())
            .flatten()
        {
            processes.push((pid, rss_kb));
        }
    }

    anyhow::ensure!(!processes.is_empty(), "未检测到可扫描的微信进程");
    processes.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(processes)
}

fn is_wechat_process(pid: i32) -> bool {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    let exe_path = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let exe_name = Path::new(&exe_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let haystack = format!("{} {}", comm, exe_name).to_ascii_lowercase();
    haystack.contains("wechat") || haystack.contains("weixin")
}

fn read_rss_kb(pid: i32) -> Result<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .with_context(|| format!("读取 /proc/{pid}/statm 失败"))?;
    let rss_pages = statm
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("statm 缺少 rss 字段"))?
        .parse::<u64>()
        .context("解析 statm rss 失败")?;
    Ok(rss_pages * 4)
}

fn get_readable_regions(pid: i32) -> Result<Vec<(usize, usize)>> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .with_context(|| format!("读取 /proc/{pid}/maps 失败"))?;
    let mut regions = Vec::new();
    for line in maps.lines() {
        let mut parts = line.split_whitespace();
        let Some(range) = parts.next() else {
            continue;
        };
        let Some(perms) = parts.next() else {
            continue;
        };
        if !perms.contains('r') {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let Ok(start_addr) = usize::from_str_radix(start, 16) else {
            continue;
        };
        let Ok(end_addr) = usize::from_str_radix(end, 16) else {
            continue;
        };
        if end_addr <= start_addr {
            continue;
        }
        let size = end_addr - start_addr;
        if size > MAX_REGION_SIZE {
            continue;
        }
        regions.push((start_addr, size));
    }
    Ok(regions)
}

fn decode_key(hex: &[u8]) -> Option<[u8; KEY_SZ]> {
    if hex.len() != KEY_SZ * 2 {
        return None;
    }
    let mut out = [0u8; KEY_SZ];
    for (index, chunk) in hex.chunks_exact(2).enumerate() {
        out[index] = decode_hex_byte(chunk)?;
    }
    Some(out)
}

fn decode_salt(hex: &[u8]) -> Option<[u8; SALT_SZ]> {
    if hex.len() != SALT_SZ * 2 {
        return None;
    }
    let mut out = [0u8; SALT_SZ];
    for (index, chunk) in hex.chunks_exact(2).enumerate() {
        out[index] = decode_hex_byte(chunk)?;
    }
    Some(out)
}

fn decode_fixed_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (index, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        out[index] = decode_hex_byte(chunk)?;
    }
    Some(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn decode_hex_byte(hex: &[u8]) -> Option<u8> {
    let hi = decode_nibble(*hex.first()?)?;
    let lo = decode_nibble(*hex.get(1)?)?;
    Some((hi << 4) | lo)
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn verify_enc_key(enc_key: &[u8; KEY_SZ], db_page1: &[u8; PAGE_SZ]) -> bool {
    let salt = &db_page1[..SALT_SZ];
    let mac_salt: Vec<u8> = salt.iter().map(|byte| byte ^ 0x3a).collect();
    let mac_key = pbkdf2_hmac_array::<Sha512, KEY_SZ>(enc_key, &mac_salt, 2);

    let hmac_data = &db_page1[SALT_SZ..PAGE_SZ - 80 + 16];
    let stored_hmac = &db_page1[PAGE_SZ - 64..PAGE_SZ];
    let Ok(mut mac) = Hmac::<Sha512>::new_from_slice(&mac_key) else {
        return false;
    };
    mac.update(hmac_data);
    mac.update(&1u32.to_le_bytes());
    mac.finalize().into_bytes().as_slice() == stored_hmac
}
