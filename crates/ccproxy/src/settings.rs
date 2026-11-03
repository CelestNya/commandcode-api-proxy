//! The editable settings file — one document, three readers.
//!
//! `ccproxy.json` is the only place a preference is stored, and **three**
//! things read it: the proxy at startup, the tray menu (rebuilt per
//! right-click), and the WebUI settings screen. They previously disagreed on
//! *which* file: the proxy resolved the config beside its own executable
//! (`<root>/service/ccproxy.json`) while the tray resolved it beside the tray
//! (`<root>/ccproxy.json`). So a tray toggle rewrote a file the proxy never
//! read — the checkbox said on, the running proxy stayed off, and the WebUI
//! would have shown a third opinion. This module is the single answer:
//! [`config_path`] for the running process, [`path_for_proxy`] for anyone
//! asking on the proxy's behalf, and both name the same file.
//!
//! Two more properties are load-bearing:
//!
//! * **Unknown keys survive.** The file is edited as a JSON object, never
//!   re-serialised from a typed struct, so a key a future build adds (or one a
//!   user hand-wrote) is not silently deleted by a toggle. A downgrade that
//!   dropped keys would change behaviour on the *next* startup, which is the
//!   worst kind of regression to debug.
//! * **Writes are atomic.** The tray may restart the proxy the instant after a
//!   save; a half-written file read at that moment is a startup failure. Write
//!   a sibling temp file, then rename over the target — a rename within one
//!   directory is atomic on both platforms this ships to.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Environment override for the settings file. Used by the test harness (and
/// by anyone running a second instance from the same build tree) to keep a
/// scratch config away from the install's own.
pub const CONFIG_ENV: &str = "CC_PROXY_CONFIG";

/// The file name, beside whichever executable owns it.
const FILE_NAME: &str = "ccproxy.json";

/// Reported when the file does not exist yet. Sixteen zeros, so it has the
/// same shape as a real revision and a comparison never accidentally matches.
pub const ABSENT_REVISION: &str = "0000000000000000";

/// The settings file of the process that is asking.
///
/// `CC_PROXY_CONFIG` wins, then the file beside `current_exe`. For the proxy
/// that is `<install>/service/ccproxy.json`; a bare `cargo build` puts both
/// binaries in one directory and the same rule then names that directory's
/// file, which is what the integration tests rely on.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var(CONFIG_ENV) {
        if !raw.trim().is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    std::env::current_exe().ok().map(|exe| path_for_exe(&exe))
}

/// The settings file that belongs to `exe`.
#[must_use]
pub fn path_for_exe(exe: &Path) -> PathBuf {
    exe.parent()
        .map_or_else(|| PathBuf::from(FILE_NAME), |dir| dir.join(FILE_NAME))
}

/// The settings file the *proxy* reads, resolved from the proxy binary rather
/// than from the caller's own location.
///
/// The tray must ask this way: it sits one level above the proxy in the
/// shipped layout, so resolving from `current_exe` would name a different file
/// — the defect this module exists to remove.
#[must_use]
pub fn path_for_proxy(proxy_binary: &Path) -> PathBuf {
    path_for_exe(proxy_binary)
}

/// One editable setting, as the WebUI renders it and the API validates it.
pub struct Field {
    /// JSON key in `ccproxy.json`.
    pub key: &'static str,
    /// Human label (Chinese; the page is zh-CN throughout).
    pub label: &'static str,
    /// Grouping heading on the settings screen.
    pub group: &'static str,
    pub kind: Kind,
    /// Environment variable that overrides the file for the running process,
    /// if any. `config::load` reads env first, so an edit to a shadowed field
    /// is stored but does not take effect — the page says so instead of
    /// pretending the apply worked.
    pub env: Option<&'static str>,
    /// Default as a JSON literal, so the table can be `const`.
    pub default: &'static str,
    /// One line of guidance under the control.
    pub note: &'static str,
}

/// The shape of a field's value, with the bounds the API enforces.
pub enum Kind {
    Bool,
    Int {
        min: u64,
        max: u64,
        /// How far one tap of the stepper moves the value. Not 1 for every
        /// field: nudging a millisecond budget by 1 ms is unusable, and the
        /// page has no room for "type the number, then press up 60000 times".
        step: u64,
        /// Shown after the input, e.g. `ms`.
        unit: &'static str,
    },
    Text {
        max_len: usize,
    },
    Choice(&'static [&'static str]),
}

