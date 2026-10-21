//! Agent logins: sign in to Claude Code, Codex, Gemini or OpenCode once per server,
//! and every VM is signed in.
//!
//! Each agent CLI keeps its login in a few files in the home folder. A fresh VM has
//! none, so Cove keeps a copy of each agent's files on the server
//! (`logins/<agent>.json`, readable only by Cove) and keeps every VM in step with it:
//!
//! - a VM that starts gets the saved files;
//! - a VM where someone signed in, or where the agent refreshed its token, hands the
//!   new files back, and the other VMs get them on their next sync;
//! - signing out (`cove agents logout`) removes the files from the VMs too.
//!
//! Syncing compares each file in a VM with what Cove saw there last time
//! (`vms/<name>/logins.json`, hashes only), so it can tell which side changed.

use crate::paths::Paths;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// SHA-256 of a file's content, hex.
pub fn digest(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An agent CLI whose login Cove can keep.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    /// The CLI's program name.
    pub bin: &'static str,
    /// Its login files, relative to the home folder.
    pub files: &'static [&'static str],
    /// What signs it in on a machine without a browser.
    pub login: &'static [&'static str],
    /// Variables the login command needs, such as one that asks for a code instead of
    /// opening a browser.
    #[serde(skip)]
    pub login_env: &'static [(&'static str, &'static str)],
    /// How to install the CLI when the VM has none.
    #[serde(skip)]
    pub install: &'static str,
    /// What to do in the login command, for the terminal.
    pub hint: &'static str,
}

const NPM: &str = "command -v npm >/dev/null 2>&1 || { apt-get update -qq && apt-get install -y -qq nodejs npm; }";

pub const AGENTS: [Agent; 5] = [
    Agent {
        id: "claude",
        name: "Claude Code",
        bin: "claude",
        files: &[".claude/.credentials.json"],
        login: &["claude"],
        login_env: &[],
        install: "curl -fsSL https://claude.ai/install.sh | bash",
        hint: "Pick a login method, open the link it shows on any device and paste the code back here. Type /exit when it's done.",
    },
    Agent {
        id: "codex",
        name: "Codex",
        bin: "codex",
        files: &[".codex/auth.json"],
        login: &["codex", "login", "--device-auth"],
        login_env: &[],
        install: "{ NPM } && npm install -g @openai/codex",
        hint: "Open the link it shows on any device and enter the code.",
    },
    Agent {
        id: "gemini",
        name: "Gemini",
        bin: "gemini",
        files: &[
            ".gemini/oauth_creds.json",
            ".gemini/google_accounts.json",
            ".gemini/settings.json",
        ],
        login: &["gemini"],
        login_env: &[("NO_BROWSER", "true")],
        install: "{ NPM } && npm install -g @google/gemini-cli",
        hint: "Pick Login with Google, open the link on any device and paste the code back here. Type /quit when it's done.",
    },
    Agent {
        id: "opencode",
        name: "OpenCode",
        bin: "opencode",
        files: &[".local/share/opencode/auth.json"],
        login: &["opencode", "auth", "login"],
        login_env: &[],
        install: "curl -fsSL https://opencode.ai/install | bash",
        hint: "Pick a provider and follow its steps.",
    },
    Agent {
        id: "qwen",
        name: "Qwen Code",
        bin: "qwen",
        files: &[".qwen/oauth_creds.json", ".qwen/settings.json"],
        login: &["qwen"],
        login_env: &[],
        install: "{ NPM } && npm install -g @qwen-code/qwen-code",
        hint: "Pick Qwen OAuth, open the link on any device and sign in. Type /quit when it's done.",
    },
];

