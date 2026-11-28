//! The directory serializes binding selection and worker retirement. A revision
//! carries a committed picker effort to the owner's preferences even after a crash.
use super::{state, SessionDb};
use crate::native_engine::session_binding::{store, BindingSnapshot};
use anyhow::{Context, Result};
use rusqlite::{OpenFlags, OptionalExtension};

pub(super) struct BindingControl {
    pub pending: bool,
    pub retiring_here: bool,
    effort: Option<(i64, Option<String>)>,
}

impl SessionDb {
    pub fn observe_provider(&self) -> Result<store::Observed> {
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.stores.global().read(|c| store::read(c, self.id()))
    }

    pub fn publish_provider(
        &self,
        expected: u64,
        selection: &BindingSnapshot,
        retiring: Option<(&str, Option<u32>)>,
        requester: &str,
        picker_effort: Option<Option<&str>>,
    ) -> Result<store::Write> {
        let start = std::time::Instant::now();
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        let _admission = self.provider_fence(true, crate::db::STARTUP_WRITE_DEADLINE)?;
        let result = self
            .stores
            .global()
            .connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                crate::db::write_tx(
                    c,
                    store::PUBLISH_LABEL,
                    crate::db::STARTUP_WRITE_DEADLINE.saturating_sub(start.elapsed()),
                    |tx| {
                        store::publish_choice_in(
                            tx,
                            self.id(),
                            expected,
                            selection,
                            retiring,
                            requester,
                            picker_effort,
                            store::BindingStore::Directory,
                        )
                    },
                )
            })?;
        Ok(result)
    }

    pub fn activate_provider(&self, worker: &str, generation: u64) -> Result<()> {
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        let _admission = self.provider_fence(true, crate::db::STARTUP_WRITE_DEADLINE)?;
        self.stores
            .global()
            .connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                store::activate_on(
                    c,
                    self.id(),
                    worker,
                    generation,
                    store::BindingStore::Directory,
                )
            })
    }

    /// Hold the execution parent's publication fence across an owned child's inbox commit.
    /// A deleted or replaced parent can never publish a selector for this child again.
    pub(super) fn peer_parent_fence(&self) -> Result<Option<std::fs::File>> {
        let parent=self.stores.global().read(|c|Ok(c.query_row(
            "SELECT p.id,p.storage_key,p.incarnation FROM session_execution_parents e
             JOIN conversations p ON p.id=e.parent_id AND p.incarnation=e.parent_incarnation WHERE e.child_id=?1",
            [&self.id], |r|Ok(SessionDb{stores:self.stores.clone(),id:r.get(0)?,key:r.get(1)?,incarnation:r.get(2)?}))
            .optional()?))?;
        let Some(parent) = parent else {
            return Ok(None);
        };
        let guard = parent.provider_fence(false, crate::db::STARTUP_WRITE_DEADLINE);
        let retiring:Option<bool>=self.stores.global().read(|c|Ok(c.query_row(
            "SELECT coalesce(b.pending IS NOT NULL AND b.legacy_pid=?3,0) FROM conversations p
             LEFT JOIN cli_binding_generations b ON b.session_id=p.id WHERE p.id=?1 AND p.incarnation=?2",
            rusqlite::params![parent.id,parent.incarnation,std::process::id()],|r|r.get(0)).optional()?))?;
        let Some(retiring) = retiring else {
            return Ok(None);
        };
        let guard = guard?;
        anyhow::ensure!(
            !retiring,
            "Provider switch pending; owned child inbox retained"
        );
        Ok(Some(guard))
    }

    pub(super) fn binding_control(&self) -> Result<BindingControl> {
        let control = self.stores.global().read(|c| Ok(c.query_row(
            "SELECT b.pending IS NOT NULL,e.revision,e.effort,coalesce(b.pending IS NOT NULL AND b.legacy_pid=?4,0) FROM conversations d
             LEFT JOIN cli_binding_generations b ON b.session_id=d.id LEFT JOIN cli_binding_efforts e ON e.session_id=d.id
             WHERE d.id=?1 AND d.incarnation=?2 AND d.storage_key=?3",
            rusqlite::params![self.id,self.incarnation,self.key,std::process::id()], |r| Ok(BindingControl {
                pending: r.get(0)?, retiring_here:r.get(3)?, effort:r.get::<_,Option<i64>>(1)?.map(|rev|r.get(2).map(|value|(rev,value))).transpose()?,
            })).optional()?.context("Session incarnation is no longer visible")?))?;
        if let Some((revision, effort)) = &control.effort {
            let applied = |c: &rusqlite::Connection| -> Result<i64> {
                Ok(c.query_row("SELECT coalesce((SELECT CAST(value AS INTEGER) FROM peripheral_settings WHERE key='binding.effort-revision'),0)", [], |r|r.get(0))?)
            };
            if self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| applied(c))?
                < *revision
            {
                self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                    crate::db::write_tx(c,"apply model picker effort",crate::db::STARTUP_WRITE_DEADLINE,|tx| {
                        if applied(tx)? < *revision {
                            tx.execute("INSERT INTO peripheral_settings(key,value) VALUES('cli.session',json_object('effort',?1))
                                ON CONFLICT(key) DO UPDATE SET value=json_set(value,'$.effort',?1)", [effort])?;
                            state::set_local(tx,"binding.effort-revision",&revision.to_string())?;
                            super::history::advance(tx)?;
                        }
                        Ok(())
                    })
                })?;
            }
        }
        Ok(control)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(super) struct ClearBinding {
    generation: u64,
    provider: String,
    model: String,
    digest: String,
    options: String,
}
impl ClearBinding {
    pub(super) fn from_snapshot(snapshot: &BindingSnapshot) -> Result<Self> {
        anyhow::ensure!(
            !snapshot.observation_only,
            "Cannot clear with unavailable session credentials"
        );
        let active = snapshot.active()?;
        let model = active
            .provider
            .models
            .iter()
            .find(|m| m.id == active.model.id)
            .context("Selected model unavailable")?;
        Ok(Self {
            generation: snapshot.generation,
            provider: snapshot.provider_id.clone(),
            model: snapshot.model.clone(),
            options: serde_json::to_string(&(model, &model.effort))?,
            digest: store::fingerprint(snapshot)?,
        })
    }
    pub(super) fn publish(
        &self,
        tx: &rusqlite::Connection,
        parent: &str,
        child: &str,
    ) -> Result<()> {
        let current = store::read(tx, parent)?;
        anyhow::ensure!(
            current.generation == self.generation && current.pending.is_none(),
            "Clear did not apply: the source binding changed or is pending"
        );
        tx.execute(
            "UPDATE conversations SET provider=?1,model=?2 WHERE id=?3",
            [&self.provider, &self.model, child],
        )?;
        tx.execute("INSERT INTO cli_binding_generations(session_id,generation,target_provider,target_model) VALUES(?1,1,?2,?3)",[child,&self.provider,&self.model])?;
        tx.execute("INSERT INTO cli_binding_revisions(session_id,generation,previous,digest,options) VALUES(?1,1,0,?2,?3)",[child,&self.digest,&self.options])?;
        Ok(())
    }
}