impl Kind {
    /// The `kind`/bounds the page needs, as JSON.
    #[must_use]
    fn describe(&self) -> Value {
        match self {
            Self::Bool => serde_json::json!({"kind": "bool"}),
            Self::Int {
                min,
                max,
                step,
                unit,
            } => {
                serde_json::json!({"kind": "int", "min": min, "max": max, "step": step, "unit": unit})
            }
            Self::Text { max_len } => serde_json::json!({"kind": "text", "maxLen": max_len}),
            Self::Choice(options) => serde_json::json!({"kind": "choice", "options": options}),
        }
    }
}

/// Every setting the settings screen may edit.
///
/// Deliberately not the whole `FileConfig`: `host`/`port` are omitted because
/// the port is a production contract (moving it strands the tray and the
/// WebUI), and `ccVersion` is managed by the npm lookup. A key absent from
/// this table is still preserved in the file — it simply cannot be edited from
/// the page.
pub const FIELDS: &[Field] = &[
    Field {
        key: "proxy",
        label: "出站代理",
        group: "网络",
        kind: Kind::Text { max_len: 200 },
        env: Some("CC_PROXY"),
        default: "\"default\"",
        note:
            "default = 探测系统代理，不可用就直连；direct = 从不使用代理；也可填 http://host:port",
    },
    Field {
        key: "imageCompress",
        label: "图片压缩",
        group: "图片压缩",
        kind: Kind::Bool,
        env: Some("CC_IMAGE_COMPRESS"),
        default: "false",
        note: "把大截图重编码为 JPEG，只在更小的时候替换；与托盘菜单同一开关",
    },
    Field {
        key: "imageCompressQuality",
        label: "压缩质量",
        group: "图片压缩",
        kind: Kind::Int {
            min: 1,
            max: 100,
            step: 1,
            unit: "",
        },
        env: Some("CC_IMAGE_COMPRESS_QUALITY"),
        default: "80",
        note: "越低越小；文字截图建议不低于 70",
    },
    Field {
        key: "imageCompressMinBytes",
        label: "压缩门槛",
        group: "图片压缩",
        kind: Kind::Int {
            min: 0,
            max: 33_554_432,
            step: 65_536,
            unit: "字节",
        },
        env: None,
        default: "512000",
        note: "解码后的原始字节低于此值的图片原样转发",
    },
    Field {
        key: "logLevel",
        label: "日志级别",
        group: "运行",
        kind: Kind::Choice(&["error", "warn", "info", "debug"]),
        env: Some("LOG_LEVEL"),
        default: "\"info\"",
        note: "debug 会把每个转发体的体积写进日志，仅排障时使用",
    },
    Field {
        key: "upstreamTimeoutMs",
        label: "上游超时",
        group: "超时",
        kind: Kind::Int {
            min: 1_000,
            max: 1_800_000,
            step: 60_000,
            unit: "ms",
        },
        env: Some("CC_UPSTREAM_TIMEOUT_MS"),
        default: "600000",
        note: "建立连接与等待响应头的上限",
    },
    Field {
        key: "idleTimeoutMs",
        label: "流空闲超时",
        group: "超时",
        kind: Kind::Int {
            min: 0,
            max: 1_800_000,
            step: 30_000,
            unit: "ms",
        },
        env: Some("CC_IDLE_TIMEOUT_MS"),
        default: "120000",
        note: "两段流数据之间的最大间隔；0 = 不限制",
    },
    Field {
        key: "noOutputTimeoutMs",
        label: "首字节超时",
        group: "超时",
        kind: Kind::Int {
            min: 0,
            max: 1_800_000,
            step: 5_000,
            unit: "ms",
        },
        env: Some("CC_NO_OUTPUT_TIMEOUT_MS"),
        default: "30000",
        note: "上游一个字节都没回就重发；0 = 关闭",
    },
    Field {
        key: "noOutputRetries",
        label: "首字节重试",
        group: "超时",
        kind: Kind::Int {
            min: 0,
            max: 10,
            step: 1,
            unit: "次",
        },
        env: Some("CC_NO_OUTPUT_RETRIES"),
        default: "3",
        note: "重发次数上限，超过后把错误交给客户端",
    },
];

/// The field definition for `key`, if the page may edit it.
#[must_use]
pub fn field(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.key == key)
}