pub fn agent(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

impl Agent {
    /// A `bash -lc` script that installs the CLI if the VM has none, then runs the
    /// login command (`"$@"`).
    pub fn login_script(&self) -> String {
        let install = self.install.replace("{ NPM }", NPM);
        format!(
            r#"export PATH="$HOME/.local/bin:$HOME/.opencode/bin:$PATH"
if ! command -v {bin} >/dev/null 2>&1; then
  echo "Installing {name}…"
  {{ command -v curl >/dev/null 2>&1 || {{ apt-get update -qq && apt-get install -y -qq curl ca-certificates; }}; }} >/dev/null
  {{ {install}; }} || {{ echo "Couldn't install {name}." >&2; exit 127; }}
fi
exec "$@""#,
            bin = self.bin,
            name = self.name,
        )
    }
}

/// Where an agent's files live in a VM: the guest runs everything as root.
pub const GUEST_HOME: &str = "/root";

/// One agent's saved login.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Saved {
    /// File path (from the home folder) to its content, base64.
    pub files: BTreeMap<String, cove_proto::Bytes>,
    pub updated_at: u64,
    /// The VM it last came from, or `import`.
    #[serde(default)]
    pub from: String,
}

/// An agent's login as clients see it: never the files themselves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoginInfo {
    pub agent: String,
    pub name: String,
    pub signed_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub login: Vec<String>,
    #[serde(default)]
    pub hint: String,
}

fn path(paths: &Paths, agent: &str) -> std::path::PathBuf {
    paths.logins_dir().join(format!("{agent}.json"))
}

pub fn load(paths: &Paths, agent: &str) -> Option<Saved> {
    let text = std::fs::read_to_string(path(paths, agent)).ok()?;
    serde_json::from_str::<Saved>(&text)
        .ok()
        .filter(|s| !s.files.is_empty())
}

fn write_private(file: &std::path::Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
        crate::paths::restrict(dir, 0o700);
    }
    let tmp = file.with_extension("json.tmp");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
    }
    crate::paths::restrict(&tmp, 0o600);
    std::fs::rename(&tmp, file)?;
    Ok(())
}

pub fn save(paths: &Paths, agent: &str, saved: &Saved) -> Result<()> {
    write_private(&path(paths, agent), &serde_json::to_vec_pretty(saved)?)
}

/// Store files for an agent from elsewhere (another computer's login). Only the
/// agent's own login files are taken.
pub fn import(paths: &Paths, id: &str, files: BTreeMap<String, cove_proto::Bytes>) -> Result<()> {
    let Some(agent) = agent(id) else {
        bail!("Cove doesn't keep logins for {id:?}; it knows {}", known());
    };
    if files.is_empty() {
        bail!("no login files given");
    }
    for name in files.keys() {
        if !agent.files.contains(&name.as_str()) {
            bail!(
                "{name} is not one of {}'s login files ({})",
                agent.name,
                agent.files.join(", ")
            );
        }
    }
    let mut saved = load(paths, id).unwrap_or_default();
    saved.files.extend(files);
    saved.updated_at = crate::now();
    saved.from = "import".into();
    save(paths, id, &saved)
}

