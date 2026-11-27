//! One record per live stdio instance, independent of the supervisor's worker.json.
//! Reserve the token before spawn so even a killed engine leaves an identifiable owner.
use super::{drain, proc_file, Kernel, Members, Scope, INSTANCE_ENV};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use uuid::Uuid;

static SESSION: OnceLock<SessionFences> = OnceLock::new();

struct SessionFences {
    directory: PathBuf,
    boot: Uuid,
    predecessors: Mutex<Vec<Fence>>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Fence {
    server: String,
    owner: Uuid,
    instance: Uuid,
    boot: Uuid,
    members: Members,
}

impl Fence {
    fn entry(&self) -> String {
        format!("{INSTANCE_ENV}={}:{}", self.owner, self.instance)
    }

    fn path(&self, directory: &Path) -> PathBuf {
        directory.join(format!("{}.json", self.instance))
    }

    fn save(&self, directory: &Path) -> Result<()> {
        crate::services::background_sessions::save(&self.path(directory), self)
    }

    fn clear(&self, directory: &Path) -> Result<()> {
        match fs::remove_file(self.path(directory)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            result => result?,
        }
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    }
}

fn load(directory: &Path) -> Result<Vec<Fence>> {
    let mut fences = Vec::new();
    for file in fs::read_dir(directory)? {
        let path = file?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let fence: Fence = serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("Invalid MCP fence {}", path.display()))?;
            anyhow::ensure!(fence.path(directory) == path, "MCP fence identity mismatch");
            fences.push(fence);
        }
    }
    Ok(fences)
}

pub(super) fn initialize(session_dir: &Path) -> Result<()> {
    let directory = session_dir.join("mcp-fences");
    fs::create_dir_all(&directory)?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .parse()?;
    let predecessors = load(&directory)?.into();
    SESSION
        .set(SessionFences {
            directory,
            boot,
            predecessors,
        })
        .map_err(|_| anyhow::anyhow!("MCP session fences already initialized"))
}

pub(crate) fn drain_predecessors(server: &str) -> Result<()> {
    let Some(session) = SESSION.get() else {
        return Ok(());
    };
    let mut fences = session.predecessors.lock().unwrap();
    while let Some(index) = fences.iter().position(|f| f.server == server) {
        let fence = &mut fences[index];
        let entry = fence.entry();
        if fence.boot != session.boot {
            // Start times identify a PID only within the boot that recorded it.
            fence.members = Members::default();
            fence.boot = session.boot;
        }
        if let Err(error) = drain(
            &mut fence.members,
            Scope::Predecessor { entry: &entry },
            &Kernel,
            |_| Ok(true),
        ) {
            fence.save(&session.directory)?;
            anyhow::bail!("MCP server {server} could not be stopped: {error}");
        }
        fence.clear(&session.directory)?;
        fences.swap_remove(index);
    }
    Ok(())
}

pub(super) fn reserve(server: &str, owner: Uuid, instance: Uuid) -> Result<Option<Fence>> {
    let Some(session) = SESSION.get() else {
        return Ok(None);
    };
    let fence = Fence {
        server: server.into(),
        owner,
        instance,
        boot: session.boot,
        members: Members::default(),
    };
    fence.save(&session.directory)?;
    Ok(Some(fence))
}

impl Fence {
    pub(super) fn clear_reservation(&self) -> Result<()> {
        self.clear(&SESSION.get().unwrap().directory)
    }

    pub(super) fn finish(&mut self, result: &std::io::Result<()>, members: &Members) -> Result<()> {
        if result.is_ok() {
            self.clear_reservation()
        } else {
            self.members = members.clone();
            self.save(&SESSION.get().unwrap().directory)
        }
    }
}

/// The engine may have died before it observed a detached survivor. Associate
/// the supervisor's observations with the original reservations by exact token.
pub(super) fn supervisor_finished(
    session_dir: &Path,
    owner: &str,
    members: &Members,
    drained: bool,
) -> Result<()> {
    let directory = session_dir.join("mcp-fences");
    if !directory.try_exists()? {
        return Ok(());
    }
    let owner = Uuid::parse_str(owner)?;
    for mut fence in load(&directory)?.into_iter().filter(|f| f.owner == owner) {
        // An instance drain can know tokenless members that a supervisor pass
        // never saw. Leave those records to be checked on admission.
        if drained && fence.members.0.is_empty() {
            fence.clear(&directory)?;
        } else if !drained {
            let entry = fence.entry();
            for (&pid, &birth) in &members.0 {
                if proc_file(pid, "environ")
                    .is_ok_and(|env| env.split(|&b| b == 0).any(|e| e == entry.as_bytes()))
                {
                    fence.members.0.insert(pid, birth);
                }
            }
            fence.save(&directory)?;
        }
    }
    Ok(())
}