/// The documented default of one field.
#[must_use]
pub fn default_value(field: &Field) -> Value {
    serde_json::from_str(field.default).unwrap_or(Value::Null)
}

/// Validate and normalise one value for `key`.
///
/// Rejecting rather than clamping is deliberate: a quality of 500 is a typo,
/// and silently writing 100 leaves the user believing the number they typed is
/// in effect. The message names the bound so the page can show it verbatim.
pub fn validate(key: &str, value: &Value) -> Result<Value, String> {
    let Some(field) = field(key) else {
        return Err(format!("未知设置项：{key}"));
    };
    match &field.kind {
        Kind::Bool => match value {
            Value::Bool(b) => Ok(Value::Bool(*b)),
            _ => Err(format!("{} 需要一个布尔值", field.label)),
        },
        Kind::Int { min, max, unit, .. } => {
            let n = value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|v| u64::try_from(v).ok()))
                .ok_or_else(|| format!("{} 需要一个整数", field.label))?;
            if n < *min || n > *max {
                let bounds = if unit.is_empty() {
                    format!("{min}–{max}")
                } else {
                    format!("{min}–{max} {unit}")
                };
                return Err(format!("{} 超出范围（{bounds}）", field.label));
            }
            Ok(Value::from(n))
        }
        Kind::Text { max_len } => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("{} 需要一个字符串", field.label))?;
            let text = text.trim();
            if text.chars().count() > *max_len {
                return Err(format!("{} 过长（最多 {max_len} 个字符）", field.label));
            }
            Ok(Value::String(text.to_string()))
        }
        Kind::Choice(options) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("{} 需要一个字符串", field.label))?;
            if !options.contains(&text) {
                return Err(format!("{} 只能是 {}", field.label, options.join(" / ")));
            }
            Ok(Value::String(text.to_string()))
        }
    }
}

/// A loaded settings document.
pub struct Doc {
    path: PathBuf,
    values: Map<String, Value>,
    revision: String,
    /// The file existed but did not parse. The page warns; a save rewrites it
    /// from whatever could be understood (nothing), which is the only safe
    /// recovery — merging into garbage is not.
    corrupt: bool,
}