/// Sign an agent out: forget its files. VMs drop them on their next sync.
pub fn remove(paths: &Paths, id: &str) -> Result<bool> {
    if agent(id).is_none() {
        bail!("Cove doesn't keep logins for {id:?}; it knows {}", known());
    }
    match std::fs::remove_file(path(paths, id)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn known() -> String {
    AGENTS.iter().map(|a| a.id).collect::<Vec<_>>().join(", ")
}

pub fn list(paths: &Paths) -> Vec<LoginInfo> {
    AGENTS
        .iter()
        .map(|a| {
            let saved = load(paths, a.id);
            LoginInfo {
                agent: a.id.into(),
                name: a.name.into(),
                // The first file is the login itself; the others are settings.
                signed_in: saved
                    .as_ref()
                    .is_some_and(|s| s.files.contains_key(a.files[0])),
                updated_at: saved.as_ref().map(|s| s.updated_at),
                from: saved.as_ref().map(|s| s.from.clone()).unwrap_or_default(),
                files: a.files.iter().map(|f| f.to_string()).collect(),
                login: a.login.iter().map(|f| f.to_string()).collect(),
                hint: a.hint.into(),
            }
        })
        .collect()
}

/// What a VM had of each login file at its last sync: agent → file → hash.
pub type Seen = BTreeMap<String, BTreeMap<String, String>>;

pub fn load_seen(vm_dir: &std::path::Path) -> Seen {
    std::fs::read_to_string(vm_dir.join("logins.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_seen(vm_dir: &std::path::Path, seen: &Seen) -> Result<()> {
    write_private(&vm_dir.join("logins.json"), &serde_json::to_vec(seen)?)
}

/// What to do with one login file in one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Both sides agree.
    Keep,
    /// The VM has a new version: save it for every VM.
    Save,
    /// Give the VM the saved version.
    Push,
    /// The agent was signed out: delete the VM's copy.
    Remove,
}

/// Decide from the VM's copy, the saved copy and what the VM had last time.
pub fn decide(in_vm: Option<&[u8]>, saved: Option<&[u8]>, last: Option<&str>) -> Step {
    let vm_hash = in_vm.map(digest);
    let saved_hash = saved.map(digest);
    if vm_hash == saved_hash {
        return Step::Keep;
    }
    let changed_in_vm = vm_hash.as_deref() != last;
    match (vm_hash.is_some(), saved_hash.is_some()) {
        // Signed in, or a token refreshed, in this VM. A VM seen for the first time
        // takes the saved login instead: its file is older (a snapshot's, say).
        (true, _) if changed_in_vm && (last.is_some() || saved_hash.is_none()) => Step::Save,
        (_, true) => Step::Push,
        // Saved nothing and the VM's copy is the one it had: signed out elsewhere.
        (true, false) if !changed_in_vm => Step::Remove,
        _ => Step::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_steps() {
        let a: &[u8] = b"token-a";
        let b: &[u8] = b"token-b";
        let ha = digest(a);
        let hb = digest(b);
        // A new VM gets the saved login.
        assert_eq!(decide(None, Some(a), None), Step::Push);
        // Someone signed in inside a VM: saved for the others.
        assert_eq!(decide(Some(a), None, None), Step::Save);
        // The agent refreshed its token in the VM.
        assert_eq!(decide(Some(b), Some(a), Some(&ha)), Step::Save);
        // Another VM refreshed it: this one catches up.
        assert_eq!(decide(Some(a), Some(b), Some(&ha)), Step::Push);
        // A VM seen for the first time with an old copy takes the saved one.
        assert_eq!(decide(Some(a), Some(b), None), Step::Push);
        // Signed out: the VM's unchanged copy goes.
        assert_eq!(decide(Some(a), None, Some(&ha)), Step::Remove);
        // ...unless someone signed in there again since.
        assert_eq!(decide(Some(b), None, Some(&ha)), Step::Save);
        // Deleted in the VM: it gets the saved one back.
        assert_eq!(decide(None, Some(a), Some(&ha)), Step::Push);
        assert_eq!(decide(Some(b), Some(b), Some(&hb)), Step::Keep);
        assert_eq!(decide(None, None, Some(&ha)), Step::Keep);
    }

    #[test]
    fn saved_logins_are_private_and_listed() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path());
        assert!(!list(&paths)[0].signed_in);
        let mut files = BTreeMap::new();
        files.insert(
            ".codex/auth.json".to_string(),
            cove_proto::Bytes(b"{}".to_vec()),
        );
        import(&paths, "codex", files).unwrap();
        let codex = list(&paths)
            .into_iter()
            .find(|l| l.agent == "codex")
            .unwrap();
        assert!(codex.signed_in);
        assert_eq!(codex.from, "import");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(paths.logins_dir().join("codex.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Only the agent's own files.
        let mut other = BTreeMap::new();
        other.insert(
            ".ssh/authorized_keys".to_string(),
            cove_proto::Bytes::default(),
        );
        assert!(import(&paths, "codex", other).is_err());
        assert!(remove(&paths, "codex").unwrap());
        assert!(!remove(&paths, "codex").unwrap());
        assert!(remove(&paths, "nope").is_err());
    }

    #[test]
    fn login_script_installs_then_runs() {
        let s = agent("codex").unwrap().login_script();
        assert!(s.contains("npm install -g @openai/codex"));
        assert!(s.contains("apt-get install -y -qq nodejs npm"));
        assert!(s.ends_with(r#"exec "$@""#));
    }
}