impl super::Stores {
    /// Snapshot the published columns and the worker-retirement fence together.
    pub fn stored_binding(
        &self,
        id: &str,
    ) -> Result<(crate::db::stored_binding::StoredBinding, bool)> {
        self.global()
            .read_directory_row(id, |c| {
                Ok(c.query_row(
                    "SELECT c.provider,c.model,g.pending IS NOT NULL FROM conversations c
                 LEFT JOIN cli_binding_generations g ON g.session_id=c.id WHERE c.id=?1",
                    [id],
                    |row| {
                        Ok((
                            crate::db::stored_binding::StoredBinding::from_row(
                                row.get(0)?,
                                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                            ),
                            row.get(2)?,
                        ))
                    },
                )
                .optional()?)
            })?
            .context("Session not found")
    }

    pub fn observed_binding(&self,id:&str)->Result<store::Observed>{
        self.global().read_directory_row(id,|c|store::read(c,id).map(Some))?.context("Session not found")
    }
    pub fn binding_notice(&self,id:&str,requester:&str)->Result<Option<(u64,String,String,bool)>> {
        self.global().read_directory_row(id,|c|store::frontend_failure(c,id,requester))
    }
    pub fn acknowledge_binding(&self,id:&str,requester:&str,generation:u64)->Result<()> {
        self.global().write("acknowledge binding notice",id,false,|tx|store::acknowledge(tx,id,requester,generation))
    }
    pub fn release_binding_requester(&self,requester:&str)->Result<()> {
        self.global().connection(OpenFlags::SQLITE_OPEN_READ_WRITE,|c|store::release_requester(c,requester))
    }
}