impl Doc {
    /// Read `path`, or start from an empty document when it is absent.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).ok();
        let revision = text
            .as_deref()
            .map_or_else(|| ABSENT_REVISION.to_string(), revision_of);
        let mut corrupt = false;
        let values = match text.as_deref().map(serde_json::from_str::<Value>) {
            Some(Ok(Value::Object(map))) => map,
            Some(Ok(_)) => {
                // A JSON scalar or array at the top level: syntactically valid,
                // structurally not ours.
                corrupt = true;
                Map::new()
            }
            Some(Err(_)) => {
                corrupt = true;
                Map::new()
            }
            None => Map::new(),
        };
        Self {
            path: path.to_path_buf(),
            values,
            revision,
            corrupt,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The revision of the bytes this document was loaded from (or last saved
    /// to). Compared against the running process's revision to answer "is this
    /// on disk but not yet in effect?".
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    #[must_use]
    pub fn is_corrupt(&self) -> bool {
        self.corrupt
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// A stored value, or the field's documented default.
    #[must_use]
    pub fn get_or_default(&self, key: &str) -> Option<Value> {
        let field = field(key)?;
        Some(
            self.values
                .get(key)
                .cloned()
                .unwrap_or_else(|| default_value(field)),
        )
    }

    /// Apply a patch of `key -> value`. `null` removes the key (restoring the
    /// default). Returns the keys that actually changed, sorted, so the caller
    /// can log precisely what moved.
    ///
    /// All-or-nothing: the first invalid entry aborts the whole patch, so a
    /// half-applied form cannot be saved.
    pub fn apply(&mut self, patch: &Map<String, Value>) -> Result<Vec<String>, String> {
        let mut staged = self.values.clone();
        let mut changed = Vec::new();
        for (key, value) in patch {
            if value.is_null() {
                if staged.remove(key).is_some() {
                    changed.push(key.clone());
                }
                continue;
            }
            let normalized = validate(key, value)?;
            if staged.get(key) != Some(&normalized) {
                staged.insert(key.clone(), normalized);
                changed.push(key.clone());
            }
        }
        self.values = staged;
        changed.sort();
        Ok(changed)
    }

    /// Insert one key as-is, bypassing [`validate`], and report whether it
    /// changed.
    ///
    /// For migrating keys this build does not model — the point of the
    /// unknown-key guarantee is that they are carried across, so routing them
    /// through the validator (which rejects every unknown key by design) would
    /// defeat it.
    pub fn apply_key(&mut self, key: &str, value: Value) -> bool {
        if self.values.get(key) == Some(&value) {
            return false;
        }
        self.values.insert(key.to_string(), value);
        true
    }

    /// Write the document, atomically.
    pub fn save(&mut self) -> std::io::Result<()> {
        let mut text = serde_json::to_string_pretty(&Value::Object(self.values.clone()))
            .map_err(std::io::Error::other)?;
        text.push('\n');
        let tmp = self.path.with_extension("json.tmp");
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&tmp, text.as_bytes())?;
        // Same directory, so the rename replaces the target rather than
        // crossing a volume boundary (which would not be atomic).
        std::fs::rename(&tmp, &self.path)?;
        self.revision = revision_of(&text);
        self.corrupt = false;
        Ok(())
    }

    /// The document plus its provenance, for the settings API.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut values = Map::new();
        for f in FIELDS {
            values.insert(
                f.key.to_string(),
                self.get_or_default(f.key).unwrap_or(Value::Null),
            );
        }
        serde_json::json!({
            "path": self.path.display().to_string(),
            "revision": self.revision,
            "corrupt": self.corrupt,
            "values": values,
        })
    }

    /// The field table the page renders from, with the live env shadow (if
    /// any) resolved for this process.
    #[must_use]
    pub fn fields_json(&self) -> Value {
        let fields: Vec<Value> = FIELDS
            .iter()
            .map(|f| {
                let mut obj = f.kind.describe();
                if let Value::Object(map) = &mut obj {
                    map.insert("key".into(), Value::from(f.key));
                    map.insert("label".into(), Value::from(f.label));
                    map.insert("group".into(), Value::from(f.group));
                    map.insert("note".into(), Value::from(f.note));
                    map.insert("default".into(), default_value(f));
                    map.insert(
                        "value".into(),
                        self.get_or_default(f.key).unwrap_or(Value::Null),
                    );
                    let shadow = f
                        .env
                        .and_then(|name| std::env::var(name).ok())
                        .filter(|v| !v.trim().is_empty());
                    map.insert("env".into(), f.env.map_or(Value::Null, Value::from));
                    map.insert("envValue".into(), shadow.map_or(Value::Null, Value::from));
                    // A restart is what makes a stored value take effect: the
                    // proxy reads this file once, at startup.
                    map.insert("restart".into(), Value::Bool(true));
                }
                obj
            })
            .collect();
        Value::Array(fields)
    }
}

/// A quoted 16-hex-character digest of `text`.
///
/// The same shape as the WebUI's asset ETag, and for the same reason: it has
/// to be comparable across processes, not secure. Sixteen hex characters make
/// an accidental collision between two successive edits implausible.
#[must_use]
pub fn revision_of(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The revision of the file at `path`, or [`ABSENT_REVISION`].
#[must_use]
pub fn revision_at(path: &Path) -> String {
    std::fs::read_to_string(path).map_or_else(|_| ABSENT_REVISION.to_string(), |t| revision_of(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ccproxy-settings-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_config_sits_beside_the_proxy_not_the_tray() {
        // The production layout: tray at the root, proxy under service/.
        let root = PathBuf::from(r"D:\CCProxy-Release\CCProxy-v0.7.7-rust");
        let tray = root.join("CCProxyTray.exe");
        let proxy = root.join("service").join("ccproxy.exe");
        assert_eq!(
            path_for_exe(&proxy),
            root.join("service").join("ccproxy.json"),
            "the proxy reads its own directory"
        );
        assert_eq!(
            path_for_proxy(&proxy),
            root.join("service").join("ccproxy.json"),
            "the tray must resolve the same file"
        );
        // Resolving from the tray's own path would be the old, divergent answer.
        assert_ne!(path_for_exe(&tray), path_for_proxy(&proxy));
    }

    #[test]
    fn a_patch_preserves_keys_this_build_does_not_model() {
        let dir = temp_dir("patch");
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"proxy":"direct","somethingFromTheFuture":{"a":1},"maxBodyBytes":1048576}"#,
        )
        .expect("seed");

        let mut doc = Doc::load(&path);
        let patch = serde_json::json!({"imageCompress": true});
        let changed = doc
            .apply(patch.as_object().expect("object"))
            .expect("valid patch");
        assert_eq!(changed, vec!["imageCompress".to_string()]);
        doc.save().expect("save");

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(on_disk.get("imageCompress"), Some(&Value::Bool(true)));
        assert_eq!(on_disk.get("proxy"), Some(&Value::from("direct")));
        assert_eq!(
            on_disk.get("somethingFromTheFuture"),
            Some(&serde_json::json!({"a": 1})),
            "an unknown key must survive an edit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_entry_aborts_the_whole_patch() {
        let dir = temp_dir("atomic-patch");
        let path = dir.join(FILE_NAME);
        let mut doc = Doc::load(&path);
        let patch = serde_json::json!({"logLevel": "debug", "imageCompressQuality": 500});
        let err = doc
            .apply(patch.as_object().expect("object"))
            .expect_err("500 is out of range");
        assert!(
            err.contains("压缩质量"),
            "the message names the field: {err}"
        );
        assert_eq!(
            doc.get("logLevel"),
            None,
            "nothing is applied when one entry is invalid"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_key_is_rejected_by_name() {
        let mut doc = Doc::load(&PathBuf::from("does-not-exist.json"));
        let patch = serde_json::json!({"noSuchSetting": 1});
        let err = doc
            .apply(patch.as_object().expect("object"))
            .expect_err("unknown keys are refused");
        assert!(err.contains("noSuchSetting"), "{err}");
    }

    #[test]
    fn null_removes_a_key_and_restores_the_default() {
        let dir = temp_dir("null");
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, r#"{"logLevel":"debug"}"#).expect("seed");
        let mut doc = Doc::load(&path);
        assert_eq!(doc.get_or_default("logLevel"), Some(Value::from("debug")));
        let patch = serde_json::json!({"logLevel": null});
        let changed = doc
            .apply(patch.as_object().expect("object"))
            .expect("valid");
        assert_eq!(changed, vec!["logLevel".to_string()]);
        assert_eq!(doc.get("logLevel"), None);
        assert_eq!(doc.get_or_default("logLevel"), Some(Value::from("info")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_changes_the_revision_and_leaves_no_temp_file() {
        let dir = temp_dir("revision");
        let path = dir.join(FILE_NAME);
        let mut doc = Doc::load(&path);
        assert_eq!(doc.revision(), ABSENT_REVISION, "absent file");
        doc.apply(
            serde_json::json!({"imageCompress": true})
                .as_object()
                .expect("object"),
        )
        .expect("valid");
        doc.save().expect("save");
        assert_ne!(doc.revision(), ABSENT_REVISION);
        assert_eq!(
            doc.revision(),
            revision_at(&path),
            "the in-memory revision must match the file's"
        );
        assert!(!path.with_extension("json.tmp").exists(), "temp renamed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_is_flagged_and_rewritten_cleanly() {
        let dir = temp_dir("corrupt");
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "{ not json").expect("seed");
        let mut doc = Doc::load(&path);
        assert!(doc.is_corrupt());
        doc.apply(
            serde_json::json!({"imageCompress": true})
                .as_object()
                .expect("object"),
        )
        .expect("valid");
        doc.save().expect("save");
        assert!(!doc.is_corrupt());
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(on_disk.get("imageCompress"), Some(&Value::Bool(true)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_field_has_a_default_that_validates() {
        // The page offers "restore default" on every row, so a default that
        // its own validator rejects would be a dead button.
        for field in FIELDS {
            let default = default_value(field);
            assert!(
                validate(field.key, &default).is_ok(),
                "default for {} is invalid: {default}",
                field.key
            );
        }
    }

    #[test]
    fn text_values_are_trimmed_and_bounded() {
        let ok = validate("proxy", &Value::from("  direct  ")).expect("valid");
        assert_eq!(ok, Value::from("direct"));
        let long = "x".repeat(201);
        assert!(validate("proxy", &Value::from(long)).is_err());
        assert!(validate("proxy", &Value::from(7)).is_err());
    }

    #[test]
    fn a_choice_rejects_a_value_outside_its_list() {
        assert!(validate("logLevel", &Value::from("verbose")).is_err());
        assert_eq!(
            validate("logLevel", &Value::from("debug")).expect("valid"),
            Value::from("debug")
        );
    }
}
